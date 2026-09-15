//! Own the process tree before any child code can run, including descendants
//! whose immediate parent exits before cancellation or timeout.
use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};
use std::time::{Duration, Instant};
use windows_sys::Win32::System::Diagnostics::ProcessSnapshotting::{
    PssCaptureSnapshot, PssFreeSnapshot, PssWalkMarkerCreate, PssWalkMarkerFree, PssWalkSnapshot,
    HPSS, HPSSWALK, PSS_CAPTURE_THREADS, PSS_THREAD_ENTRY, PSS_WALK_THREADS,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenThread, ResumeThread, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    THREAD_SUSPEND_RESUME,
};

pub(crate) struct Job(OwnedHandle);

#[cfg(test)]
thread_local! {
    static FAIL_BEFORE_RESUME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static CREATED_PID: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

impl Job {
    pub(crate) fn spawn(command: &mut Command) -> io::Result<(Child, Self)> {
        #[cfg(test)]
        super::performance::mark("job_begin");
        // SAFETY: unnamed job, default security, owned exactly once below.
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Self(unsafe { OwnedHandle::from_raw_handle(raw) });
        // SAFETY: this Windows structure permits zero initialization.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: pointer and size describe a live initialized limits structure.
        if unsafe {
            SetInformationJobObject(
                raw,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        #[cfg(test)]
        super::performance::mark("job_configured");
        let mut child = command
            .creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW)
            .spawn()?;
        #[cfg(test)]
        super::performance::mark("process_created");
        #[cfg(test)]
        CREATED_PID.set(child.id());
        // SAFETY: both handles remain owned throughout assignment.
        let assigned = unsafe { AssignProcessToJobObject(raw, child.as_raw_handle()) };
        let assignment_error = (assigned == 0).then(io::Error::last_os_error);
        #[cfg(test)]
        super::performance::mark("job_assigned");
        let setup = if let Some(error) = assignment_error {
            Err(error)
        } else {
            resume_initial_thread(&child)
        };
        if let Err(error) = setup {
            // Never leave a suspended child behind if setup fails.
            let _ = child.kill();
            let _ = super::wait_for_exit(&mut child, Duration::from_millis(500));
            return Err(error);
        }
        Ok((child, job))
    }

    pub(crate) fn terminate(&self) -> io::Result<()> {
        // SAFETY: the job handle remains owned while termination is requested.
        if unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let started = Instant::now();
        loop {
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
            // SAFETY: writable buffer matches the selected information class.
            if unsafe {
                QueryInformationJobObject(
                    self.0.as_raw_handle(),
                    JobObjectBasicAccountingInformation,
                    (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            if info.ActiveProcesses == 0 {
                return Ok(());
            }
            if started.elapsed() >= Duration::from_millis(500) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "job termination not confirmed",
                ));
            }
            std::thread::sleep(super::CANCELLATION_POLL_INTERVAL);
        }
    }
}

struct ThreadSnapshot(HPSS);

impl Drop for ThreadSnapshot {
    fn drop(&mut self) {
        // The snapshot was captured into this process, not into the target.
        // SAFETY: this wrapper exclusively owns a live, locally captured snapshot.
        let _code = unsafe { PssFreeSnapshot(GetCurrentProcess(), self.0) };
        #[cfg(test)]
        assert_eq!(_code, 0, "snapshot cleanup failed");
    }
}

struct WalkMarker(HPSSWALK);

impl Drop for WalkMarker {
    fn drop(&mut self) {
        // SAFETY: this wrapper exclusively owns the marker returned by Create.
        let _code = unsafe { PssWalkMarkerFree(self.0) };
        #[cfg(test)]
        assert_eq!(_code, 0, "snapshot walk marker cleanup failed");
    }
}

fn resume_initial_thread(child: &Child) -> io::Result<()> {
    // Stable std does not expose the initial thread handle. Capture only this
    // child's thread IDs instead of enumerating every thread on the machine.
    // The child is already in its job and remains suspended throughout setup.
    let mut raw = std::ptr::null_mut();
    // SAFETY: the child handle remains owned; raw is writable. Only IDs are
    // requested, without cloning memory, handles or thread contexts.
    let code =
        unsafe { PssCaptureSnapshot(child.as_raw_handle(), PSS_CAPTURE_THREADS, 0, &mut raw) };
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    let snapshot = ThreadSnapshot(raw);
    #[cfg(test)]
    super::performance::mark("snapshot_created");
    let mut raw_marker = std::ptr::null_mut();
    // SAFETY: a null allocator selects the system allocator; output is writable.
    let code = unsafe { PssWalkMarkerCreate(std::ptr::null(), &mut raw_marker) };
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    let marker = WalkMarker(raw_marker);
    let mut entry: PSS_THREAD_ENTRY = unsafe { zeroed() };
    // SAFETY: snapshot/marker are live, and the buffer matches the information class.
    let code = unsafe {
        PssWalkSnapshot(
            snapshot.0,
            PSS_WALK_THREADS,
            marker.0,
            (&mut entry as *mut PSS_THREAD_ENTRY).cast(),
            size_of::<PSS_THREAD_ENTRY>() as u32,
        )
    };
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    if entry.ProcessId != child.id() {
        return Err(io::Error::other(
            "suspended process thread identity mismatch",
        ));
    }
    #[cfg(test)]
    super::performance::mark("thread_found");
    let raw = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.ThreadId) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    let thread = unsafe { OwnedHandle::from_raw_handle(raw) };
    #[cfg(test)]
    if FAIL_BEFORE_RESUME.replace(false) {
        return Err(io::Error::other("injected setup failure before resume"));
    }
    if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
        return Err(io::Error::last_os_error());
    }
    #[cfg(test)]
    super::performance::mark("thread_resumed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    #[test]
    fn setup_failure_cleans_snapshot_and_never_runs_child() {
        let root = std::env::temp_dir().join(format!(
            "slim-suspended-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let mut command = Command::new(std::env::var_os("ComSpec").unwrap());
        command
            .args(["/d", "/c", "echo escaped>escaped.txt"])
            .current_dir(&root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        FAIL_BEFORE_RESUME.set(true);
        let result = Job::spawn(&mut command);
        assert!(
            !FAIL_BEFORE_RESUME.get(),
            "fixture must reach the failure point"
        );
        assert!(result.is_err());
        let pid = CREATED_PID.get();
        assert_ne!(pid, 0);
        let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if !raw.is_null() {
            let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
            let mut code = 259;
            assert_ne!(
                unsafe { GetExitCodeProcess(handle.as_raw_handle(), &mut code) },
                0
            );
            assert_ne!(code, 259, "suspended child survived setup failure");
        }
        let escaped = root.join("escaped.txt").exists();
        std::fs::remove_dir_all(root).unwrap();
        assert!(!escaped, "child executed before setup completed");
    }

    #[test]
    fn process_creation_failure_is_returned() {
        let mut command = Command::new("\0");
        assert!(Job::spawn(&mut command).is_err());
    }
}
