use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::runtime::CancellationToken;
use serde::{Deserialize, Serialize};

#[cfg(all(test, windows))]
mod performance;
#[cfg(windows)]
pub(crate) mod windows_job;

const EXECUTABLE_CACHE_CAPACITY: usize = 128;
const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Clone, Debug)]
pub struct ExecutableResolver {
    inner: Arc<Mutex<ResolverState>>,
    environment: ResolverEnvironment,
}

#[derive(Clone, Debug)]
enum ResolverEnvironment {
    Process,
    Fixed { path: OsString, path_ext: OsString },
}

#[derive(Debug, Default)]
struct ResolverState {
    generation: u64,
    cache: HashMap<ResolverKey, Option<PathBuf>>,
    order: VecDeque<ResolverKey>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ResolverKey {
    generation: u64,
    program: OsString,
    path: OsString,
    path_ext: OsString,
}

impl Default for ExecutableResolver {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ResolverState::default())),
            environment: ResolverEnvironment::Process,
        }
    }
}

impl ExecutableResolver {
    /// Fixed environment for deterministic tests without process PATH changes.
    pub fn with_environment(path: OsString, path_ext: OsString) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ResolverState::default())),
            environment: ResolverEnvironment::Fixed { path, path_ext },
        }
    }

    pub fn resolve(&self, program: impl AsRef<Path>) -> io::Result<Option<PathBuf>> {
        let program = program.as_ref();
        if has_path_component(program) {
            let path_ext = self.environment_values().1;
            return Ok(resolve_explicit_path(program, &path_ext));
        }

        let (path, path_ext) = self.environment_values();
        let generation = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .generation;
        let key = ResolverKey {
            generation,
            program: program.as_os_str().to_os_string(),
            path: path.clone(),
            path_ext: path_ext.clone(),
        };
        {
            let mut state = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(cached) = state.cache.get(&key).cloned() {
                if cached
                    .as_ref()
                    .is_none_or(|candidate| is_executable_file(candidate))
                {
                    touch_cache_key(&mut state.order, &key);
                    return Ok(cached);
                }
                state.cache.remove(&key);
                state.order.retain(|candidate| candidate != &key);
            }
        }

        let resolved = search_path(program.as_os_str(), &path, &path_ext);
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        insert_bounded(&mut state, key, resolved.clone());
        Ok(resolved)
    }

    /// Clears positive and negative entries while retaining this shared instance.
    pub fn bump_generation(&self) {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.generation = state.generation.wrapping_add(1);
        state.cache.clear();
        state.order.clear();
    }

    fn environment_values(&self) -> (OsString, OsString) {
        match &self.environment {
            ResolverEnvironment::Process => (
                std::env::var_os("PATH").unwrap_or_default(),
                std::env::var_os("PATHEXT").unwrap_or_default(),
            ),
            ResolverEnvironment::Fixed { path, path_ext } => (path.clone(), path_ext.clone()),
        }
    }
}

fn has_path_component(program: &Path) -> bool {
    program.is_absolute() || program.components().count() > 1
}

/// Absolute or relative paths skip PATH search, but Windows still rejects
/// extensionless shell shims (`npx`, npm's bash wrapper) so the `.cmd`
/// sibling is used instead of CreateProcess error 193.
fn resolve_explicit_path(program: &Path, path_ext: &OsStr) -> Option<PathBuf> {
    if is_executable_file(program) {
        return Some(program.to_path_buf());
    }
    #[cfg(windows)]
    {
        if program.extension().is_none() {
            if let (Some(name), Some(parent)) = (program.file_name(), program.parent()) {
                return executable_names(name, path_ext)
                    .into_iter()
                    .map(|candidate| parent.join(candidate))
                    .find(|candidate| is_executable_file(candidate));
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = path_ext;
    }
    None
}

fn touch_cache_key(order: &mut VecDeque<ResolverKey>, key: &ResolverKey) {
    order.retain(|candidate| candidate != key);
    order.push_back(key.clone());
}

fn insert_bounded(state: &mut ResolverState, key: ResolverKey, value: Option<PathBuf>) {
    touch_cache_key(&mut state.order, &key);
    state.cache.insert(key, value);
    while state.order.len() > EXECUTABLE_CACHE_CAPACITY {
        if let Some(oldest) = state.order.pop_front() {
            state.cache.remove(&oldest);
        }
    }
}

fn search_path(program: &OsStr, path: &OsStr, path_ext: &OsStr) -> Option<PathBuf> {
    let candidates = executable_names(program, path_ext);
    std::env::split_paths(path).find_map(|directory| {
        candidates.iter().find_map(|name| {
            let candidate = directory.join(name);
            is_executable_file(&candidate).then_some(candidate)
        })
    })
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        // Windows only executes extensionless files that are real PE images;
        // shell shims such as npm's `npx` must fall through to `npx.cmd`.
        path.extension().is_some() || has_pe_header(path)
    }
    #[cfg(all(not(unix), not(windows)))]
    {
        true
    }
}

#[cfg(windows)]
fn has_pe_header(path: &Path) -> bool {
    use std::io::Read;

    let mut magic = [0u8; 2];
    std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .is_ok()
        && magic == *b"MZ"
}

fn executable_names(program: &OsStr, path_ext: &OsStr) -> Vec<OsString> {
    #[cfg(windows)]
    {
        if Path::new(program).extension().is_some() {
            return vec![program.to_os_string()];
        }
        let mut names = vec![program.to_os_string()];
        let extensions = path_ext.to_string_lossy();
        for extension in extensions
            .split(';')
            .filter(|extension| !extension.is_empty())
        {
            let mut name = program.to_os_string();
            name.push(extension);
            names.push(name);
        }
        names
    }
    #[cfg(not(windows))]
    {
        let _ = path_ext;
        vec![program.to_os_string()]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessOutputBudget {
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
}

impl ProcessOutputBudget {
    pub const fn per_stream(bytes: usize) -> Self {
        Self {
            stdout_bytes: bytes,
            stderr_bytes: bytes,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProcessRequest {
    pub cwd: PathBuf,
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub timeout: Duration,
    pub cancellation: Option<CancellationToken>,
    pub output_budget: ProcessOutputBudget,
}

#[derive(Debug)]
pub struct ProcessRunOutput {
    pub output: Output,
    pub timed_out: bool,
    pub cancelled: bool,
    pub stdout_discarded_bytes: usize,
    pub stderr_discarded_bytes: usize,
}

/// Facts observed while running one process. These describe process
/// execution only; they do not establish whether the surrounding task
/// succeeded.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProcessExecutionFacts {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub stdout_discarded_bytes: u64,
    pub stderr_discarded_bytes: u64,
}

impl ProcessExecutionFacts {
    pub(crate) fn from_output(
        output: &Output,
        timed_out: bool,
        cancelled: bool,
        stdout_discarded_bytes: usize,
        stderr_discarded_bytes: usize,
    ) -> Self {
        let stdout_discarded_bytes = u64::try_from(stdout_discarded_bytes).unwrap_or(u64::MAX);
        let stderr_discarded_bytes = u64::try_from(stderr_discarded_bytes).unwrap_or(u64::MAX);
        ProcessExecutionFacts {
            exit_code: output.status.code(),
            timed_out,
            cancelled,
            stdout_bytes: u64::try_from(output.stdout.len())
                .unwrap_or(u64::MAX)
                .saturating_add(stdout_discarded_bytes),
            stderr_bytes: u64::try_from(output.stderr.len())
                .unwrap_or(u64::MAX)
                .saturating_add(stderr_discarded_bytes),
            stdout_discarded_bytes,
            stderr_discarded_bytes,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProcessProgress {
    pub elapsed_ms: u64,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    pub last_line: String,
}

#[derive(Clone, Debug, Default)]
pub struct ProcessRunner {
    resolver: ExecutableResolver,
}

impl ProcessRunner {
    pub fn new(resolver: ExecutableResolver) -> Self {
        Self { resolver }
    }

    pub fn resolver(&self) -> &ExecutableResolver {
        &self.resolver
    }

    pub fn resolve_powershell(&self) -> io::Result<Option<PathBuf>> {
        match self.resolver.resolve("pwsh")? {
            Some(program) => Ok(Some(program)),
            None => self.resolver.resolve("powershell"),
        }
    }

    pub fn run(&self, request: ProcessRequest) -> io::Result<ProcessRunOutput> {
        self.run_with_progress(request, |_| {})
    }

    pub fn run_with_progress(
        &self,
        request: ProcessRequest,
        mut on_progress: impl FnMut(ProcessProgress),
    ) -> io::Result<ProcessRunOutput> {
        #[cfg(all(test, windows))]
        performance::mark("runner_enter");
        let program = self.resolver.resolve(&request.program)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("executable not found: {}", request.program.display()),
            )
        })?;
        let mut command = std::process::Command::new(program);
        command
            .args(&request.args)
            .current_dir(&request.cwd)
            // Tool text is UTF-8. Python otherwise uses the Windows ANSI code
            // page for pipes; preserve an explicitly configured override.
            .env(
                "PYTHONIOENCODING",
                std::env::var_os("PYTHONIOENCODING").unwrap_or_else(|| "utf-8".into()),
            )
            // Tools are noninteractive; runtime input must remain with the UI.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        let (mut child, job) = windows_job::Job::spawn(&mut command)?;
        #[cfg(not(windows))]
        let mut child = command.spawn()?;
        #[cfg(all(test, windows))]
        performance::mark("spawn_complete");
        #[cfg(unix)]
        let pid = child.id();
        let stdout = child.stdout.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "process stdout pipe unavailable")
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "process stderr pipe unavailable")
        })?;
        let progress = Arc::new(Mutex::new(PipeProgress::default()));
        let stop_reading = CancellationToken::new();
        let stdout_reader = spawn_pipe_reader(
            stdout,
            PipeKind::Stdout,
            request.output_budget.stdout_bytes,
            Arc::clone(&progress),
            stop_reading.clone(),
        );
        let stderr_reader = spawn_pipe_reader(
            stderr,
            PipeKind::Stderr,
            request.output_budget.stderr_bytes,
            Arc::clone(&progress),
            stop_reading.clone(),
        );
        #[cfg(all(test, windows))]
        performance::mark("readers_spawned");
        let started = Instant::now();
        let mut next_progress = Duration::from_secs(1);
        let mut timed_out = false;
        let mut cancelled = false;
        let status = (|| -> io::Result<ExitStatus> {
            let mut status = None;
            loop {
                if status.is_none() {
                    status = child.try_wait()?;
                    #[cfg(windows)]
                    if status.is_some() {
                        // The process may close both pipes while their readers
                        // are parked. Recheck EOF now, without cancelling drain:
                        // descendants may still own and write to those pipes.
                        stdout_reader.thread().unpark();
                        stderr_reader.thread().unpark();
                    }
                    #[cfg(all(test, windows))]
                    if status.is_some() {
                        performance::mark_exit(&child);
                        performance::mark("exit_observed");
                    }
                }
                if let Some(status) = status {
                    if stdout_reader.is_finished() && stderr_reader.is_finished() {
                        return Ok(status);
                    }
                }
                cancelled = request
                    .cancellation
                    .as_ref()
                    .is_some_and(CancellationToken::is_cancelled);
                timed_out = !cancelled && started.elapsed() >= request.timeout;
                if cancelled || timed_out {
                    stop_reading.cancel();
                    #[cfg(windows)]
                    let tree_result = job.terminate();
                    #[cfg(unix)]
                    let tree_result = terminate_process_tree(&self.resolver, pid);
                    if status.is_none() {
                        // Retain the child handle: even a missing or failed tree
                        // utility must not prevent terminating the direct child.
                        let _ = child.kill();
                        status = wait_for_exit(&mut child, Duration::from_millis(500))?;
                    }
                    tree_result.map_err(|error| io::Error::other(format!(
                        "process interrupted; descendant termination unconfirmed; side effects may still occur: {error}"
                    )))?;
                    return status.ok_or_else(|| io::Error::new(
                        io::ErrorKind::TimedOut,
                        "process interrupted but termination is unconfirmed; do not automatically repeat the operation",
                    ));
                }
                let elapsed = started.elapsed();
                if elapsed >= next_progress {
                    let snapshot = progress
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .snapshot(elapsed);
                    on_progress(snapshot);
                    next_progress = next_progress.saturating_add(Duration::from_secs(1));
                }
                let wait = CANCELLATION_POLL_INTERVAL.min(request.timeout.saturating_sub(elapsed));
                #[cfg(windows)]
                wait_for_process_progress(
                    &child,
                    &stdout_reader,
                    &stderr_reader,
                    status.is_some(),
                    wait,
                )?;
                #[cfg(not(windows))]
                std::thread::sleep(wait);
            }
        })();
        #[cfg(all(test, windows))]
        performance::mark("wait_complete");
        // Pipe readers use readiness polling, so stopping and joining them does
        // not depend on a descendant closing an inherited output handle.
        stop_reading.cancel();
        #[cfg(windows)]
        {
            stdout_reader.thread().unpark();
            stderr_reader.thread().unpark();
        }
        let stdout = join_pipe(stdout_reader, "stdout");
        let stderr = join_pipe(stderr_reader, "stderr");
        #[cfg(all(test, windows))]
        performance::mark("readers_joined");
        let status = status.map_err(|error| process_error_with_output(error, &stdout, &stderr))?;
        let stdout = stdout?;
        let stderr = stderr?;
        let result = ProcessRunOutput {
            output: Output {
                status,
                stdout: stdout.bytes,
                stderr: stderr.bytes,
            },
            timed_out,
            cancelled,
            stdout_discarded_bytes: stdout.discarded_bytes,
            stderr_discarded_bytes: stderr.discarded_bytes,
        };
        #[cfg(all(test, windows))]
        performance::mark("result_ready");
        #[cfg(windows)]
        drop(job);
        #[cfg(all(test, windows))]
        performance::mark("job_closed");
        drop(child);
        #[cfg(all(test, windows))]
        performance::mark("handles_closed");
        Ok(result)
    }
}

#[cfg(windows)]
fn wait_for_process_progress(
    child: &std::process::Child,
    stdout: &JoinHandle<io::Result<PipeCapture>>,
    stderr: &JoinHandle<io::Result<PipeCapture>>,
    process_exited: bool,
    timeout: Duration,
) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::WAIT_FAILED;
    use windows_sys::Win32::System::Threading::{WaitForMultipleObjects, WaitForSingleObject};

    // Windows waits take whole milliseconds. Round up like a timed sleep,
    // rather than busy-polling during the final fractional millisecond.
    let millis = u32::try_from(timeout.as_nanos().div_ceil(1_000_000)).unwrap_or(u32::MAX - 1);
    // SAFETY: all handles are borrowed from live owners and cannot be closed
    // during the wait. Process/thread handles stay signaled after termination.
    // Wait for BOTH readers after exit: waiting for either would spin on the
    // first finished reader while a descendant still holds the other pipe.
    let result = unsafe {
        if process_exited {
            let readers = [stdout.as_raw_handle(), stderr.as_raw_handle()];
            WaitForMultipleObjects(readers.len() as u32, readers.as_ptr(), 1, millis)
        } else {
            WaitForSingleObject(child.as_raw_handle(), millis)
        }
    };
    if result == WAIT_FAILED {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn process_error_with_output(
    error: io::Error,
    stdout: &io::Result<PipeCapture>,
    stderr: &io::Result<PipeCapture>,
) -> io::Error {
    let describe = |capture: &io::Result<PipeCapture>| match capture {
        Ok(capture) => format!(
            "{}{}",
            String::from_utf8_lossy(&capture.bytes),
            if capture.discarded_bytes == 0 {
                String::new()
            } else {
                format!(
                    "\n[{} bytes omitted by capture limit]",
                    capture.discarded_bytes
                )
            }
        ),
        Err(error) => format!("[capture failed: {error}]"),
    };
    io::Error::new(
        error.kind(),
        format!(
            "{error}\nstdout:\n{}\nstderr:\n{}",
            describe(stdout),
            describe(stderr)
        ),
    )
}

fn wait_for_exit(
    child: &mut std::process::Child,
    timeout: Duration,
) -> io::Result<Option<ExitStatus>> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if started.elapsed() >= timeout {
            return Ok(None);
        }
        std::thread::sleep(CANCELLATION_POLL_INTERVAL);
    }
}

#[cfg(unix)]
use std::os::fd::AsRawFd as PipeHandle;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle as PipeHandle;

// Each pipe has a single reader. Read only bytes reported ready, never wait
// inside Read for a writer that may outlive its parent.
#[cfg(windows)]
fn ready_bytes(pipe: &impl PipeHandle) -> io::Result<usize> {
    #[link(name = "kernel32")]
    extern "system" {
        fn PeekNamedPipe(
            handle: *mut std::ffi::c_void,
            buffer: *mut std::ffi::c_void,
            size: u32,
            read: *mut u32,
            available: *mut u32,
            remaining: *mut u32,
        ) -> i32;
    }
    let mut available = 0;
    // SAFETY: the owned pipe remains alive throughout the call; available is
    // writable, and the API permits null pointers for unused outputs/buffer.
    let success = unsafe {
        PeekNamedPipe(
            pipe.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    };
    if success != 0 {
        Ok(available as usize)
    } else {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(109) {
            Ok(usize::MAX)
        } else {
            Err(error)
        }
    }
}

#[cfg(unix)]
fn ready_bytes(pipe: &impl PipeHandle) -> io::Result<usize> {
    let mut descriptor = libc::pollfd {
        fd: pipe.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: descriptor points to one initialized pollfd and the timeout is zero.
    let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    if descriptor.revents & libc::POLLNVAL != 0 {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "invalid process pipe",
        ));
    }
    Ok(if result == 0 { 0 } else { 16 * 1024 })
}

struct PipeCapture {
    bytes: Vec<u8>,
    discarded_bytes: usize,
}

#[derive(Clone, Copy)]
enum PipeKind {
    Stdout,
    Stderr,
}

#[derive(Default)]
struct PipeProgress {
    stdout_bytes: usize,
    stderr_bytes: usize,
    last_line: String,
}

impl PipeProgress {
    fn snapshot(&self, elapsed: Duration) -> ProcessProgress {
        ProcessProgress {
            elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            stdout_bytes: self.stdout_bytes,
            stderr_bytes: self.stderr_bytes,
            last_line: self.last_line.clone(),
        }
    }
}

fn spawn_pipe_reader(
    pipe: impl Read + PipeHandle + Send + 'static,
    kind: PipeKind,
    budget: usize,
    progress: Arc<Mutex<PipeProgress>>,
    stop_reading: CancellationToken,
) -> JoinHandle<io::Result<PipeCapture>> {
    std::thread::spawn(move || read_pipe(pipe, kind, budget, progress, stop_reading))
}

fn read_pipe(
    mut pipe: impl Read + PipeHandle,
    kind: PipeKind,
    budget: usize,
    progress: Arc<Mutex<PipeProgress>>,
    stop_reading: CancellationToken,
) -> io::Result<PipeCapture> {
    #[cfg(all(test, windows))]
    performance::mark(match kind {
        PipeKind::Stdout => "stdout_start",
        PipeKind::Stderr => "stderr_start",
    });
    let head_budget = budget / 2 + budget % 2;
    let tail_budget = budget - head_budget;
    let mut head = Vec::with_capacity(head_budget.min(16 * 1024));
    let mut tail = VecDeque::<u8>::with_capacity(tail_budget.min(16 * 1024));
    let mut total_bytes = 0usize;
    let mut chunk = [0_u8; 16 * 1024];
    while !stop_reading.is_cancelled() {
        let available = match ready_bytes(&pipe) {
            Ok(available) => available,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if available == 0 {
            #[cfg(windows)]
            std::thread::park_timeout(CANCELLATION_POLL_INTERVAL);
            #[cfg(not(windows))]
            std::thread::sleep(CANCELLATION_POLL_INTERVAL);
            continue;
        }
        #[cfg(windows)]
        if available == usize::MAX {
            break;
        }
        let capacity = available.min(chunk.len());
        let read = match pipe.read(&mut chunk[..capacity]) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        total_bytes = total_bytes.saturating_add(read);
        let head_captured = read.min(head_budget.saturating_sub(head.len()));
        head.extend_from_slice(&chunk[..head_captured]);
        let trailing = &chunk[head_captured..read];
        if tail_budget > 0 && !trailing.is_empty() {
            if trailing.len() >= tail_budget {
                tail.clear();
                tail.extend(&trailing[trailing.len() - tail_budget..]);
            } else {
                let overflow = tail
                    .len()
                    .saturating_add(trailing.len())
                    .saturating_sub(tail_budget);
                tail.drain(..overflow);
                tail.extend(trailing);
            }
        }
        let mut progress = progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match kind {
            PipeKind::Stdout => progress.stdout_bytes = progress.stdout_bytes.saturating_add(read),
            PipeKind::Stderr => progress.stderr_bytes = progress.stderr_bytes.saturating_add(read),
        }
        if let Some(line) = String::from_utf8_lossy(&chunk[..read])
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
        {
            progress.last_line = bound_last_line(line);
        }
    }
    let discarded_bytes = total_bytes.saturating_sub(head.len().saturating_add(tail.len()));
    head.extend(tail);
    drop(pipe);
    #[cfg(all(test, windows))]
    performance::mark(match kind {
        PipeKind::Stdout => "stdout_closed",
        PipeKind::Stderr => "stderr_closed",
    });
    Ok(PipeCapture {
        bytes: head,
        discarded_bytes,
    })
}

fn bound_last_line(line: &str) -> String {
    const LIMIT: usize = 160;
    let trimmed = line.trim();
    let mut chars = trimmed.chars();
    let mut bounded = chars.by_ref().take(LIMIT).collect::<String>();
    if chars.next().is_some() {
        bounded.pop();
        bounded.push('…');
    }
    bounded
}

fn join_pipe(reader: JoinHandle<io::Result<PipeCapture>>, stream: &str) -> io::Result<PipeCapture> {
    reader
        .join()
        .map_err(|_| io::Error::other(format!("process {stream} reader panicked")))?
}

#[cfg(any(unix, test))]
fn resolve_termination_program(resolver: &ExecutableResolver) -> io::Result<Option<PathBuf>> {
    #[cfg(windows)]
    let program = "taskkill.exe";
    #[cfg(unix)]
    let program = "kill";
    resolver.resolve(program)
}

#[cfg(unix)]
pub(crate) fn terminate_process_tree(resolver: &ExecutableResolver, pid: u32) -> io::Result<()> {
    let program = resolve_termination_program(resolver)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "process tree termination utility unavailable",
        )
    })?;
    #[cfg(windows)]
    let args = vec![
        "/PID".to_owned(),
        pid.to_string(),
        "/T".to_owned(),
        "/F".to_owned(),
    ];
    #[cfg(unix)]
    let args = vec!["-KILL".to_owned(), "--".to_owned(), format!("-{pid}")];
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let mut child = command.spawn()?;
    match wait_for_exit(&mut child, Duration::from_millis(500))? {
        Some(status) if status.success() => Ok(()),
        Some(status) => Err(io::Error::other(format!(
            "termination utility exited with {status}"
        ))),
        None => {
            let _ = child.kill();
            let _ = wait_for_exit(&mut child, Duration::from_millis(100));
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "termination utility timed out",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn interruption_error_preserves_captured_output_and_limits() {
        let stdout = Ok(PipeCapture {
            bytes: b"partial stdout".to_vec(),
            discarded_bytes: 7,
        });
        let stderr = Ok(PipeCapture {
            bytes: b"partial stderr".to_vec(),
            discarded_bytes: 0,
        });
        let error = process_error_with_output(
            io::Error::new(io::ErrorKind::TimedOut, "termination unconfirmed"),
            &stdout,
            &stderr,
        );
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let message = error.to_string();
        assert!(message.contains("termination unconfirmed"));
        assert!(message.contains("stdout:\npartial stdout"));
        assert!(message.contains("stderr:\npartial stderr"));
        assert!(message.contains("7 bytes omitted"));
    }

    #[test]
    fn interruption_error_keeps_other_stream_when_one_reader_fails() {
        let error = process_error_with_output(
            io::Error::other("termination unconfirmed"),
            &Ok(PipeCapture {
                bytes: b"completed effect receipt".to_vec(),
                discarded_bytes: 0,
            }),
            &Err(io::Error::other("broken reader")),
        );
        let message = error.to_string();
        assert!(message.contains("completed effect receipt"));
        assert!(message.contains("capture failed: broken reader"));
    }

    #[test]
    #[ignore = "subprocess fixture"]
    fn inherited_pipe_holder() {
        std::thread::sleep(Duration::from_secs(3));
    }

    #[test]
    #[ignore = "subprocess fixture"]
    fn inherited_pipe_parent() {
        // This subprocess fixture must exit before its child to reproduce
        // inherited pipes surviving their original parent. The holder exits
        // after three seconds; waiting here would remove the regression.
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "process::tests::inherited_pipe_holder",
            "--ignored",
        ]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        #[allow(clippy::zombie_processes)]
        let _child = command.spawn().unwrap();
    }

    #[test]
    fn timeout_includes_pipes_inherited_by_a_descendant() {
        let started = Instant::now();
        let result = ProcessRunner::default().run(ProcessRequest {
            cwd: std::env::current_dir().unwrap(),
            program: std::env::current_exe().unwrap(),
            args: [
                "--exact",
                "process::tests::inherited_pipe_parent",
                "--ignored",
            ]
            .into_iter()
            .map(OsString::from)
            .collect(),
            timeout: Duration::from_millis(250),
            cancellation: None,
            output_budget: ProcessOutputBudget::per_stream(1024),
        });
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "pipe wait escaped timeout: {:?}",
            started.elapsed()
        );
        assert!(result.is_err() || result.unwrap().timed_out);
    }

    #[cfg(unix)]
    #[test]
    fn missing_termination_utility_does_not_block_cancellation() {
        let runner = ProcessRunner::new(ExecutableResolver::with_environment("".into(), "".into()));
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let started = Instant::now();
        let error = runner
            .run(ProcessRequest {
                cwd: std::env::current_dir().unwrap(),
                program: std::env::current_exe().unwrap(),
                args: [
                    "--exact",
                    "process::tests::inherited_pipe_holder",
                    "--ignored",
                ]
                .into_iter()
                .map(OsString::from)
                .collect(),
                timeout: Duration::from_secs(30),
                cancellation: Some(cancellation),
                output_budget: ProcessOutputBudget::per_stream(1024),
            })
            .expect_err("unconfirmed tree termination must be visible");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            error.to_string().contains("termination unconfirmed"),
            "{error}"
        );
    }

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "slim-executable-resolver-{}-{name}",
            std::process::id()
        ))
    }

    #[test]
    fn termination_utility_is_resolved_from_explicit_path_environment() {
        let root = temp_root("termination");
        fs::create_dir_all(&root).expect("root");
        let name = if cfg!(windows) {
            "taskkill.exe"
        } else {
            "kill"
        };
        let executable = root.join(name);
        fs::write(&executable, b"fixture").expect("executable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
                .expect("executable mode");
        }
        let resolver = ExecutableResolver::with_environment(
            std::env::join_paths([&root]).expect("PATH"),
            OsString::from(if cfg!(windows) { ".EXE" } else { "" }),
        );

        assert_eq!(
            resolve_termination_program(&resolver).expect("resolve"),
            Some(executable)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn resolver_skips_non_executable_unix_candidates() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root("unix-executable");
        let first = root.join("first");
        let second = root.join("second");
        fs::create_dir_all(&first).expect("first");
        fs::create_dir_all(&second).expect("second");
        let blocked = first.join("fixture");
        let executable = second.join("fixture");
        fs::write(&blocked, b"blocked").expect("blocked");
        fs::write(&executable, b"executable").expect("executable");
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o644)).expect("blocked mode");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
            .expect("executable mode");
        let resolver = ExecutableResolver::with_environment(
            std::env::join_paths([&first, &second]).expect("PATH"),
            OsString::new(),
        );

        assert_eq!(
            resolver.resolve("fixture").expect("resolve"),
            Some(executable)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolver_caches_by_environment_without_where_processes() {
        let root = temp_root("cache");
        fs::create_dir_all(&root).expect("root");
        let executable = root.join(if cfg!(windows) { "fake.EXE" } else { "fake" });
        fs::write(&executable, b"fake").expect("executable");
        let resolver = ExecutableResolver::with_environment(
            std::env::join_paths([&root]).expect("PATH"),
            OsString::from(if cfg!(windows) { ".EXE" } else { "" }),
        );

        assert_eq!(resolver.resolve("fake").expect("resolve"), Some(executable));
        assert_eq!(resolver.resolve("missing").expect("negative"), None);
        fs::write(
            root.join(if cfg!(windows) {
                "missing.EXE"
            } else {
                "missing"
            }),
            b"late",
        )
        .expect("late executable");
        assert_eq!(resolver.resolve("missing").expect("cached negative"), None);
        resolver.bump_generation();
        assert!(resolver.resolve("missing").expect("invalidated").is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn resolver_skips_extensionless_shell_shim_for_path_ext_candidate() {
        let root = temp_root("windows-shim");
        fs::create_dir_all(&root).expect("root");
        let shim = root.join("shim");
        let command = root.join("shim.CMD");
        fs::write(&shim, b"#!/bin/sh\nexec node \"$@\"\n").expect("shim");
        fs::write(&command, b"@echo off\r\n").expect("command shim");
        let resolver = ExecutableResolver::with_environment(
            std::env::join_paths([&root]).expect("PATH"),
            OsString::from(".COM;.EXE;.BAT;.CMD"),
        );

        assert_eq!(
            resolver.resolve("shim").expect("resolve"),
            Some(command.clone())
        );
        assert_eq!(
            resolver.resolve(&shim).expect("absolute shim"),
            Some(command)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn resolved_cmd_shim_spawns_without_win32_error_193() {
        use std::process::{Command, Stdio};

        let root = temp_root("cmd-shim-spawn");
        fs::create_dir_all(&root).expect("root");
        let shim = root.join("shim");
        let command = root.join("shim.CMD");
        fs::write(&shim, b"#!/bin/sh\nexit 1\n").expect("shim");
        fs::write(&command, b"@echo off\r\necho SHIM_OK\r\n").expect("command shim");
        let resolver = ExecutableResolver::with_environment(
            std::env::join_paths([&root]).expect("PATH"),
            OsString::from(".COM;.EXE;.BAT;.CMD"),
        );
        let resolved = resolver.resolve("shim").expect("resolve").expect("found");
        assert_eq!(resolved, command);

        let mut process = Command::new(&resolved);
        process
            .current_dir(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let (child, _job) = super::windows_job::Job::spawn(&mut process).expect("spawn");
        let output = child.wait_with_output().expect("wait");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "shim spawn failed: {output:?} stdout={stdout}"
        );
        assert!(
            stdout.contains("SHIM_OK"),
            "expected cmd shim output, got {stdout:?}"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn resolver_accepts_extensionless_pe_image() {
        let root = temp_root("windows-pe");
        fs::create_dir_all(&root).expect("root");
        let executable = root.join("bare");
        fs::write(&executable, b"MZ\x90\x00").expect("pe image");
        let resolver = ExecutableResolver::with_environment(
            std::env::join_paths([&root]).expect("PATH"),
            OsString::from(".COM;.EXE;.BAT;.CMD"),
        );

        assert_eq!(resolver.resolve("bare").expect("resolve"), Some(executable));
        let _ = fs::remove_dir_all(root);
    }
}
