mod anthropic;
pub mod browser;
pub mod callback;
mod codex;
pub mod pkce;
mod store;
mod types;

use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, watch};

pub use browser::{BrowserLauncher, SystemBrowser};
pub use codex::account_id as codex_account_id;
pub use store::OAuthStore;
pub use types::{OAuthCredential, OAuthEndpoints, OAuthError, OAuthProgress, OAuthProvider};

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
        })
    }

    pub fn active(&self) -> Result<Option<(OAuthProvider, OAuthCredential)>, OAuthError> {
        self.store.active()
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
        };
        self.store.save(provider, &credential)?;
        Ok(credential)
    }

    pub async fn fresh_credential(
        &self,
        provider: OAuthProvider,
        credential: OAuthCredential,
    ) -> Result<FreshCredential, OAuthError> {
        if credential.expires > now_ms().saturating_add(5 * 60 * 1000) {
            return Ok(FreshCredential {
                credential,
                persistence_warning: None,
            });
        }
        let store = self.store.clone();
        let _guard = tokio::task::spawn_blocking(move || store.lock_exclusive())
            .await
            .map_err(|_| OAuthError::Store("auth store lock task failed".into()))??;
        let credential = self.store.credential(provider)?.unwrap_or(credential);
        if credential.expires > now_ms().saturating_add(5 * 60 * 1000) {
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
        };
        let persistence_warning = self
            .store
            .save_locked(provider, &refreshed)
            .err()
            .map(|error| format!("OAuth refreshed in memory but was not persisted: {error}"));
        Ok(FreshCredential {
            credential: refreshed,
            persistence_warning,
        })
    }

    pub fn logout(&self, provider: OAuthProvider) -> Result<(), OAuthError> {
        self.store.remove(provider)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
