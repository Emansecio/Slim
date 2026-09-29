use super::*;
use std::sync::atomic::AtomicU8;

/// One explicit retry permit for the currently paused provider request.
/// Never durable: reopening a session must still use recovery/preflight.
#[derive(Clone, Debug, Default)]
pub struct ManualRetryHandle(Arc<RetryState>);

/// No provider request is paused: `request` is refused.
const UNAVAILABLE: u8 = 0;
/// The paused request waits for the user's `/retry`.
const WAITING: u8 = 1;
/// The user asked for the retry; the paused request may resume.
const ACCEPTED: u8 = 2;

#[derive(Debug, Default)]
struct RetryState {
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
            .compare_exchange(WAITING, ACCEPTED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.0.notify.notify_one();
        true
    }

    pub fn is_waiting(&self) -> bool {
        self.0.state.load(Ordering::Acquire) == WAITING
    }

    async fn requested(&self) {
        loop {
            let notified = self.0.notify.notified();
            if self.0.state.load(Ordering::Acquire) == ACCEPTED {
                return;
            }
            notified.await;
        }
    }
}

struct RetryWaitGuard(ManualRetryHandle);
impl Drop for RetryWaitGuard {
    fn drop(&mut self) {
        self.0 .0.state.store(UNAVAILABLE, Ordering::Release);
    }
}

/// Reports the paused connection to the user; `detail` says why or what next.
fn push_connecting(
    app: &mut AppHandle,
    next_seq: &mut u64,
    detail: String,
) -> Result<(), ProviderError> {
    push_runtime_event(
        app,
        next_seq,
        crate::EventKind::ProviderPhase {
            phase: ProviderPhase::Connecting,
            elapsed_ms: 0,
            detail: Some(detail),
        },
    )
}

impl Runtime {
    pub fn set_manual_retry_handle(&mut self, handle: ManualRetryHandle) {
        self.manual_retry = Some(handle);
    }

    /// A pause is only offered for a recoverable failure that has not yet
    /// produced side effects or visible output that a retry would duplicate.
    fn may_pause(&self, error: &ProviderError, event_start: usize) -> bool {
        recoverable_provider_error(error)
            && !self.shell_jobs.running()
            && !request_emitted_tools(&self.app, event_start)
            && !self
                .app
                .events()
                .get(event_start..)
                .unwrap_or_default()
                .iter()
                .any(|event| {
                    matches!(&event.kind,
                        crate::EventKind::AssistantTextDelta { text }
                        | crate::EventKind::ReasoningDelta { text }
                        if !text.is_empty())
                })
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
        if !self.may_pause(error, event_start) {
            return Ok(false);
        }
        let delay = requested_provider_recovery_delay(error, 1, backoff);
        let Some(deadline) = tokio::time::Instant::now().checked_add(delay) else {
            return Ok(false);
        };
        let guard = RetryWaitGuard(handle.clone());
        handle.0.state.store(WAITING, Ordering::Release);
        push_runtime_event(&mut self.app, next_seq, crate::EventKind::ThinkingEnded)?;
        let reason = self.redact_sensitive(&provider_retry_reason(error));
        push_connecting(
            &mut self.app,
            next_seq,
            format!(
                "Conexão pausada · /retry para tentar novamente · Esc para cancelar · {reason}"
            ),
        )?;
        let resumed = tokio::select! {
            biased;
            _ = CancellationToken::cancelled_or_pending(self.cancellation.clone()) => false,
            result = async {
                handle.requested().await;
                push_connecting(
                    &mut self.app,
                    next_seq,
                    "Retry solicitado · aguardando intervalo do provedor".into(),
                )?;
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
