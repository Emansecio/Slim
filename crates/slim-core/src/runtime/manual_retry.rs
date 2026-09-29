use super::*;
use std::sync::atomic::AtomicU8;

/// One explicit retry permit for the currently paused provider request.
/// Never durable: reopening a session must still use recovery/preflight.
#[derive(Clone, Debug, Default)]
pub struct ManualRetryHandle(Arc<RetryState>);

#[derive(Debug, Default)]
struct RetryState {
    // 0: unavailable, 1: waiting for user, 2: request accepted.
    state: AtomicU8,
    notify: Notify,
}

impl PartialEq for ManualRetryHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for ManualRetryHandle {}

impl ManualRetryHandle {
    pub fn request(&self) -> bool {
        if self
            .0
            .state
            .compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.0.notify.notify_one();
        true
    }

    pub fn is_waiting(&self) -> bool {
        self.0.state.load(Ordering::Acquire) == 1
    }

    async fn requested(&self) {
        loop {
            let notified = self.0.notify.notified();
            if self.0.state.load(Ordering::Acquire) == 2 {
                return;
            }
            notified.await;
        }
    }
}

struct RetryWaitGuard(ManualRetryHandle);
impl Drop for RetryWaitGuard {
    fn drop(&mut self) {
        self.0 .0.state.store(0, Ordering::Release);
    }
}

impl Runtime {
    pub fn set_manual_retry_handle(&mut self, handle: ManualRetryHandle) {
        self.manual_retry = Some(handle);
    }

    pub(super) async fn wait_for_manual_retry(
        &mut self,
        error: &ProviderError,
        event_start: usize,
        backoff: std::time::Duration,
        next_seq: &mut u64,
    ) -> Result<bool, ProviderError> {
        let Some(handle) = self.manual_retry.clone() else {
            return Ok(false);
        };
        if !recoverable_provider_error(error)
            || self.shell_jobs.running()
            || request_emitted_tools(&self.app, event_start)
            || self.app.events().get(event_start..).unwrap_or_default().iter().any(|event| {
                matches!(&event.kind,
                    crate::EventKind::AssistantTextDelta { text } | crate::EventKind::ReasoningDelta { text }
                    if !text.is_empty())
            })
        {
            return Ok(false);
        }
        let delay = requested_provider_recovery_delay(error, 1, backoff);
        let Some(deadline) = tokio::time::Instant::now().checked_add(delay) else {
            return Ok(false);
        };
        let guard = RetryWaitGuard(handle.clone());
        handle.0.state.store(1, Ordering::Release);
        push_runtime_event(&mut self.app, next_seq, crate::EventKind::ThinkingEnded)?;
        let reason = self.redact_sensitive(&provider_retry_reason(error));
        push_runtime_event(
            &mut self.app,
            next_seq,
            crate::EventKind::ProviderPhase {
                phase: ProviderPhase::Connecting,
                elapsed_ms: 0,
                detail: Some(format!(
                    "Conexão pausada · /retry para tentar novamente · Esc para cancelar · {reason}"
                )),
            },
        )?;
        let cancellation = self.cancellation.clone();
        let resumed = tokio::select! {
            biased;
            _ = async {
                match cancellation {
                    Some(token) => token.cancelled().await,
                    None => std::future::pending::<()>().await,
                }
            } => false,
            result = async {
                handle.requested().await;
                push_runtime_event(&mut self.app, next_seq, crate::EventKind::ProviderPhase {
                    phase: ProviderPhase::Connecting,
                    elapsed_ms: 0,
                    detail: Some("Retry solicitado · aguardando intervalo do provedor".into()),
                })?;
                push_runtime_event(&mut self.app, next_seq, crate::EventKind::RetryScheduled {
                    attempt: 1,
                    limit: 1,
                    wait_ms: u64::try_from(deadline.saturating_duration_since(tokio::time::Instant::now()).as_millis()).unwrap_or(u64::MAX),
                    reason: Some("Tentativa manual solicitada pelo usuário".into()),
                })?;
                // An explicit request never bypasses Retry-After/backoff.
                tokio::time::sleep_until(deadline).await;
                Ok::<(), ProviderError>(())
            } => { result?; true },
        };
        drop(guard);
        if resumed {
            self.uncommitted_event_start = None;
        }
        Ok(resumed)
    }
}
