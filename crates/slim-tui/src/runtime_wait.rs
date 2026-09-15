use std::io;
use std::time::Duration;

use crate::api::WakeSignal;
use crate::app::FrameClock;

pub(crate) const MOTION_INTERVAL_MS: u64 = 83;
const STATUS_INTERVAL_MS: u64 = 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WaitOutcome {
    Input,
    Wake,
    Deadline,
}

fn elapsed_millis(elapsed: Duration) -> u64 {
    elapsed.as_millis().min(u64::MAX as u128) as u64
}

pub(crate) fn runtime_clock(elapsed: Duration) -> FrameClock {
    let elapsed_ms = elapsed_millis(elapsed);
    FrameClock {
        frame: elapsed_ms / MOTION_INTERVAL_MS,
        elapsed_ms,
    }
}

pub(crate) fn next_visual_deadline(
    elapsed: Duration,
    last_motion_frame: u64,
    last_status_second: u64,
    motion_visible: bool,
    status_visible: bool,
    next_toast_expiry_ms: Option<u64>,
) -> Option<Duration> {
    let now_ms = elapsed_millis(elapsed);
    let motion = motion_visible.then(|| {
        Duration::from_millis(
            last_motion_frame
                .saturating_add(1)
                .saturating_mul(MOTION_INTERVAL_MS)
                .saturating_sub(now_ms),
        )
    });
    let status = status_visible.then(|| {
        Duration::from_millis(
            last_status_second
                .saturating_add(1)
                .saturating_mul(STATUS_INTERVAL_MS)
                .saturating_sub(now_ms),
        )
    });
    let toast = next_toast_expiry_ms
        .map(|expiry_ms| Duration::from_millis(expiry_ms.saturating_sub(now_ms)));
    [motion, status, toast].into_iter().flatten().min()
}

#[cfg(windows)]
pub(crate) fn wait_for_runtime_signal(
    wake: &WakeSignal,
    timeout: Option<Duration>,
) -> io::Result<WaitOutcome> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};

    let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if input.is_null() || input == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    wait_for_handles(Some(input), wake, timeout)
}

#[cfg(windows)]
fn wait_for_handles(
    input: Option<windows_sys::Win32::Foundation::HANDLE>,
    wake: &WakeSignal,
    timeout: Option<Duration>,
) -> io::Result<WaitOutcome> {
    use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{WaitForMultipleObjects, INFINITE};

    let mut handles = Vec::with_capacity(2);
    if let Some(input) = input {
        handles.push(input);
    }
    let wake_index = handles.len() as u32;
    handles.push(wake.raw_handle());
    let timeout_ms = timeout.map_or(INFINITE, |value| {
        if value.is_zero() {
            0
        } else {
            value.as_millis().clamp(1, (INFINITE - 1) as u128) as u32
        }
    });
    // SAFETY: both handles remain valid for this call, the slice length
    // matches the pointer, and WaitForMultipleObjects does not take ownership.
    let outcome =
        unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, timeout_ms) };
    if outcome == WAIT_TIMEOUT {
        return Ok(WaitOutcome::Deadline);
    }
    if outcome == WAIT_FAILED {
        return Err(io::Error::last_os_error());
    }
    let index = outcome
        .checked_sub(WAIT_OBJECT_0)
        .ok_or_else(|| io::Error::other(format!("unexpected runtime wait outcome {outcome}")))?;
    if input.is_some() && index == 0 {
        Ok(WaitOutcome::Input)
    } else if index == wake_index {
        Ok(WaitOutcome::Wake)
    } else {
        Err(io::Error::other(format!(
            "unexpected runtime wait handle index {index}"
        )))
    }
}

/// Unix wait: `poll` on terminal input (fd 0 — the TUI only runs on a TTY)
/// and the wake pipe together, so lane events interrupt the wait without
/// waiting for terminal input or a 60 s fallback deadline.
#[cfg(unix)]
pub(crate) fn wait_for_runtime_signal(
    wake: &WakeSignal,
    timeout: Option<Duration>,
) -> io::Result<WaitOutcome> {
    use std::os::fd::AsRawFd;

    // Keep the legacy 60 s cap as a defensive backstop even though callers
    // armed the signal and probed the lanes before entering.
    let timeout_ms = timeout.map_or(60_000, |value| {
        i32::try_from(value.as_millis()).unwrap_or(i32::MAX).max(0)
    });
    let mut fds = [
        libc::pollfd {
            fd: std::io::stdin().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake.raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        // SAFETY: `fds` is a valid slice for the duration of the call and
        // `nfds` matches its length.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready == 0 {
            return Ok(WaitOutcome::Deadline);
        }
        break;
    }
    let input_ready =
        fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0;
    let wake_ready = fds[1].revents & libc::POLLIN != 0;
    if wake_ready {
        wake.drain_pipe();
    }
    if input_ready {
        return Ok(WaitOutcome::Input);
    }
    if wake_ready {
        return Ok(WaitOutcome::Wake);
    }
    // e.g. POLLNVAL on the wake fd — nothing actionable; treat as a deadline
    // so the loop re-probes instead of spinning on a dead descriptor.
    Ok(WaitOutcome::Deadline)
}

#[cfg(not(any(windows, unix)))]
pub(crate) fn wait_for_runtime_signal(
    _wake: &WakeSignal,
    timeout: Option<Duration>,
) -> io::Result<WaitOutcome> {
    let timeout = timeout.unwrap_or(Duration::from_secs(60));
    if crossterm::event::poll(timeout)? {
        Ok(WaitOutcome::Input)
    } else {
        Ok(WaitOutcome::Deadline)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn runtime_clock_uses_elapsed_motion_boundaries() {
        assert_eq!(runtime_clock(Duration::from_millis(82)).frame, 0);
        assert_eq!(runtime_clock(Duration::from_millis(83)).frame, 1);
        assert_eq!(runtime_clock(Duration::from_millis(249)).frame, 3);
        assert_eq!(runtime_clock(Duration::from_millis(249)).elapsed_ms, 249);
    }

    #[test]
    fn idle_runtime_has_no_periodic_deadline() {
        assert_eq!(
            next_visual_deadline(Duration::from_secs(7), 84, 7, false, false, None),
            None
        );
    }

    #[test]
    fn idle_toast_keeps_only_its_expiry_deadline() {
        assert_eq!(
            next_visual_deadline(
                Duration::from_millis(4_800),
                57,
                4,
                false,
                false,
                Some(5_000),
            ),
            Some(Duration::from_millis(200))
        );
    }

    #[test]
    fn nearest_visual_deadline_uses_remaining_boundary() {
        assert_eq!(
            next_visual_deadline(Duration::from_millis(90), 1, 0, true, true, None),
            Some(Duration::from_millis(76))
        );
        assert_eq!(
            next_visual_deadline(Duration::from_millis(1_000), 12, 0, true, true, None),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn event_draw_consumes_current_motion_and_status_boundaries() {
        // After an idle session starts work, the event-driven draw paints
        // this clock before the runtime computes its next visual deadline.
        let elapsed = Duration::from_millis(10_000);
        let painted = runtime_clock(elapsed);
        assert_eq!(
            next_visual_deadline(
                elapsed,
                painted.frame,
                painted.elapsed_ms / 1_000,
                true,
                true,
                None,
            ),
            Some(Duration::from_millis(43)),
        );
        // Reduced motion retains the next real elapsed-time update.
        assert_eq!(
            next_visual_deadline(elapsed, painted.frame, 10, false, true, None),
            Some(Duration::from_secs(1)),
        );
        // Recording a draw must not suppress an earlier toast milestone.
        assert_eq!(
            next_visual_deadline(elapsed, painted.frame, 10, true, true, Some(10_020)),
            Some(Duration::from_millis(20)),
        );
    }

    #[cfg(windows)]
    #[test]
    fn wake_interrupts_a_long_runtime_wait() {
        let wake = crate::api::WakeSignal::new().expect("wake");
        let notifier = wake.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            notifier.notify();
        });

        let started = std::time::Instant::now();
        let outcome = wait_for_handles(None, &wake, Some(Duration::from_secs(2))).expect("wait");
        thread.join().expect("notifier");

        assert_eq!(outcome, WaitOutcome::Wake);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn wake_interrupts_a_long_runtime_wait() {
        let wake = crate::api::WakeSignal::new().expect("wake");
        let notifier = wake.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            notifier.notify();
        });

        let started = std::time::Instant::now();
        let outcome = wait_for_runtime_signal(&wake, Some(Duration::from_secs(2))).expect("wait");
        thread.join().expect("notifier");

        assert_eq!(outcome, WaitOutcome::Wake);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
