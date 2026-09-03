use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::runtime::CancellationToken;

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
            return Ok(program.is_file().then(|| program.to_path_buf()));
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

    /// Explicit configuration wins over PATH and never silently falls back.
    pub fn resolve_configured(
        &self,
        configured: Option<&Path>,
        fallback: impl AsRef<Path>,
    ) -> io::Result<Option<PathBuf>> {
        match configured {
            Some(program) => self.resolve(program),
            None => self.resolve(fallback),
        }
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
    #[cfg(not(unix))]
    {
        true
    }
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
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn()?;
        let pid = child.id();
        let stdout = child.stdout.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "process stdout pipe unavailable")
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "process stderr pipe unavailable")
        })?;
        let progress = Arc::new(Mutex::new(PipeProgress::default()));
        let stdout_reader = spawn_pipe_reader(
            stdout,
            PipeKind::Stdout,
            request.output_budget.stdout_bytes,
            Arc::clone(&progress),
        );
        let stderr_reader = spawn_pipe_reader(
            stderr,
            PipeKind::Stderr,
            request.output_budget.stderr_bytes,
            Arc::clone(&progress),
        );
        let (status_tx, status_rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = status_tx.send(child.wait());
        });

        let started = Instant::now();
        let mut next_progress = Duration::from_secs(1);
        let mut timed_out = false;
        let mut cancelled = false;
        let status = loop {
            match status_rx.try_recv() {
                Ok(status) => break status?,
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "process waiter stopped before returning status",
                    ));
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            if request
                .cancellation
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
            {
                cancelled = true;
                terminate_process_tree(&self.resolver, pid);
                break recv_status(&status_rx)?;
            }
            if started.elapsed() >= request.timeout {
                timed_out = true;
                terminate_process_tree(&self.resolver, pid);
                break recv_status(&status_rx)?;
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
            let wait = CANCELLATION_POLL_INTERVAL
                .min(next_progress.saturating_sub(elapsed))
                .min(request.timeout.saturating_sub(elapsed));
            match status_rx.recv_timeout(wait) {
                Ok(status) => break status?,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "process waiter stopped before returning status",
                    ));
                }
            }
        };
        let stdout = join_pipe(stdout_reader, "stdout")?;
        let stderr = join_pipe(stderr_reader, "stderr")?;
        Ok(ProcessRunOutput {
            output: Output {
                status,
                stdout: stdout.bytes,
                stderr: stderr.bytes,
            },
            timed_out,
            cancelled,
            stdout_discarded_bytes: stdout.discarded_bytes,
            stderr_discarded_bytes: stderr.discarded_bytes,
        })
    }
}

fn recv_status(receiver: &mpsc::Receiver<io::Result<ExitStatus>>) -> io::Result<ExitStatus> {
    receiver.recv().map_err(|_| {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            "process waiter stopped before returning status",
        )
    })?
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
    pipe: impl Read + Send + 'static,
    kind: PipeKind,
    budget: usize,
    progress: Arc<Mutex<PipeProgress>>,
) -> JoinHandle<io::Result<PipeCapture>> {
    std::thread::spawn(move || read_pipe(pipe, kind, budget, progress))
}

fn read_pipe(
    mut pipe: impl Read,
    kind: PipeKind,
    budget: usize,
    progress: Arc<Mutex<PipeProgress>>,
) -> io::Result<PipeCapture> {
    let head_budget = budget / 2 + budget % 2;
    let tail_budget = budget - head_budget;
    let mut head = Vec::with_capacity(head_budget.min(16 * 1024));
    let mut tail = VecDeque::<u8>::with_capacity(tail_budget.min(16 * 1024));
    let mut total_bytes = 0usize;
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        let read = match pipe.read(&mut chunk) {
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

fn resolve_termination_program(resolver: &ExecutableResolver) -> io::Result<Option<PathBuf>> {
    #[cfg(windows)]
    let program = "taskkill.exe";
    #[cfg(unix)]
    let program = "kill";
    resolver.resolve(program)
}

fn terminate_process_tree(resolver: &ExecutableResolver, pid: u32) {
    let Ok(Some(program)) = resolve_termination_program(resolver) else {
        return;
    };
    #[cfg(windows)]
    let args = vec![
        "/PID".to_owned(),
        pid.to_string(),
        "/T".to_owned(),
        "/F".to_owned(),
    ];
    #[cfg(unix)]
    let args = vec!["-KILL".to_owned(), "--".to_owned(), format!("-{pid}")];
    let _ = std::process::Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
}
