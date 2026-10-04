mod anthropic;
pub mod browser;
pub mod callback;
mod codex;
pub mod pkce;
mod store;
mod types;
mod xai;

#[cfg(test)]
mod tests;

use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use tokio::sync::{mpsc, oneshot, watch, Mutex as AsyncMutex};
use tokio::task::JoinHandle;

use store::{AuthEntrySnapshot, RefreshGuard};

pub use browser::{BrowserLauncher, SystemBrowser};
pub use codex::account_id as codex_account_id;
pub use store::OAuthStore;
pub use types::{OAuthCredential, OAuthEndpoints, OAuthError, OAuthProgress, OAuthProvider};

const MAX_OAUTH_RESPONSE_BYTES: usize = 64 * 1024;
const OAUTH_BLOCKING_REFRESH_MS: u64 = 30_000;
const OAUTH_BACKGROUND_REFRESH_MS: u64 = 10 * 60 * 1000;
const OAUTH_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(35);
const REFRESH_WAITING: u8 = 0;
const REFRESH_DISPATCHED: u8 = 1;
const REFRESH_COMPLETE: u8 = 2;
const REFRESH_CANCELLED: u8 = 3;
const REFRESH_BACKGROUND: u8 = 4;

type RefreshWindow = Option<(OAuthCredential, u64)>;

pub struct FreshCredential {
    pub credential: OAuthCredential,
    pub persistence_warning: Option<String>,
}

impl fmt::Debug for FreshCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FreshCredential")
            .field("credential", &self.credential)
            .field("persistence_warning", &self.persistence_warning)
            .finish()
    }
}

/// A supervised OAuth preparation. Dropping it cancels before HTTP
/// A supervised OAuth preparation. Dropping it cancels work before HTTP
/// dispatch; once dispatch starts, the service keeps the transaction alive.
pub struct OAuthCredentialRequest {
    control: Arc<RefreshControl>,
    receiver: Option<oneshot::Receiver<Result<FreshCredential, OAuthError>>>,
    cancel_on_drop: bool,
}

impl OAuthCredentialRequest {
    pub async fn wait(mut self) -> Result<FreshCredential, OAuthError> {
        let receiver = self
            .receiver
            .take()
            .expect("OAuth credential request can only be awaited once");
        let result = receiver.await.unwrap_or_else(|_| {
            Err(OAuthError::Store(
                "OAuth supervisor ended unexpectedly".into(),
            ))
        });
        if result.is_ok() {
            self.cancel_on_drop = false;
        }
        result
    }

    pub fn cancel(&self) {
        self.control.cancel_before_dispatch();
    }
}

impl Drop for OAuthCredentialRequest {
    fn drop(&mut self) {
        if self.cancel_on_drop {
            self.control.cancel_before_dispatch();
        }
    }
}

#[derive(Clone)]
struct OAuthWorker {
    client: reqwest::Client,
    endpoints: OAuthEndpoints,
    browser: Arc<dyn BrowserLauncher>,
    store: OAuthStore,
    refresh_gate: Arc<[AsyncMutex<()>; 3]>,
    shared: Arc<Mutex<OAuthSharedState>>,
    background_inflight: Arc<[AtomicBool; 3]>,
    lifecycle: Arc<OAuthLifecycle>,
}

pub struct OAuthService {
    worker: OAuthWorker,
    lifecycle: Arc<OAuthLifecycle>,
}

impl Clone for OAuthService {
    fn clone(&self) -> Self {
        self.lifecycle
            .external_owners
            .fetch_add(1, Ordering::AcqRel);
        Self {
            worker: self.worker.clone(),
            lifecycle: Arc::clone(&self.lifecycle),
        }
    }
}

impl Drop for OAuthService {
    fn drop(&mut self) {
        if self
            .lifecycle
            .external_owners
            .fetch_sub(1, Ordering::AcqRel)
            == 1
        {
            self.lifecycle.request_shutdown();
        }
    }
}

impl OAuthService {
    pub fn production() -> Result<Self, OAuthError> {
        Self::new(
            OAuthEndpoints::default(),
            Arc::new(SystemBrowser),
            OAuthStore::default_path()?,
        )
    }

    pub fn new(
        endpoints: OAuthEndpoints,
        browser: Arc<dyn BrowserLauncher>,
        store: OAuthStore,
    ) -> Result<Self, OAuthError> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| OAuthError::Transport("OAuth HTTP client creation failed".into()))?;
        let lifecycle = Arc::new(OAuthLifecycle::new());
        let worker = OAuthWorker {
            client,
            endpoints,
            browser,
            store,
            refresh_gate: Arc::new([
                AsyncMutex::new(()),
                AsyncMutex::new(()),
                AsyncMutex::new(()),
            ]),
            shared: Arc::new(Mutex::new(OAuthSharedState::new())),
            background_inflight: Arc::new([
                AtomicBool::new(false),
                AtomicBool::new(false),
                AtomicBool::new(false),
            ]),
            lifecycle: Arc::clone(&lifecycle),
        };
        Ok(Self { worker, lifecycle })
    }

    pub fn active(&self) -> Result<Option<(OAuthProvider, OAuthCredential)>, OAuthError> {
        let active = self.worker.store.active()?;
        if let Some((provider, _)) = active.as_ref() {
            self.worker.note_store_credential_seen(*provider);
        }
        Ok(active)
    }

    pub fn credential(
        &self,
        provider: OAuthProvider,
    ) -> Result<Option<OAuthCredential>, OAuthError> {
        let credential = self.worker.store.credential(provider)?;
        if credential.is_some() {
            self.worker.note_store_credential_seen(provider);
        }
        Ok(credential)
    }

    pub fn activate(
        &self,
        provider: OAuthProvider,
        credential: &OAuthCredential,
    ) -> Result<(), OAuthError> {
        let _store_guard = self.worker.store.lock_exclusive()?;
        self.worker.store.save_locked(provider, credential)?;
        self.worker.invalidate_local_locked(provider);
        Ok(())
    }

    pub fn save_api_key(&self, provider: &str, key: &str) -> Result<(), OAuthError> {
        self.worker.store.save_api_key(provider, key)?;
        if provider == OAuthProvider::Xai.key() {
            self.worker.invalidate_local(OAuthProvider::Xai);
        }
        Ok(())
    }

    pub fn activate_api_key(&self, provider: &str) -> Result<Option<String>, OAuthError> {
        let key = self.worker.store.activate_api_key(provider)?;
        if key.is_some() && provider == OAuthProvider::Xai.key() {
            self.worker.invalidate_local(OAuthProvider::Xai);
        }
        Ok(key)
    }

    pub fn remove_api_key(&self, provider: &str) -> Result<(), OAuthError> {
        self.worker.store.remove_api_key(provider)?;
        if provider == OAuthProvider::Xai.key() {
            self.worker.invalidate_local(OAuthProvider::Xai);
        }
        Ok(())
    }

    pub fn active_provider_key(&self) -> Result<Option<String>, OAuthError> {
        self.worker.store.active_provider_key()
    }

    pub async fn login(
        &self,
        provider: OAuthProvider,
        progress: mpsc::UnboundedSender<OAuthProgress>,
        cancel: watch::Receiver<bool>,
    ) -> Result<OAuthCredential, OAuthError> {
        let credential = match provider {
            OAuthProvider::Anthropic => {
                anthropic::login(
                    &self.worker.client,
                    &self.worker.endpoints,
                    self.worker.browser.clone(),
                    &progress,
                    cancel,
                )
                .await?
            }
            OAuthProvider::OpenAiCodex => {
                codex::login(
                    &self.worker.client,
                    &self.worker.endpoints,
                    self.worker.browser.clone(),
                    &progress,
                    cancel,
                )
                .await?
            }
            OAuthProvider::Xai => {
                xai::login(
                    &self.worker.client,
                    &self.worker.endpoints,
                    self.worker.browser.clone(),
                    &progress,
                    cancel,
                )
                .await?
            }
        };
        self.activate(provider, &credential)?;
        Ok(credential)
    }

    /// Starts a supervised credential preparation. Dropping the returned
    /// request cancels before dispatch but only detaches the waiter afterward.
    pub fn request_fresh_credential(
        &self,
        provider: OAuthProvider,
        credential: OAuthCredential,
    ) -> Result<OAuthCredentialRequest, OAuthError> {
        let control = Arc::new(RefreshControl::new(provider));
        let (sender, receiver) = oneshot::channel();
        let worker = self.worker.clone();
        let task_control = Arc::clone(&control);
        let task = async move {
            let _completion = JobCompletion(Arc::clone(&task_control), false);
            let result = worker
                .prepare_credential(
                    provider,
                    credential,
                    task_control,
                    OAUTH_BLOCKING_REFRESH_MS,
                    true,
                )
                .await;
            let _ = sender.send(result);
        };
        self.lifecycle.spawn(Arc::clone(&control), task)?;
        Ok(OAuthCredentialRequest {
            control,
            receiver: Some(receiver),
            cancel_on_drop: true,
        })
    }

    pub async fn fresh_credential(
        &self,
        provider: OAuthProvider,
        credential: OAuthCredential,
    ) -> Result<FreshCredential, OAuthError> {
        self.request_fresh_credential(provider, credential)?
            .wait()
            .await
    }

    pub fn logout(&self, provider: OAuthProvider) -> Result<(), OAuthError> {
        let _store_guard = self.worker.store.lock_exclusive()?;
        self.worker.store.remove_locked(provider)?;
        self.worker.note_store_credential_seen(provider);
        self.worker.invalidate_local_locked(provider);
        Ok(())
    }

    /// Stops accepting new refresh jobs, lets dispatched requests finish, and
    /// makes one final persistence reconciliation before releasing pending locks.
    pub async fn shutdown(&self) -> Vec<String> {
        self.shutdown_until(tokio::time::Instant::now() + OAUTH_SHUTDOWN_TIMEOUT)
            .await
    }

    pub(crate) fn take_warnings(&self) -> Vec<String> {
        self.worker.take_warnings()
    }

    #[cfg(test)]
    pub(crate) fn start_background_refresh_for_test(
        &self,
        provider: OAuthProvider,
        credential: OAuthCredential,
    ) {
        self.worker.spawn_background_refresh(
            provider,
            credential,
            Arc::new(RefreshControl::new(provider)),
        );
    }

    async fn shutdown_until(&self, deadline: tokio::time::Instant) -> Vec<String> {
        self.lifecycle.request_shutdown();
        let mut warnings = self.lifecycle.join_tasks_until(deadline).await;
        match tokio::time::timeout_at(deadline, self.worker.finalize_pending(deadline)).await {
            Ok(final_warnings) => warnings.extend(final_warnings),
            Err(_) => {
                warnings
                    .push("OAuth persistence reconciliation exceeded its shutdown deadline".into());
                self.worker.release_unresolved_at_shutdown();
            }
        }
        warnings.extend(self.worker.take_warnings());
        warnings.sort();
        warnings.dedup();
        warnings
    }
}

struct RefreshControl {
    provider: OAuthProvider,
    state: AtomicU8,
}

impl RefreshControl {
    fn new(provider: OAuthProvider) -> Self {
        Self {
            provider,
            state: AtomicU8::new(REFRESH_WAITING),
        }
    }

    fn cancelled(&self) -> bool {
        self.state.load(Ordering::Acquire) == REFRESH_CANCELLED
    }

    fn begin_dispatch(&self) -> Result<(), OAuthError> {
        loop {
            let state = self.state.load(Ordering::Acquire);
            if !matches!(state, REFRESH_WAITING | REFRESH_BACKGROUND) {
                return Err(OAuthError::Cancelled);
            }
            if self
                .state
                .compare_exchange(
                    state,
                    REFRESH_DISPATCHED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return Ok(());
            }
        }
    }

    fn cancel_before_dispatch(&self) {
        loop {
            let state = self.state.load(Ordering::Acquire);
            if !matches!(state, REFRESH_WAITING | REFRESH_BACKGROUND) {
                return;
            }
            if self
                .state
                .compare_exchange(
                    state,
                    REFRESH_CANCELLED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return;
            }
        }
    }

    fn transfer_to_background(&self) -> bool {
        self.state
            .compare_exchange(
                REFRESH_WAITING,
                REFRESH_BACKGROUND,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn complete_foreground(&self) {
        loop {
            let state = self.state.load(Ordering::Acquire);
            if !matches!(state, REFRESH_WAITING | REFRESH_DISPATCHED) {
                return;
            }
            if self
                .state
                .compare_exchange(state, REFRESH_COMPLETE, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    fn complete_owner(&self) {
        loop {
            let state = self.state.load(Ordering::Acquire);
            if matches!(state, REFRESH_COMPLETE | REFRESH_CANCELLED) {
                return;
            }
            if self
                .state
                .compare_exchange(state, REFRESH_COMPLETE, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }
}

struct JobCompletion(Arc<RefreshControl>, bool);

impl Drop for JobCompletion {
    fn drop(&mut self) {
        if self.1 {
            self.0.complete_owner();
        } else {
            self.0.complete_foreground();
        }
    }
}

struct LifecycleState {
    tasks: Vec<JoinHandle<()>>,
    controls: Vec<Weak<RefreshControl>>,
}

struct OAuthLifecycle {
    shutting_down: AtomicBool,
    external_owners: AtomicUsize,
    state: Mutex<LifecycleState>,
}

impl OAuthLifecycle {
    fn new() -> Self {
        Self {
            shutting_down: AtomicBool::new(false),
            external_owners: AtomicUsize::new(1),
            state: Mutex::new(LifecycleState {
                tasks: Vec::new(),
                controls: Vec::new(),
            }),
        }
    }

    fn spawn(
        self: &Arc<Self>,
        control: Arc<RefreshControl>,
        task: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), OAuthError> {
        let mut state = lock_mutex(&self.state);
        if self.shutting_down.load(Ordering::Acquire) {
            control.cancel_before_dispatch();
            return Err(OAuthError::Cancelled);
        }
        let runtime = crate::headless::provider_runtime_handle()
            .map_err(|_| OAuthError::Store("OAuth supervisor runtime is unavailable".into()))?;
        state.tasks.retain(|task| !task.is_finished());
        state.controls.retain(|control| control.strong_count() > 0);
        state.controls.push(Arc::downgrade(&control));
        state.tasks.push(runtime.spawn(task));
        Ok(())
    }

    fn request_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
        let state = lock_mutex(&self.state);
        for control in state.controls.iter().filter_map(Weak::upgrade) {
            control.cancel_before_dispatch();
        }
    }

    fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }

    async fn join_tasks_until(&self, deadline: tokio::time::Instant) -> Vec<String> {
        self.request_shutdown();
        let tasks = std::mem::take(&mut lock_mutex(&self.state).tasks);
        let mut warnings = Vec::new();
        let mut tasks = tasks.into_iter();
        while let Some(mut task) = tasks.next() {
            match tokio::time::timeout_at(deadline, &mut task).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => warnings.push("OAuth supervised task ended unexpectedly".into()),
                Err(_) => {
                    warnings.push("OAuth supervision exceeded its shutdown deadline".into());
                    task.abort();
                    for task in tasks {
                        task.abort();
                    }
                    break;
                }
            }
        }
        warnings
    }
}

struct PendingRefresh {
    base_snapshot: AuthEntrySnapshot,
    successor: OAuthCredential,
    generation: u64,
    warning: Option<String>,
    _guard: RefreshGuard,
}

struct VolatileCredential {
    base_snapshot: AuthEntrySnapshot,
    credential: OAuthCredential,
    generation: u64,
    warning: String,
}

struct OAuthSharedState {
    pending: [Option<PendingRefresh>; 3],
    volatile: [Option<VolatileCredential>; 3],
    generations: [u64; 3],
    store_credential_seen: [bool; 3],
    refresh_windows: [RefreshWindow; 3],
    reconciling: [bool; 3],
    warnings: Vec<String>,
}

impl OAuthSharedState {
    fn new() -> Self {
        Self {
            pending: [None, None, None],
            volatile: [None, None, None],
            generations: [0; 3],
            store_credential_seen: [false; 3],
            refresh_windows: [None, None, None],
            reconciling: [false; 3],
            warnings: Vec::new(),
        }
    }
}

struct SelectedCredential {
    credential: OAuthCredential,
    snapshot: AuthEntrySnapshot,
    generation: u64,
    pending: bool,
    persistence_warning: Option<String>,
}

struct BackgroundRefreshFinish {
    inflight: Arc<[AtomicBool; 3]>,
    shared: Arc<Mutex<OAuthSharedState>>,
    slot: usize,
    succeeded: bool,
}

impl Drop for BackgroundRefreshFinish {
    fn drop(&mut self) {
        self.inflight[self.slot].store(false, Ordering::Release);
        if !self.succeeded {
            add_warning(
                &self.shared,
                "OAuth background refresh task ended unexpectedly".into(),
            );
        }
    }
}

struct ReconciliationFinish {
    shared: Arc<Mutex<OAuthSharedState>>,
    slot: usize,
}

impl Drop for ReconciliationFinish {
    fn drop(&mut self) {
        lock_mutex(&self.shared).reconciling[self.slot] = false;
    }
}

impl OAuthWorker {
    async fn prepare_credential(
        &self,
        provider: OAuthProvider,
        fallback: OAuthCredential,
        control: Arc<RefreshControl>,
        refresh_if_remaining_ms: u64,
        allow_background: bool,
    ) -> Result<FreshCredential, OAuthError> {
        let _gate = self.refresh_gate[background_inflight_slot(provider)]
            .lock()
            .await;
        if control.cancelled() {
            return Err(OAuthError::Cancelled);
        }
        let mut selected = self
            .select_credential(provider, fallback.clone(), Some(Arc::clone(&control)))
            .await?;
        if control.cancelled() {
            return Err(OAuthError::Cancelled);
        }
        if !selected.pending && selected.credential != fallback {
            self.note_observed_credential(provider, &selected.credential);
        }
        if selected.pending {
            return self.return_pending(selected, refresh_if_remaining_ms);
        }
        let remaining = expiry_ms(selected.credential.expires).saturating_sub(now_ms());
        if remaining > refresh_if_remaining_ms {
            if allow_background
                && remaining < OAUTH_BACKGROUND_REFRESH_MS
                && self.background_refresh_due(provider, &selected.credential)
            {
                self.spawn_background_refresh(
                    provider,
                    selected.credential.clone(),
                    Arc::clone(&control),
                );
            }
            return Ok(self.fresh_result(selected.credential, selected.persistence_warning));
        }

        let store = self.store.clone();
        let lock_control = Arc::clone(&control);
        let refresh_guard = tokio::task::spawn_blocking(move || {
            store.lock_refresh(provider, || lock_control.cancelled())
        })
        .await
        .map_err(|_| OAuthError::Store("OAuth refresh lock task failed".into()))??;
        if control.cancelled() {
            drop(refresh_guard);
            return Err(OAuthError::Cancelled);
        }

        selected = self
            .select_credential(
                provider,
                selected.credential.clone(),
                Some(Arc::clone(&control)),
            )
            .await?;
        if control.cancelled() {
            drop(refresh_guard);
            return Err(OAuthError::Cancelled);
        }
        if !selected.pending && selected.credential != fallback {
            self.note_observed_credential(provider, &selected.credential);
        }
        if selected.pending {
            drop(refresh_guard);
            return self.return_pending(selected, refresh_if_remaining_ms);
        }
        let remaining = expiry_ms(selected.credential.expires).saturating_sub(now_ms());
        if remaining > refresh_if_remaining_ms {
            drop(refresh_guard);
            if allow_background
                && remaining < OAUTH_BACKGROUND_REFRESH_MS
                && self.background_refresh_due(provider, &selected.credential)
            {
                self.spawn_background_refresh(
                    provider,
                    selected.credential.clone(),
                    Arc::clone(&control),
                );
            }
            return Ok(self.fresh_result(selected.credential, selected.persistence_warning));
        }
        if control.cancelled() {
            drop(refresh_guard);
            return Err(OAuthError::Cancelled);
        }

        let base_snapshot = selected.snapshot;
        let generation = selected.generation;
        let refreshed = self
            .send_refresh(provider, &selected.credential, Arc::clone(&control))
            .await?;
        if !base_snapshot.has_oauth() {
            let warning = self
                .validate_ephemeral_successor(
                    provider,
                    base_snapshot,
                    generation,
                    refreshed.clone(),
                    refresh_guard,
                )
                .await?;
            return Ok(self.fresh_result(refreshed, warning));
        }

        self.register_successor(
            provider,
            base_snapshot,
            generation,
            refreshed.clone(),
            refresh_guard,
        )?;
        self.spawn_reconciler(provider);
        let selected = self
            .select_credential(provider, refreshed.clone(), Some(Arc::clone(&control)))
            .await?;
        if selected.generation != generation || selected.credential != refreshed {
            return Err(OAuthError::CredentialsChanged);
        }
        let warning = selected.persistence_warning.or_else(|| {
            selected
                .pending
                .then(|| "OAuth refreshed in memory but is not yet persisted".into())
        });
        Ok(self.fresh_result(refreshed, warning))
    }

    fn return_pending(
        &self,
        selected: SelectedCredential,
        refresh_if_remaining_ms: u64,
    ) -> Result<FreshCredential, OAuthError> {
        let remaining = expiry_ms(selected.credential.expires).saturating_sub(now_ms());
        if remaining <= refresh_if_remaining_ms {
            return Err(OAuthError::Store(
                "OAuth successor is not persisted; refusing another refresh with it".into(),
            ));
        }
        Ok(self.fresh_result(
            selected.credential,
            selected
                .persistence_warning
                .or_else(|| Some("OAuth refreshed in memory but is not yet persisted".into())),
        ))
    }

    fn fresh_result(
        &self,
        credential: OAuthCredential,
        warning: Option<String>,
    ) -> FreshCredential {
        FreshCredential {
            credential,
            persistence_warning: warning,
        }
    }

    async fn select_credential(
        &self,
        provider: OAuthProvider,
        fallback: OAuthCredential,
        control: Option<Arc<RefreshControl>>,
    ) -> Result<SelectedCredential, OAuthError> {
        let store = self.store.clone();
        let shared = Arc::clone(&self.shared);
        let lifecycle = Arc::clone(&self.lifecycle);
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        tokio::task::spawn_blocking(move || {
            select_credential_sync(
                &store,
                &shared,
                provider,
                fallback,
                deadline,
                control,
                Some(lifecycle),
            )
        })
        .await
        .map_err(|_| OAuthError::Store("auth store selection task failed".into()))?
    }

    async fn select_credential_until(
        &self,
        provider: OAuthProvider,
        fallback: OAuthCredential,
        deadline: tokio::time::Instant,
    ) -> Result<SelectedCredential, OAuthError> {
        let store = self.store.clone();
        let shared = Arc::clone(&self.shared);
        let deadline = deadline.into_std();
        tokio::task::spawn_blocking(move || {
            select_credential_sync(&store, &shared, provider, fallback, deadline, None, None)
        })
        .await
        .map_err(|_| OAuthError::Store("auth store selection task failed".into()))?
    }

    async fn validate_ephemeral_successor(
        &self,
        provider: OAuthProvider,
        expected: AuthEntrySnapshot,
        generation: u64,
        successor: OAuthCredential,
        refresh_guard: RefreshGuard,
    ) -> Result<Option<String>, OAuthError> {
        let store = self.store.clone();
        let shared = Arc::clone(&self.shared);
        tokio::task::spawn_blocking(move || {
            let _refresh_guard = refresh_guard;
            let _store_guard = store.lock_exclusive()?;
            let mut state = lock_mutex(&shared);
            if state.generations[background_inflight_slot(provider)] != generation {
                return Err(OAuthError::CredentialsChanged);
            }
            match store.credential_snapshot(provider) {
                Ok((current_credential, current)) => {
                    if current != expected {
                        return Err(OAuthError::CredentialsChanged);
                    }
                    if current_credential.is_some() {
                        return Err(OAuthError::CredentialsChanged);
                    }
                    set_refresh_window(&mut state, provider, &successor);
                    Ok(None)
                }
                Err(error) => {
                    let warning = format!(
                        "OAuth refreshed in memory but auth store could not be checked: {error}"
                    );
                    let slot = background_inflight_slot(provider);
                    state.volatile[slot] = Some(VolatileCredential {
                        base_snapshot: expected,
                        credential: successor.clone(),
                        generation,
                        warning: warning.clone(),
                    });
                    set_refresh_window(&mut state, provider, &successor);
                    Ok(Some(warning))
                }
            }
        })
        .await
        .map_err(|_| OAuthError::Store("OAuth credential validation task failed".into()))?
    }

    fn register_successor(
        &self,
        provider: OAuthProvider,
        base_snapshot: AuthEntrySnapshot,
        generation: u64,
        successor: OAuthCredential,
        refresh_guard: RefreshGuard,
    ) -> Result<(), OAuthError> {
        let slot = background_inflight_slot(provider);
        let mut state = lock_mutex(&self.shared);
        if state.generations[slot] != generation {
            return Err(OAuthError::CredentialsChanged);
        }
        set_refresh_window(&mut state, provider, &successor);
        state.pending[slot] = Some(PendingRefresh {
            base_snapshot,
            successor,
            generation,
            warning: None,
            _guard: refresh_guard,
        });
        state.volatile[slot] = None;
        Ok(())
    }

    async fn send_refresh(
        &self,
        provider: OAuthProvider,
        credential: &OAuthCredential,
        control: Arc<RefreshControl>,
    ) -> Result<OAuthCredential, OAuthError> {
        match provider {
            OAuthProvider::Anthropic => {
                let control = Arc::clone(&control);
                anthropic::refresh_with_dispatch(
                    &self.client,
                    &self.endpoints,
                    credential,
                    move || control.begin_dispatch(),
                )
                .await
            }
            OAuthProvider::OpenAiCodex => {
                let control = Arc::clone(&control);
                codex::refresh_with_dispatch(&self.client, &self.endpoints, credential, move || {
                    control.begin_dispatch()
                })
                .await
            }
            OAuthProvider::Xai => {
                let control = Arc::clone(&control);
                xai::refresh_with_dispatch(&self.client, &self.endpoints, credential, move || {
                    control.begin_dispatch()
                })
                .await
            }
        }
    }

    fn spawn_background_refresh(
        &self,
        provider: OAuthProvider,
        credential: OAuthCredential,
        control: Arc<RefreshControl>,
    ) {
        let slot = background_inflight_slot(provider);
        if self.background_inflight[slot].swap(true, Ordering::AcqRel) {
            return;
        }
        if !control.transfer_to_background() {
            self.background_inflight[slot].store(false, Ordering::Release);
            return;
        }
        let worker = self.clone();
        let task_control = Arc::clone(&control);
        let task = async move {
            let _completion = JobCompletion(Arc::clone(&task_control), true);
            let mut finish = BackgroundRefreshFinish {
                inflight: Arc::clone(&worker.background_inflight),
                shared: Arc::clone(&worker.shared),
                slot,
                succeeded: false,
            };
            match worker
                .prepare_credential(
                    provider,
                    credential,
                    task_control,
                    OAUTH_BACKGROUND_REFRESH_MS,
                    false,
                )
                .await
            {
                Ok(_result) => finish.succeeded = true,
                Err(OAuthError::Cancelled | OAuthError::CredentialsChanged) => {
                    finish.succeeded = true
                }
                Err(error) => {
                    add_warning(&worker.shared, error.to_string());
                    finish.succeeded = true;
                }
            }
        };
        if self.lifecycle.spawn(Arc::clone(&control), task).is_err() {
            self.background_inflight[slot].store(false, Ordering::Release);
            control.complete_owner();
        }
    }

    fn background_refresh_due(
        &self,
        provider: OAuthProvider,
        credential: &OAuthCredential,
    ) -> bool {
        lock_mutex(&self.shared).refresh_windows[background_inflight_slot(provider)]
            .as_ref()
            .is_none_or(|(refreshed, not_before)| {
                refreshed != credential || now_ms() >= *not_before
            })
    }

    fn note_observed_credential(&self, provider: OAuthProvider, credential: &OAuthCredential) {
        let mut state = lock_mutex(&self.shared);
        let slot = background_inflight_slot(provider);
        state.store_credential_seen[slot] = true;
        let already_known = state.refresh_windows[slot]
            .as_ref()
            .is_some_and(|(known, _)| known == credential);
        if !already_known {
            set_refresh_window(&mut state, provider, credential);
        }
    }

    fn note_store_credential_seen(&self, provider: OAuthProvider) {
        lock_mutex(&self.shared).store_credential_seen[background_inflight_slot(provider)] = true;
    }

    fn spawn_reconciler(&self, provider: OAuthProvider) {
        let slot = background_inflight_slot(provider);
        {
            let mut state = lock_mutex(&self.shared);
            if state.pending[slot].is_none() || state.reconciling[slot] {
                return;
            }
            state.reconciling[slot] = true;
        }
        let control = Arc::new(RefreshControl::new(provider));
        let worker = self.clone();
        let task_control = Arc::clone(&control);
        let task = async move {
            let _completion = JobCompletion(Arc::clone(&task_control), true);
            let _finish = ReconciliationFinish {
                shared: Arc::clone(&worker.shared),
                slot,
            };
            let mut delay = Duration::from_millis(250);
            loop {
                if worker.lifecycle.is_shutting_down() {
                    let _ = worker.reconcile_pending(provider).await;
                    worker.release_pending_at_shutdown(provider);
                    break;
                }
                tokio::time::sleep(delay).await;
                match worker.reconcile_pending(provider).await {
                    Ok(true) => {
                        delay = (delay * 2).min(Duration::from_secs(5));
                    }
                    Ok(false) | Err(OAuthError::CredentialsChanged) => break,
                    Err(_) => {
                        delay = (delay * 2).min(Duration::from_secs(5));
                    }
                }
                if worker.lifecycle.is_shutting_down() {
                    let _ = worker.reconcile_pending(provider).await;
                    worker.release_pending_at_shutdown(provider);
                    break;
                }
            }
        };
        if self.lifecycle.spawn(Arc::clone(&control), task).is_err() {
            lock_mutex(&self.shared).reconciling[slot] = false;
        }
    }

    async fn reconcile_pending(&self, provider: OAuthProvider) -> Result<bool, OAuthError> {
        let fallback = {
            let state = lock_mutex(&self.shared);
            state.pending[background_inflight_slot(provider)]
                .as_ref()
                .map(|pending| pending.successor.clone())
        };
        let Some(fallback) = fallback else {
            return Ok(false);
        };
        let _gate = self.refresh_gate[background_inflight_slot(provider)]
            .lock()
            .await;
        let selected = self.select_credential(provider, fallback, None).await?;
        Ok(selected.pending)
    }

    async fn reconcile_pending_until(
        &self,
        provider: OAuthProvider,
        deadline: tokio::time::Instant,
    ) -> Result<bool, OAuthError> {
        let fallback = {
            let state = lock_mutex(&self.shared);
            state.pending[background_inflight_slot(provider)]
                .as_ref()
                .map(|pending| pending.successor.clone())
        };
        let Some(fallback) = fallback else {
            return Ok(false);
        };
        let _gate = tokio::time::timeout_at(
            deadline,
            self.refresh_gate[background_inflight_slot(provider)].lock(),
        )
        .await
        .map_err(|_| {
            OAuthError::Store("OAuth persistence reconciliation deadline exceeded".into())
        })?;
        let selected = self
            .select_credential_until(provider, fallback, deadline)
            .await?;
        Ok(selected.pending)
    }

    fn release_pending_at_shutdown(&self, provider: OAuthProvider) {
        let slot = background_inflight_slot(provider);
        let mut state = lock_mutex(&self.shared);
        if let Some(pending) = state.pending[slot].take() {
            if let Some(warning) = pending.warning {
                push_warning(&mut state.warnings, warning);
            }
            push_warning(
                &mut state.warnings,
                "OAuth successor could not be persisted before service shutdown".into(),
            );
        }
        if let Some(volatile) = state.volatile[slot].take() {
            push_warning(&mut state.warnings, volatile.warning);
            push_warning(
                &mut state.warnings,
                "OAuth successor remained process-local because the auth store was unavailable at shutdown".into(),
            );
        }
    }

    fn release_unresolved_at_shutdown(&self) {
        for provider in [
            OAuthProvider::Anthropic,
            OAuthProvider::OpenAiCodex,
            OAuthProvider::Xai,
        ] {
            self.release_pending_at_shutdown(provider);
        }
    }

    async fn finalize_pending(&self, deadline: tokio::time::Instant) -> Vec<String> {
        let providers = [
            OAuthProvider::Anthropic,
            OAuthProvider::OpenAiCodex,
            OAuthProvider::Xai,
        ];
        let mut warnings = Vec::new();
        for provider in providers {
            if let Err(error) = self.reconcile_pending_until(provider, deadline).await {
                if !matches!(error, OAuthError::CredentialsChanged) {
                    warnings.push(error.to_string());
                }
            }
            self.release_pending_at_shutdown(provider);
        }
        warnings
    }

    fn invalidate_local(&self, provider: OAuthProvider) {
        let slot = background_inflight_slot(provider);
        {
            let mut state = lock_mutex(&self.shared);
            state.generations[slot] = state.generations[slot].wrapping_add(1);
            state.pending[slot].take();
            state.volatile[slot].take();
            state.refresh_windows[slot] = None;
        }
        self.lifecycle.cancel_provider(provider);
    }

    fn invalidate_local_locked(&self, provider: OAuthProvider) {
        self.invalidate_local(provider);
    }

    fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut lock_mutex(&self.shared).warnings)
    }
}

impl OAuthLifecycle {
    fn cancel_provider(&self, provider: OAuthProvider) {
        let state = lock_mutex(&self.state);
        for control in state.controls.iter().filter_map(Weak::upgrade) {
            if control.provider == provider {
                control.cancel_before_dispatch();
            }
        }
    }
}

fn select_credential_sync(
    store: &OAuthStore,
    shared: &Mutex<OAuthSharedState>,
    provider: OAuthProvider,
    fallback: OAuthCredential,
    deadline: std::time::Instant,
    cancel_control: Option<Arc<RefreshControl>>,
    cancel_on_shutdown: Option<Arc<OAuthLifecycle>>,
) -> Result<SelectedCredential, OAuthError> {
    let slot = background_inflight_slot(provider);
    let pending_hint = {
        let state = lock_mutex(shared);
        state.pending[slot].as_ref().map(|pending| {
            (
                pending.successor.clone(),
                pending.base_snapshot.clone(),
                pending.generation,
                pending.warning.clone(),
            )
        })
    };
    let _store_guard = match store.lock_exclusive_until(deadline, || {
        selection_cancelled(&cancel_control, &cancel_on_shutdown)
    }) {
        Ok(guard) => guard,
        Err(error) => {
            if selection_cancelled(&cancel_control, &cancel_on_shutdown) {
                return Err(OAuthError::Cancelled);
            }
            if std::time::Instant::now() >= deadline {
                return Err(error);
            }
            if let Some((credential, snapshot, generation, warning)) = pending_hint {
                let still_pending = {
                    let state = lock_mutex(shared);
                    state.generations[slot] == generation
                        && state.pending[slot].as_ref().is_some_and(|pending| {
                            pending.generation == generation
                                && pending.successor == credential
                                && pending.base_snapshot == snapshot
                        })
                };
                if !still_pending {
                    return Err(OAuthError::CredentialsChanged);
                }
                if let Ok((_, current)) = store.credential_snapshot(provider) {
                    if current != snapshot
                        && !(current.selection_matches(&snapshot)
                            && current.oauth_matches(&credential))
                    {
                        return Err(OAuthError::CredentialsChanged);
                    }
                }
                let message = warning.unwrap_or_else(|| {
                    format!("OAuth refreshed in memory but persistence is pending: {error}")
                });
                return Ok(SelectedCredential {
                    credential,
                    snapshot,
                    generation,
                    pending: true,
                    persistence_warning: Some(message),
                });
            }
            return Err(error);
        }
    };
    if selection_cancelled(&cancel_control, &cancel_on_shutdown) {
        return Err(OAuthError::Cancelled);
    }
    let mut state = lock_mutex(shared);
    let generation = state.generations[slot];
    if let Some(pending) = state.pending[slot].as_ref() {
        if pending.generation != generation {
            state.pending[slot].take();
            return Err(OAuthError::CredentialsChanged);
        }
        let base_snapshot = pending.base_snapshot.clone();
        let successor = pending.successor.clone();
        match store.credential_snapshot(provider) {
            Ok((_, current_snapshot)) => {
                if current_snapshot == base_snapshot {
                    match store.persist_refresh_locked_if_unchanged(
                        provider,
                        &base_snapshot,
                        &successor,
                    ) {
                        Ok(true) => {
                            state.pending[slot].take();
                            return Ok(SelectedCredential {
                                credential: successor.clone(),
                                snapshot: base_snapshot.with_oauth(&successor)?,
                                generation,
                                pending: false,
                                persistence_warning: None,
                            });
                        }
                        Ok(false) => {
                            state.pending[slot].take();
                            return Err(OAuthError::CredentialsChanged);
                        }
                        Err(error) => {
                            let warning =
                                format!("OAuth refreshed in memory but was not persisted: {error}");
                            if let Some(pending) = state.pending[slot].as_mut() {
                                pending.warning = Some(warning.clone());
                            }
                            return Ok(SelectedCredential {
                                credential: successor,
                                snapshot: base_snapshot,
                                generation,
                                pending: true,
                                persistence_warning: Some(warning),
                            });
                        }
                    }
                }
                if current_snapshot.selection_matches(&base_snapshot)
                    && current_snapshot.oauth_matches(&successor)
                {
                    state.pending[slot].take();
                    return Ok(SelectedCredential {
                        credential: successor,
                        snapshot: current_snapshot,
                        generation,
                        pending: false,
                        persistence_warning: None,
                    });
                }
                state.pending[slot].take();
                Err(OAuthError::CredentialsChanged)
            }
            Err(error) => {
                let warning = format!(
                    "OAuth refreshed in memory but auth store could not be reconciled: {error}"
                );
                if let Some(pending) = state.pending[slot].as_mut() {
                    pending.warning = Some(warning.clone());
                }
                Ok(SelectedCredential {
                    credential: successor,
                    snapshot: base_snapshot,
                    generation,
                    pending: true,
                    persistence_warning: Some(warning),
                })
            }
        }
    } else {
        match store.credential_snapshot(provider) {
            Ok((stored, snapshot)) => {
                if snapshot.prefers_api_key() || (snapshot.prefers_oauth() && stored.is_none()) {
                    return Err(OAuthError::CredentialsChanged);
                }
                if let Some(volatile) = state.volatile[slot].as_ref() {
                    if volatile.generation != generation {
                        state.volatile[slot].take();
                        return Err(OAuthError::CredentialsChanged);
                    }
                    if snapshot == volatile.base_snapshot {
                        return Ok(SelectedCredential {
                            credential: volatile.credential.clone(),
                            snapshot,
                            generation,
                            pending: false,
                            persistence_warning: None,
                        });
                    }
                }
                if state.volatile[slot].is_some() {
                    state.volatile[slot].take();
                    if let Some(stored) = stored {
                        state.store_credential_seen[slot] = true;
                        return Ok(SelectedCredential {
                            credential: stored,
                            snapshot,
                            generation,
                            pending: false,
                            persistence_warning: None,
                        });
                    }
                    return Err(OAuthError::CredentialsChanged);
                }
                if stored.is_some() {
                    state.store_credential_seen[slot] = true;
                } else if state.store_credential_seen[slot] {
                    return Err(OAuthError::CredentialsChanged);
                }
                Ok(SelectedCredential {
                    credential: stored.unwrap_or(fallback),
                    snapshot,
                    generation,
                    pending: false,
                    persistence_warning: None,
                })
            }
            Err(error) => {
                if let Some(volatile) = state.volatile[slot].as_ref() {
                    if volatile.generation != generation {
                        state.volatile[slot].take();
                        return Err(OAuthError::CredentialsChanged);
                    }
                    return Ok(SelectedCredential {
                        credential: volatile.credential.clone(),
                        snapshot: volatile.base_snapshot.clone(),
                        generation,
                        pending: false,
                        persistence_warning: Some(volatile.warning.clone()),
                    });
                }
                Err(error)
            }
        }
    }
}

fn selection_cancelled(
    control: &Option<Arc<RefreshControl>>,
    lifecycle: &Option<Arc<OAuthLifecycle>>,
) -> bool {
    control.as_ref().is_some_and(|control| control.cancelled())
        || lifecycle
            .as_ref()
            .is_some_and(|lifecycle| lifecycle.is_shutting_down())
}

fn set_refresh_window(
    state: &mut OAuthSharedState,
    provider: OAuthProvider,
    credential: &OAuthCredential,
) {
    let now = now_ms();
    let delay =
        (expiry_ms(credential.expires).saturating_sub(now) / 2).min(OAUTH_BACKGROUND_REFRESH_MS);
    state.refresh_windows[background_inflight_slot(provider)] =
        Some((credential.clone(), now.saturating_add(delay)));
}

fn add_warning(shared: &Mutex<OAuthSharedState>, warning: String) {
    push_warning(&mut lock_mutex(shared).warnings, warning);
}

fn push_warning(warnings: &mut Vec<String>, warning: String) {
    if !warning.is_empty() && !warnings.contains(&warning) {
        warnings.push(warning);
    }
}

async fn parse_json_response_bounded<T: DeserializeOwned>(
    mut response: reqwest::Response,
    invalid_message: &'static str,
) -> Result<T, OAuthError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_OAUTH_RESPONSE_BYTES as u64)
    {
        return Err(OAuthError::InvalidResponse(invalid_message.into()));
    }
    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(MAX_OAUTH_RESPONSE_BYTES as u64) as usize,
    );
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| OAuthError::InvalidResponse(invalid_message.into()))?
    {
        if bytes
            .len()
            .checked_add(chunk.len())
            .is_none_or(|length| length > MAX_OAUTH_RESPONSE_BYTES)
        {
            return Err(OAuthError::InvalidResponse(invalid_message.into()));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| OAuthError::InvalidResponse(invalid_message.into()))
}

impl OAuthService {
    /// The launcher this service opens authorization URLs with; MCP sign-in
    /// (`/mcp login`) opens its own URLs with the same one.
    pub(crate) fn browser(&self) -> Arc<dyn BrowserLauncher> {
        Arc::clone(&self.worker.browser)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn expiry_ms(expires: u64) -> u64 {
    if expires < 1_000_000_000_000 {
        expires.saturating_mul(1_000)
    } else {
        expires
    }
}

fn background_inflight_slot(provider: OAuthProvider) -> usize {
    match provider {
        OAuthProvider::Anthropic => 0,
        OAuthProvider::OpenAiCodex => 1,
        OAuthProvider::Xai => 2,
    }
}

fn lock_mutex<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
