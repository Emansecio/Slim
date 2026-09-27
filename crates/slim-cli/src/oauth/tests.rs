use super::*;

use std::future::Future;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::task::Poll;
use std::thread;

use serde_json::json;

use crate::cli::{append_oauth_warnings, refresh_headless_oauth_with_service, CliOutput};
use crate::ExitCode;

#[derive(Clone, Copy)]
struct TestBrowser;

impl BrowserLauncher for TestBrowser {
    fn open(&self, _url: &str) -> Result<(), OAuthError> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_refresh_write_keeps_the_successor_and_reconciles_without_another_post() {
    let root = temp_root("pending-refresh");
    std::fs::create_dir_all(&root).unwrap();
    let auth_path = root.join("auth.json");
    let old = OAuthCredential {
        access: "old-access-secret-fixture".into(),
        refresh: "old-refresh-secret-fixture".into(),
        expires: 1,
        account_id: Some("account-fixture".into()),
    };
    write_auth_with_sibling(&auth_path, &old);
    let store = OAuthStore::at(&auth_path);
    store.fail_refresh_persistence(true);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let posts = thread::spawn(move || {
        let (mut stream, _) = accept_fixture(&listener);
        assert!(read_request(&mut stream).contains("old-refresh-secret-fixture"));
        write_refresh_response(
            &mut stream,
            "new-access-secret-fixture",
            "new-refresh-secret-fixture",
            3600,
        );
        1
    });
    let service = service(format!("http://{address}"), store.clone());

    let first = service
        .fresh_credential(OAuthProvider::Anthropic, old.clone())
        .await
        .expect("valid response remains usable in memory after persistence fails");
    assert_eq!(first.credential.access, "new-access-secret-fixture");
    assert!(first.persistence_warning.is_some());
    assert_eq!(
        store.credential(OAuthProvider::Anthropic).unwrap(),
        Some(old.clone())
    );

    let second = service
        .fresh_credential(OAuthProvider::Anthropic, old.clone())
        .await
        .expect("the pending successor is shared with later lookups");
    assert_eq!(second.credential, first.credential);
    assert!(second.persistence_warning.is_some());
    assert_eq!(
        posts.join().unwrap(),
        1,
        "a pending successor must not refresh again"
    );

    store.fail_refresh_persistence(false);
    let reconciled = service
        .fresh_credential(OAuthProvider::Anthropic, old)
        .await
        .expect("retry persistence without another remote refresh");
    assert_eq!(reconciled.credential, first.credential);
    assert!(
        reconciled.persistence_warning.is_none(),
        "successful reconciliation clears the persistence warning"
    );
    assert_eq!(
        store.credential(OAuthProvider::Anthropic).unwrap(),
        Some(first.credential.clone())
    );
    let auth = std::fs::read_to_string(&auth_path).unwrap();
    assert!(auth.contains("sibling-api-key-fixture"));
    assert!(auth.contains("openai-codex"));
    assert_eq!(
        store.active_provider_key().unwrap().as_deref(),
        Some("openai-codex"),
        "background refresh persistence must not activate its provider"
    );
    let shutdown_warnings = service.shutdown().await;
    assert!(
        shutdown_warnings.is_empty(),
        "shutdown after reconciliation has no persistence warning"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_deadline_releases_pending_successor_without_late_auth_lock_acquisition() {
    let root = temp_root("shutdown-deadline-pending");
    std::fs::create_dir_all(&root).unwrap();
    let auth_path = root.join("auth.json");
    let store = OAuthStore::at(&auth_path);
    let provider = OAuthProvider::Anthropic;
    let old = OAuthCredential {
        access: "shutdown-old-access-fixture".into(),
        refresh: "shutdown-old-refresh-fixture".into(),
        expires: 1,
        account_id: Some("shutdown-account-fixture".into()),
    };
    let successor = OAuthCredential {
        access: "shutdown-successor-access-fixture".into(),
        refresh: "shutdown-successor-refresh-fixture".into(),
        expires: now_ms() / 1000 + 3600,
        account_id: old.account_id.clone(),
    };
    store.save(provider, &old).unwrap();
    let (_, snapshot) = store.credential_snapshot(provider).unwrap();

    store.fail_refresh_persistence(true);
    let persistence_error = store
        .persist_refresh_locked_if_unchanged(provider, &snapshot, &successor)
        .expect_err("fixture must exercise a real injected persistence failure");
    store.fail_refresh_persistence(false);
    assert!(persistence_error
        .to_string()
        .contains("injected OAuth persistence failure"));

    let service = service("http://127.0.0.1:1".into(), store.clone());
    let refresh_guard = store.lock_refresh(provider, || false).unwrap();
    service
        .worker
        .register_successor(provider, snapshot, 0, successor.clone(), refresh_guard)
        .unwrap();
    {
        let mut state = lock_mutex(&service.worker.shared);
        let pending = state.pending[background_inflight_slot(provider)]
            .as_mut()
            .expect("test setup must leave the failed successor pending");
        pending.warning = Some(format!(
            "OAuth refreshed in memory but was not persisted: {persistence_error}"
        ));
    }

    // Windows auth-store locking is a real exclusive file handle. Keep it
    // through the short shutdown deadline, then release it only after shutdown
    // has abandoned reconciliation.
    let held_auth_lock = store.lock_exclusive().unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(180);
    let started = tokio::time::Instant::now();
    let warnings = service.shutdown_until(deadline).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "shutdown must return within the short test deadline"
    );
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("injected OAuth persistence failure")),
        "shutdown must report the original persistence failure: {warnings:?}"
    );
    assert!(
        warnings.iter().any(|warning| {
            warning.contains("auth store lock timed out") || warning.contains("shutdown deadline")
        }),
        "shutdown must report its bounded reconciliation failure: {warnings:?}"
    );
    let pending_released = {
        let state = lock_mutex(&service.worker.shared);
        state.pending[background_inflight_slot(provider)].is_none()
    };
    assert!(
        pending_released,
        "shutdown must release the pending successor and refresh lock"
    );

    // Give the deadline-aware blocking acquisition time to observe expiration
    // while the external lock remains held. Once released, the store is
    // immediately available and the old persisted OAuth value is unchanged.
    tokio::time::sleep(Duration::from_millis(250)).await;
    drop(held_auth_lock);
    let probe_started = std::time::Instant::now();
    let probe_lock = store.lock_exclusive().unwrap();
    assert!(
        probe_started.elapsed() < Duration::from_millis(500),
        "an abandoned blocking selection must not retain or reacquire the auth lock"
    );
    drop(probe_lock);
    assert_eq!(store.credential(provider).unwrap(), Some(old));
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_background_refresh_retains_a_failed_successor_until_reconciliation() {
    let root = temp_root("idle-background");
    std::fs::create_dir_all(&root).unwrap();
    let auth_path = root.join("auth.json");
    let now = now_ms() / 1000;
    let live = OAuthCredential {
        access: "background-old-access-fixture".into(),
        refresh: "background-old-refresh-fixture".into(),
        expires: now + 120,
        account_id: Some("background-account-fixture".into()),
    };
    let store = OAuthStore::at(&auth_path);
    store.save(OAuthProvider::Anthropic, &live).unwrap();
    store.fail_refresh_persistence(true);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = accept_fixture(&listener);
        assert!(read_request(&mut stream).contains("background-old-refresh-fixture"));
        write_refresh_response(
            &mut stream,
            "background-new-access-fixture",
            "background-new-refresh-fixture",
            3600,
        );
    });
    let service = service(format!("http://{address}"), store.clone());

    let returned = service
        .fresh_credential(OAuthProvider::Anthropic, live.clone())
        .await
        .expect("initial background refresh does not delay the live credential");
    assert_eq!(returned.credential, live);
    server.join().unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let current = service
            .fresh_credential(OAuthProvider::Anthropic, returned.credential.clone())
            .await;
        if current
            .as_ref()
            .is_ok_and(|fresh| fresh.credential.access == "background-new-access-fixture")
        {
            assert!(current.unwrap().persistence_warning.is_some());
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "background owner must retain its successor"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    store.fail_refresh_persistence(false);
    let successor = service
        .fresh_credential(OAuthProvider::Anthropic, returned.credential)
        .await
        .unwrap();
    assert_eq!(successor.credential.access, "background-new-access-fixture");
    assert_eq!(
        store
            .credential(OAuthProvider::Anthropic)
            .unwrap()
            .unwrap()
            .refresh,
        "background-new-refresh-fixture"
    );
    let _ = service.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn autonomous_reconciler_releases_pending_owner_after_other_service_logout() {
    let root = temp_root("idle-owner-logout");
    std::fs::create_dir_all(&root).unwrap();
    let auth_path = root.join("auth.json");
    let provider = OAuthProvider::Anthropic;
    let live = OAuthCredential {
        access: "logout-owner-old-access-fixture".into(),
        refresh: "logout-owner-old-refresh-fixture".into(),
        expires: now_ms() / 1000 + 120,
        account_id: Some("logout-owner-account-fixture".into()),
    };
    let owner_store = OAuthStore::at(&auth_path);
    owner_store.save(provider, &live).unwrap();
    owner_store.fail_refresh_persistence(true);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (finish_server_tx, finish_server_rx) = std::sync::mpsc::channel::<()>();
    let server = thread::spawn(move || {
        let (mut stream, _) = accept_fixture(&listener);
        let first_request = read_request(&mut stream);
        assert!(first_request.contains("logout-owner-old-refresh-fixture"));
        write_refresh_response(
            &mut stream,
            "logout-owner-next-access-fixture",
            "logout-owner-next-refresh-fixture",
            3600,
        );
        let mut posts = 1;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if finish_server_rx.try_recv().is_ok() {
                return posts;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    posts += 1;
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let _ = read_request(&mut stream);
                    let body = r#"{"error":"unexpected_extra_post"}"#;
                    let _ = stream.write_all(
                        format!(
                            "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "test must finish while the loopback fixture remains available"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("loopback accept failed: {error}"),
            }
        }
    });

    let owner = service(format!("http://{address}"), owner_store.clone());
    let returned = owner
        .fresh_credential(provider, live.clone())
        .await
        .expect("background refresh must not delay the current live credential");
    assert_eq!(returned.credential, live);

    let slot = background_inflight_slot(provider);
    let pending_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let pending_warning = lock_mutex(&owner.worker.shared).pending[slot]
            .as_ref()
            .and_then(|pending| pending.warning.clone());
        let background_inflight = owner.worker.background_inflight[slot].load(Ordering::Acquire);
        if pending_warning.is_some() && !background_inflight {
            assert!(
                pending_warning
                    .as_deref()
                    .is_some_and(|warning| warning.contains("injected OAuth persistence failure")),
                "the installed pending successor must come from a real persistence failure"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < pending_deadline,
            "background refresh must finish with its real failed-persistence successor still pending"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let contender_store = OAuthStore::at(&auth_path);
    let contender = service(format!("http://{address}"), contender_store.clone());
    contender
        .logout(provider)
        .expect("independent service removes the persisted OAuth credential");
    assert_eq!(
        contender_store.credential(provider).unwrap(),
        None,
        "logout must leave the OAuth credential absent"
    );
    assert_eq!(
        contender_store.active_provider_key().unwrap(),
        None,
        "logout must clear active selection instead of restoring the provider"
    );
    assert_eq!(contender_store.active().unwrap(), None);

    let cleared_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let cleared = {
            let state = lock_mutex(&owner.worker.shared);
            state.pending[slot].is_none() && !state.reconciling[slot]
        };
        if cleared {
            break;
        }
        assert!(
            tokio::time::Instant::now() < cleared_deadline,
            "owner reconciler must observe logout and clear its pending successor"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let lock_store = contender_store.clone();
    let acquired = tokio::task::spawn_blocking(move || lock_store.lock_refresh(provider, || false))
        .await
        .expect("contender lease acquisition task must finish")
        .expect("independent contender must acquire the released refresh lease");
    drop(acquired);

    let owner_warnings = owner.shutdown().await;
    let contender_warnings = contender.shutdown().await;
    assert_eq!(
        contender_store.credential(provider).unwrap(),
        None,
        "owner shutdown must not restore OAuth after logout"
    );
    assert_eq!(contender_store.active_provider_key().unwrap(), None);
    assert_eq!(contender_store.active().unwrap(), None);
    let _ = finish_server_tx.send(());
    let posts = server.join().unwrap();
    let _ = std::fs::remove_dir_all(root);

    assert_eq!(posts, 1, "refresh endpoint must receive exactly one POST");
    assert!(owner_warnings.is_empty());
    assert!(contender_warnings.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_service_cannot_post_from_the_base_while_successor_is_pending() {
    let root = temp_root("pending-exclusion");
    std::fs::create_dir_all(&root).unwrap();
    let auth_path = root.join("auth.json");
    let old = OAuthCredential {
        access: "exclusive-old-access".into(),
        refresh: "exclusive-old-refresh".into(),
        expires: 1,
        account_id: None,
    };
    let first_store = OAuthStore::at(&auth_path);
    first_store.save(OAuthProvider::Anthropic, &old).unwrap();
    first_store.fail_refresh_persistence(true);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (finish_server_tx, finish_server_rx) = std::sync::mpsc::channel::<()>();
    let server = thread::spawn(move || {
        let (mut stream, _) = accept_fixture(&listener);
        assert!(read_request(&mut stream).contains("exclusive-old-refresh"));
        write_refresh_response(
            &mut stream,
            "exclusive-next-access",
            "exclusive-next-refresh",
            3600,
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut extra_requests = 0;
        loop {
            if finish_server_rx.try_recv().is_ok() {
                return extra_requests;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    extra_requests += 1;
                    let _ = read_request(&mut stream);
                    write_refresh_response(
                        &mut stream,
                        "unexpected-post-access",
                        "unexpected-post-refresh",
                        3600,
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "test must finish while loopback server remains available"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("loopback accept failed: {error}"),
            }
        }
    });
    let endpoint = format!("http://{address}");
    let first = service(endpoint.clone(), first_store.clone());
    let refreshed = first
        .fresh_credential(OAuthProvider::Anthropic, old.clone())
        .await
        .unwrap();
    assert_eq!(refreshed.credential.access, "exclusive-next-access");

    let second_store = OAuthStore::at(&auth_path);
    let second = service(endpoint, second_store);
    let waiting = second
        .request_fresh_credential(OAuthProvider::Anthropic, old.clone())
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    waiting.cancel();
    assert!(matches!(waiting.wait().await, Err(OAuthError::Cancelled)));

    first_store.fail_refresh_persistence(false);
    let next = first
        .fresh_credential(OAuthProvider::Anthropic, old.clone())
        .await
        .unwrap();
    assert_eq!(next.credential.access, "exclusive-next-access");
    assert_eq!(
        first_store.credential(OAuthProvider::Anthropic).unwrap(),
        Some(next.credential.clone())
    );
    let reused = second
        .fresh_credential(OAuthProvider::Anthropic, old)
        .await
        .expect("second service should reuse the reconciled successor");
    assert_eq!(reused.credential, next.credential);
    finish_server_tx.send(()).unwrap();
    assert_eq!(
        server.join().unwrap(),
        0,
        "pending ownership excludes refresh from the old base and persisted successor"
    );
    let _ = first.shutdown().await;
    let _ = second.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_during_locked_selection_prevents_refresh_post() {
    let root = temp_root("cancel-locked-selection");
    std::fs::create_dir_all(&root).unwrap();
    let store = OAuthStore::at(root.join("auth.json"));
    let live = OAuthCredential {
        access: "locked-old-access-fixture".into(),
        refresh: "locked-old-refresh-fixture".into(),
        expires: now_ms() / 1000 + 120,
        account_id: Some("locked-account-fixture".into()),
    };
    store.save(OAuthProvider::Anthropic, &live).unwrap();
    let store_guard = store.lock_exclusive().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (finish_server_tx, finish_server_rx) = std::sync::mpsc::channel::<()>();
    let server = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut posts = 0;
        loop {
            if finish_server_rx.try_recv().is_ok() {
                return posts;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    posts += 1;
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let _ = read_request(&mut stream);
                    write_refresh_response(
                        &mut stream,
                        "unexpected-locked-refresh-access",
                        "unexpected-locked-refresh-token",
                        3600,
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "test must finish while loopback listener remains available"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("loopback listener accept failed: {error}"),
            }
        }
    });

    let service = service(format!("http://{address}"), store.clone());
    let control = Arc::new(RefreshControl::new(OAuthProvider::Anthropic));
    let worker = service.worker.clone();
    let mut preparation = Box::pin(worker.prepare_credential(
        OAuthProvider::Anthropic,
        live,
        Arc::clone(&control),
        OAUTH_BLOCKING_REFRESH_MS,
        true,
    ));
    // The provider gate is free. Pending means selection is waiting on the
    // held auth-store lock.
    std::future::poll_fn(|context| match preparation.as_mut().poll(context) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(result) => {
            panic!("selection unexpectedly completed while auth store was locked: {result:?}")
        }
    })
    .await;

    control.cancel_before_dispatch();
    let result = tokio::time::timeout(Duration::from_secs(1), preparation.as_mut())
        .await
        .expect("cancellation must end lock selection while the auth lock stays held");
    assert!(matches!(result, Err(OAuthError::Cancelled)));
    thread::sleep(Duration::from_millis(100));
    drop(store_guard);
    let shutdown_warnings = service.shutdown().await;
    finish_server_tx.send(()).unwrap();
    let posts = server.join().unwrap();
    let _ = std::fs::remove_dir_all(root);

    assert!(shutdown_warnings.is_empty());
    assert_eq!(
        posts, 0,
        "cancelled selection must not dispatch a refresh POST"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observed_logout_rejects_the_old_fallback_without_refreshing() {
    let root = temp_root("observed-logout");
    std::fs::create_dir_all(&root).unwrap();
    let store = OAuthStore::at(root.join("auth.json"));
    let old = OAuthCredential {
        access: "logout-old-access-fixture".into(),
        refresh: "logout-old-refresh-fixture".into(),
        expires: 1,
        account_id: None,
    };
    store.save(OAuthProvider::Anthropic, &old).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (finish_server_tx, finish_server_rx) = std::sync::mpsc::channel::<()>();
    let server = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut posts = 0;
        loop {
            if finish_server_rx.try_recv().is_ok() {
                return posts;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    posts += 1;
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let _ = read_request(&mut stream);
                    write_refresh_response(
                        &mut stream,
                        "unexpected-after-logout-access",
                        "unexpected-after-logout-refresh",
                        3600,
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "test must finish while loopback server remains available"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("loopback accept failed: {error}"),
            }
        }
    });

    let service = service(format!("http://{address}"), store.clone());
    let observed = service
        .credential(OAuthProvider::Anthropic)
        .unwrap()
        .expect("OAuth credential must be observed before logout");
    assert_eq!(observed, old);
    service.logout(OAuthProvider::Anthropic).unwrap();
    let after_logout = service.credential(OAuthProvider::Anthropic).unwrap();
    let result = service
        .fresh_credential(OAuthProvider::Anthropic, old)
        .await;
    let warnings = service.shutdown().await;
    finish_server_tx.send(()).unwrap();
    let posts = server.join().unwrap();
    let _ = std::fs::remove_dir_all(root);

    assert_eq!(after_logout, None, "logout must remove persisted OAuth");
    assert_eq!(result.unwrap_err(), OAuthError::CredentialsChanged);
    assert!(warnings.is_empty());
    assert_eq!(posts, 0, "stale fallback after logout must not POST");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_key_preference_blocks_oauth_fallback_and_switching_methods_preserves_sibling_auth() {
    let root = temp_root("preferred-api-key");
    std::fs::create_dir_all(&root).unwrap();
    let store = OAuthStore::at(root.join("auth.json"));
    let oauth = OAuthCredential {
        access: "preferred-oauth-access-fixture".into(),
        refresh: "preferred-oauth-refresh-fixture".into(),
        expires: now_ms() / 1000 + 3600,
        account_id: Some("preferred-account-fixture".into()),
    };
    store.save(OAuthProvider::Xai, &oauth).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (finish_server_tx, finish_server_rx) = std::sync::mpsc::channel::<()>();
    let server = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut posts = 0;
        loop {
            if finish_server_rx.try_recv().is_ok() {
                return posts;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    posts += 1;
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let _ = read_request(&mut stream);
                    write_refresh_response(
                        &mut stream,
                        "unexpected-oauth-fallback-access",
                        "unexpected-oauth-fallback-refresh",
                        3600,
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "test must finish while loopback listener remains available"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("loopback listener accept failed: {error}"),
            }
        }
    });

    let service = service_for_xai(format!("http://{address}"), store.clone());
    service
        .save_api_key(OAuthProvider::Xai.key(), "xai-api-key-fixture")
        .unwrap();
    service.activate(OAuthProvider::Xai, &oauth).unwrap();
    assert_eq!(
        service.credential(OAuthProvider::Xai).unwrap(),
        Some(oauth.clone())
    );

    assert_eq!(
        service
            .activate_api_key(OAuthProvider::Xai.key())
            .unwrap()
            .as_deref(),
        Some("xai-api-key-fixture")
    );
    assert_eq!(service.credential(OAuthProvider::Xai).unwrap(), None);
    let fallback = service
        .fresh_credential(OAuthProvider::Xai, oauth.clone())
        .await;

    service.activate(OAuthProvider::Xai, &oauth).unwrap();
    let selected_oauth = service.credential(OAuthProvider::Xai).unwrap();
    let preserved_api_key = store.api_key(OAuthProvider::Xai.key()).unwrap();
    service.remove_api_key(OAuthProvider::Xai.key()).unwrap();
    let oauth_after_key_removal = service.credential(OAuthProvider::Xai).unwrap();
    let removed_api_key = store.api_key(OAuthProvider::Xai.key()).unwrap();
    let shutdown_warnings = service.shutdown().await;
    finish_server_tx.send(()).unwrap();
    let posts = server.join().unwrap();
    let _ = std::fs::remove_dir_all(root);

    assert!(matches!(fallback, Err(OAuthError::CredentialsChanged)));
    assert_eq!(selected_oauth, Some(oauth.clone()));
    assert_eq!(preserved_api_key.as_deref(), Some("xai-api-key-fixture"));
    assert_eq!(oauth_after_key_removal, Some(oauth));
    assert_eq!(removed_api_key, None);
    assert!(shutdown_warnings.is_empty());
    assert_eq!(posts, 0, "API-key preference cannot trigger OAuth refresh");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_key_preference_change_discards_an_in_flight_oauth_response() {
    let root = temp_root("api-key-wins-in-flight");
    std::fs::create_dir_all(&root).unwrap();
    let auth_path = root.join("auth.json");
    let first_store = OAuthStore::at(&auth_path);
    let second_store = OAuthStore::at(&auth_path);
    let old = OAuthCredential {
        access: "expired-access-fixture".into(),
        refresh: "in-flight-old-refresh-fixture".into(),
        expires: 1,
        account_id: None,
    };
    first_store.save(OAuthProvider::Xai, &old).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (request_seen_tx, request_seen_rx) = tokio::sync::oneshot::channel();
    let (release_response_tx, release_response_rx) = std::sync::mpsc::channel::<()>();
    let server = thread::spawn(move || {
        let (mut stream, _) = accept_fixture(&listener);
        assert!(read_request(&mut stream).contains("in-flight-old-refresh-fixture"));
        request_seen_tx.send(()).unwrap();
        release_response_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("test releases in-flight OAuth response");
        write_refresh_response(
            &mut stream,
            "stale-in-flight-access-fixture",
            "stale-in-flight-refresh-fixture",
            3600,
        );
    });

    let first = service_for_xai(format!("http://{address}"), first_store.clone());
    let second = service_for_xai(format!("http://{address}"), second_store.clone());
    second
        .save_api_key(OAuthProvider::Xai.key(), "switching-api-key-fixture")
        .unwrap();
    first.activate(OAuthProvider::Xai, &old).unwrap();
    let request = first
        .request_fresh_credential(OAuthProvider::Xai, old.clone())
        .unwrap();
    let waiter = tokio::spawn(request.wait());
    tokio::time::timeout(Duration::from_secs(5), request_seen_rx)
        .await
        .expect("loopback refresh request arrives")
        .unwrap();

    assert_eq!(
        second
            .activate_api_key(OAuthProvider::Xai.key())
            .unwrap()
            .as_deref(),
        Some("switching-api-key-fixture")
    );
    release_response_tx.send(()).unwrap();
    let result = waiter.await.unwrap();
    server.join().unwrap();
    let selected_oauth = first.credential(OAuthProvider::Xai).unwrap();
    let shutdown_warnings = first.shutdown().await;
    let second_warnings = second.shutdown().await;
    let auth: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&auth_path).unwrap()).unwrap();
    let _ = std::fs::remove_dir_all(root);

    assert!(matches!(result, Err(OAuthError::CredentialsChanged)));
    assert_eq!(
        selected_oauth, None,
        "API-key selection hides OAuth fallback"
    );
    assert_eq!(
        auth["providers"]["xai"]["oauth"]["access"], "expired-access-fixture",
        "stale response must not overwrite persisted OAuth"
    );
    assert_eq!(auth["providers"]["xai"]["preferred_method"], "api_key");
    assert_eq!(
        auth["providers"]["xai"]["api_key"],
        "switching-api-key-fixture"
    );
    assert!(shutdown_warnings.is_empty());
    assert!(second_warnings.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_reports_a_panicking_refresh_owner_without_exposing_its_payload() {
    let root = temp_root("owner-panic");
    let service = service(
        "http://127.0.0.1:1".into(),
        OAuthStore::at(root.join("auth.json")),
    );
    let control = Arc::new(RefreshControl::new(OAuthProvider::Anthropic));
    service
        .lifecycle
        .spawn(control, async move { panic!("synthetic owner panic") })
        .unwrap();
    let warnings = service.shutdown().await;
    let diagnostics = format!("{warnings:?}");
    assert!(diagnostics.contains("OAuth supervised task ended unexpectedly"));
    assert!(!diagnostics.contains("synthetic owner panic"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn headless_oauth_warnings_stay_in_stderr_for_text_and_jsonl() {
    let warnings = vec!["refresh persistence is pending".to_owned()];
    let text = CliOutput {
        code: ExitCode::Success,
        stdout: "answer text\n".into(),
        stderr: String::new(),
    };
    let text = append_oauth_warnings(text, &warnings);
    assert_eq!(text.code, ExitCode::Success);
    assert_eq!(text.stdout, "answer text\n");
    assert_eq!(
        text.stderr,
        "OAuth warning: refresh persistence is pending\n"
    );

    let jsonl_stdout = "{\"version\":1,\"kind\":\"assistant\",\"text\":\"answer\"}\n";
    let jsonl = CliOutput {
        code: ExitCode::Success,
        stdout: jsonl_stdout.into(),
        stderr: String::new(),
    };
    let jsonl = append_oauth_warnings(jsonl, &warnings);
    assert_eq!(jsonl.code, ExitCode::Success);
    assert_eq!(
        jsonl.stdout, jsonl_stdout,
        "warnings cannot add protocol records"
    );
    assert_eq!(
        jsonl.stderr,
        "OAuth warning: refresh persistence is pending\n"
    );
}

#[test]
fn headless_context_keeps_background_owner_alive_until_shutdown() {
    let root = temp_root("headless-owner");
    std::fs::create_dir_all(&root).unwrap();
    let store = OAuthStore::at(root.join("auth.json"));
    let live = OAuthCredential {
        access: "headless-old-access-fixture".into(),
        refresh: "headless-old-refresh-fixture".into(),
        expires: now_ms() / 1000 + 120,
        account_id: Some("headless-account-fixture".into()),
    };
    store.save(OAuthProvider::Anthropic, &live).unwrap();
    store.fail_refresh_persistence(true);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (request_seen_tx, request_seen_rx) = std::sync::mpsc::channel::<()>();
    let (release_response_tx, release_response_rx) = std::sync::mpsc::channel::<()>();
    let (response_done_tx, response_done_rx) = std::sync::mpsc::channel::<()>();
    let server = thread::spawn(move || {
        let (mut stream, _) = accept_fixture(&listener);
        assert!(read_request(&mut stream).contains("headless-old-refresh-fixture"));
        request_seen_tx.send(()).unwrap();
        release_response_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("test releases loopback refresh response");
        write_refresh_response(
            &mut stream,
            "headless-new-access-fixture",
            "headless-new-refresh-fixture",
            3600,
        );
        response_done_tx.send(()).unwrap();
    });
    let service = service(format!("http://{address}"), store.clone());
    let context =
        refresh_headless_oauth_with_service(service, OAuthProvider::Anthropic, live.clone())
            .expect("headless helper returns the currently usable credential");
    assert_eq!(context.access, live.access);
    assert_eq!(
        context.account_id.as_deref(),
        Some("headless-account-fixture")
    );

    let request_deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match request_seen_rx.try_recv() {
            Ok(()) => break,
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                assert!(
                    std::time::Instant::now() < request_deadline,
                    "background owner must dispatch refresh after helper returns"
                );
                thread::sleep(Duration::from_millis(5));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                panic!("loopback server exited before receiving refresh")
            }
        }
    }
    assert_eq!(
        store.credential(OAuthProvider::Anthropic).unwrap(),
        Some(live.clone()),
        "the response remains in flight after the helper returns"
    );
    release_response_tx.send(()).unwrap();
    let response_deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match response_done_rx.try_recv() {
            Ok(()) => break,
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                assert!(
                    std::time::Instant::now() < response_deadline,
                    "background owner must finish the refresh while its context is alive"
                );
                thread::sleep(Duration::from_millis(5));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                panic!("loopback server exited before responding")
            }
        }
    }
    server.join().unwrap();

    let warnings = context.shutdown();
    assert!(
        warnings.iter().any(|warning| warning.contains("persist")),
        "shutdown must report the pending persistence failure"
    );
    let diagnostics = format!("{warnings:?}");
    assert!(!diagnostics.contains("headless-old-access-fixture"));
    assert!(!diagnostics.contains("headless-old-refresh-fixture"));
    assert!(!diagnostics.contains("headless-new-access-fixture"));
    assert!(!diagnostics.contains("headless-new-refresh-fixture"));
    assert_eq!(
        store.credential(OAuthProvider::Anthropic).unwrap(),
        Some(live),
        "a failed refresh write must leave the old readable auth document intact"
    );
    let _ = std::fs::remove_dir_all(root);
}

fn service(endpoint: String, store: OAuthStore) -> OAuthService {
    OAuthService::new(
        OAuthEndpoints {
            anthropic_token: endpoint,
            ..OAuthEndpoints::default()
        },
        Arc::new(TestBrowser),
        store,
    )
    .unwrap()
}

fn service_for_xai(endpoint: String, store: OAuthStore) -> OAuthService {
    OAuthService::new(
        OAuthEndpoints {
            xai_token: endpoint,
            ..OAuthEndpoints::default()
        },
        Arc::new(TestBrowser),
        store,
    )
    .unwrap()
}

fn temp_root(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "slim-oauth-unit-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn write_auth_with_sibling(path: &std::path::Path, credential: &OAuthCredential) {
    let auth = json!({
        "version": 1,
        "active_provider": "openai-codex",
        "providers": {
            "anthropic": {"oauth": credential},
            "openai-codex": {"api_key": "sibling-api-key-fixture"}
        }
    });
    std::fs::write(path, serde_json::to_vec(&auth).unwrap()).unwrap();
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let count = stream.read(&mut chunk).unwrap();
        assert!(count > 0, "fixture request ended unexpectedly");
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                return String::from_utf8(bytes).unwrap();
            }
        }
    }
}

fn write_refresh_response(stream: &mut TcpStream, access: &str, refresh: &str, expires_in: u32) {
    let body = format!(
        r#"{{"access_token":"{access}","refresh_token":"{refresh}","expires_in":{expires_in}}}"#
    );
    stream
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .unwrap();
}

fn accept_fixture(listener: &TcpListener) -> (TcpStream, std::net::SocketAddr) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, address)) => {
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                return (stream, address);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "loopback refresh request timed out"
                );
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("loopback listener failed: {error}"),
        }
    }
}
