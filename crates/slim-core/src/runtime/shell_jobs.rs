//! Session or run owned shell jobs; workers use the existing process executor.
use super::*;
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write as IoWrite};
use std::sync::MutexGuard;
use std::time::Duration;

const MAX_RUNNING: usize = 4;
const MAX_RETAINED: usize = 32;
const JOB_PREFIX: &str = "shell-";
/// Longest single `shell_job` wait; also the schema's `timeout_ms` maximum.
const MAX_WAIT_MS: u64 = 10_000;
/// One page of job output as the model reads it. The offsets stay raw byte
/// positions in the log, so a page can be followed by `next_offset`; only the
/// text is cleaned of terminal noise, as a finished command's output is.
fn format_output_page(id: &str, output: &ShellJobOutput) -> String {
    format!(
        "job_id={id} start_offset={} next_offset={} output_bytes={} truncated={}\n{}",
        output.start_offset,
        output.next_offset,
        output.output_bytes,
        output.truncated,
        crate::tools::normalize_shell_text(&output.text)
    )
}
fn capped_wait_ms(requested: u64) -> u64 {
    requested.min(MAX_WAIT_MS)
}
const CONTROL_HINT: &str = "Completion is delivered automatically during a run, or with the next prompt when idle. Continue independent work; do not poll. Use wait only when the result is needed.";

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShellJobLimits {
    pub max_running: usize,
    pub max_retained: usize,
    pub memory_bytes: usize,
    pub interrupt_grace_ms: u64,
}
impl Default for ShellJobLimits {
    fn default() -> Self {
        Self {
            max_running: MAX_RUNNING,
            max_retained: MAX_RETAINED,
            memory_bytes: 256 * 1024,
            interrupt_grace_ms: 1500,
        }
    }
}
impl ShellJobLimits {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=64).contains(&self.max_running)
            || !(self.max_running..=1024).contains(&self.max_retained)
            || !(4096..=16 * 1024 * 1024).contains(&self.memory_bytes)
            || !(50..=10_000).contains(&self.interrupt_grace_ms)
        {
            return Err("shell_jobs: max_running=1..64, max_retained=max_running..1024, memory_bytes=4096..16777216, interrupt_grace_ms=50..10000".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ShellJobInfo {
    pub id: String,
    pub command: String,
    pub origin: String,
    pub state: String,
    pub elapsed_ms: u64,
    pub exit_code: Option<i32>,
    pub output_bytes: u64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellJobOutput {
    pub text: String,
    pub start_offset: u64,
    pub output_bytes: u64,
    pub next_offset: u64,
    pub truncated: bool,
    pub log_path: Option<PathBuf>,
}

fn job_name(key: u64) -> String {
    format!("{JOB_PREFIX}{key}")
}
fn job_key(name: &str) -> Option<u64> {
    let key: u64 = name.strip_prefix(JOB_PREFIX)?.parse().ok()?;
    (job_name(key) == name).then_some(key)
}

#[derive(Clone)]
pub struct ShellJobs(Arc<JobsState>);
impl Default for ShellJobs {
    fn default() -> Self {
        Self::new(ShellJobLimits::default()).expect("default job limits")
    }
}
impl std::fmt::Debug for ShellJobs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShellJobs")
            .field("limits", &self.0.limits)
            .finish_non_exhaustive()
    }
}
impl PartialEq for ShellJobs {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for ShellJobs {}
struct JobsState {
    jobs: Mutex<BTreeMap<u64, Job>>,
    serial: std::sync::atomic::AtomicU64,
    changed: Notify,
    limits: ShellJobLimits,
    journal: Mutex<std::sync::Weak<Mutex<crate::session::ManualRunJournal>>>,
    metadata: Mutex<BTreeMap<String, ShellJobInfo>>,
}
struct Job {
    cancellation: CancellationToken,
    interrupt: Arc<AtomicBool>,
    started: Instant,
    preview: String,
    command: String,
    origin: String,
    output: JobOutput,
    batch_id: String,
    call_id: String,
    delivered: bool,
    notified: bool,
    done: Option<Done>,
    lost: Option<ShellJobInfo>,
}
struct JobOutput {
    pending: [Vec<u8>; 2],
    suppress: [bool; 2],
    json_value: [bool; 2],
    text_pending: [String; 2],
    exact_pending: [String; 2],
    tail: String,
    bytes: u64,
    cap: usize,
    secrets: Vec<String>,
    store: Option<ArtifactStore>,
    log: Option<(std::fs::File, PathBuf)>,
    error: Option<String>,
}
impl JobOutput {
    fn new(cap: usize, secrets: Vec<String>, store: Option<ArtifactStore>) -> Self {
        Self {
            pending: Default::default(),
            text_pending: Default::default(),
            exact_pending: Default::default(),
            suppress: [false; 2],
            json_value: [false; 2],
            tail: String::new(),
            bytes: 0,
            cap,
            secrets,
            store,
            log: None,
            error: None,
        }
    }
    fn append(&mut self, stream: usize, data: &[u8], eof: bool) -> std::io::Result<()> {
        self.pending[stream].extend_from_slice(data);
        // Decode before cursor/redaction operations; invalid bytes have one stable replacement.
        loop {
            let raw = &self.pending[stream];
            match std::str::from_utf8(raw) {
                Ok(text) => {
                    self.text_pending[stream].push_str(text);
                    self.pending[stream].clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    self.text_pending[stream]
                        .push_str(std::str::from_utf8(&raw[..valid]).expect("valid UTF-8 prefix"));
                    if let Some(invalid) = error.error_len() {
                        self.text_pending[stream].push('\u{fffd}');
                        self.pending[stream].drain(..valid + invalid);
                    } else if eof {
                        self.text_pending[stream].push_str(&String::from_utf8_lossy(&raw[valid..]));
                        self.pending[stream].clear();
                        break;
                    } else {
                        self.pending[stream].drain(..valid);
                        break;
                    }
                }
            }
        }
        loop {
            let pending = &self.text_pending[stream];
            if self.json_value[stream] {
                let mut parser = serde_json::Deserializer::from_str(pending).into_iter::<Value>();
                match parser.next() {
                    Some(Ok(_)) => {
                        let count = parser.byte_offset();
                        if count == pending.len() && !eof && !pending.ends_with(['"', ']', '}']) {
                            break;
                        }
                        self.text_pending[stream].drain(..count);
                        self.json_value[stream] = false;
                        continue;
                    }
                    Some(Err(error)) if error.is_eof() && !eof => break,
                    None if !eof => break,
                    _ => {
                        self.json_value[stream] = false;
                        self.suppress[stream] = true;
                    }
                }
            }
            if !self.suppress[stream] {
                if let Some(at) =
                    crate::redaction::job_credential_value_start(&self.text_pending[stream])
                {
                    let prefix = &self.text_pending[stream][..at];
                    if prefix
                        .trim_end()
                        .strip_suffix(':')
                        .is_some_and(|s| s.trim_end().ends_with('"'))
                    {
                        let safe = format!("{prefix}\"[REDACTED]\"");
                        self.text_pending[stream].drain(..at);
                        self.json_value[stream] = true;
                        let safe = stream_normalizer::take_redacted_stream_chunk(
                            &mut self.exact_pending[stream],
                            &safe,
                            &self.secrets,
                            false,
                        );
                        self.publish(&safe)?;
                        continue;
                    }
                }
            }
            let pending = &self.text_pending[stream];
            let newline = pending.find('\n').map(|at| at + 1);
            let pending_key = (!eof && !self.suppress[stream])
                .then(|| crate::redaction::job_pending_key_start(pending))
                .flatten();
            let count = newline
                .filter(|count| pending_key.is_none_or(|at| *count <= at))
                .or_else(|| eof.then_some(pending.len()));
            if let Some(count) = count {
                if count == 0 {
                    break;
                }
                let text: String = self.text_pending[stream].drain(..count).collect();
                let safe = if self.suppress[stream] {
                    self.suppress[stream] = false;
                    if text.ends_with("\r\n") {
                        "\r\n".into()
                    } else if text.ends_with('\n') {
                        "\n".into()
                    } else {
                        String::new()
                    }
                } else if let Some(at) = crate::redaction::job_credential_value_start(&text) {
                    format!(
                        "{}[REDACTED]{}",
                        &text[..at],
                        if text.ends_with("\r\n") {
                            "\r\n"
                        } else if text.ends_with('\n') {
                            "\n"
                        } else {
                            ""
                        }
                    )
                } else {
                    text
                };
                let safe = stream_normalizer::take_redacted_stream_chunk(
                    &mut self.exact_pending[stream],
                    &safe,
                    &self.secrets,
                    false,
                );
                self.publish(&safe)?;
            } else {
                if self.suppress[stream] {
                    self.text_pending[stream].clear();
                    break;
                }
                if let Some(at) = crate::redaction::job_credential_value_start(pending) {
                    let generic = format!("{}[REDACTED]", &pending[..at]);
                    self.suppress[stream] = true;
                    self.text_pending[stream].clear();
                    let safe = stream_normalizer::take_redacted_stream_chunk(
                        &mut self.exact_pending[stream],
                        &generic,
                        &self.secrets,
                        false,
                    );
                    self.publish(&safe)?;
                    break;
                }
                // Hold only credential-name prefixes; normal no-newline output is live.
                let lower = pending.to_ascii_lowercase();
                let names: Vec<_> = crate::redaction::SENSITIVE_HEADER_NAMES
                    .iter()
                    .map(|s| format!("{s}:"))
                    .collect();
                let mut end = stream_normalizer::safe_stream_split(&lower, &names);
                if let Some(at) = pending_key {
                    end = end.min(at);
                }
                for name in crate::redaction::SENSITIVE_HEADER_NAMES {
                    if let Some(at) = lower.rfind(name) {
                        if lower[at + name.len()..]
                            .chars()
                            .all(|c| c.is_whitespace() || c == '"')
                        {
                            end = end.min(at);
                        }
                    }
                }
                // A JSON key being split may include its opening quote; keep it for the heuristic.
                if end > 0 && pending[..end].ends_with('"') {
                    end -= 1;
                }
                let text: String = self.text_pending[stream].drain(..end).collect();
                let safe = stream_normalizer::take_redacted_stream_chunk(
                    &mut self.exact_pending[stream],
                    &text,
                    &self.secrets,
                    false,
                );
                self.publish(&safe)?;
                break;
            }
        }
        if eof {
            let safe = stream_normalizer::take_redacted_stream_chunk(
                &mut self.exact_pending[stream],
                "",
                &self.secrets,
                true,
            );
            self.publish(&safe)?;
        }
        if self.text_pending[stream].len() + self.exact_pending[stream].len() > self.cap / 8 {
            return Err(std::io::Error::other(
                "redaction carry exceeds job memory budget; output stopped safely",
            ));
        }
        self.pending[stream].shrink_to(4);
        self.text_pending[stream].shrink_to(self.cap / 8);
        self.exact_pending[stream].shrink_to(self.cap / 8);
        Ok(())
    }
    fn publish(&mut self, text: &str) -> std::io::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        if self.log.is_none() && self.tail.len().saturating_add(text.len()) > self.cap / 2 {
            let store = self
                .store
                .as_ref()
                .ok_or_else(|| std::io::Error::other("job output needs a log store"))?;
            let (mut file, path) = store.create_job_log()?;
            file.write_all(self.tail.as_bytes())?;
            self.log = Some((file, path));
        }
        if let Some((file, _)) = &mut self.log {
            file.seek(SeekFrom::End(0))?;
            file.write_all(text.as_bytes())?;
        }
        self.bytes = self.bytes.saturating_add(text.len() as u64);
        let budget = self.cap / 2;
        let mut cut = text.len().saturating_sub(budget);
        while !text.is_char_boundary(cut) {
            cut += 1;
        }
        let text = &text[cut..];
        let mut remove = self
            .tail
            .len()
            .saturating_add(text.len())
            .saturating_sub(budget);
        while !self.tail.is_char_boundary(remove) {
            remove += 1;
        }
        self.tail.drain(..remove);
        self.tail.push_str(text);
        self.tail.shrink_to(self.cap / 2);
        Ok(())
    }
    fn read(
        &self,
        since: Option<u64>,
        tail: Option<usize>,
        max_bytes: usize,
    ) -> Result<ShellJobOutput, String> {
        self.read_window(since, tail, None, max_bytes)
    }
    fn read_window(
        &self,
        since: Option<u64>,
        tail: Option<usize>,
        before: Option<u64>,
        max_bytes: usize,
    ) -> Result<ShellJobOutput, String> {
        if since.is_some() && tail.is_some() {
            return Err("Use either since/offset or tail, not both".into());
        }
        let end = before.unwrap_or(self.bytes);
        if end > self.bytes {
            return Err("page end exceeds output length".into());
        }
        let offset = if tail.is_some() || before.is_some() {
            end.saturating_sub(max_bytes as u64)
        } else {
            since.unwrap_or(0)
        };
        if offset > end {
            return Err(format!("offset exceeds output length {end}"));
        }
        let mut bytes = Vec::new();
        if let Some((file, _)) = &self.log {
            let mut file = file.try_clone().map_err(|e| e.to_string())?;
            file.seek(SeekFrom::Start(offset))
                .map_err(|e| e.to_string())?;
            file.take((end - offset).min(max_bytes as u64 + 3))
                .read_to_end(&mut bytes)
                .map_err(|e| e.to_string())?;
        } else {
            let offset = usize::try_from(offset).map_err(|e| e.to_string())?;
            if tail.is_none() && before.is_none() && !self.tail.is_char_boundary(offset) {
                return Err("offset must be a UTF-8 boundary".into());
            }
            bytes.extend_from_slice(
                &self.tail.as_bytes()[offset..(end as usize).min(offset + max_bytes + 3)],
            );
        }
        // Arbitrary caller offsets are rejected; tail aligns forward to a character.
        let mut leading = 0;
        if tail.is_some() || before.is_some() {
            while leading < bytes.len() && bytes[leading] & 0xc0 == 0x80 {
                leading += 1;
            }
        }
        let bytes = &bytes[leading..];
        let mut count = bytes.len().min(max_bytes);
        while std::str::from_utf8(&bytes[..count]).is_err() && count > 0 {
            count -= 1;
        }
        if !bytes.is_empty() && count == 0 {
            return Err("offset must be a UTF-8 boundary".into());
        }
        let mut text = std::str::from_utf8(&bytes[..count])
            .map_err(|e| e.to_string())?
            .to_owned();
        let mut start = offset + leading as u64;
        let mut next = start + count as u64;
        let mut truncated = next < self.bytes;
        if let Some(lines) = tail {
            next = end;
            let skip = text.lines().count().saturating_sub(lines);
            let cut = text
                .split_inclusive('\n')
                .take(skip)
                .map(str::len)
                .sum::<usize>();
            text.drain(..cut);
            start += cut as u64;
            truncated = start > 0;
        }
        Ok(ShellJobOutput {
            text,
            start_offset: start,
            output_bytes: self.bytes,
            next_offset: next,
            truncated,
            log_path: self.log.as_ref().map(|(_, path)| path.clone()),
        })
    }
}
struct Done {
    result: ToolResult,
    receipt: Option<ToolExecutionReceipt>,
    elapsed_ms: u64,
}
struct Completion {
    id: u64,
    elapsed_ms: u64,
    result: ToolResult,
    process: Option<crate::process::ProcessExecutionFacts>,
    receipt: Option<ToolExecutionReceipt>,
    batch_id: String,
    call_id: String,
}
impl Job {
    fn completion(&self, id: u64) -> Option<Completion> {
        let done = self.done.as_ref()?;
        Some(Completion {
            id,
            elapsed_ms: done.elapsed_ms,
            result: done.result.clone(),
            process: done.receipt.as_ref().and_then(|r| r.process.clone()),
            receipt: done.receipt.clone(),
            batch_id: self.batch_id.clone(),
            call_id: self.call_id.clone(),
        })
    }
    fn info(&self, key: u64) -> ShellJobInfo {
        if let Some(lost) = &self.lost {
            return lost.clone();
        }
        let process = self
            .done
            .as_ref()
            .and_then(|d| d.receipt.as_ref())
            .and_then(|r| r.process.as_ref());
        let state = if let Some(done) = &self.done {
            if process.is_some_and(|p| p.cancelled) || self.cancellation.is_cancelled() {
                "cancelled"
            } else if self.interrupt.load(Ordering::Relaxed) {
                "interrupted"
            } else if done.result.success {
                "completed"
            } else {
                "failed"
            }
        } else if self.cancellation.is_cancelled() {
            "cancelling"
        } else if self.interrupt.load(Ordering::Relaxed) {
            "interrupting"
        } else {
            "running"
        };
        ShellJobInfo {
            id: job_name(key),
            command: self.command.clone(),
            origin: self.origin.clone(),
            state: state.into(),
            elapsed_ms: self.done.as_ref().map_or_else(
                || self.started.elapsed().as_millis() as u64,
                |d| d.elapsed_ms,
            ),
            exit_code: process.and_then(|p| p.exit_code),
            output_bytes: self.output.bytes,
        }
    }
}
/// Owner guard cancels on unwinding as well as normal exits.
pub struct ShellJobScope(ShellJobs);
impl Drop for ShellJobScope {
    fn drop(&mut self) {
        self.0.cancel_all();
    }
}
impl ShellJobs {
    pub fn new(limits: ShellJobLimits) -> Result<Self, String> {
        limits.validate()?;
        Ok(Self(Arc::new(JobsState {
            jobs: Mutex::new(BTreeMap::new()),
            serial: Default::default(),
            changed: Notify::new(),
            limits,
            journal: Mutex::new(std::sync::Weak::new()),
            metadata: Mutex::new(BTreeMap::new()),
        })))
    }
    pub(crate) fn attach_journal(
        &self,
        journal: Option<&Arc<Mutex<crate::session::ManualRunJournal>>>,
    ) {
        *crate::tools::lock_mutex(&self.0.journal) =
            journal.map(Arc::downgrade).unwrap_or_default();
    }
    fn record_metadata(&self, info: ShellJobInfo) {
        let journal = crate::tools::lock_mutex(&self.0.journal).upgrade();
        if let Some(journal) = journal {
            if crate::tools::lock_mutex(&journal)
                .record_event(&crate::EventKind::ShellJobChanged { job: info.clone() })
                .is_ok()
            {
                crate::tools::lock_mutex(&self.0.metadata).remove(&info.id);
                return;
            }
        }
        crate::tools::lock_mutex(&self.0.metadata).insert(info.id.clone(), info);
    }
    pub(crate) fn record_snapshot(&self) {
        for (key, job) in self.jobs().iter() {
            self.record_metadata(job.info(*key));
        }
    }
    pub fn take_metadata(&self) -> Vec<ShellJobInfo> {
        std::mem::take(&mut *crate::tools::lock_mutex(&self.0.metadata))
            .into_values()
            .collect()
    }
    pub fn return_metadata(&self, infos: Vec<ShellJobInfo>) {
        let jobs = self.jobs();
        let mut pending = crate::tools::lock_mutex(&self.0.metadata);
        for info in infos {
            let info = job_key(&info.id)
                .and_then(|k| jobs.get(&k).map(|j| j.info(k)))
                .unwrap_or(info);
            pending.entry(info.id.clone()).or_insert(info);
        }
    }
    pub fn clear_finished(&self) {
        self.jobs()
            .retain(|_, j| j.done.is_none() && j.lost.is_none());
    }
    pub async fn reset(&self) {
        self.shutdown().await;
        self.jobs().clear();
    }
    pub fn restore_lost(&self, infos: &[ShellJobInfo]) {
        let mut jobs = self.jobs();
        for key in infos.iter().filter_map(|i| job_key(&i.id)) {
            self.0.serial.fetch_max(key, Ordering::Relaxed);
        }
        let mut infos = infos.to_vec();
        infos.sort_by_key(|i| job_key(&i.id));
        for info in infos.iter().rev().take(self.0.limits.max_retained).rev() {
            let Some(key) = job_key(&info.id) else {
                continue;
            };
            self.0.serial.fetch_max(key, Ordering::Relaxed);
            let mut restored = info.clone();
            if matches!(
                restored.state.as_str(),
                "running" | "interrupting" | "cancelling"
            ) {
                restored.state = "lost".into();
            }
            jobs.insert(
                key,
                Job {
                    cancellation: CancellationToken::new(),
                    interrupt: Arc::new(AtomicBool::new(false)),
                    started: Instant::now(),
                    preview: String::new(),
                    command: restored.command.clone(),
                    origin: restored.origin.clone(),
                    output: JobOutput::new(self.0.limits.memory_bytes, Vec::new(), None),
                    batch_id: String::new(),
                    call_id: String::new(),
                    delivered: true,
                    notified: true,
                    done: None,
                    lost: Some(restored),
                },
            );
        }
    }
    pub(crate) fn last_id(&self) -> u64 {
        self.0.serial.load(Ordering::Relaxed)
    }
    pub fn limits(&self) -> ShellJobLimits {
        self.0.limits.clone()
    }
    fn jobs(&self) -> MutexGuard<'_, BTreeMap<u64, Job>> {
        crate::tools::lock_mutex(&self.0.jobs)
    }
    pub fn scope(&self) -> ShellJobScope {
        ShellJobScope(self.clone())
    }
    pub fn running_count(&self) -> usize {
        self.jobs()
            .values()
            .filter(|j| j.done.is_none() && j.lost.is_none())
            .count()
    }
    pub fn running(&self) -> bool {
        self.running_count() != 0
    }
    pub fn list(&self) -> Vec<ShellJobInfo> {
        self.jobs()
            .iter()
            .map(|(key, job)| job.info(*key))
            .collect()
    }
    pub(super) fn progress_summary(&self) -> String {
        format!(
            "Aguardando {} job(s) · {}",
            self.running_count(),
            self.list()
                .iter()
                .filter(|j| j.state == "running")
                .map(|j| format!("{} · {}s", j.id, j.elapsed_ms / 1000))
                .collect::<Vec<_>>()
                .join(" · ")
        )
    }
    pub fn cancel_all(&self) {
        for job in self.jobs().values() {
            if job.done.is_none() {
                job.cancellation.cancel();
            }
        }
    }
    pub async fn shutdown(&self) {
        self.cancel_all();
        while self.running() {
            self.wait().await;
        }
    }
    pub(super) async fn wait(&self) {
        let _ = tokio::time::timeout(Duration::from_millis(100), self.0.changed.notified()).await;
    }
    pub fn cancel(&self, id: &str) -> Result<(), String> {
        self.control(id, true).map(|_| ())
    }
    pub fn interrupt(&self, id: &str) -> Result<(), String> {
        let jobs = self.jobs();
        let job = job_key(id)
            .and_then(|k| jobs.get(&k))
            .ok_or("Unknown or expired shell job in this session")?;
        if job.lost.is_some() {
            return Err("Job lost after restart; no live process".into());
        }
        if job.done.is_none() {
            job.interrupt.store(true, Ordering::Relaxed);
        }
        Ok(())
    }
    pub fn output(
        &self,
        id: &str,
        since: Option<u64>,
        tail: Option<usize>,
        max_bytes: usize,
    ) -> Result<ShellJobOutput, String> {
        if !(1..=64 * 1024).contains(&max_bytes) || tail.is_some_and(|n| n == 0 || n > 1000) {
            return Err("max_bytes=1..65536; tail=1..1000".into());
        }
        let jobs = self.jobs();
        let job = job_key(id)
            .and_then(|k| jobs.get(&k))
            .ok_or("Unknown or expired shell job in this session")?;
        if job.lost.is_some() {
            return Err("Job lost after restart; live output unavailable".into());
        }
        job.output.read(since, tail, max_bytes)
    }
    pub fn output_before(
        &self,
        id: &str,
        before: u64,
        max_bytes: usize,
    ) -> Result<ShellJobOutput, String> {
        if !(1..=65536).contains(&max_bytes) {
            return Err("max_bytes=1..65536".into());
        }
        let jobs = self.jobs();
        let job = job_key(id)
            .and_then(|k| jobs.get(&k))
            .ok_or("Unknown or expired shell job in this session")?;
        if job.lost.is_some() {
            return Err("Job lost after restart; live output unavailable".into());
        }
        job.output.read_window(None, None, Some(before), max_bytes)
    }
    pub fn take_notifications(&self) -> Vec<ShellJobInfo> {
        let mut jobs = self.jobs();
        jobs.iter_mut()
            .filter_map(|(key, job)| {
                if job.notified || job.done.is_none() {
                    return None;
                }
                job.notified = true;
                Some(job.info(*key))
            })
            .collect()
    }
    pub fn take_completions(&self) -> Vec<(String, String)> {
        let mut jobs = self.jobs();
        jobs.iter_mut()
            .filter_map(|(key, job)| {
                if job.delivered {
                    return None;
                }
                let output = job.done.as_ref()?.result.output.clone();
                job.delivered = true;
                Some((job_name(*key), output))
            })
            .collect()
    }
    pub fn return_completions(&self, notes: &[(String, String)]) {
        let mut jobs = self.jobs();
        for (id, _) in notes {
            if let Some(job) = job_key(id).and_then(|k| jobs.get_mut(&k)) {
                job.delivered = false;
            }
        }
    }
    pub fn mark_delivered(&self, id: &str) {
        if let Some(key) = job_key(id) {
            if let Some(job) = self.jobs().get_mut(&key) {
                job.delivered = true;
            }
        }
    }
    pub fn start_user(
        &self,
        mut tools: ToolRegistry,
        cwd: &Path,
        command: &str,
        secrets: &[String],
        store: ArtifactStore,
    ) -> Result<String, String> {
        tools.configure_artifacts(Some(store), secrets);
        let args = json!({"command": command, "background": true}).to_string();
        let prepared = tools.prepare_invocation(crate::OperatingMode::Auto, cwd, "shell", &args);
        if let Some(error) = &prepared.error {
            return Err(error.clone());
        }
        self.start(
            tools,
            prepared,
            None,
            ToolInvocation {
                batch_id: "user",
                call_id: "user",
                name: "shell",
                arguments: &args,
            },
        )
    }
    pub(super) fn start(
        &self,
        mut tools: ToolRegistry,
        prepared: PreparedToolInvocation,
        parent: Option<CancellationToken>,
        invocation: ToolInvocation<'_>,
    ) -> Result<String, String> {
        let (key, cancellation) = self.register(invocation)?;
        {
            let mut jobs = self.jobs();
            let job = jobs.get_mut(&key).unwrap();
            job.command = match &prepared.arguments {
                PreparedToolArguments::Shell { command, .. } => {
                    crate::redaction::redact_credentials(&redact_values(
                        &tools.sensitive_values,
                        command,
                    ))
                }
                _ => String::new(),
            };
            job.origin = if invocation.batch_id == "user" {
                "user"
            } else {
                "model"
            }
            .into();
            job.output.secrets = tools.sensitive_values.to_vec();
            job.output.store = tools.job_artifacts();
            let state = self.clone();
            let preview_state = self.clone();
            tools.process_observer = Some(crate::process::ProcessObserver {
                redacted_output: Arc::new(move || {
                    let jobs = preview_state.jobs();
                    jobs.get(&key)
                        .map(|j| {
                            // The tail is already redacted; clean terminal noise
                            // before windowing so it does not fill the preview.
                            let cleaned = crate::tools::normalize_shell_text(&j.output.tail);
                            let mut at = cleaned
                                .len()
                                .saturating_sub(preview_state.0.limits.memory_bytes / 8);
                            while !cleaned.is_char_boundary(at) {
                                at += 1;
                            }
                            let text = &cleaned[at..];
                            if at > 0 || j.output.log.is_some() {
                                format!(
                                    "[preview; use shell_job output for full redacted log]\n{text}"
                                )
                            } else {
                                text.to_owned()
                            }
                        })
                        .unwrap_or_default()
                }),
                interrupt: job.interrupt.clone(),
                capture_bytes: self.0.limits.memory_bytes / 8,
                grace: Duration::from_millis(self.0.limits.interrupt_grace_ms),
                output: Arc::new(move |stream, data, eof| {
                    let mut jobs = state.jobs();
                    if let Some(job) = jobs.get_mut(&key) {
                        if job.output.error.is_none() {
                            if let Err(error) = job.output.append(stream, data, eof) {
                                job.output.error =
                                    Some(format!("job log failed: {error}; capture incomplete"));
                                job.output.pending = Default::default();
                                job.output.text_pending = Default::default();
                                job.output.exact_pending = Default::default();
                                job.cancellation.cancel();
                            }
                            job.preview = job
                                .output
                                .tail
                                .lines()
                                .last()
                                .unwrap_or("no output yet")
                                .chars()
                                .take(160)
                                .collect();
                        }
                    }
                }),
            });
        }
        if parent.as_ref().is_some_and(CancellationToken::is_cancelled) {
            cancellation.cancel();
        }
        if let Some(job) = self.jobs().get(&key) {
            self.record_metadata(job.info(key));
        }
        self.spawn_worker(key, tools, prepared, parent, cancellation);
        Ok(job_name(key))
    }
    fn register(&self, invocation: ToolInvocation<'_>) -> Result<(u64, CancellationToken), String> {
        let mut jobs = self.jobs();
        let limits = &self.0.limits;
        if jobs
            .values()
            .filter(|j| j.done.is_none() && j.lost.is_none())
            .count()
            >= limits.max_running
        {
            return Err(format!(
                "At most {} shell jobs may run; finish or cancel an existing job first.",
                limits.max_running
            ));
        }
        if jobs.len() >= limits.max_retained {
            if let Some(key) = jobs
                .iter()
                .find(|(_, j)| j.delivered && (j.done.is_some() || j.lost.is_some()))
                .map(|(k, _)| *k)
            {
                jobs.remove(&key);
            }
        }
        if jobs.len() >= limits.max_retained {
            return Err("Shell job completion queue is full.".into());
        }
        let key = self
            .0
            .serial
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| "Shell job identity exhausted")?
            + 1;
        let cancellation = CancellationToken::new();
        jobs.insert(
            key,
            Job {
                cancellation: cancellation.clone(),
                interrupt: Arc::new(AtomicBool::new(false)),
                started: Instant::now(),
                preview: "no output yet".into(),
                command: String::new(),
                origin: "model".into(),
                output: JobOutput::new(limits.memory_bytes, Vec::new(), None),
                batch_id: if invocation.batch_id == "user" {
                    format!("user-shell-{key}")
                } else {
                    invocation.batch_id.into()
                },
                call_id: if invocation.batch_id == "user" {
                    job_name(key)
                } else {
                    invocation.call_id.into()
                },
                delivered: false,
                notified: false,
                done: None,
                lost: None,
            },
        );
        Ok((key, cancellation))
    }
    fn spawn_worker(
        &self,
        key: u64,
        tools: ToolRegistry,
        prepared: PreparedToolInvocation,
        parent: Option<CancellationToken>,
        cancellation: CancellationToken,
    ) {
        let state = self.clone();
        let work = parent
            .as_ref()
            .map(CancellationToken::track_background_work);
        tokio::spawn(async move {
            let worker_cancel = cancellation.clone();
            let mut task = tokio::task::spawn_blocking(move || {
                let _work = work;
                if worker_cancel.is_cancelled() {
                    return (
                        ToolResult::fail(
                            "shell",
                            "shell job cancelled before it started; command not executed",
                        ),
                        None,
                    );
                }
                let outcome = tools.execute_prepared_with_cancellation_and_progress(
                    &prepared,
                    Some(&worker_cancel),
                    |_| {},
                );
                (outcome.result, Some(outcome.receipt))
            });
            let (mut result, receipt) = tokio::select! {
                result = &mut task => result,
                _ = CancellationToken::cancelled_or_pending(parent) => { cancellation.cancel(); task.await }
            }.unwrap_or_else(|error| (ToolResult::fail("shell", format!("shell worker failed: {error}; side effects unverified")), None));
            if let Some(job) = state.jobs().get_mut(&key) {
                result.output = crate::redaction::redact_credentials(&redact_values(
                    &job.output.secrets,
                    &result.output,
                ));
                if let Some(error) = &job.output.error {
                    result.success = false;
                    result.output.push_str(&format!("\n{error}"));
                }
                if let Some((_, path)) = &job.output.log {
                    result.output.push_str(&format!(
                        "\n[full redacted output: {}; use shell_job output]",
                        path.display()
                    ));
                }
                job.done = Some(Done {
                    result,
                    receipt,
                    elapsed_ms: job.started.elapsed().as_millis() as u64,
                });
                state.record_metadata(job.info(key));
            }
            state.0.changed.notify_one();
        });
    }
    pub(super) fn control(&self, id: &str, cancel: bool) -> Result<ToolResult, String> {
        let jobs = self.jobs();
        let key = job_key(id).ok_or("Unknown or expired shell job in this session")?;
        let job = jobs
            .get(&key)
            .ok_or("Unknown or expired shell job in this session")?;
        if job.lost.is_some() {
            return Err("Job lost after restart; no live process".into());
        }
        if cancel && job.done.is_none() {
            job.cancellation.cancel();
        }
        if let Some(done) = &job.done {
            return Ok(done.result.clone());
        }
        let info = job.info(key);
        Ok(ToolResult::ok(
            "shell_job",
            format!(
                "job_id={id} state={} elapsed_ms={}\n{}\n{CONTROL_HINT}",
                info.state, info.elapsed_ms, job.preview
            ),
        ))
    }
    fn ready(&self) -> Vec<Completion> {
        self.jobs()
            .iter()
            .filter(|(_, j)| !j.delivered)
            .filter_map(|(k, j)| j.completion(*k))
            .collect()
    }
    fn inline_ready(&self, id: &str) -> Option<Completion> {
        let key = job_key(id)?;
        self.jobs().get(&key)?.completion(key)
    }
}
pub(super) fn definition() -> Value {
    json!({"name":"shell_job", "description":"Control owned shell jobs: list, status, output by byte cursor or tail lines, bounded wait, interrupt (graceful then forced tree stop), cancel (tree stop). TUI IDs survive responses in the same session, not restart. Completion arrives automatically; do not poll.",
        "input_schema":{"type":"object","properties":{"job_id":{"type":"string"},"action":{"type":"string","enum":["status","cancel","interrupt","output","wait","list"]},"since":{"type":"integer","minimum":0},"offset":{"type":"integer","minimum":0},"tail":{"type":"integer","minimum":1,"maximum":1000},"max_bytes":{"type":"integer","minimum":1,"maximum":65536},"timeout_ms":{"type":"integer","minimum":0,"maximum":10000}},"required":["action"],"additionalProperties":false}})
}

/// How a managed shell call ended up before its result is published.
enum Managed {
    /// Result available without waiting on a job: refusal, control or start error.
    Immediate(Result<ToolResult, String>),
    /// The job finished inside its yield window; it is delivered inline.
    Finished(Box<Completion>),
    /// The yield window elapsed; the job keeps running and completes later.
    Yielded(Result<ToolResult, String>),
}

impl Runtime {
    pub(super) async fn execute_managed_shell(
        &mut self,
        mode: crate::OperatingMode,
        invocation: ToolInvocation<'_>,
        prepared: &PreparedToolInvocation,
        mut seq: u64,
    ) -> Result<(ToolExecutionOutcome, u64), ProviderError> {
        let started = Instant::now();
        let mut receipt = ToolExecutionReceipt::unobserved(
            self.tools.workspace_revision(),
            self.tools.workspace_revision(),
            0,
        );
        let arguments = self.redact_sensitive(invocation.arguments);
        push_runtime_event(
            &mut self.app,
            &mut seq,
            crate::EventKind::ToolStarted {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                arguments,
            },
        )?;
        let managed = if !mode.allows_mutation() {
            Managed::Immediate(Err("Managed shell jobs require Auto mode".into()))
        } else if let Some(error) = &prepared.error {
            Managed::Immediate(Err(error.clone()))
        } else if invocation.name == "shell_job" {
            Managed::Immediate(self.control_from_args(invocation.arguments).await)
        } else {
            self.run_inline(invocation, prepared, &mut seq).await?
        };
        let mut inline_job_id = None;
        let (result, yielded) = match managed {
            Managed::Immediate(result) => (result, false),
            Managed::Finished(done) => {
                let Completion {
                    id,
                    result,
                    receipt: actual,
                    ..
                } = *done;
                if let Some(actual) = actual {
                    receipt = actual;
                }
                inline_job_id = Some(job_name(id));
                (Ok(result), false)
            }
            Managed::Yielded(result) => {
                receipt.effects_uncertain = true;
                (result, true)
            }
        };
        let mut result = result.unwrap_or_else(|error| ToolResult::fail(invocation.name, error));
        result.name = invocation.name.into();
        result.output = self.redact_sensitive(&result.output);
        self.record_existing_artifact(&result, &mut seq)?;
        push_runtime_event(
            &mut self.app,
            &mut seq,
            crate::EventKind::ToolOutput {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                output: result.output.clone(),
            },
        )?;
        if !yielded {
            push_runtime_event(
                &mut self.app,
                &mut seq,
                crate::EventKind::ToolFinished {
                    batch_id: invocation.batch_id.into(),
                    call_id: invocation.call_id.into(),
                    name: invocation.name.into(),
                    success: result.success,
                    duration_ms: started.elapsed().as_millis() as u64,
                },
            )?;
        }
        if let Some(id) = inline_job_id {
            self.shell_jobs.mark_delivered(&id);
        }
        Ok((ToolExecutionOutcome { result, receipt }, seq))
    }
    async fn control_from_args(&self, arguments: &str) -> Result<ToolResult, String> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Control {
            action: String,
            job_id: Option<String>,
            since: Option<u64>,
            offset: Option<u64>,
            tail: Option<usize>,
            max_bytes: Option<usize>,
            timeout_ms: Option<u64>,
        }
        let args: Control = serde_json::from_str(arguments)
            .map_err(|e| format!("Invalid shell_job arguments: {e}"))?;
        if args.action == "list" {
            return Ok(ToolResult::ok(
                "shell_job",
                serde_json::to_string(&self.shell_jobs.list()).map_err(|e| e.to_string())?,
            ));
        }
        let id = args
            .job_id
            .as_deref()
            .ok_or("job_id is required for this action; use list for owned IDs")?;
        match args.action.as_str() {
            "status" => self.shell_jobs.control(id, false),
            "cancel" => self.shell_jobs.control(id, true),
            "interrupt" => {
                self.shell_jobs.interrupt(id)?;
                self.shell_jobs.control(id, false)
            }
            "output" => {
                if args.since.is_some() && args.offset.is_some() {
                    return Err("Use either since or offset".into());
                }
                let output = self.shell_jobs.output(
                    id,
                    args.since.or(args.offset),
                    args.tail,
                    args.max_bytes.unwrap_or(16 * 1024),
                )?;
                Ok(ToolResult::ok("shell_job", format_output_page(id, &output)))
            }
            "wait" => {
                // A longer wait is a bounded wait asked for too eagerly: serve
                // the maximum and say so, so the call can simply be repeated.
                let requested = args.timeout_ms.unwrap_or(1000);
                let ms = capped_wait_ms(requested);
                self.shell_jobs.control(id, false)?;
                let until = tokio::time::Instant::now() + Duration::from_millis(ms);
                while self.shell_jobs.inline_ready(id).is_none()
                    && tokio::time::Instant::now() < until
                    && !self.is_cancelled()
                {
                    let _ = tokio::time::timeout_at(until, self.shell_jobs.wait()).await;
                }
                let mut result = self.shell_jobs.control(id, false)?;
                if requested > MAX_WAIT_MS {
                    result.output = format!(
                        "[note: wait is capped at {MAX_WAIT_MS} ms; waited {ms} ms, call wait again if the job is still running]\n{}",
                        result.output
                    );
                }
                Ok(result)
            }
            _ => Err("action must be status, cancel, interrupt, output, wait or list".into()),
        }
    }
    /// Starts the job and waits for it up to its `yield_ms`.
    async fn run_inline(
        &mut self,
        invocation: ToolInvocation<'_>,
        prepared: &PreparedToolInvocation,
        seq: &mut u64,
    ) -> Result<Managed, ProviderError> {
        let id = match self.shell_jobs.start(
            self.tools.clone(),
            prepared.clone(),
            self.cancellation.clone(),
            invocation,
        ) {
            Ok(id) => id,
            Err(error) => return Ok(Managed::Immediate(Err(error))),
        };
        if let Some(info) = self.shell_jobs.list().into_iter().find(|j| j.id == id) {
            push_runtime_event(
                &mut self.app,
                seq,
                crate::EventKind::ShellJobChanged { job: info },
            )?;
        }
        let yield_ms = match prepared.arguments {
            PreparedToolArguments::Shell { yield_ms, .. } => yield_ms,
            _ => 0,
        };
        let until = tokio::time::Instant::now() + Duration::from_millis(yield_ms);
        loop {
            if let Some(done) = self.shell_jobs.inline_ready(&id) {
                if let Some(info) = self.shell_jobs.list().into_iter().find(|j| j.id == id) {
                    push_runtime_event(
                        &mut self.app,
                        seq,
                        crate::EventKind::ShellJobChanged { job: info },
                    )?;
                }
                push_tool_result_facts(
                    &mut self.app,
                    seq,
                    &done.batch_id,
                    &done.call_id,
                    "shell",
                    done.process.as_ref(),
                    None,
                )?;
                return Ok(Managed::Finished(Box::new(done)));
            }
            if tokio::time::Instant::now() >= until {
                return Ok(Managed::Yielded(self.shell_jobs.control(&id, false)));
            }
            let _ = tokio::time::timeout_at(until, self.shell_jobs.wait()).await;
        }
    }
    pub(super) fn deliver_shell_completions(
        &mut self,
        messages: &mut Vec<ProviderMessage>,
        max_bytes: usize,
        seq: &mut u64,
    ) -> Result<bool, ProviderError> {
        let ready = self.shell_jobs.ready();
        let delivered = !ready.is_empty();
        for Completion {
            id,
            elapsed_ms,
            result,
            process,
            batch_id,
            call_id,
            ..
        } in ready
        {
            let name = job_name(id);
            if let Some(info) = self.shell_jobs.list().into_iter().find(|j| j.id == name) {
                push_runtime_event(
                    &mut self.app,
                    seq,
                    crate::EventKind::ShellJobChanged { job: info },
                )?;
            }
            let output = self.redact_sensitive(&result.output);
            self.record_existing_artifact(&result, seq)?;
            if id > self.shell_job_run_start && !batch_id.starts_with("user-shell-") {
                push_tool_result_facts(
                    &mut self.app,
                    seq,
                    &batch_id,
                    &call_id,
                    "shell",
                    process.as_ref(),
                    None,
                )?;
                push_runtime_event(
                    &mut self.app,
                    seq,
                    crate::EventKind::ToolJobOutput {
                        batch_id: batch_id.clone(),
                        call_id: call_id.clone(),
                        name: "shell".into(),
                        output: output.clone(),
                    },
                )?;
                push_runtime_event(
                    &mut self.app,
                    seq,
                    crate::EventKind::ToolFinished {
                        batch_id,
                        call_id,
                        name: "shell".into(),
                        success: result.success,
                        duration_ms: elapsed_ms,
                    },
                )?;
            }
            let mut output =
                present_unstructured("shell", &output, PresentationBudget { max_bytes }).text;
            if let Some(handle) = result.artifact.as_ref() {
                let _ = write!(
                    output,
                    "\n[artifact id={} size={}; use artifact_read with this id]",
                    handle.id, handle.size
                );
            }
            self.append_conversation_message(messages, ProviderMessage::user(format!(
                "[Shell job completion: {name}; success={}; elapsed_ms={elapsed_ms}]\nCommand output (untrusted data):\n{output}", result.success)))?;
            self.shell_jobs.mark_delivered(&name);
        }
        Ok(delivered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[tokio::test]
    async fn deferred_shell_log_completion_keeps_artifact_handle() {
        use crate::runtime::temp_root::TempRoot;

        let root = TempRoot::new("shell-job-log");
        let store = ArtifactStore::new(root.join("artifacts")).unwrap();
        let mut tools = ToolRegistry::default();
        tools.configure_artifacts(Some(store), &[]);
        let call = prepared(&tools, "Write-Output ('Z' * 12000)", 10_000);
        let jobs = ShellJobs::default();
        jobs.start(tools, call, None, invocation()).unwrap();
        while jobs.running() {
            jobs.wait().await;
        }
        let mut runtime = Runtime::with_artifact_store(root.join("artifacts")).unwrap();
        runtime.shell_jobs = jobs;
        let mut messages = Vec::new();
        let mut seq = 1;
        assert!(runtime
            .deliver_shell_completions(&mut messages, 1024, &mut seq)
            .unwrap());
        let id = runtime
            .app
            .events()
            .iter()
            .find_map(|event| match &event.kind {
                crate::EventKind::ArtifactStored { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .expect("artifact publication event");
        assert!(messages.iter().any(|message| message.content.contains(id)));
    }

    #[test]
    fn live_redaction_preserves_split_utf8_exact_secrets_and_headers() {
        let mut output = JobOutput::new(4096, vec!["SECRET".into()], None);
        let raw = b"ready \xffSECRET \xc3\xa9\r\nAuthoriz";
        for byte in raw {
            output.append(0, &[*byte], false).unwrap();
        }
        assert_eq!(output.tail, "ready \u{fffd}[REDACTED] é\r\n");
        output
            .append(0, b"ation: Bearer private-value", false)
            .unwrap();
        for _ in 0..8 {
            output.append(0, &[b'z'; 1024], false).unwrap();
        }
        output.append(0, b"\r\nnext", true).unwrap();
        assert!(
            output.tail.contains("Authorization: [REDACTED]\r\nnext"),
            "{}",
            output.tail
        );
        assert!(!output.tail.contains("private-value"));
        assert!(!output.tail.contains("zzzz"));
        assert!(output.pending.iter().all(Vec::is_empty));
    }

    #[test]
    fn output_pages_drop_terminal_noise_but_keep_raw_offsets() {
        let mut output = JobOutput::new(4096, Vec::new(), None);
        let raw = "\u{1b}[32mok\u{1b}[0m\r\n10%\r100%\r\ndone\r\n";
        output.append(0, raw.as_bytes(), true).unwrap();
        // The stored text keeps its `\r\n`; only the page is cleaned.
        assert!(output.tail.contains("\r\n"));
        let page = output.read(Some(0), None, 4096).unwrap();
        let formatted = format_output_page("shell-1", &page);
        assert_eq!(
            formatted,
            format!(
                "job_id=shell-1 start_offset=0 next_offset={} output_bytes={} truncated=false\nok\n100%\ndone\n",
                raw.len(),
                raw.len()
            )
        );
        // The next page starts at the raw offset, noise or not.
        let rest = output.read(Some(page.next_offset), None, 4096).unwrap();
        assert!(rest.text.is_empty());
    }

    #[test]
    fn output_cursor_spills_without_loss_or_overwrite_and_tail_aligns_utf8() {
        use crate::runtime::temp_root::TempRoot;
        let root = TempRoot::new("job-cursor");
        let store = ArtifactStore::new(root.join("logs")).unwrap();
        let mut output = JobOutput::new(4096, Vec::new(), Some(store));
        let first = "first-é\r\n".repeat(1000);
        output.append(0, first.as_bytes(), true).unwrap();
        let one = output.read(Some(0), None, 1024).unwrap();
        assert!(one.truncated);
        output.append(1, b"stderr-next\n", true).unwrap();
        let mut read = one.text;
        let mut offset = one.next_offset;
        while offset < output.bytes {
            let page = output.read(Some(offset), None, 1024).unwrap();
            assert!(page.next_offset > offset);
            offset = page.next_offset;
            read.push_str(&page.text);
        }
        assert_eq!(read, format!("{first}stderr-next\n"));
        assert!(output.tail.len() <= 4096);
        assert!(std::fs::read_to_string(&output.log.as_ref().unwrap().1)
            .unwrap()
            .ends_with("stderr-next\n"));
        let mut small = JobOutput::new(4096, Vec::new(), None);
        small.append(0, "ééé".as_bytes(), true).unwrap();
        assert_eq!(small.read(None, Some(1), 5).unwrap().text, "éé");
        assert!(small.read(Some(1), None, 5).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_interrupt_reports_graceful_and_escalated_and_releases_tree() {
        let resolver = crate::process::ExecutableResolver::default();
        let python = resolver
            .resolve(if cfg!(windows) { "python" } else { "python3" })
            .unwrap()
            .expect("Python fixture runtime");
        for (ignore, cancel_during_grace) in [(false, false), (true, false), (true, true)] {
            let jobs = ShellJobs::new(ShellJobLimits {
                interrupt_grace_ms: 1500,
                ..Default::default()
            })
            .unwrap();
            let _scope = jobs.scope();
            let tools = ToolRegistry::default();
            let script = format!("import signal,time,sys; signal.signal({}, {}); print('READY', flush=True)\nwhile True: time.sleep(0.01)", if cfg!(windows) { "signal.SIGBREAK" } else { "signal.SIGINT" }, if cancel_during_grace { "lambda *_: print('SIGNAL',flush=True)" } else if ignore { "signal.SIG_IGN" } else { "lambda *_: sys.exit(0)" });
            let args = json!({"command":python.to_string_lossy(),"args":["-u","-c",script],"yield_ms":0,"timeout_ms":10000}).to_string();
            let call = tools.prepare_invocation(
                crate::OperatingMode::Auto,
                std::env::temp_dir(),
                "shell",
                &args,
            );
            let id = jobs.start(tools, call, None, invocation()).unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                while !jobs
                    .output(&id, Some(0), None, 1024)
                    .unwrap()
                    .text
                    .contains("READY")
                {
                    jobs.wait().await;
                }
            })
            .await
            .unwrap();
            jobs.interrupt(&id).unwrap();
            if cancel_during_grace {
                tokio::time::timeout(Duration::from_secs(1), async {
                    while !jobs
                        .output(&id, Some(0), None, 1024)
                        .unwrap()
                        .text
                        .contains("SIGNAL")
                    {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                jobs.cancel(&id).unwrap();
            }
            tokio::time::timeout(Duration::from_secs(5), async {
                while jobs.running() {
                    jobs.wait().await;
                }
            })
            .await
            .unwrap();
            let result = jobs.control(&id, false).unwrap();
            assert!(
                result.output.contains(if ignore {
                    "interrupt=forced"
                } else {
                    "interrupt=graceful"
                }),
                "{}",
                result.output
            );
            assert_eq!(
                jobs.list()[0].state,
                if cancel_during_grace {
                    "cancelled"
                } else {
                    "interrupted"
                }
            );
        }
    }

    #[test]
    fn restore_active_metadata_is_lost_and_never_reuses_the_id() {
        let jobs = ShellJobs::default();
        jobs.restore_lost(&[ShellJobInfo {
            id: "shell-9".into(),
            command: "command".into(),
            origin: "model".into(),
            state: "running".into(),
            elapsed_ms: 12,
            exit_code: None,
            output_bytes: 0,
        }]);
        assert_eq!(jobs.list()[0].state, "lost");
        assert!(!jobs.running());
        assert!(jobs.control("shell-9", false).unwrap_err().contains("lost"));
        assert!(jobs.interrupt("shell-9").is_err());
        let (key, _) = jobs.register(invocation()).unwrap();
        assert_eq!(key, 10);
    }

    #[test]
    fn restore_considers_evicted_ids_and_rejects_exhausted_serial() {
        let jobs = ShellJobs::default();
        let mut infos = Vec::new();
        for i in 1..=100 {
            infos.push(ShellJobInfo {
                id: job_name(i),
                command: String::new(),
                origin: "model".into(),
                state: "running".into(),
                elapsed_ms: 0,
                exit_code: None,
                output_bytes: 0,
            });
        }
        infos.sort_by(|a, b| a.id.cmp(&b.id));
        jobs.restore_lost(&infos);
        assert_eq!(jobs.register(invocation()).unwrap().0, 101);
        let jobs = ShellJobs::default();
        jobs.restore_lost(&[ShellJobInfo {
            id: job_name(u64::MAX),
            command: String::new(),
            origin: "model".into(),
            state: "running".into(),
            elapsed_ms: 0,
            exit_code: None,
            output_bytes: 0,
        }]);
        assert!(jobs
            .register(invocation())
            .unwrap_err()
            .contains("exhausted"));
    }

    #[test]
    fn user_jobs_have_unique_durable_identity_and_reserved_notes_are_once_only() {
        let jobs = ShellJobs::default();
        let user = ToolInvocation {
            batch_id: "user",
            call_id: "user",
            ..invocation()
        };
        let first = jobs.register(user).unwrap().0;
        let second = jobs.register(user).unwrap().0;
        {
            let mut entries = jobs.jobs();
            assert_ne!(entries[&first].call_id, entries[&second].call_id);
            assert_ne!(entries[&first].batch_id, entries[&second].batch_id);
            for entry in entries.values_mut() {
                entry.done = Some(Done {
                    result: ToolResult::ok("shell", "done"),
                    receipt: None,
                    elapsed_ms: 1,
                });
            }
        }
        let notes = jobs.take_completions();
        assert_eq!(notes.len(), 2);
        assert!(
            jobs.ready().is_empty(),
            "Runtime cannot redeliver reserved notes"
        );
        jobs.return_completions(&notes);
        assert_eq!(jobs.take_completions(), notes);
        assert!(jobs.take_completions().is_empty());
    }
    #[test]
    fn backward_pages_align_utf8_and_return_the_actual_start() {
        use crate::runtime::temp_root::TempRoot;
        let root = TempRoot::new("job-pages");
        let mut output = JobOutput::new(
            4096,
            vec![],
            Some(ArtifactStore::new(root.join("logs")).unwrap()),
        );
        let raw = "é中\r\n".repeat(2000);
        output.append(0, raw.as_bytes(), true).unwrap();
        let last = output.read(None, Some(1000), 4097).unwrap();
        assert_eq!(last.next_offset, output.bytes);
        assert!(last.start_offset > 0);
        let previous = output
            .read_window(None, None, Some(last.start_offset), 4097)
            .unwrap();
        assert_eq!(previous.next_offset, last.start_offset);
        assert!(raw.is_char_boundary(previous.start_offset as usize));
        assert_eq!(
            previous.text,
            raw[previous.start_offset as usize..last.start_offset as usize]
        );
    }

    #[test]
    fn ordinary_long_quoted_output_spills_without_redaction_cancellation() {
        let root = crate::runtime::temp_root::TempRoot::new("job-quoted-output");
        let store = ArtifactStore::new(root.join("logs")).unwrap();
        for raw in [
            format!("\"{}\"\n", "x".repeat(200000)),
            format!("{{\"message\":\"{}\"}}\n", "x".repeat(200000)),
        ] {
            let mut output = JobOutput::new(256 * 1024, vec![], Some(store.clone()));
            for chunk in raw.as_bytes().chunks(16384) {
                output.append(0, chunk, false).unwrap();
            }
            output.append(0, b"", true).unwrap();
            assert!(output.log.is_some());
            let mut full = String::new();
            let mut offset = 0;
            while offset < output.bytes {
                let page = output.read(Some(offset), None, 65536).unwrap();
                assert!(page.next_offset > offset);
                offset = page.next_offset;
                full.push_str(&page.text);
            }
            assert_eq!(full, raw);
        }
    }

    #[test]
    fn streaming_redaction_never_releases_credential_fields_or_retains_large_chunk_capacity() {
        for chunks in [
            ["[REDACTED] Authorization: Bearer alpha", "BETA\n"],
            ["{\"authorization\":\"alpha", "BETA\"}\n"],
            ["{\"authorization\":\n  \"alpha", "BETA\"\n}\n"],
            ["{\"set-cookie\": [\n \"alpha", "BETA\"]}\n"],
            ["{\n \"authorization\"\n", " : \"alphaBETA\"\n}\n"],
            [r#"{"auth\u006frization""#, "\n : \"alphaBETA\"}\n"],
        ] {
            let mut output = JobOutput::new(4096, vec![], None);
            for chunk in chunks {
                for byte in chunk.bytes() {
                    output.append(0, &[byte], false).unwrap();
                }
            }
            output.append(0, b"", true).unwrap();
            assert!(
                !output.tail.contains("alpha") && !output.tail.contains("BETA"),
                "{}",
                output.tail
            );
            assert!(output.tail.contains("[REDACTED]"));
        }
        let root = crate::runtime::temp_root::TempRoot::new("job-capacity");
        let mut output = JobOutput::new(
            4096,
            vec![],
            Some(ArtifactStore::new(root.join("logs")).unwrap()),
        );
        output.append(0, &vec![b'x'; 16000], false).unwrap();
        assert!(output.pending[0].capacity() <= 4);
        assert!(output.text_pending[0].capacity() <= 512);
        assert!(output.exact_pending[0].capacity() <= 512);
        assert!(output.tail.capacity() <= 2048);
        let before = output.read_window(None, None, Some(100), 65536).unwrap();
        assert_eq!(before.next_offset, 100);
        let mut small = JobOutput::new(65536, vec![], None);
        small.append(0, &vec![b'a'; 4000], true).unwrap();
        assert_eq!(
            small
                .read_window(None, None, Some(2000), 65536)
                .unwrap()
                .text
                .len(),
            2000
        );
    }
    #[tokio::test]
    async fn completion_and_artifact_use_redaction_before_any_capture_cut() {
        let root = crate::runtime::temp_root::TempRoot::new("job-cut-secret");
        let store = ArtifactStore::new(root.join("logs")).unwrap();
        let jobs = ShellJobs::new(ShellJobLimits {
            memory_bytes: 4096,
            ..Default::default()
        })
        .unwrap();
        let _scope = jobs.scope();
        let secret = "SecretValue-abcdefghijklmnopqrstuvwxyz-0123456789";
        let command = if cfg!(windows) {
            format!("Write-Output (('L' * 240) + '{secret}' + ('Z' * 3000))")
        } else {
            format!(
                "printf '%s\n' '{}{secret}{}'",
                "L".repeat(240),
                "Z".repeat(3000)
            )
        };
        let id = jobs
            .start_user(
                Default::default(),
                &root,
                &command,
                &[secret.into()],
                store.clone(),
            )
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while jobs.running() {
                jobs.wait().await;
            }
        })
        .await
        .unwrap();
        let facts = jobs.inline_ready(&id).unwrap().process.unwrap();
        assert!(facts.stdout_discarded_bytes > 0);
        assert!(facts.stdout_bytes - facts.stdout_discarded_bytes <= 512);
        let result = jobs.control(&id, false).unwrap();
        assert!(!result.output.contains(&secret[..16]), "{}", result.output);
        let full = jobs.output(&id, Some(0), None, 65536).unwrap();
        assert!(full.text.contains("[REDACTED]"));
        assert!(!full.text.contains(secret));
        if let Some(handle) = result.artifact {
            let artifact = String::from_utf8(store.read(&handle).unwrap()).unwrap();
            assert!(!artifact.contains(&secret[..16]));
        }
    }
    #[tokio::test]
    async fn preview_drops_terminal_noise_before_windowing_and_the_log_stays_raw() {
        let root = crate::runtime::temp_root::TempRoot::new("job-preview-noise");
        let store = ArtifactStore::new(root.join("logs")).unwrap();
        // memory_bytes / 8 = 512: the raw noise (~1.4 KiB) would push the
        // first line out of the preview window.
        let jobs = ShellJobs::new(ShellJobLimits {
            memory_bytes: 4096,
            ..Default::default()
        })
        .unwrap();
        let _scope = jobs.scope();
        let command = if cfg!(windows) {
            "Write-Output 'ERROR: boom'; 1..60 | ForEach-Object { Write-Output 'noise noise noise noise' }"
        } else {
            "echo 'ERROR: boom'; for i in $(seq 60); do echo 'noise noise noise noise'; done"
        };
        let id = jobs
            .start_user(Default::default(), &root, command, &[], store)
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while jobs.running() {
                jobs.wait().await;
            }
        })
        .await
        .unwrap();
        jobs.inline_ready(&id).unwrap();
        let result = jobs.control(&id, false).unwrap();
        assert!(result.output.contains("ERROR: boom"), "{}", result.output);
        assert!(
            result
                .output
                .contains("[previous line repeated 59 more times]"),
            "{}",
            result.output
        );
        let full = jobs.output(&id, Some(0), None, 65536).unwrap();
        assert_eq!(full.text.matches("noise noise noise noise").count(), 60);
        assert!(!full.text.contains("repeated"));
    }
    #[test]
    fn journal_snapshots_replace_pending_metadata_and_prior_run_tools_do_not_alias() {
        use crate::session::{DurableSessionHeader, JsonlRepo, ManualRunJournal, ManualRunSpec};
        let root = crate::runtime::temp_root::TempRoot::new("job-metadata-order");
        let repo = JsonlRepo::create(
            root.join("session.jsonl"),
            DurableSessionHeader::new("jobs", "now", root.to_str().unwrap(), None, None),
        )
        .unwrap();
        let journal = Arc::new(Mutex::new(
            ManualRunJournal::start(
                repo,
                ManualRunSpec::new("op", "attempt", "input", "final", "prompt", 0),
            )
            .unwrap(),
        ));
        let jobs = ShellJobs::default();
        let key = jobs.register(invocation()).unwrap().0;
        jobs.record_metadata(jobs.list()[0].clone());
        jobs.attach_journal(Some(&journal));
        jobs.record_snapshot();
        assert!(jobs.take_metadata().is_empty());
        {
            let mut entries = jobs.jobs();
            let job = entries.get_mut(&key).unwrap();
            job.done = Some(Done {
                result: ToolResult::ok("shell", "done"),
                receipt: None,
                elapsed_ms: 1,
            });
            jobs.record_metadata(job.info(key));
        }
        let mut stale = jobs.list()[0].clone();
        stale.state = "running".into();
        jobs.return_metadata(vec![stale]);
        assert_eq!(jobs.take_metadata()[0].state, "completed");
        let mut runtime = Runtime::new();
        runtime.shell_job_run_start = key;
        runtime.shell_jobs = jobs;
        let mut messages = vec![];
        runtime
            .deliver_shell_completions(&mut messages, 1024, &mut 1)
            .unwrap();
        assert_eq!(messages.len(), 1);
        assert!(!runtime.app.events().iter().any(|e| matches!(
            e.kind,
            crate::EventKind::ToolJobOutput { .. } | crate::EventKind::ToolFinished { .. }
        )));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_stops_child_and_grandchild_before_session_reset_finishes() {
        let root = crate::runtime::temp_root::TempRoot::new("job-grandchild");
        let python = crate::process::ExecutableResolver::default()
            .resolve(if cfg!(windows) { "python" } else { "python3" })
            .unwrap()
            .unwrap();
        let leaf="import os,time; open('leaf.pid','w').write(str(os.getpid()))\nwhile True: time.sleep(0.01)";
        let child=format!("import subprocess,sys,time; subprocess.Popen([sys.executable,'-u','-c',{}])\nwhile True: time.sleep(0.01)",serde_json::to_string(leaf).unwrap());
        let parent=format!("import subprocess,sys,time; subprocess.Popen([sys.executable,'-u','-c',{}])\nwhile True: time.sleep(0.01)",serde_json::to_string(&child).unwrap());
        let jobs = ShellJobs::default();
        let _scope = jobs.scope();
        let tools = ToolRegistry::default();
        let args=json!({"command":python.to_string_lossy(),"args":["-u","-c",parent],"background":true,"timeout_ms":30000}).to_string();
        let call = tools.prepare_invocation(crate::OperatingMode::Auto, &root, "shell", &args);
        jobs.start(tools, call, None, invocation()).unwrap();
        let pid = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(root.join("leaf.pid")) {
                    if let Ok(pid) = text.parse::<u32>() {
                        break pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        #[cfg(windows)]
        let handle = unsafe {
            windows_sys::Win32::System::Threading::OpenProcess(0x00100000, false.into(), pid)
        };
        jobs.reset().await;
        assert!(jobs.list().is_empty());
        #[cfg(windows)]
        {
            assert!(!handle.is_null()); // SAFETY: valid SYNCHRONIZE handle retained through termination.
            unsafe {
                assert_eq!(
                    windows_sys::Win32::System::Threading::WaitForSingleObject(handle, 5000),
                    0
                );
                windows_sys::Win32::Foundation::CloseHandle(handle);
            }
        }
        #[cfg(unix)]
        tokio::time::timeout(Duration::from_secs(5), async {
            while unsafe { libc::kill(pid as i32, 0) } == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn inline_completion_records_terminal_metadata() {
        let mut runtime = Runtime::new();
        let args=json!({"command":if cfg!(windows){"Write-Output done"}else{"printf done"},"yield_ms":10000}).to_string();
        let prepared = runtime.tools.prepare_invocation(
            crate::OperatingMode::Auto,
            std::env::temp_dir(),
            "shell",
            &args,
        );
        runtime
            .execute_managed_shell(
                crate::OperatingMode::Auto,
                ToolInvocation {
                    arguments: &args,
                    ..invocation()
                },
                &prepared,
                1,
            )
            .await
            .unwrap();
        let states: Vec<_> = runtime
            .app
            .events()
            .iter()
            .filter_map(|e| match &e.kind {
                crate::EventKind::ShellJobChanged { job } => Some(job.state.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(states.last(), Some(&"completed"));
    }
    #[test]
    fn log_read_remains_on_the_owned_handle_when_the_path_is_replaced() {
        let root = crate::runtime::temp_root::TempRoot::new("job-log-identity");
        let store = ArtifactStore::new(root.join("logs")).unwrap();
        let mut output = JobOutput::new(4096, vec![], Some(store));
        output.append(0, &vec![b'x'; 5000], true).unwrap();
        let path = output.log.as_ref().unwrap().1.clone();
        let moved = path.with_extension("moved");
        std::fs::rename(&path, &moved).unwrap();
        std::fs::write(&path, "foreign-secret").unwrap();
        assert_eq!(output.read(Some(0), None, 20).unwrap().text, "x".repeat(20));
    }

    fn prepared(tools: &ToolRegistry, command: &str, timeout_ms: u64) -> PreparedToolInvocation {
        tools.prepare_invocation(
            crate::OperatingMode::Auto,
            std::env::temp_dir(),
            "shell",
            &json!({"command":command,"timeout_ms":timeout_ms,"yield_ms":0}).to_string(),
        )
    }
    fn invocation() -> ToolInvocation<'static> {
        ToolInvocation {
            batch_id: "batch",
            call_id: "launch",
            name: "shell",
            arguments: "{}",
        }
    }
    #[tokio::test]
    async fn status_cancel_and_completion_are_owned_and_delivered_once() {
        let jobs = ShellJobs::default();
        let tools = ToolRegistry::default();
        let call = prepared(
            &tools,
            "Write-Output 'started'; Start-Sleep -Seconds 8; Write-Output 'should-not-finish'",
            10_000,
        );
        let id = jobs.start(tools, call, None, invocation()).unwrap();
        assert!(jobs
            .control(&id, false)
            .unwrap()
            .output
            .contains("state=running"));
        tokio::time::timeout(Duration::from_secs(5), async {
            while !jobs.control(&id, false).unwrap().output.contains("started") {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        assert!(jobs.control("foreign-job", true).is_err());
        assert!(jobs
            .control(&id, true)
            .unwrap()
            .output
            .contains("state=cancelling"));
        tokio::time::timeout(Duration::from_secs(5), jobs.shutdown())
            .await
            .unwrap();
        let ready = jobs.ready();
        assert_eq!(ready.len(), 1);
        assert!(!ready[0].result.success);
        assert!(ready[0].process.as_ref().unwrap().cancelled);
        assert!(!ready[0].result.output.contains("should-not-finish"));
        assert!(jobs.inline_ready(&id).is_some());
        assert_eq!(jobs.ready().len(), 1);
        jobs.mark_delivered(&id);
        assert!(jobs.ready().is_empty());
        assert!(!jobs.running());
    }
    #[test]
    fn waits_longer_than_the_maximum_are_served_at_the_maximum() {
        assert_eq!(capped_wait_ms(0), 0);
        assert_eq!(capped_wait_ms(MAX_WAIT_MS), MAX_WAIT_MS);
        assert_eq!(capped_wait_ms(MAX_WAIT_MS + 1), MAX_WAIT_MS);
        assert_eq!(capped_wait_ms(u64::MAX), MAX_WAIT_MS);
    }
    #[tokio::test]
    async fn parent_cancellation_and_scope_drop_stop_jobs() {
        for parent_cancel in [true, false] {
            let jobs = ShellJobs::default();
            let scope = jobs.scope();
            let parent = CancellationToken::new();
            let tools = ToolRegistry::default();
            let call = prepared(&tools, "Start-Sleep -Seconds 8", 10_000);
            jobs.start(tools, call, Some(parent.clone()), invocation())
                .unwrap();
            if parent_cancel {
                parent.cancel();
            } else {
                drop(scope);
            }
            tokio::time::timeout(Duration::from_secs(5), async {
                while jobs.running() {
                    jobs.wait().await;
                }
                parent.wait_for_native_work().await;
            })
            .await
            .unwrap();
            assert!(!jobs.ready()[0].result.success);
        }
    }
    #[tokio::test]
    async fn job_limit_and_deadline_remain_enforced() {
        let jobs = ShellJobs::default();
        let _scope = jobs.scope();
        let tools = ToolRegistry::default();
        let call = prepared(&tools, "Start-Sleep -Seconds 8", 10_000);
        for _ in 0..MAX_RUNNING {
            jobs.start(tools.clone(), call.clone(), None, invocation())
                .unwrap();
        }
        assert!(jobs.start(tools.clone(), call, None, invocation()).is_err());
        jobs.shutdown().await;
        let jobs = ShellJobs::default();
        let call = prepared(&tools, "Start-Sleep -Seconds 8", 100);
        jobs.start(tools, call, None, invocation()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while jobs.running() {
                jobs.wait().await;
            }
        })
        .await
        .unwrap();
        let result = jobs.ready();
        assert!(result[0].process.as_ref().unwrap().timed_out);
    }

    fn prepared_in(
        tools: &ToolRegistry,
        cwd: &std::path::Path,
        command: &str,
        timeout_ms: u64,
    ) -> PreparedToolInvocation {
        tools.prepare_invocation(
            crate::OperatingMode::Auto,
            cwd,
            "shell",
            &json!({"command":command,"timeout_ms":timeout_ms,"yield_ms":0}).to_string(),
        )
    }
    fn finished_job() -> Job {
        Job {
            cancellation: CancellationToken::new(),
            started: Instant::now(),
            preview: String::new(),
            interrupt: Arc::new(AtomicBool::new(false)),
            command: String::new(),
            origin: "model".into(),
            output: JobOutput::new(256 * 1024, Vec::new(), None),
            notified: false,
            lost: None,
            batch_id: "batch".into(),
            call_id: "call".into(),
            delivered: false,
            done: Some(Done {
                result: ToolResult::ok("shell", "done"),
                receipt: None,
                elapsed_ms: 0,
            }),
        }
    }
    #[tokio::test]
    async fn completion_reports_run_time_not_delivery_delay() {
        let jobs = ShellJobs::default();
        let tools = ToolRegistry::default();
        let call = prepared(&tools, "Write-Output done", 10_000);
        let launched = Instant::now();
        jobs.start(tools, call, None, invocation()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while jobs.running() {
                jobs.wait().await;
            }
        })
        .await
        .unwrap();
        let observed_ms = launched.elapsed().as_millis() as u64;
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let ready = jobs.ready();
        assert_eq!(ready.len(), 1);
        assert!(
            ready[0].elapsed_ms <= observed_ms,
            "elapsed_ms={} must not include the {}ms delivery delay (run finished within {observed_ms}ms)",
            ready[0].elapsed_ms,
            1200
        );
    }
    #[test]
    fn ready_delivers_in_numeric_order() {
        let jobs = ShellJobs::default();
        {
            let mut map = jobs.0.jobs.lock().unwrap();
            for n in 1..=11u64 {
                map.insert(n, finished_job());
            }
        }
        let ids: Vec<String> = jobs.ready().into_iter().map(|c| job_name(c.id)).collect();
        let expected: Vec<String> = (1..=11).map(|n| format!("shell-{n}")).collect();
        assert_eq!(ids, expected);
    }
    #[tokio::test]
    async fn already_cancelled_parent_never_starts_the_command() {
        use crate::runtime::temp_root::TempRoot;

        let root = TempRoot::new("shell-job-precancel");
        let tools = ToolRegistry::default();
        let call = prepared_in(
            &tools,
            &root,
            "Set-Content -Path sentinel.txt -Value x",
            10_000,
        );
        let jobs = ShellJobs::default();
        let parent = CancellationToken::new();
        parent.cancel();
        jobs.start(tools, call, Some(parent.clone()), invocation())
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while jobs.running() {
                jobs.wait().await;
            }
            parent.wait_for_native_work().await;
        })
        .await
        .unwrap();
        let ready = jobs.ready();
        assert_eq!(ready.len(), 1);
        assert!(!ready[0].result.success);
        assert!(
            ready[0].process.is_none(),
            "the command must not be spawned"
        );
        assert!(!root.join("sentinel.txt").exists());
    }

    /// Table with `count` finished jobs, oldest first, keyed 1..=count.
    fn jobs_with_finished(count: u64) -> ShellJobs {
        let jobs = ShellJobs::default();
        let base = Instant::now();
        {
            let mut map = jobs.jobs();
            for n in 1..=count {
                let mut job = finished_job();
                job.started = base + Duration::from_millis(n);
                map.insert(n, job);
            }
        }
        jobs.0.serial.store(count, Ordering::Relaxed);
        jobs
    }
    #[test]
    fn retained_limit_evicts_oldest_delivered_job_then_reports_full_queue() {
        let jobs = jobs_with_finished(MAX_RETAINED as u64);
        // Nothing delivered yet: no job may be evicted, the queue is full.
        assert_eq!(
            jobs.register(invocation()).err().as_deref(),
            Some("Shell job completion queue is full.")
        );
        assert_eq!(jobs.jobs().len(), MAX_RETAINED);
        // Delivered jobs are evictable, oldest first; undelivered ones stay.
        jobs.mark_delivered("shell-9");
        jobs.mark_delivered("shell-5");
        let (key, _) = jobs.register(invocation()).unwrap();
        assert_eq!(key, MAX_RETAINED as u64 + 1);
        let table = jobs.jobs();
        assert_eq!(table.len(), MAX_RETAINED);
        assert!(!table.contains_key(&5), "oldest delivered job is evicted");
        assert!(table.contains_key(&9) && table.contains_key(&key));
        drop(table);
        // The next registration evicts the remaining delivered job; after
        // that nothing is evictable and the queue is full again.
        jobs.register(invocation()).unwrap();
        assert!(!jobs.jobs().contains_key(&9));
        assert_eq!(jobs.jobs().len(), MAX_RETAINED);
        assert_eq!(
            jobs.register(invocation()).err().as_deref(),
            Some("Shell job completion queue is full.")
        );
    }
    #[test]
    fn running_limit_message_keeps_its_text() {
        let jobs = ShellJobs::default();
        {
            let mut map = jobs.jobs();
            for n in 1..=MAX_RUNNING as u64 {
                let mut job = finished_job();
                job.done = None;
                map.insert(n, job);
            }
        }
        assert_eq!(
            jobs.register(invocation()).err().as_deref(),
            Some("At most 4 shell jobs may run; finish or cancel an existing job first.")
        );
    }
    #[test]
    fn control_on_a_finished_job_returns_its_result_for_status_and_cancel() {
        let jobs = jobs_with_finished(7);
        for cancel in [false, true] {
            let result = jobs.control("shell-7", cancel).unwrap();
            assert!(result.success);
            assert_eq!(result.output, "done");
        }
        assert!(!jobs.jobs()[&7].cancellation.is_cancelled());
        // Only the exact `shell-N` spelling names a job.
        for id in ["7", "shell-07", "shell-+7", "shell-8", "shell-", "shell-7 "] {
            assert!(jobs.control(id, false).is_err(), "{id} must be unknown");
        }
    }
}
