//! Guards that hold a wasteful call back once per run, as the claude-pro-kit
//! reread-guard, write-guard and loop-guard mods do for Claude Code:
//!
//! - a shell read (`cat`, `sed -n`, `head`, `tail`, `Get-Content`) of a file
//!   unchanged since the same read ran;
//! - a full `write` over an existing file worth a `patch`: the first one in a
//!   run goes through with a note, a later one is held;
//! - a shell command that already failed twice in a row.
//!
//! The model reads a hold as the call's error. Retrying the held call goes
//! through, for an output that left the context or a deliberate rerun.
//! A native `read` needs no guard: an identical output still in context is
//! already replaced by a pointer.

use crate::tools::{PreparedToolArguments, PreparedToolInvocation};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Files under this size cost little to rewrite; they are left alone.
const MIN_REWRITE_BYTES: u64 = 2048;
const FAILS_BEFORE_HOLD: u32 = 2;
const MAX_TRACKED: usize = 500;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
}

/// What a call's outcome must settle once it ran.
enum Pending {
    Shell {
        command: String,
        /// The read it claimed; given back if the command fails.
        read: Option<String>,
    },
    /// The run's first full rewrite: its result carries a note.
    Rewrite { name: String },
}

#[derive(Default)]
pub(super) struct RunGuards {
    /// Shell reads by `path\0range`, with the file's stamp when they ran.
    reads: HashMap<String, Stamp>,
    /// Consecutive failures of each shell command.
    shell_failures: HashMap<String, u32>,
    /// Calls held once; the next identical call goes through.
    held: HashSet<String>,
    rewrote: bool,
    pending: HashMap<String, Pending>,
}

impl RunGuards {
    /// Holds `prepared` back when a guard applies; otherwise remembers what
    /// its outcome must settle.
    pub(super) fn admit(&mut self, call_id: &str, prepared: &mut PreparedToolInvocation) {
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
            PreparedToolArguments::Write { .. } => prepared
                .target_paths
                .first()
                .and_then(|path| self.admit_write(call_id, path)),
            _ => None,
        };
        if let Some(message) = hold {
            prepared.reject(&message);
        }
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
        let loop_key = format!("loop\0{command}");
        if fails >= FAILS_BEFORE_HOLD && self.held.insert(loop_key.clone()) {
            return Some(format!("This exact command failed {fails} times in a row. Change the approach instead of rerunning it. If a rerun is intended, retry the same command."));
        }
        self.held.remove(&loop_key);

        let read = args
            .is_none()
            .then(|| shell_read(&command))
            .flatten()
            .and_then(|read| Some((locate(&read.path, workspace)?, read.range)));
        let mut claimed = None;
        if let Some(((path, stamp), range)) = read {
            let read_key = format!("{}\0{range}", path.display());
            let held_key = format!("read\0{read_key}");
            if self.reads.get(&read_key) == Some(&stamp) && self.held.insert(held_key.clone()) {
                return Some(format!("{} unchanged since this same read; use that output. If it's gone from context, retry the same command.", file_name(&path)));
            }
            self.held.remove(&held_key);
            // Claimed before it runs, so an identical read later in the
            // same batch is held too.
            insert_bounded(&mut self.reads, read_key.clone(), stamp);
            claimed = Some(read_key);
        }
        self.pending.insert(
            call_id.to_owned(),
            Pending::Shell {
                command,
                read: claimed,
            },
        );
        None
    }

    fn admit_write(&mut self, call_id: &str, path: &Path) -> Option<String> {
        let metadata = std::fs::metadata(path).ok()?;
        if !metadata.is_file() || metadata.len() < MIN_REWRITE_BYTES {
            return None;
        }
        let name = file_name(path);
        if !self.rewrote {
            self.rewrote = true;
            self.pending
                .insert(call_id.to_owned(), Pending::Rewrite { name });
            return None;
        }
        let key = format!("write\0{}", path.display());
        if self.held.insert(key.clone()) {
            return Some(format!("{name} exists; change it with patch, not a full write. If a full rewrite is intended, retry the same write."));
        }
        self.held.remove(&key);
        None
    }

    /// Folds a call's outcome in; returns a note for its result.
    pub(super) fn settle(&mut self, call_id: &str, success: bool) -> Option<String> {
        match self.pending.remove(call_id)? {
            Pending::Shell { command, read } => {
                if success {
                    self.shell_failures.remove(&command);
                } else {
                    let fails = self.shell_failures.get(&command).copied().unwrap_or(0);
                    insert_bounded(&mut self.shell_failures, command, fails.saturating_add(1));
                    if let Some(read) = read {
                        self.reads.remove(&read);
                    }
                }
                None
            }
            Pending::Rewrite { name } => success.then(|| {
                format!("You rewrote all of {name}. For changes to an existing file use patch: it sends only the changed lines.")
            }),
        }
    }
}

fn insert_bounded<V>(map: &mut HashMap<String, V>, key: String, value: V) {
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

/// The file a shell in `workspace` reads at `path`, with its current stamp.
fn locate(path: &str, workspace: &Path) -> Option<(PathBuf, Stamp)> {
    let real = std::fs::canonicalize(workspace.join(path)).ok()?;
    let metadata = std::fs::metadata(&real)
        .ok()
        .filter(|meta| meta.is_file())?;
    let stamp = Stamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    };
    Some((real, stamp))
}

#[derive(Debug, Eq, PartialEq)]
struct ShellRead {
    path: String,
    /// The lines it prints; empty for the whole file.
    range: String,
}

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
fn line_flag(flags: &[String], tail: bool) -> Option<String> {
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
            return count(Some(from)).map(|from| format!("{from}-end"));
        }
    }
    let lines = count(Some(word))?;
    Some(if tail {
        format!("last {lines}")
    } else {
        format!("1-{lines}")
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
    let range = match name {
        "cat" if flags.is_empty() => String::new(),
        "sed" if flags.len() == 2 && flags[0] == "-n" => {
            let lines = flags[1].strip_suffix('p')?;
            let (from, to) = lines.split_once(',').unwrap_or((lines, lines));
            let (from, to) = (count(Some(from))?, count(Some(to))?);
            if to < from {
                return None;
            }
            format!("{from}-{to}")
        }
        "head" | "tail" => line_flag(flags, name == "tail")?,
        _ => return None,
    };
    Some(ShellRead {
        path: path.clone(),
        range,
    })
}

fn powershell_read(name: &str, args: &[String]) -> Option<ShellRead> {
    if !["get-content", "gc", "cat", "type"].contains(&name.to_ascii_lowercase().as_str()) {
        return None;
    }
    let mut path: Option<String> = None;
    let mut range = String::new();
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
                if !range.is_empty() {
                    return None;
                }
                range = if matches!(arg.as_str(), "-tail" | "-last") {
                    format!("last {lines}")
                } else {
                    format!("1-{lines}")
                };
            }
            _ if arg.starts_with('-') || path.is_some() => return None,
            _ => path = Some(args[index].clone()),
        }
        index += 1;
    }
    path.filter(|path| !path.starts_with('-'))
        .map(|path| ShellRead { path, range })
}

#[cfg(test)]
mod tests {
    use super::{shell_read, RunGuards, ShellRead};
    use crate::runtime::temp_root::TempRoot;
    use crate::tools::{PreparedToolInvocation, ToolRegistry};
    use crate::OperatingMode;

    fn read(path: &str, range: &str) -> Option<ShellRead> {
        Some(ShellRead {
            path: path.into(),
            range: range.into(),
        })
    }

    #[cfg(not(windows))]
    #[test]
    fn bash_reads_are_recognized_and_anything_more_is_not() {
        assert_eq!(shell_read("cat src/main.rs"), read("src/main.rs", ""));
        assert_eq!(shell_read("sed -n '10,40p' a.rs"), read("a.rs", "10-40"));
        assert_eq!(shell_read("sed -n 7p a.rs"), read("a.rs", "7-7"));
        assert_eq!(shell_read("head -n 20 a.rs"), read("a.rs", "1-20"));
        assert_eq!(shell_read("head -20 a.rs"), read("a.rs", "1-20"));
        assert_eq!(shell_read("tail -n +5 a.rs"), read("a.rs", "5-end"));
        assert_eq!(
            shell_read("tail -n5 \"my file.rs\""),
            read("my file.rs", "last 5")
        );
        for other in [
            "cat a.rs | head",
            "cat a.rs b.rs",
            "cat $FILE",
            "cat ~/a.rs",
            "cat a.rs > b.rs",
            "cat -n a.rs",
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
            read("a.rs", "")
        );
        assert_eq!(shell_read("gc a.rs -TotalCount 20"), read("a.rs", "1-20"));
        assert_eq!(
            shell_read("Get-Content a.rs -Tail 5"),
            read("a.rs", "last 5")
        );
        assert_eq!(shell_read("Get-Content $env:X"), None);
        assert_eq!(
            shell_read("Get-Content a.rs | Select-Object -First 3"),
            None
        );
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

        /// Admits a call; `Err` carries the hold message.
        fn admit(
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
    }

    #[cfg(not(windows))]
    #[test]
    fn an_unchanged_shell_read_is_held_once_and_a_retry_goes_through() {
        let mut rig = Rig::new("reread");
        std::fs::write(rig.root.join("a.txt"), "one\ntwo\n").expect("file");
        assert!(rig.shell("1", "cat a.txt").is_ok());
        assert_eq!(rig.guards.settle("1", true), None);
        let held = rig.shell("2", "cat a.txt").unwrap_err();
        assert!(
            held.starts_with("a.txt unchanged since this same read"),
            "{held}"
        );
        assert!(rig.shell("3", "cat a.txt").is_ok(), "a retry goes through");
        rig.guards.settle("3", true);
        assert!(
            rig.shell("4", "head -n 1 a.txt").is_ok(),
            "another range is another read"
        );

        std::fs::write(rig.root.join("a.txt"), "one\ntwo\nthree\n").expect("edit");
        assert!(
            rig.shell("5", "cat a.txt").is_ok(),
            "a changed file is read again"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn a_failed_shell_read_gives_its_claim_back() {
        let mut rig = Rig::new("reread-failed");
        std::fs::write(rig.root.join("a.txt"), "one\n").expect("file");
        assert!(rig.shell("1", "cat a.txt").is_ok());
        rig.guards.settle("1", false);
        assert!(rig.shell("2", "cat a.txt").is_ok());
    }

    #[test]
    fn a_command_failing_twice_in_a_row_is_held_once() {
        let mut rig = Rig::new("loop");
        let command = "cargo build --bad-flag";
        for id in ["1", "2"] {
            assert!(rig.shell(id, command).is_ok());
            rig.guards.settle(id, false);
        }
        let held = rig.shell("3", command).unwrap_err();
        assert!(
            held.starts_with("This exact command failed 2 times in a row."),
            "{held}"
        );
        assert!(
            rig.shell("4", command).is_ok(),
            "a deliberate rerun goes through"
        );
        rig.guards.settle("4", true);
        assert!(
            rig.shell("5", command).is_ok(),
            "a success clears the count"
        );
        assert!(
            rig.shell("6", "cargo build").is_ok(),
            "other commands are untouched"
        );
    }

    #[test]
    fn the_first_full_rewrite_is_noted_and_a_later_one_is_held_once() {
        let mut rig = Rig::new("rewrite");
        let big = "x".repeat(4096);
        for name in ["a.txt", "b.txt"] {
            std::fs::write(rig.root.join(name), &big).expect("file");
        }
        std::fs::write(rig.root.join("small.txt"), "tiny").expect("file");
        let write = |path: &str| serde_json::json!({"path": path, "content": "new"});

        assert!(rig.admit("1", "write", write("a.txt")).is_ok());
        let note = rig.guards.settle("1", true).expect("note");
        assert!(note.starts_with("You rewrote all of a.txt."), "{note}");

        let held = rig.admit("2", "write", write("b.txt")).unwrap_err();
        assert!(
            held.starts_with("b.txt exists; change it with patch"),
            "{held}"
        );
        assert!(
            rig.admit("3", "write", write("b.txt")).is_ok(),
            "a deliberate rewrite goes through"
        );
        assert_eq!(rig.guards.settle("3", true), None);

        assert!(
            rig.admit("4", "write", write("small.txt")).is_ok(),
            "small files are left alone"
        );
        assert!(
            rig.admit("5", "write", write("new.txt")).is_ok(),
            "new files are left alone"
        );
    }
}
