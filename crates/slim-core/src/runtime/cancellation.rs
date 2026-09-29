use super::*;

#[derive(Clone)]
pub struct CancellationToken(pub(super) Arc<CancellationState>);

pub(super) struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
    mcp: McpCancellation,
    pub(super) native_work: AtomicUsize,
    background_work: AtomicUsize,
    native_idle: Notify,
}

pub(super) struct NativeWorkGuard {
    token: CancellationToken,
    background: bool,
}

impl Drop for NativeWorkGuard {
    fn drop(&mut self) {
        let state = &self.token.0;
        let count = if self.background {
            &state.background_work
        } else {
            &state.native_work
        };
        if count.fetch_sub(1, Ordering::AcqRel) == 1 {
            state.native_idle.notify_waiters();
        }
    }
}

impl CancellationToken {
    pub fn new() -> Self {
        Self(Arc::new(CancellationState {
            cancelled: AtomicBool::new(false),
            notify: Notify::new(),
            mcp: McpCancellation::new(),
            native_work: AtomicUsize::new(0),
            background_work: AtomicUsize::new(0),
            native_idle: Notify::new(),
        }))
    }

    pub fn cancel(&self) {
        if !self.0.cancelled.swap(true, Ordering::AcqRel) {
            self.0.notify.notify_waiters();
            self.0.mcp.cancel();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    pub(super) fn mcp_cancellation(&self) -> McpCancellation {
        self.0.mcp.clone()
    }

    pub async fn cancelled(&self) {
        let notified = self.0.notify.notified();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }

    /// Resolves on cancellation; without a token it never resolves. For
    /// `select!` arms that race an operation against an optional token.
    pub(crate) async fn cancelled_or_pending(token: Option<Self>) {
        match token {
            Some(token) => token.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    }

    pub(super) fn track_native_work(&self) -> NativeWorkGuard {
        self.0.native_work.fetch_add(1, Ordering::AcqRel);
        NativeWorkGuard {
            token: self.clone(),
            background: false,
        }
    }

    pub(super) fn track_background_work(&self) -> NativeWorkGuard {
        self.0.background_work.fetch_add(1, Ordering::AcqRel);
        NativeWorkGuard {
            token: self.clone(),
            background: true,
        }
    }

    pub(super) async fn wait_for_foreground_work(&self) {
        loop {
            let notified = self.0.native_idle.notified();
            if self.0.native_work.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }

    /// Aborting an async task does not stop its blocking native worker.
    /// Hosts must wait for these workers before acknowledging cancellation.
    pub async fn wait_for_native_work(&self) {
        loop {
            let notified = self.0.native_idle.notified();
            if self.0.native_work.load(Ordering::Acquire) == 0
                && self.0.background_work.load(Ordering::Acquire) == 0
            {
                return;
            }
            notified.await;
        }
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for CancellationToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl PartialEq for CancellationToken {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for CancellationToken {}
