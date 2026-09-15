mod anthropic;
pub mod browser;
pub mod callback;
mod codex;
pub mod pkce;
mod store;
mod types;
mod xai;

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use tokio::sync::{mpsc, watch, Mutex as AsyncMutex};

pub use browser::{BrowserLauncher, SystemBrowser};
pub use codex::account_id as codex_account_id;
pub use store::OAuthStore;
pub use types::{OAuthCredential, OAuthEndpoints, OAuthError, OAuthProgress, OAuthProvider};

const MAX_OAUTH_RESPONSE_BYTES: usize = 64 * 1024;
const OAUTH_BLOCKING_REFRESH_MS: u64 = 30_000;
const OAUTH_BACKGROUND_REFRESH_MS: u64 = 10 * 60 * 1000;

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

#[derive(Clone)]
pub struct OAuthService {
    client: reqwest::Client,
    endpoints: OAuthEndpoints,
    browser: Arc<dyn BrowserLauncher>,
    store: OAuthStore,
    refresh_gate: Arc<AsyncMutex<()>>,
    background_inflight: Arc<[AtomicBool; 3]>,
    background_warning: Arc<Mutex<Option<String>>>,
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
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| OAuthError::Transport("OAuth HTTP client creation failed".into()))?;
        Ok(Self {
            client,
            endpoints,
            browser,
            store,
            refresh_gate: Arc::new(AsyncMutex::new(())),
            background_inflight: Arc::new([
                AtomicBool::new(false),
                AtomicBool::new(false),
                AtomicBool::new(false),
            ]),
            background_warning: Arc::new(Mutex::new(None)),
        })
    }

    pub fn active(&self) -> Result<Option<(OAuthProvider, OAuthCredential)>, OAuthError> {
        self.store.active()
    }

    pub fn credential(
        &self,
        provider: OAuthProvider,
    ) -> Result<Option<OAuthCredential>, OAuthError> {
        self.store.credential(provider)
    }

    pub fn activate(
        &self,
        provider: OAuthProvider,
        credential: &OAuthCredential,
    ) -> Result<(), OAuthError> {
        self.store.save(provider, credential)
    }

    pub fn save_api_key(&self, provider: &str, key: &str) -> Result<(), OAuthError> {
        self.store.save_api_key(provider, key)
    }

    pub fn activate_api_key(&self, provider: &str) -> Result<Option<String>, OAuthError> {
        self.store.activate_api_key(provider)
    }

    pub fn remove_api_key(&self, provider: &str) -> Result<(), OAuthError> {
        self.store.remove_api_key(provider)
    }

    pub fn active_provider_key(&self) -> Result<Option<String>, OAuthError> {
        self.store.active_provider_key()
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
                    &self.client,
                    &self.endpoints,
                    self.browser.clone(),
                    &progress,
                    cancel,
                )
                .await?
            }
            OAuthProvider::OpenAiCodex => {
                codex::login(
                    &self.client,
                    &self.endpoints,
                    self.browser.clone(),
                    &progress,
                    cancel,
                )
                .await?
            }
            OAuthProvider::Xai => {
                xai::login(
                    &self.client,
                    &self.endpoints,
                    self.browser.clone(),
                    &progress,
                    cancel,
                )
                .await?
            }
        };
        self.store.save(provider, &credential)?;
        Ok(credential)
    }

    pub async fn fresh_credential(
        &self,
        provider: OAuthProvider,
        credential: OAuthCredential,
    ) -> Result<FreshCredential, OAuthError> {
        let credential = match self.store.credential(provider) {
            Ok(Some(stored)) => stored,
            Ok(None) | Err(_) => credential,
        };
        let remaining = expiry_ms(credential.expires).saturating_sub(now_ms());
        if remaining > OAUTH_BLOCKING_REFRESH_MS {
            if remaining < OAUTH_BACKGROUND_REFRESH_MS {
                self.spawn_background_refresh(provider, credential.clone());
            }
            return Ok(FreshCredential {
                credential,
                persistence_warning: self.take_background_warning(),
            });
        }
        self.refresh_locked(provider, credential, OAUTH_BLOCKING_REFRESH_MS)
            .await
    }

    fn spawn_background_refresh(&self, provider: OAuthProvider, credential: OAuthCredential) {
        let slot = background_inflight_slot(provider);
        if self.background_inflight[slot].swap(true, Ordering::AcqRel) {
            return;
        }
        let service = self.clone();
        tokio::spawn(async move {
            let result = service
                .refresh_locked(provider, credential, OAUTH_BACKGROUND_REFRESH_MS)
                .await;
            service.background_inflight[slot].store(false, Ordering::Release);
            if let Err(error) = result {
                *lock_mutex(&service.background_warning) = Some(error.to_string());
            }
        });
    }

    fn take_background_warning(&self) -> Option<String> {
        lock_mutex(&self.background_warning).take()
    }

    async fn refresh_locked(
        &self,
        provider: OAuthProvider,
        credential: OAuthCredential,
        refresh_if_remaining_ms: u64,
    ) -> Result<FreshCredential, OAuthError> {
        let _gate = self.refresh_gate.lock().await;
        let credential = self.load_credential_locked(provider, credential).await?;
        if expiry_ms(credential.expires).saturating_sub(now_ms()) > refresh_if_remaining_ms {
            return Ok(FreshCredential {
                credential,
                persistence_warning: None,
            });
        }
        let refreshed = match provider {
            OAuthProvider::Anthropic => {
                anthropic::refresh(&self.client, &self.endpoints, &credential).await?
            }
            OAuthProvider::OpenAiCodex => {
                codex::refresh(&self.client, &self.endpoints, &credential).await?
            }
            OAuthProvider::Xai => xai::refresh(&self.client, &self.endpoints, &credential).await?,
        };
        let persistence_warning = self.persist_refreshed(provider, &refreshed).await;
        Ok(FreshCredential {
            credential: refreshed,
            persistence_warning,
        })
    }

    async fn load_credential_locked(
        &self,
        provider: OAuthProvider,
        fallback: OAuthCredential,
    ) -> Result<OAuthCredential, OAuthError> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = store.lock_exclusive()?;
            Ok(store.credential(provider)?.unwrap_or(fallback))
        })
        .await
        .map_err(|_| OAuthError::Store("auth store lock task failed".into()))?
    }

    async fn persist_refreshed(
        &self,
        provider: OAuthProvider,
        refreshed: &OAuthCredential,
    ) -> Option<String> {
        let store = self.store.clone();
        let refreshed = refreshed.clone();
        match tokio::task::spawn_blocking(move || {
            let _guard = store.lock_exclusive()?;
            store.save_locked(provider, &refreshed)
        })
        .await
        {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(format!(
                "OAuth refreshed in memory but was not persisted: {error}"
            )),
            Err(_) => Some(
                "OAuth refreshed in memory but was not persisted: auth store lock task failed"
                    .into(),
            ),
        }
    }

    pub fn logout(&self, provider: OAuthProvider) -> Result<(), OAuthError> {
        self.store.remove(provider)
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
