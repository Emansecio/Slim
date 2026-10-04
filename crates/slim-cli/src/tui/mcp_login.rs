//! `/mcp login <server> [redirect-url]` and `/mcp logout <server>`: OAuth
//! sign-in for HTTP MCP servers from the TUI. The sign-in runs off the
//! worker lane. The authorization URL reaches the UI as `McpAuthorization`
//! (the sign-in panel: full copyable URL and a field for a pasted redirect),
//! the outcome as a notification, and `McpLoginEnded` closes the panel. A
//! pasted redirect URL (`/mcp login <server> <url>` or the panel field)
//! completes a sign-in the browser could not reach this machine for (SSH).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use slim_core::mcp::McpManager;
use slim_tui::api::{SensitiveText, UiEvent};
use tokio::sync::{mpsc, watch};

use super::{mcp_server_views, redact_mcp_text, EventSink};
use crate::mcp::oauth::{self, LoginError, LoginNotice, LoginOptions};
use crate::oauth::BrowserLauncher;

/// One running sign-in.
pub(super) struct LoginSession {
    id: u64,
    paste: mpsc::UnboundedSender<String>,
    cancel: watch::Sender<bool>,
}

/// Running sign-ins by server name.
pub(super) type McpLogins = Arc<Mutex<HashMap<String, LoginSession>>>;

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

fn lock(logins: &McpLogins) -> std::sync::MutexGuard<'_, HashMap<String, LoginSession>> {
    logins
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn notify(sink: &EventSink, message: String) {
    let _ = sink.send(UiEvent::Notification { message });
}

fn publish(manager: &McpManager, revision: &AtomicU64, sink: &EventSink) {
    revision.store(manager.revision(), Ordering::Relaxed);
    let _ = sink.send(UiEvent::McpServersChanged {
        servers: mcp_server_views(manager),
    });
}

/// Test hook: registers a sign-in for `name` that runs nothing, and returns
/// what it would observe: the cancel flag and the pasted redirect URLs.
#[cfg(test)]
pub(super) fn register_for_tests(
    logins: &McpLogins,
    name: &str,
) -> (watch::Receiver<bool>, mpsc::UnboundedReceiver<String>) {
    let (paste, pasted) = mpsc::unbounded_channel();
    let (cancel, cancelled) = watch::channel(false);
    lock(logins).insert(
        name.to_owned(),
        LoginSession {
            id: NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
            paste,
            cancel,
        },
    );
    (cancelled, pasted)
}

/// Stops every running sign-in (the TUI is shutting down).
pub(super) fn cancel_all(logins: &McpLogins) {
    for session in lock(logins).values() {
        let _ = session.cancel.send(true);
    }
}

/// The panel was dismissed: stops the sign-in running for `name`, if any.
pub(super) fn cancel_login(logins: &McpLogins, name: &str) {
    if let Some(session) = lock(logins).remove(name) {
        let _ = session.cancel.send(true);
    }
}

/// Hands a pasted redirect URL to the sign-in running for `name`.
pub(super) fn paste_redirect(logins: &McpLogins, sink: &EventSink, name: &str, url: String) {
    match lock(logins).get(name) {
        Some(session) => {
            let _ = session.paste.send(url);
        }
        None => notify(
            sink,
            format!("mcp {name}: nenhum login em andamento; use /mcp login {name} primeiro"),
        ),
    }
}

/// `/mcp login <name>`: starts a sign-in (a running one for the same server is
/// replaced). With `redirect_url`: hands the pasted URL to the running one.
pub(super) fn start_login(
    manager: &Option<Arc<McpManager>>,
    logins: &McpLogins,
    revision: &Arc<AtomicU64>,
    sink: &EventSink,
    browser: Arc<dyn BrowserLauncher>,
    name: String,
    redirect_url: Option<String>,
) {
    let Some(manager) = manager.clone() else {
        notify(sink, "Nenhum servidor MCP configurado".into());
        return;
    };
    let Some(auth) = manager.auth_handle(&name) else {
        notify(
            sink,
            format!(
                "mcp {name}: sem login OAuth para este servidor (só servidores HTTP ativos, sem cabeçalho Authorization, usam OAuth)"
            ),
        );
        return;
    };
    if let Some(url) = redirect_url {
        paste_redirect(logins, sink, &name, url);
        return;
    }
    let (paste_tx, paste_rx) = mpsc::unbounded_channel();
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    if let Some(previous) = lock(logins).insert(
        name.clone(),
        LoginSession {
            id,
            paste: paste_tx,
            cancel: cancel_tx,
        },
    ) {
        let _ = previous.cancel.send(true);
    }
    let logins = Arc::clone(logins);
    let revision = Arc::clone(revision);
    let sink = sink.clone();
    tokio::spawn(async move {
        let notice_name = name.clone();
        let notice_sink = sink.clone();
        let notice_manager = Arc::clone(&manager);
        let options = LoginOptions {
            timeout: oauth::LOGIN_TIMEOUT,
            browser,
            notify: Arc::new(move |notice| match notice {
                LoginNotice::AuthorizationUrl {
                    url,
                    browser_opened,
                    ..
                } => {
                    let _ = notice_sink.send(UiEvent::McpAuthorization {
                        name: notice_name.clone(),
                        url: SensitiveText::from(redact_mcp_text(&notice_manager, &url)),
                        browser_opened,
                    });
                }
                LoginNotice::Warning(text) => notify(
                    &notice_sink,
                    redact_mcp_text(&notice_manager, &format!("mcp {notice_name}: {text}")),
                ),
            }),
            pasted: Some(paste_rx),
            cancel: cancel_rx,
        };
        let result = oauth::login(&auth, options).await;
        // A newer sign-in for the same server owns the panel now: this one
        // ends silently. Otherwise the panel closes (also when the user
        // already dismissed it or `logout` cancelled the sign-in).
        let replaced = {
            let mut running = lock(&logins);
            match running.get(&name) {
                Some(session) if session.id == id => {
                    running.remove(&name);
                    false
                }
                Some(_) => true,
                None => false,
            }
        };
        if replaced {
            return;
        }
        let message = match result {
            Ok(_) => Some(match manager.reconnect(&name).await {
                Ok(()) => format!("mcp {name}: login feito e servidor conectado"),
                Err(error) => format!("mcp {name}: login feito, mas a conexão falhou: {error}"),
            }),
            // Cancelled by the user (or by `logout`): they already know.
            Err(LoginError::Cancelled) => None,
            Err(LoginError::Timeout) => Some(format!(
                "mcp {name}: tempo do login esgotado (5 minutos); tente de novo com l ou /mcp login {name}"
            )),
            Err(error) => Some(format!("mcp {name}: falha ao entrar: {error}")),
        };
        if let Some(message) = message {
            notify(&sink, redact_mcp_text(&manager, &message));
        }
        publish(&manager, &revision, &sink);
        let _ = sink.send(UiEvent::McpLoginEnded { name });
    });
}

/// `/mcp logout <name>`: deletes the stored credentials and reconnects (the
/// server then shows `needs-auth` if it requires them).
pub(super) fn start_logout(
    manager: &Option<Arc<McpManager>>,
    logins: &McpLogins,
    revision: &Arc<AtomicU64>,
    sink: &EventSink,
    name: String,
) {
    let Some(manager) = manager.clone() else {
        notify(sink, "Nenhum servidor MCP configurado".into());
        return;
    };
    let Some(auth) = manager.auth_handle(&name) else {
        notify(
            sink,
            format!("mcp {name}: sem login OAuth para este servidor"),
        );
        return;
    };
    cancel_login(logins, &name);
    let revision = Arc::clone(revision);
    let sink = sink.clone();
    tokio::spawn(async move {
        let message = match oauth::logout(&auth).await {
            Ok(removed) => {
                let _ = manager.reconnect(&name).await;
                if removed {
                    format!("mcp {name}: saiu; credenciais salvas apagadas")
                } else {
                    format!("mcp {name}: não havia credenciais salvas")
                }
            }
            Err(error) => format!("mcp {name}: falha ao sair: {error}"),
        };
        notify(&sink, redact_mcp_text(&manager, &message));
        publish(&manager, &revision, &sink);
    });
}
