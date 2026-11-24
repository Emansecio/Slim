//! Guards that hold a wasteful call back, as the claude-pro-kit reread-guard,
//! write-guard and loop-guard mods do for Claude Code:
//!
//! - a shell read (`cat`, `nl -ba`, `sed -n`, `head`, `tail`, `Get-Content`)
//!   of lines an unchanged file already gave this run, through the shell or
//!   the native `read`;
//! - a read, native or shell, that would return more than
//!   [`MAX_READ_LINES`] lines;
//! - a full `write` over an existing file worth a `patch` goes through with a
//!   note: holding it would make the model send the whole file again;
//! - a shell command run again right after it failed, or after it failed
//!   twice in a row.
//!
//! The model reads a hold as the call's error. Retrying the held call goes
//! through, for an output that left the context or a deliberate rerun. Only
//! the line limit stays: a narrower read answers it.
//! A native `read` is not held as a re-read: an identical output still in
//! context is already replaced by a pointer.

use crate::tools::{
    read_page_lines, PreparedToolArguments, PreparedToolInvocation, ToolEffectClass,
};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Files under this size cost little to rewrite; they are left alone.
const MIN_REWRITE_BYTES: u64 = 2048;
const FAILS_BEFORE_HOLD: u32 = 2;
const MAX_TRACKED: usize = 500;
/// Lines one read may return; past it the model searches or pages.
const MAX_READ_LINES: u64 = 1000;
/// Bytes scanned to count a file's lines.
const MAX_SCAN_BYTES: u64 = 10 * 1024 * 1024;
/// Reads remembered per file.
const MAX_SPANS: usize = 64;
/// The last line of a file, whatever its number.
const END: u64 = u64::MAX;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Start {
    Line(u64),
    /// This many lines before the end, in a file too long to count.
    Tail(u64),
}

/// The lines a read returns: from `start` through line `to`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Span {
    start: Start,
    to: u64,
    /// Printed with line numbers, which the plain lines do not stand for.
    numbered: bool,
}

/// What a run read of a file while it kept this stamp.
struct Seen {
    stamp: Stamp,
    spans: Vec<Span>,
}

impl Seen {
    /// A read from the same line that went at least as far gave these lines.
    /// A slice from another line is not covered: it tells where it falls.
    fn covers(&self, span: Span) -> bool {
        self.spans.iter().any(|seen| {
            seen.numbered == span.numbered && seen.start == span.start && span.to <= seen.to
        })
    }
}

/// A read admitted to run, remembered once it succeeds.
struct Claim {
    path: PathBuf,
    stamp: Stamp,
    span: Span,
}

/// What a call's outcome must settle once it ran.
enum Pending {
    Shell {
        command: String,
        /// `effects` when it was admitted.
        at: u64,
        read: Option<Claim>,
    },
    Read(Claim),
    /// The run's first full rewrite: its result carries a note.
    Rewrite {
        name: String,
    },
}

#[derive(Default)]
pub(super) struct RunGuards {
    /// What this run read of each file.
    reads: HashMap<PathBuf, Seen>,
    /// Consecutive failures of each shell command.
    shell_failures: HashMap<String, u32>,
    /// The shell command that failed last, with `effects` when it ran.
    last_failed: Option<(String, u64)>,
    /// Calls admitted that may change something: every one but a read.
    effects: u64,
    /// `effects` when the batch being admitted began.
    batch_start: u64,
    /// Calls held once; the next identical call goes through.
    held: HashSet<String>,
    /// Ids of the calls a guard held, until their results are recorded.
    holding: HashSet<String>,
    pending: HashMap<String, Pending>,
    consecutive_patch_failures: u32,
    consecutive_single_read_turns: u32,
    patch_health_advised: bool,
    read_batch_health_advised: bool,
}

impl RunGuards {
    /// Starts a batch: a read admitted after a call of the same batch that
    /// may change its file is not held as a re-read.
    pub(super) fn begin_batch(&mut self) {
        self.batch_start = self.effects;
    }

    /// Holds `prepared` back when a guard applies; otherwise remembers what
    /// its outcome must settle.
    pub(super) fn admit(&mut self, call_id: &str, prepared: &mut PreparedToolInvocation) {
        self.admit_inner(call_id, prepared, true);
    }

    /// Like [`admit`](Self::admit) for internal `codemode` calls: reads are not
    /// shown directly to the model (`shown == false`), so bounded reads skip
    /// synchronous disk line-counting on the async host thread.
    pub(super) fn admit_codemode(&mut self, call_id: &str, prepared: &mut PreparedToolInvocation) {
        self.admit_inner(call_id, prepared, false);
    }

    fn admit_inner(
        &mut self,
        call_id: &str,
        prepared: &mut PreparedToolInvocation,
        track_reads: bool,
    ) {
        if prepared.error.is_some() {
            return;
        }
        let hold = match &prepared.arguments {
            PreparedToolArguments::Shell { command, args, .. } => self.admit_shell(
                call_id,
                command,
                args.as_deref(),
                &prepared.canonical_workspace,
            ),
            PreparedToolArguments::Write { .. } => {
                let hold = prepared
                    .target_paths
                    .first()
                    .and_then(|path| self.admit_write(call_id, path));
                if hold.is_none() {
                    self.effects += 1;
                }
                hold
            }
            PreparedToolArguments::Read {
                offset,
                max_lines,
                outline,
                ranges,
                symbols,
                ..
            } => {
                if *outline || !ranges.is_empty() || !symbols.is_empty() {
                    None
                } else {
                    prepared
                        .target_paths
                        .first()
                        .and_then(|path| self.admit_read(call_id, path, *offset, *max_lines, track_reads))
                }
            }
            _ => {
                if prepared
                    .spec
                    .is_none_or(|spec| spec.effect_class != ToolEffectClass::SnapshotRead)
                {
                    self.effects += 1;
                }
                None
            }
        };
        if let Some(message) = hold {
            self.holding.insert(call_id.to_owned());
            prepared.reject(&message);
        }
    }

    /// Whether a guard held `call_id`. A hold is the guard's own answer, not
    /// a failure of the call: asking again must not end the run as a failed
    /// call repeated does.
    pub(super) fn was_held(&mut self, call_id: &str) -> bool {
        self.holding.remove(call_id)
    }

    /// Observes tool-batch ergonomics and emits a one-shot advisory when a
    /// run shows repeated patch failures (>=3) or repeated single-read turns (>=4).
    pub(super) fn observe_batch_health(
        &mut self,
        calls: &[crate::provider::ProviderToolCall],
        results: &[crate::tools::ToolResult],
    ) -> Option<String> {
        for (call, result) in calls.iter().zip(results.iter()) {
            if call.name == "patch" {
                if result.success {
                    self.consecutive_patch_failures = 0;
                } else {
                    self.consecutive_patch_failures =
                        self.consecutive_patch_failures.saturating_add(1);
                }
            }
        }
        if calls.len() == 1 && calls[0].name == "read" {
            self.consecutive_single_read_turns =
                self.consecutive_single_read_turns.saturating_add(1);
        } else {
            self.consecutive_single_read_turns = 0;
        }

        if !self.patch_health_advised && self.consecutive_patch_failures >= 3 {
            self.patch_health_advised = true;
            return Some(
                "[harness advisory] 3+ consecutive patch failures — read the exact target lines (or use read with outline/ranges) before patching again, or use write if edits span most of the file."
                    .into(),
            );
        }
        if !self.read_batch_health_advised && self.consecutive_single_read_turns >= 4 {
            self.read_batch_health_advised = true;
            return Some(
                "[harness advisory] 4+ consecutive single-read turns — batch independent read calls in one turn (or use read with ranges/symbols/outline) to cut round-trip latency."
                    .into(),
            );
        }
        None
    }

    fn admit_shell(
        &mut self,
        call_id: &str,
        command: &str,
        args: Option<&[String]>,
        workspace: &Path,
    ) -> Option<String> {
        let command = match args {
            Some(args) => format!("{}\0{}", command.trim(), args.join("\0")),
            None => command.trim().to_owned(),
        };
        let fails = self.shell_failures.get(&command).copied().unwrap_or(0);
        // Nothing but reads ran since it failed.
        let just_failed = self
            .last_failed
            .as_ref()
            .is_some_and(|(failed, at)| *at == self.effects && *failed == command);
        let loop_key = format!("loop\0{command}");
        if (just_failed || fails >= FAILS_BEFORE_HOLD) && self.held.insert(loop_key.clone()) {
            let failed = if fails >= FAILS_BEFORE_HOLD {
                format!("failed {fails} times in a row")
            } else {
                "just failed".to_owned()
            };
            return Some(format!("This exact command {failed}. Change the approach instead of rerunning it. If a rerun is intended, retry the same command."));
        }
        self.held.remove(&loop_key);

        let read = args
            .is_none()
            .then(|| shell_read(&command))
            .flatten()
            .and_then(|read| Some((locate(&workspace.join(&read.path))?, read.span)));
        let mut claim = None;
        if let Some(((path, stamp), span)) = read {
            let name = file_name(&path);
            let Some(span) = fit(span, &path) else {
                return Some(format!("{name}: this command would print more than {MAX_READ_LINES} lines. Find the part you need with search, or print a range of up to {MAX_READ_LINES} lines."));
            };
            let held_key = format!("read\0{}\0{span:?}", path.display());
            let seen = self.effects == self.batch_start
                && self
                    .reads
                    .get(&path)
                    .is_some_and(|seen| seen.stamp == stamp && seen.covers(span));
            if seen && self.held.insert(held_key.clone()) {
                return Some(format!("{name} is unchanged and these lines were already read; use that output. If it's gone from context, retry the same command."));
            }
            self.held.remove(&held_key);
            claim = Some(Claim { path, stamp, span });
        } else {
            self.effects += 1;
        }
        self.pending.insert(
            call_id.to_owned(),
            Pending::Shell {
                command,
                at: self.effects,
                read: claim,
            },
        );
        None
    }

    fn admit_read(
        &mut self,
        call_id: &str,
        path: &Path,
        offset: usize,
        max_lines: Option<usize>,
        track_reads: bool,
    ) -> Option<String> {
        let (path, stamp) = locate(path)?;
        let first = offset.max(1);
        let lines = read_page_lines(first, max_lines, stamp.len);
        let first = u64::try_from(first).unwrap_or(END);
        let wanted = u64::try_from(lines).unwrap_or(END);
        if !track_reads {
            // CodeMode never remembers read spans (`shown == false`) and filters in JS RAM;
            // allow up to `MAX_READ_LINES_CAP` lines while avoiding serial disk scans on small reads.
            let code_cap = u64::try_from(crate::tools::MAX_READ_LINES_CAP).unwrap_or(4096);
            let over = first.saturating_add(code_cap);
            if wanted <= code_cap || stamp.len < over {
                return None;
            }
            if count_lines(&path, over).is_some_and(|count| count >= over) {
                return Some(format!("{}: this read would return more than {code_cap} lines. Find the part you need with search, or read up to {code_cap} lines per call with max_lines and offset.", file_name(&path)));
            }
            return None;
        }
        let span = Span {
            start: Start::Line(first),
            to: first.saturating_add(wanted).saturating_sub(1),
            numbered: false,
        };
        let Some(span) = fit(span, &path) else {
            return Some(format!("{}: this read would return more than {MAX_READ_LINES} lines. Find the part you need with search, or read up to {MAX_READ_LINES} lines per call with max_lines and offset.", file_name(&path)));
        };
        self.pending.insert(
            call_id.to_owned(),
            Pending::Read(Claim { path, stamp, span }),
        );
        None
    }

    fn admit_write(&mut self, call_id: &str, path: &Path) -> Option<String> {
        let metadata = std::fs::metadata(path).ok()?;
        if !metadata.is_file() || metadata.len() < MIN_REWRITE_BYTES {
            return None;
        }
        let name = file_name(path);
        self.pending
            .insert(call_id.to_owned(), Pending::Rewrite { name });
        None
    }

    /// Folds a call's outcome in; returns a note for its result. `shown`
    /// tells whether the model received the whole output.
    pub(super) fn settle(&mut self, call_id: &str, success: bool, shown: bool) -> Option<String> {
        match self.pending.remove(call_id)? {
            Pending::Shell { command, at, read } => {
                if success {
                    self.shell_failures.remove(&command);
                    if self
                        .last_failed
                        .as_ref()
                        .is_some_and(|(failed, _)| *failed == command)
                    {
                        self.last_failed = None;
                    }
                    if let Some(read) = read.filter(|_| shown) {
                        self.remember(read);
                    }
                } else {
                    let fails = self.shell_failures.get(&command).copied().unwrap_or(0);
                    insert_bounded(
                        &mut self.shell_failures,
                        command.clone(),
                        fails.saturating_add(1),
                    );
                    self.last_failed = Some((command, at));
                }
                None
            }
            Pending::Read(read) => {
                if success && shown {
                    self.remember(read);
                }
                None
            }
            Pending::Rewrite { name } => success.then(|| {
                format!("You rewrote all of {name}. For changes to an existing file use patch: it sends only the changed lines.")
            }),
        }
    }

    fn remember(&mut self, read: Claim) {
        match self.reads.get_mut(&read.path) {
            Some(seen) if seen.stamp == read.stamp => {
                if !seen.covers(read.span) {
                    if seen.spans.len() >= MAX_SPANS {
                        seen.spans.clear();
                    }
                    seen.spans.push(read.span);
                }
            }
            _ => insert_bounded(
                &mut self.reads,
                read.path,
                Seen {
                    stamp: read.stamp,
                    spans: vec![read.span],
                },
            ),
        }
    }
}

fn insert_bounded<K: Eq + Hash, V>(map: &mut HashMap<K, V>, key: K, value: V) {
    if map.len() >= MAX_TRACKED && !map.contains_key(&key) {
        map.clear();
    }
    map.insert(key, value);
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// The file at `path`, with its current stamp.
fn locate(path: &Path) -> Option<(PathBuf, Stamp)> {
    let real = std::fs::canonicalize(path).ok()?;
    let metadata = std::fs::metadata(&real)
        .ok()
        .filter(|meta| meta.is_file())?;
    let stamp = Stamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    };
    Some((real, stamp))
}

/// Lines in the file, counted up to `cap`: `cap` stands for at least that
/// many. `None` when it cannot be read, or is too long to tell.
fn count_lines(path: &Path, cap: u64) -> Option<u64> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut chunk = [0u8; 16 * 1024];
    let (mut lines, mut scanned, mut line_start) = (0u64, 0u64, true);
    loop {
        let read = file.read(&mut chunk).ok()?;
        if read == 0 {
            return Some(lines);
        }
        for &byte in &chunk[..read] {
            if line_start {
                lines += 1;
                if lines >= cap {
                    return Some(cap);
                }
            }
            line_start = byte == b'\n';
        }
        scanned = scanned.saturating_add(read as u64);
        if scanned >= MAX_SCAN_BYTES {
            return None;
        }
    }
}

/// `span` as it falls on the file at `path`, with the end of the file it
/// reaches folded in. `None` when it would return more than
/// [`MAX_READ_LINES`] lines.
fn fit(mut span: Span, path: &Path) -> Option<Span> {
    let (first, wanted) = match span.start {
        Start::Line(line) => (line, span.to.saturating_sub(line).saturating_add(1)),
        Start::Tail(lines) => (1, lines),
    };
    // The line whose presence puts more than the limit after `first`.
    let over = first.saturating_add(MAX_READ_LINES);
    match count_lines(path, over) {
        Some(lines) if lines >= over => (wanted <= MAX_READ_LINES).then_some(span),
        Some(lines) => {
            if let Start::Tail(tail) = span.start {
                span.start = Start::Line(lines.saturating_sub(tail).saturating_add(1));
            }
            if span.to >= lines {
                span.to = END;
            }
            Some(span)
        }
        None => Some(span),
    }
}

#[derive(Debug, Eq, PartialEq)]
struct ShellRead {
    path: String,
    span: Span,
}

const WHOLE: (Start, u64) = (Start::Line(1), END);

/// Outside quotes a word holds only these; anything else (a pipe, redirect,
/// chain, variable, glob, escape, `~`) could make the shell do more than read.
fn plain(c: char) -> bool {
    c.is_alphanumeric()
        || if cfg!(windows) {
            matches!(c, '_' | '.' | '-' | '/' | ':' | '\\')
        } else {
            matches!(c, '_' | '.' | '-' | '/' | ':' | '+' | ',' | '@' | '%' | '=')
        }
}

/// Splits a command into words with their quotes removed; `None` when any
/// part is not plain.
fn words(command: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut current: Option<String> = None;
    let mut quote: Option<char> = None;
    let mut chars = command.trim().chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(open) = quote {
            // PowerShell also closes a string on a typographic quote, and
            // spells a quote inside quotes by doubling it.
            if cfg!(windows) && ('\u{2018}'..='\u{201E}').contains(&c) {
                return None;
            }
            if c == open {
                if cfg!(windows) && chars.peek() == Some(&open) {
                    return None;
                }
                quote = None;
            } else if open == '"'
                && (matches!(c, '$' | '`') || (!cfg!(windows) && matches!(c, '\\' | '!')))
            {
                return None;
            } else {
                current.get_or_insert_with(String::new).push(c);
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                current.get_or_insert_with(String::new);
            }
            ' ' | '\t' => out.extend(current.take()),
            c if plain(c) => current.get_or_insert_with(String::new).push(c),
            _ => return None,
        }
    }
    if quote.is_some() {
        return None;
    }
    out.extend(current);
    Some(out)
}

fn count(word: Option<&str>) -> Option<u64> {
    let word = word?;
    (!word.starts_with('0') && !word.is_empty() && word.bytes().all(|b| b.is_ascii_digit()))
        .then(|| word.parse().ok())
        .flatten()
}

/// `-n N`, `-nN` or `-N`, the only flags head and tail take here.
fn line_flag(flags: &[String], tail: bool) -> Option<(Start, u64)> {
    let word = match flags {
        [flag, value] if flag == "-n" => value.as_str(),
        [flag] if flag.len() > 2 && flag.starts_with("-n") => &flag[2..],
        [flag]
            if flag.len() > 1 && flag.as_bytes()[1].is_ascii_digit() && flag.starts_with('-') =>
        {
            &flag[1..]
        }
        _ => return None,
    };
    if tail {
        if let Some(from) = word.strip_prefix('+') {
            return count(Some(from)).map(|from| (Start::Line(from), END));
        }
    }
    let lines = count(Some(word))?;
    Some(if tail {
        (Start::Tail(lines), END)
    } else {
        (Start::Line(1), lines)
    })
}

/// One plain read of one file: its path as typed and the lines it prints.
/// `None` for any command it does not fully understand.
fn shell_read(command: &str) -> Option<ShellRead> {
    let all = words(command)?;
    let (name, args) = all.split_first()?;
    if args.is_empty() {
        return None;
    }
    if cfg!(windows) {
        powershell_read(name, args)
    } else {
        bash_read(name, args)
    }
}

fn bash_read(name: &str, args: &[String]) -> Option<ShellRead> {
    let (path, flags) = args.split_last()?;
    if path.starts_with('-') {
        return None;
    }
    let ((start, to), numbered) = match name {
        "cat" if flags.is_empty() => (WHOLE, false),
        // Both number every line.
        "cat" if flags == ["-n"] => (WHOLE, true),
        "nl" if flags == ["-ba"] => (WHOLE, true),
        "sed" if flags.len() == 2 && flags[0] == "-n" => {
            let lines = flags[1].strip_suffix('p')?;
            let (from, to) = lines.split_once(',').unwrap_or((lines, lines));
            let (from, to) = (count(Some(from))?, count(Some(to))?);
            if to < from {
                return None;
            }
            ((Start::Line(from), to), false)
        }
        "head" | "tail" => (line_flag(flags, name == "tail")?, false),
        _ => return None,
    };
    Some(ShellRead {
        path: path.clone(),
        span: Span {
            start,
            to,
            numbered,
        },
    })
}

fn powershell_read(name: &str, args: &[String]) -> Option<ShellRead> {
    if !["get-content", "gc", "cat", "type"].contains(&name.to_ascii_lowercase().as_str()) {
        return None;
    }
    let mut path: Option<String> = None;
    let mut range: Option<(Start, u64)> = None;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].to_ascii_lowercase();
        match arg.as_str() {
            "-raw" => {}
            "-path" | "-literalpath" => {
                if path.is_some() {
                    return None;
                }
                index += 1;
                path = Some(args.get(index)?.clone());
            }
            "-totalcount" | "-head" | "-first" | "-tail" | "-last" => {
                index += 1;
                let lines = count(args.get(index).map(String::as_str))?;
                if range.is_some() {
                    return None;
                }
                range = Some(if matches!(arg.as_str(), "-tail" | "-last") {
                    (Start::Tail(lines), END)
                } else {
                    (Start::Line(1), lines)
                });
            }
            _ if arg.starts_with('-') || path.is_some() => return None,
            _ => path = Some(args[index].clone()),
        }
        index += 1;
    }
    let (start, to) = range.unwrap_or(WHOLE);
    path.filter(|path| !path.starts_with('-'))
        .map(|path| ShellRead {
            path,
            span: Span {
                start,
                to,
                numbered: false,
            },
        })
}

#[cfg(test)]
mod tests {
    use super::{count_lines, shell_read, RunGuards, ShellRead, Span, Start, END, MAX_READ_LINES};
    use crate::runtime::temp_root::TempRoot;
    use crate::tools::{PreparedToolInvocation, ToolRegistry};
    use crate::OperatingMode;

    fn read(path: &str, start: Start, to: u64) -> Option<ShellRead> {
        Some(ShellRead {
            path: path.into(),
            span: Span {
                start,
                to,
                numbered: false,
            },
        })
    }

    fn numbered(path: &str) -> Option<ShellRead> {
        Some(ShellRead {
            path: path.into(),
            span: Span {
                start: Start::Line(1),
                to: END,
                numbered: true,
            },
        })
    }

    #[cfg(not(windows))]
    #[test]
    fn bash_reads_are_recognized_and_anything_more_is_not() {
        let line = Start::Line;
        assert_eq!(
            shell_read("cat src/main.rs"),
            read("src/main.rs", line(1), END)
        );
        assert_eq!(shell_read("cat -n a.rs"), numbered("a.rs"));
        assert_eq!(shell_read("nl -ba a.rs"), numbered("a.rs"));
        assert_eq!(
            shell_read("sed -n '10,40p' a.rs"),
            read("a.rs", line(10), 40)
        );
        assert_eq!(shell_read("sed -n 7p a.rs"), read("a.rs", line(7), 7));
        assert_eq!(shell_read("head -n 20 a.rs"), read("a.rs", line(1), 20));
        assert_eq!(shell_read("head -20 a.rs"), read("a.rs", line(1), 20));
        assert_eq!(shell_read("tail -n +5 a.rs"), read("a.rs", line(5), END));
        assert_eq!(
            shell_read("tail -n5 \"my file.rs\""),
            read("my file.rs", Start::Tail(5), END)
        );
        for other in [
            "cat a.rs | head",
            "cat a.rs b.rs",
            "cat $FILE",
            "cat ~/a.rs",
            "cat a.rs > b.rs",
            "cat -A a.rs",
            "nl a.rs",
            "sed -n '40,10p' a.rs",
            "head -n 0 a.rs",
            "cat \"$(rm -rf x)\"",
            "cat",
            "ls a.rs",
        ] {
            assert_eq!(shell_read(other), None, "{other}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn powershell_reads_are_recognized_and_anything_more_is_not() {
        assert_eq!(
            shell_read("Get-Content -LiteralPath a.rs"),
            read("a.rs", Start::Line(1), END)
        );
        assert_eq!(
            shell_read("gc a.rs -TotalCount 20"),
            read("a.rs", Start::Line(1), 20)
        );
        assert_eq!(
            shell_read("Get-Content a.rs -Tail 5"),
            read("a.rs", Start::Tail(5), END)
        );
        assert_eq!(shell_read("Get-Content $env:X"), None);
        assert_eq!(
            shell_read("Get-Content a.rs | Select-Object -First 3"),
            None
        );
    }

    #[test]
    fn lines_are_counted_up_to_the_cap() {
        let root = TempRoot::new("count-lines");
        let count = |text: &str, cap| {
            std::fs::write(root.join("f"), text).expect("file");
            count_lines(&root.join("f"), cap)
        };
        assert_eq!(count("", 10), Some(0));
        assert_eq!(count("a", 10), Some(1));
        assert_eq!(count("a\n", 10), Some(1));
        assert_eq!(count("a\nb", 10), Some(2));
        assert_eq!(count("\n\n", 10), Some(2));
        assert_eq!(
            count("a\nb\nc\n", 3),
            Some(3),
            "the cap is at least that many"
        );
        assert_eq!(count("a\nb\nc\nd\n", 3), Some(3));
        assert_eq!(count_lines(&root.join("missing"), 10), None);
    }

    struct Rig {
        root: TempRoot,
        registry: ToolRegistry,
        guards: RunGuards,
    }

    impl Rig {
        fn new(label: &str) -> Self {
            Self {
                root: TempRoot::new(label),
                registry: ToolRegistry::default(),
                guards: RunGuards::default(),
            }
        }

        /// Admits a call as a batch of its own; `Err` carries the hold message.
        fn admit(
            &mut self,
            id: &str,
            tool: &str,
            arguments: serde_json::Value,
        ) -> Result<PreparedToolInvocation, String> {
            self.guards.begin_batch();
            self.admit_in_batch(id, tool, arguments)
        }

        fn admit_in_batch(
            &mut self,
            id: &str,
            tool: &str,
            arguments: serde_json::Value,
        ) -> Result<PreparedToolInvocation, String> {
            let mut prepared = self.registry.prepare_invocation(
                OperatingMode::Auto,
                &self.root,
                tool,
                &arguments.to_string(),
            );
            assert!(prepared.error.is_none(), "{:?}", prepared.error);
            self.guards.admit(id, &mut prepared);
            match prepared.error.take() {
                Some(message) => {
                    assert!(prepared.structural_rejection);
                    Err(message)
                }
                None => Ok(prepared),
            }
        }

        fn shell(&mut self, id: &str, command: &str) -> Result<PreparedToolInvocation, String> {
            self.admit(id, "shell", serde_json::json!({"command": command}))
        }

        /// Runs a call that succeeds and whose output the model sees whole.
        fn ran(&mut self, id: &str, tool: &str, arguments: serde_json::Value) {
            assert!(self.admit(id, tool, arguments).is_ok(), "{id}");
            self.guards.settle(id, true, true);
        }

        fn lines(&self, name: &str, count: u64) {
            let text: String = (1..=count).map(|line| format!("{line}\n")).collect();
            std::fs::write(self.root.join(name), text).expect("file");
        }
    }

    const REREAD: &str = "a.txt is unchanged and these lines were already read";

    #[cfg(not(windows))]
    #[test]
    fn an_unchanged_shell_read_is_held_once_and_a_retry_goes_through() {
        let mut rig = Rig::new("reread");
        std::fs::write(rig.root.join("a.txt"), "one\ntwo\n").expect("file");
        assert!(rig.shell("1", "cat a.txt").is_ok());
        assert_eq!(rig.guards.settle("1", true, true), None);
        let held = rig.shell("2", "cat a.txt").unwrap_err();
        assert!(held.starts_with(REREAD), "{held}");
        assert!(rig.guards.was_held("2"));
        assert!(rig.shell("3", "cat a.txt").is_ok(), "a retry goes through");
        assert!(!rig.guards.was_held("3"));
        rig.guards.settle("3", true, true);
        assert!(
            rig.shell("4", "sed -n 2p a.txt").is_ok(),
            "a slice from another line is another read"
        );

        std::fs::write(rig.root.join("a.txt"), "one\ntwo\nthree\n").expect("edit");
        assert!(
            rig.shell("5", "cat a.txt").is_ok(),
            "a changed file is read again"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn the_same_lines_spelled_another_way_are_the_same_read() {
        for (same, whole) in [
            ("tail -n +1 a.txt", true),
            ("head -n 50 a.txt", true),
            ("sed -n '1,3p' a.txt", true),
            ("tail -n 3 a.txt", true),
            ("head -n 2 a.txt", false),
        ] {
            let mut rig = Rig::new("reread-spelling");
            rig.lines("a.txt", 3);
            rig.ran("1", "shell", serde_json::json!({"command": "cat a.txt"}));
            let held = rig.shell("2", same).unwrap_err();
            assert!(held.starts_with(REREAD), "{same}: {held}");
            assert_eq!(
                rig.shell("3", "cat a.txt").is_ok(),
                whole,
                "{same}: any spelling of the held lines is its retry"
            );
        }
        let mut rig = Rig::new("reread-numbered");
        rig.lines("a.txt", 3);
        rig.ran("1", "shell", serde_json::json!({"command": "cat a.txt"}));
        assert!(
            rig.shell("2", "cat -n a.txt").is_ok(),
            "line numbers are new"
        );
        rig.guards.settle("2", true, true);
        assert!(rig.shell("3", "nl -ba a.txt").is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn a_shell_read_of_lines_the_native_read_returned_is_held() {
        let mut rig = Rig::new("reread-native");
        rig.lines("a.txt", 3);
        rig.lines("cut.txt", 3);
        rig.ran("1", "read", serde_json::json!({"path": "a.txt"}));
        let held = rig.shell("2", "cat a.txt").unwrap_err();
        assert!(held.starts_with(REREAD), "{held}");
        assert!(
            rig.admit("3", "read", serde_json::json!({"path": "a.txt"}))
                .is_ok(),
            "a native read is left to the duplicate pointer"
        );

        assert!(rig
            .admit("4", "read", serde_json::json!({"path": "cut.txt"}))
            .is_ok());
        rig.guards.settle("4", true, false);
        assert!(
            rig.shell("5", "cat cut.txt").is_ok(),
            "an output cut for the model is not remembered"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn a_read_after_a_change_in_its_batch_is_not_held() {
        let mut rig = Rig::new("reread-batch");
        std::fs::write(rig.root.join("a.txt"), "one\ntwo\n").expect("file");
        let cat = || serde_json::json!({"command": "cat a.txt"});
        rig.ran("1", "shell", cat());

        rig.guards.begin_batch();
        let patch = serde_json::json!({"path": "a.txt", "edits": [{"expected": "one", "replacement": "uno"}]});
        assert!(rig.admit_in_batch("2", "patch", patch).is_ok());
        assert!(
            rig.admit_in_batch("3", "shell", cat()).is_ok(),
            "the patch before it may change the file"
        );
        rig.guards.settle("3", true, true);
        assert!(rig.shell("4", "cat a.txt").is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn a_failed_shell_read_is_not_remembered() {
        let mut rig = Rig::new("reread-failed");
        std::fs::write(rig.root.join("a.txt"), "one\n").expect("file");
        assert!(rig.shell("1", "cat a.txt").is_ok());
        rig.guards.settle("1", false, true);
        let held = rig.shell("2", "cat a.txt").unwrap_err();
        assert!(
            held.starts_with("This exact command just failed."),
            "{held}"
        );
        assert!(rig.shell("3", "cat a.txt").is_ok());
    }

    #[test]
    fn a_read_of_more_than_the_line_limit_is_refused_until_it_is_narrowed() {
        let mut rig = Rig::new("long-read");
        rig.lines("long.txt", MAX_READ_LINES + 1);
        rig.lines("fits.txt", MAX_READ_LINES);
        let read = |path: &str| serde_json::json!({"path": path});

        for id in ["1", "2"] {
            let held = rig.admit(id, "read", read("long.txt")).unwrap_err();
            assert!(
                held.starts_with("long.txt: this read would return more than 1000 lines."),
                "{held}"
            );
            assert!(rig.guards.was_held(id));
        }
        let window = |offset: u64, max_lines: u64| serde_json::json!({"path": "long.txt", "offset": offset, "max_lines": max_lines});
        assert!(rig.admit("3", "read", window(1, 1001)).is_err());
        assert!(rig.admit("4", "read", window(1, 1000)).is_ok());
        assert!(rig.admit("5", "read", window(2, 4096)).is_ok());
        assert!(
            rig.admit(
                "6",
                "read",
                serde_json::json!({"path": "long.txt", "offset": 2})
            )
            .is_ok(),
            "a default page"
        );
        assert!(rig.admit("7", "read", read("fits.txt")).is_ok());
        assert!(rig.admit("8", "read", read("missing.txt")).is_ok());
    }

    #[cfg(not(windows))]
    #[test]
    fn a_shell_read_of_more_than_the_line_limit_is_refused() {
        let mut rig = Rig::new("long-shell-read");
        rig.lines("long.txt", MAX_READ_LINES + 1);
        rig.lines("fits.txt", MAX_READ_LINES);
        for (id, long) in [
            ("1", "cat long.txt"),
            ("2", "cat long.txt"),
            ("3", "cat -n long.txt"),
            ("4", "nl -ba long.txt"),
            ("5", "sed -n '1,1001p' long.txt"),
            ("6", "head -n 5000 long.txt"),
            ("7", "tail -n +1 long.txt"),
            ("8", "tail -n 1001 long.txt"),
        ] {
            let held = rig.shell(id, long).unwrap_err();
            assert!(
                held.starts_with("long.txt: this command would print more than 1000 lines."),
                "{long}: {held}"
            );
        }
        for (id, fits) in [
            ("9", "sed -n '1,1000p' long.txt"),
            ("10", "sed -n '2,1001p' long.txt"),
            ("11", "tail -n +2 long.txt"),
            ("12", "tail -n 1000 long.txt"),
            ("13", "head -n 20 long.txt"),
            ("14", "cat fits.txt"),
            ("15", "wc -l long.txt"),
        ] {
            assert!(rig.shell(id, fits).is_ok(), "{fits}");
        }
    }

    #[test]
    fn a_command_run_again_right_after_it_failed_is_held_once() {
        let mut rig = Rig::new("loop");
        let command = "cargo build --bad-flag";
        assert!(rig.shell("1", command).is_ok());
        rig.guards.settle("1", false, true);
        let held = rig.shell("2", command).unwrap_err();
        assert!(
            held.starts_with("This exact command just failed."),
            "{held}"
        );
        assert!(rig.guards.was_held("2"));

        assert!(
            rig.shell("3", command).is_ok(),
            "a deliberate rerun goes through"
        );
        rig.guards.settle("3", false, true);
        let held = rig.shell("4", command).unwrap_err();
        assert!(
            held.starts_with("This exact command failed 2 times in a row."),
            "{held}"
        );
        assert!(rig.shell("5", command).is_ok());
        rig.guards.settle("5", true, true);
        assert!(
            rig.shell("6", command).is_ok(),
            "a success clears the count"
        );
        assert!(
            rig.shell("7", "cargo build").is_ok(),
            "other commands are untouched"
        );
    }

    #[test]
    fn only_a_read_between_keeps_a_failed_command_held() {
        let mut rig = Rig::new("loop-between");
        std::fs::write(rig.root.join("a.txt"), "one\n").expect("file");
        let command = "cargo build --bad-flag";
        assert!(rig.shell("1", command).is_ok());
        rig.guards.settle("1", false, true);
        rig.ran("2", "read", serde_json::json!({"path": "a.txt"}));
        assert!(
            rig.shell("3", command).is_err(),
            "a read changes nothing the command saw"
        );

        let mut rig = Rig::new("loop-edited");
        std::fs::write(rig.root.join("a.txt"), "one\n").expect("file");
        assert!(rig.shell("1", command).is_ok());
        rig.guards.settle("1", false, true);
        let patch = serde_json::json!({"path": "a.txt", "edits": [{"expected": "one", "replacement": "uno"}]});
        rig.ran("2", "patch", patch);
        assert!(rig.shell("3", command).is_ok(), "an edit reopens it");
        rig.guards.settle("3", false, true);
        rig.ran("4", "shell", serde_json::json!({"command": "cargo clean"}));
        assert!(
            rig.shell("5", command).is_err(),
            "two failures in a row still hold it"
        );
    }

    #[test]
    fn the_first_full_rewrite_is_noted_and_so_is_a_later_one() {
        let mut rig = Rig::new("rewrite");
        let big = "x".repeat(4096);
        for name in ["a.txt", "b.txt"] {
            std::fs::write(rig.root.join(name), &big).expect("file");
        }
        std::fs::write(rig.root.join("small.txt"), "tiny").expect("file");
        let write = |path: &str| serde_json::json!({"path": path, "content": "new"});

        assert!(rig.admit("1", "write", write("a.txt")).is_ok());
        let note = rig.guards.settle("1", true, true).expect("note");
        assert!(note.starts_with("You rewrote all of a.txt."), "{note}");

        assert!(
            rig.admit("2", "write", write("b.txt")).is_ok(),
            "a later rewrite is not held: its tokens are already spent"
        );
        let note = rig.guards.settle("2", true, true).expect("note");
        assert!(note.starts_with("You rewrote all of b.txt."), "{note}");

        assert!(
            rig.admit("4", "write", write("small.txt")).is_ok(),
            "small files are left alone"
        );
        assert!(
            rig.admit("5", "write", write("new.txt")).is_ok(),
            "new files are left alone"
        );
    }

    #[test]
    fn health_advisories_fire_once_at_thresholds_and_reset_cleanly() {
        let mut guards = super::RunGuards::default();
        let patch_call = crate::provider::ProviderToolCall {
            id: "p".into(),
            name: "patch".into(),
            arguments: "{}".into(),
        };
        let fail_patch = crate::tools::ToolResult::fail("patch", "err");
        let ok_patch = crate::tools::ToolResult::ok("patch", "ok");
        assert!(guards
            .observe_batch_health(&[patch_call.clone()], &[fail_patch.clone()])
            .is_none());
        assert!(guards
            .observe_batch_health(&[patch_call.clone()], &[ok_patch])
            .is_none());
        assert!(guards
            .observe_batch_health(&[patch_call.clone()], &[fail_patch.clone()])
            .is_none());
        assert!(guards
            .observe_batch_health(&[patch_call.clone()], &[fail_patch.clone()])
            .is_none());
        let first_adv = guards
            .observe_batch_health(&[patch_call.clone()], &[fail_patch.clone()])
            .expect("3 consecutive failed patches should emit advisory");
        assert!(first_adv.contains("3+ consecutive patch failures"));
        // Never fires twice.
        assert!(guards
            .observe_batch_health(&[patch_call], &[fail_patch])
            .is_none());

        let read_call = crate::provider::ProviderToolCall {
            id: "r".into(),
            name: "read".into(),
            arguments: "{}".into(),
        };
        let ok_read = crate::tools::ToolResult::ok("read", "content");
        for _ in 0..3 {
            assert!(guards
                .observe_batch_health(std::slice::from_ref(&read_call), std::slice::from_ref(&ok_read))
                .is_none());
        }
        // Batched read resets the streak.
        assert!(guards
            .observe_batch_health(&[read_call.clone(), read_call.clone()], &[ok_read.clone(), ok_read.clone()])
            .is_none());
        for _ in 0..3 {
            assert!(guards
                .observe_batch_health(std::slice::from_ref(&read_call), std::slice::from_ref(&ok_read))
                .is_none());
        }
        let read_adv = guards
            .observe_batch_health(std::slice::from_ref(&read_call), std::slice::from_ref(&ok_read))
            .expect("4 consecutive single-read turns should emit advisory");
        assert!(read_adv.contains("4+ consecutive single-read turns"));
        assert!(guards
            .observe_batch_health(&[read_call], &[ok_read])
            .is_none());
    }
}
