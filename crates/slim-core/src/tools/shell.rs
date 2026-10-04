use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use super::ToolError;
use crate::process::{
    ProcessExecutionFacts, ProcessOutputBudget, ProcessProgress, ProcessRequest, ProcessRunner,
};
use crate::runtime::CancellationToken;

const SHELL_CAPTURE_CAP_BYTES: usize = 8 * 1024 * 1024;
/// CreateProcess takes at most 32,767 UTF-16 units, terminator included.
const MAX_WINDOWS_COMMAND_LINE_CHARS: usize = 32_766;

/// Runs before the model's script and shares its first line, so a position
/// PowerShell reports (`At line:N`) is the line of the script as written. No
/// `ConvertTo-Json:Depth` default: a deep graph (a `FileInfo`) fans out
/// without bound.
const SHELL_SETUP: &str = "$utf8 = [System.Text.UTF8Encoding]::new($false); \
     $OutputEncoding = $utf8; \
     [Console]::InputEncoding = $utf8; \
     [Console]::OutputEncoding = $utf8; \
     $PSDefaultParameterValues['Get-Content:Encoding'] = 'UTF8'; \
     $PSDefaultParameterValues['Invoke-WebRequest:UseBasicParsing'] = $true; \
     $PSDefaultParameterValues['Out-File:Encoding'] = 'utf8'; ";

/// PowerShell -Command otherwise collapses a native program's nonzero exit
/// code to 1. Preserve it without turning a failed cmdlet into success.
/// Windows PowerShell also clears `$?` when a native program that exited 0
/// wrote to a redirected stderr (`2>&1`); that is not a failure. A script that
/// ends successfully after a native program failed keeps exit 0 and says so on
/// stderr. Known limit: a stale `NativeCommandError` left in `$Error` by an
/// earlier, handled failure (for example under `-ErrorAction Ignore`) can still
/// turn a later real failure into exit 0.
const SHELL_EPILOGUE: &str = "if (-not $?) { if ($LASTEXITCODE) { exit $LASTEXITCODE }; \
     if ($LASTEXITCODE -eq 0 -and $Error.Count -and \
     $Error[0].FullyQualifiedErrorId -eq 'NativeCommandError') { exit 0 }; exit 1 } \
     elseif ($LASTEXITCODE) { [Console]::Error.WriteLine('[note: exit 0, but the last \
     native command exited ' + $LASTEXITCODE + ']') }";

pub(crate) enum ShellInvocation<'a> {
    Script(&'a str),
    Program {
        executable: &'a str,
        args: &'a [String],
    },
}

pub fn run_shell(cwd: impl AsRef<Path>, command: &str) -> Result<Output, ToolError> {
    let runner = ProcessRunner::default();
    let result = run_with_runner(
        &runner,
        cwd.as_ref(),
        ShellInvocation::Script(command),
        Duration::MAX,
        None,
        ProcessOutputBudget::per_stream(usize::MAX),
        (|_| {}, None),
    )?;
    Ok(result.output)
}

#[derive(Debug)]
pub struct TimedShellOutput {
    pub output: Output,
    pub timed_out: bool,
    pub cancelled: bool,
    pub stdout_discarded_bytes: usize,
    pub stderr_discarded_bytes: usize,
    pub capture_may_be_incomplete: bool,
    pub interrupted: bool,
    pub interrupt_escalated: bool,
}

impl TimedShellOutput {
    pub(crate) fn execution_facts(&self) -> ProcessExecutionFacts {
        ProcessExecutionFacts::from_output(
            &self.output,
            self.timed_out,
            self.cancelled,
            self.capture_may_be_incomplete,
            self.stdout_discarded_bytes,
            self.stderr_discarded_bytes,
        )
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ShellProgress {
    pub elapsed_ms: u64,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    pub last_line: String,
}

impl From<ProcessProgress> for ShellProgress {
    fn from(progress: ProcessProgress) -> Self {
        Self {
            elapsed_ms: progress.elapsed_ms,
            stdout_bytes: progress.stdout_bytes,
            stderr_bytes: progress.stderr_bytes,
            last_line: progress.last_line,
        }
    }
}

pub fn run_shell_timeout(
    cwd: impl AsRef<Path>,
    command: &str,
    timeout: Duration,
) -> Result<TimedShellOutput, ToolError> {
    run_shell_timeout_cancellable(cwd, command, timeout, None)
}

pub fn run_shell_timeout_cancellable(
    cwd: impl AsRef<Path>,
    command: &str,
    timeout: Duration,
    cancellation: Option<&CancellationToken>,
) -> Result<TimedShellOutput, ToolError> {
    run_shell_timeout_cancellable_with_progress(cwd, command, timeout, cancellation, |_| {})
}

pub fn run_shell_timeout_cancellable_with_progress(
    cwd: impl AsRef<Path>,
    command: &str,
    timeout: Duration,
    cancellation: Option<&CancellationToken>,
    on_progress: impl FnMut(ShellProgress),
) -> Result<TimedShellOutput, ToolError> {
    run_shell_timeout_cancellable_with_progress_and_runner(
        &ProcessRunner::default(),
        cwd,
        ShellInvocation::Script(command),
        timeout,
        cancellation,
        on_progress,
    )
}

pub(crate) fn run_shell_timeout_cancellable_with_progress_and_runner(
    runner: &ProcessRunner,
    cwd: impl AsRef<Path>,
    invocation: ShellInvocation<'_>,
    timeout: Duration,
    cancellation: Option<&CancellationToken>,
    on_progress: impl FnMut(ShellProgress),
) -> Result<TimedShellOutput, ToolError> {
    run_shell_timeout_cancellable_with_progress_and_runner_with_budget(
        runner,
        cwd,
        invocation,
        timeout,
        cancellation,
        ProcessOutputBudget::per_stream(SHELL_CAPTURE_CAP_BYTES),
        (on_progress, None),
    )
}

pub(crate) fn run_shell_timeout_cancellable_with_progress_and_runner_with_budget(
    runner: &ProcessRunner,
    cwd: impl AsRef<Path>,
    invocation: ShellInvocation<'_>,
    timeout: Duration,
    cancellation: Option<&CancellationToken>,
    output_budget: ProcessOutputBudget,
    progress: (
        impl FnMut(ShellProgress),
        Option<crate::process::ProcessObserver>,
    ),
) -> Result<TimedShellOutput, ToolError> {
    let (mut on_progress, observer) = progress;
    run_with_runner(
        runner,
        cwd.as_ref(),
        invocation,
        timeout,
        cancellation,
        output_budget,
        (|progress| on_progress(progress.into()), observer),
    )
}

fn run_with_runner(
    runner: &ProcessRunner,
    cwd: &Path,
    invocation: ShellInvocation<'_>,
    timeout: Duration,
    cancellation: Option<&CancellationToken>,
    output_budget: ProcessOutputBudget,
    progress: (
        impl FnMut(ProcessProgress),
        Option<crate::process::ProcessObserver>,
    ),
) -> Result<TimedShellOutput, ToolError> {
    let (on_progress, observer) = progress;
    let command = match &invocation {
        ShellInvocation::Script(command) => *command,
        ShellInvocation::Program { executable, .. } => *executable,
    };
    if command.trim().is_empty() {
        return Err(ToolError::InvalidInput {
            message: "shell command cannot be empty".into(),
        });
    }
    let (program, args) = match invocation {
        ShellInvocation::Script(command) => shell_invocation(runner, command)?,
        ShellInvocation::Program { executable, args } => {
            let path = Path::new(executable);
            let program = if path.is_absolute() || path.components().count() == 1 {
                path.to_path_buf()
            } else {
                cwd.join(path)
            };
            (program, args.iter().map(OsString::from).collect())
        }
    };
    let request = ProcessRequest {
        cwd: cwd.to_path_buf(),
        program,
        args,
        timeout,
        cancellation: cancellation.cloned(),
        output_budget,
    };
    let result = runner.run_observed(request, on_progress, observer)?;
    Ok(TimedShellOutput {
        output: result.output,
        timed_out: result.timed_out,
        cancelled: result.cancelled,
        capture_may_be_incomplete: result.capture_may_be_incomplete,
        stdout_discarded_bytes: result.stdout_discarded_bytes,
        stderr_discarded_bytes: result.stderr_discarded_bytes,
        interrupted: result.interrupted,
        interrupt_escalated: result.interrupt_escalated,
    })
}

fn shell_invocation(
    runner: &ProcessRunner,
    command: &str,
) -> Result<(PathBuf, Vec<OsString>), ToolError> {
    let program = powershell_program(runner)?;
    // The blank line keeps a command that ends in a backtick (line
    // continuation) from swallowing the epilogue into its last statement.
    let script = format!("{SHELL_SETUP}{command}\n\n{SHELL_EPILOGUE}");
    let args = [
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        &script,
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    let command_line_chars = windows_command_line_chars(&program, &args);
    if cfg!(windows) && command_line_chars > MAX_WINDOWS_COMMAND_LINE_CHARS {
        return Err(ToolError::InvalidInput {
            message: format!(
                "shell script is too long for the Windows command line ({command_line_chars} of {MAX_WINDOWS_COMMAND_LINE_CHARS} characters, setup included); save it to a .ps1 file with write and run the file, for example `& ([scriptblock]::Create((Get-Content -Raw -LiteralPath script.ps1)))`"
            ),
        });
    }
    Ok((program, args))
}

/// Length of the command line `std::process::Command` builds on Windows: the
/// program always quoted, an argument quoted when it holds a space, tab or
/// nothing, and each quote escaped along with the backslashes before it.
fn windows_command_line_chars(program: &Path, args: &[OsString]) -> usize {
    fn argument_chars(argument: &str, force_quotes: bool) -> usize {
        let quoted = force_quotes || argument.is_empty() || argument.contains([' ', '\t']);
        let mut chars = if quoted { 2 } else { 0 };
        let mut backslashes = 0;
        for character in argument.chars() {
            if character == '\\' {
                backslashes += 1;
            } else {
                if character == '"' {
                    chars += backslashes + 1;
                }
                backslashes = 0;
            }
            chars += character.len_utf16();
        }
        if quoted {
            chars += backslashes;
        }
        chars
    }
    argument_chars(&program.to_string_lossy(), true)
        + args
            .iter()
            .map(|argument| 1 + argument_chars(&argument.to_string_lossy(), false))
            .sum::<usize>()
}

/// Whether script commands run in Windows PowerShell 5.1 rather than
/// PowerShell 7 (`pwsh`).
pub(crate) fn script_shell_is_windows_powershell(runner: &ProcessRunner) -> bool {
    powershell_program(runner).is_ok_and(|program| {
        program
            .file_stem()
            .is_some_and(|stem| stem.eq_ignore_ascii_case("powershell"))
    })
}

fn powershell_program(runner: &ProcessRunner) -> Result<PathBuf, ToolError> {
    runner.resolve_powershell()?.ok_or_else(|| ToolError::Io {
        message: "neither pwsh nor powershell was found on PATH".into(),
    })
}

/// Shell output as the model should read it: terminal escape sequences and
/// other control characters are removed (`\n` and `\t` stay), `\r\n` becomes
/// `\n`, a lone `\r` overwrites its line (only the text after the last one
/// survives; if that is empty, the last non-empty frame does), and three or
/// more identical consecutive lines collapse into the first plus a marker.
/// Blank lines are never collapsed. Text without any of this is returned
/// unchanged. Applied before the size cap so noise does not consume it; raw
/// logs are never passed through it.
pub(crate) fn normalize_shell_text(text: &str) -> String {
    let mut cleaned = String::with_capacity(text.len());
    let mut frame = String::new();
    let mut last_frame = String::new();
    let mut chars = text.chars().peekable();
    let finish_line = |cleaned: &mut String, frame: &mut String, last_frame: &mut String| {
        let line = if frame.is_empty() {
            &*last_frame
        } else {
            &*frame
        };
        cleaned.push_str(line);
        frame.clear();
        last_frame.clear();
    };
    while let Some(character) = chars.next() {
        if let Some(line_break) = crate::mcp::skip_terminal_sequence(character, &mut chars) {
            if line_break {
                finish_line(&mut cleaned, &mut frame, &mut last_frame);
                cleaned.push('\n');
            }
            continue;
        }
        match character {
            '\n' => {
                finish_line(&mut cleaned, &mut frame, &mut last_frame);
                cleaned.push('\n');
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\r' => {
                if !frame.is_empty() {
                    std::mem::swap(&mut frame, &mut last_frame);
                    frame.clear();
                }
            }
            '\t' => frame.push('\t'),
            character if character.is_control() => {}
            character => frame.push(character),
        }
    }
    finish_line(&mut cleaned, &mut frame, &mut last_frame);
    collapse_repeated_lines(&strip_native_error_frames(&strip_script_scaffolding(
        &cleaned,
    )))
}

/// Shortest tail of the (ASCII) setup that is recognized when PowerShell shows only
/// that part of the line around an error.
const MIN_SETUP_FRAGMENT_BYTES: usize = 14;

/// PowerShell quotes the whole script it ran in front of some error messages
/// (`<script> : message`), wrapped at its console width, and echoes the part of
/// the first line around an error position (`+ ...rue; <command>`). The setup
/// and epilogue are ours, not the model's: remove them from what it reads.
/// Wrapping inserts line breaks and spaces, so the match ignores whitespace.
fn strip_script_scaffolding(text: &str) -> std::borrow::Cow<'_, str> {
    let setup_tail = "= 'utf8'; ";
    if !text.contains("$utf8") && !text.contains(setup_tail) {
        return text.into();
    }
    let mut stripped = text.to_owned();
    stripped = remove_wrapped(&stripped, SHELL_SETUP, false);
    stripped = remove_wrapped(&stripped, SHELL_EPILOGUE, true);
    stripped = stripped
        .split_inclusive('\n')
        .map(strip_setup_fragment)
        .collect();
    if stripped == text {
        text.into()
    } else {
        stripped.into()
    }
}

/// Removes every whitespace-insensitive occurrence of `part`, with the
/// whitespace before it when `eat_before`, after it otherwise.
fn remove_wrapped(text: &str, part: &str, eat_before: bool) -> String {
    let wanted = part
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<Vec<_>>();
    let mut chars = Vec::new();
    let mut origins = Vec::new();
    for (offset, character) in text.char_indices() {
        if !character.is_whitespace() {
            chars.push(character);
            origins.push(offset);
        }
    }
    let mut output = String::with_capacity(text.len());
    let mut copied = 0;
    let mut index = 0;
    while index + wanted.len() <= chars.len() {
        if chars[index..index + wanted.len()] != wanted[..] {
            index += 1;
            continue;
        }
        let last = index + wanted.len() - 1;
        let mut end = origins[last] + chars[last].len_utf8();
        let mut start = origins[index].max(copied);
        if eat_before {
            start -= text[copied..start].len() - text[copied..start].trim_end().len();
        } else {
            end += text[end..].len() - text[end..].trim_start().len();
        }
        output.push_str(&text[copied..start]);
        copied = end;
        index += wanted.len();
    }
    output.push_str(&text[copied..]);
    output
}

/// A `+ ...tail-of-setup; command` position echo without the tail of the setup.
fn strip_setup_fragment(line: &str) -> String {
    let Some(rest) = line.trim_start().strip_prefix("+ ") else {
        return line.to_owned();
    };
    let (dots, rest) = match rest.strip_prefix("... ") {
        Some(rest) => ("... ", rest),
        None => ("", rest),
    };
    for (start, _) in SHELL_SETUP.char_indices() {
        let fragment = &SHELL_SETUP[start..];
        if fragment.len() < MIN_SETUP_FRAGMENT_BYTES {
            break;
        }
        if let Some(after) = rest.strip_prefix(fragment) {
            let indent = &line[..line.len() - line.trim_start().len()];
            return format!("{indent}+ {dots}{after}");
        }
    }
    line.to_owned()
}

/// Windows PowerShell reports the first stderr line of a native program under
/// `2>&1` as an error record: the line itself, then a position line, the
/// echoed command, `+ CategoryInfo`, `+ FullyQualifiedErrorId :
/// NativeCommandError` and a blank line. Only the stderr line is output; the
/// frame around it is dropped, recognised by shape since its text is localized.
fn strip_native_error_frames(text: &str) -> std::borrow::Cow<'_, str> {
    const FRAME_END: &str = "+ FullyQualifiedErrorId : NativeCommandError";
    if !text.contains(FRAME_END) {
        return text.into();
    }
    let mut kept = Vec::<&str>::new();
    let mut lines = text.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        if line.trim() != FRAME_END {
            kept.push(line);
            continue;
        }
        while kept
            .last()
            .is_some_and(|line| line.trim_start().starts_with("+ "))
        {
            kept.pop();
        }
        // `At line:1 char:1`, in whatever language the host speaks.
        if kept.last().is_some_and(|line| {
            line.trim_end()
                .ends_with(|last: char| last.is_ascii_digit())
        }) {
            kept.pop();
        }
        lines.next_if(|line| line.trim().is_empty());
    }
    kept.concat().into()
}

fn collapse_repeated_lines(text: &str) -> String {
    fn line_at(text: &str, start: usize) -> &str {
        let rest = &text[start..];
        &rest[..rest.find('\n').map_or(rest.len(), |at| at + 1)]
    }
    let mut collapsed = String::with_capacity(text.len());
    let mut start = 0;
    while start < text.len() {
        let line = line_at(text, start);
        let content = line.strip_suffix('\n').unwrap_or(line);
        let mut end = start + line.len();
        let mut run = 1usize;
        if !content.trim().is_empty() {
            while end < text.len() {
                let next = line_at(text, end);
                if next.strip_suffix('\n').unwrap_or(next) != content {
                    break;
                }
                end += next.len();
                run += 1;
            }
        }
        let original = &text[start..end];
        // A run is replaced only when the replacement is strictly shorter, so
        // cleaning never makes the text longer; shorter runs stay verbatim.
        if run >= 3 {
            let marker = format!(
                "{content}\n[previous line repeated {} more times]{}",
                run - 1,
                if original.ends_with('\n') { "\n" } else { "" }
            );
            if marker.len() < original.len() {
                collapsed.push_str(&marker);
                start = end;
                continue;
            }
        }
        collapsed.push_str(original);
        start = end;
    }
    collapsed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::ExecutableResolver;
    use std::fs;

    fn fixture_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("slim-shell-direct-{}-{name}", std::process::id()));
        fs::create_dir_all(&root).expect("root");
        root
    }

    fn write_executable(root: &Path, name: &str) -> PathBuf {
        let exe = root.join(if cfg!(windows) {
            format!("{name}.EXE")
        } else {
            name.to_owned()
        });
        fs::write(&exe, b"fixture").expect("exe");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).expect("mode");
        }
        exe
    }

    fn runner_for(root: &Path) -> ProcessRunner {
        ProcessRunner::new(ExecutableResolver::with_environment(
            std::env::join_paths([root]).expect("PATH"),
            OsString::from(if cfg!(windows) { ".EXE" } else { "" }),
        ))
    }

    #[test]
    fn script_invocation_always_uses_powershell() {
        let root = fixture_root("script");
        let powershell = write_executable(&root, "pwsh");
        write_executable(&root, "fake");
        let runner = runner_for(&root);
        let (program, args) =
            shell_invocation(&runner, "fake --flag 'two words'").expect("resolve");

        assert_eq!(program, powershell);
        assert_eq!(args[0], OsString::from("-NoLogo"));
        assert_eq!(args[1], OsString::from("-NoProfile"));
        assert_eq!(args[2], OsString::from("-NonInteractive"));
        assert_eq!(args[3], OsString::from("-Command"));
        // The setup shares the command's first line, so line numbers PowerShell
        // reports are those of the script as written.
        let script = args[4].to_string_lossy();
        let first_line = script.lines().next().expect("first line");
        assert!(first_line.starts_with("$utf8 = "), "{first_line}");
        assert!(
            first_line.ends_with("; fake --flag 'two words'"),
            "{first_line}"
        );
        assert_eq!(
            script.lines().count(),
            3,
            "command line, blank line, epilogue: {script}"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn script_lines_keep_their_numbers_after_the_setup() {
        let root = fixture_root("script-lines");
        write_executable(&root, "pwsh");
        let runner = runner_for(&root);
        let (_, args) = shell_invocation(&runner, "first\nsecond\nthird").expect("resolve");
        let script = args[4].to_string_lossy();
        let lines = script.lines().collect::<Vec<_>>();
        assert!(lines[0].ends_with("; first"), "{}", lines[0]);
        assert_eq!(&lines[1..3], ["second", "third"]);
        // A blank line keeps a trailing backtick from reaching the epilogue.
        assert_eq!(lines[3], "");
        assert!(lines[4].starts_with("if (-not $?)"), "{}", lines[4]);
        for default in [
            "'Get-Content:Encoding'] = 'UTF8'",
            "'Invoke-WebRequest:UseBasicParsing'] = $true",
            "'Out-File:Encoding'] = 'utf8'",
        ] {
            assert!(lines[0].contains(default), "{default}");
        }
        for untouched in ["Set-Content:", "Add-Content:", "ConvertTo-Json"] {
            assert!(!script.contains(untouched), "{untouched}");
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_script_over_the_windows_command_line_limit_is_rejected_up_front() {
        let root = fixture_root("script-too-long");
        write_executable(&root, "pwsh");
        let runner = runner_for(&root);
        let long = format!("Write-Output ok # {}", "x".repeat(33_000));
        let result = shell_invocation(&runner, &long);
        if cfg!(windows) {
            let error = result.expect_err("33000 characters cannot be launched");
            assert!(
                matches!(&error, ToolError::InvalidInput { message }
                    if message.contains("too long for the Windows command line")
                        && message.contains("write")),
                "{error:?}"
            );
            // 31,500 launches (probe on Windows PowerShell 5.1); the limit is
            // measured on the whole command line, not on the script alone.
            let fits = format!("Write-Output ok # {}", "x".repeat(31_000));
            assert!(shell_invocation(&runner, &fits).is_ok());
        } else {
            assert!(result.is_ok());
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn command_line_length_follows_std_quoting() {
        let program = Path::new("C:\\bin\\pwsh.exe");
        let plain = windows_command_line_chars(program, &[OsString::from("-NoLogo")]);
        assert_eq!(plain, "\"C:\\bin\\pwsh.exe\" -NoLogo".len());
        // Spaces force quotes; each inner quote gains a backslash, and
        // backslashes before a quote (or the closing quote) double.
        let quoted = windows_command_line_chars(program, &[OsString::from("a \"b\\\" c\\")]);
        assert_eq!(
            quoted,
            "\"C:\\bin\\pwsh.exe\" \"a \\\"b\\\\\\\" c\\\\\"".len()
        );
    }

    #[test]
    fn normalize_removes_escape_sequences_and_controls_but_keeps_tabs_and_newlines() {
        assert_eq!(
            normalize_shell_text(
                "\u{1b}[31mred\u{1b}[0m\ta\u{1b}]0;title\u{7}b\u{7}\u{0}c\u{9b}1m\n"
            ),
            "red\tabc\n"
        );
        assert_eq!(normalize_shell_text("x\u{1b}\ny"), "x\ny");
        assert_eq!(normalize_shell_text("tail\u{1b}"), "tail");
    }

    #[test]
    fn normalize_collapses_carriage_return_frames_to_the_last_frame() {
        assert_eq!(
            normalize_shell_text("10%\r50%\r100%\ndone\n"),
            "100%\ndone\n"
        );
        // An empty segment after the last `\r` keeps the last non-empty frame.
        assert_eq!(normalize_shell_text("10%\r50%\r\r\nnext"), "50%\nnext");
        assert_eq!(normalize_shell_text("10%\r50%\rnext"), "next");
        assert_eq!(normalize_shell_text("a\rb\r"), "b");
    }

    #[test]
    fn normalize_turns_crlf_into_line_breaks_and_keeps_tabs() {
        assert_eq!(normalize_shell_text("a\r\nb\r\n\tc\r\n"), "a\nb\n\tc\n");
        assert_eq!(normalize_shell_text("--- a\n+\tb\r\n"), "--- a\n+\tb\n");
    }

    #[test]
    fn normalize_collapses_three_or_more_identical_lines_but_not_two() {
        let same = "downloading crate foo v1.0.0";
        assert_eq!(
            normalize_shell_text(&format!("x\n{same}\n{same}\n{same}\n{same}\ny\n")),
            format!("x\n{same}\n[previous line repeated 3 more times]\ny\n")
        );
        assert_eq!(
            normalize_shell_text(&format!("{same}\n{same}\n{same}")),
            format!("{same}\n[previous line repeated 2 more times]")
        );
        let two = format!("a\n{same}\n{same}\nb\n");
        assert_eq!(normalize_shell_text(&two), two);
        let pair = format!("{same}\n{same}");
        assert_eq!(normalize_shell_text(&pair), pair);
        // Frames that collapse to the same line also count as repeats.
        assert_eq!(
            normalize_shell_text(&format!("a\r{same}\n").repeat(3)),
            format!("{same}\n[previous line repeated 2 more times]\n")
        );
        // Blank runs are layout, not noise.
        assert_eq!(normalize_shell_text("a\n\n\n\n\nb\n"), "a\n\n\n\n\nb\n");
    }

    #[test]
    fn normalize_never_returns_more_bytes_than_it_received() {
        // A short repeated line would grow if replaced by the marker.
        assert_eq!(
            normalize_shell_text("a\na\na\nb\nb\nb\n"),
            "a\na\na\nb\nb\nb\n"
        );
        let raw = "a\na\na\nb\nb\nb\n".repeat(7164 / 12);
        assert!(raw.len() < 7 * 1024);
        assert_eq!(normalize_shell_text(&raw), raw);
        // Three lines of L bytes become L + 39: collapsed only from L = 19.
        for (length, collapses) in [(18, false), (19, true)] {
            let run = format!("{}\n", "x".repeat(length)).repeat(3);
            let out = normalize_shell_text(&run);
            assert_eq!(out != run, collapses, "L={length}: {out:?}");
            assert!(out.len() <= run.len());
        }
    }

    #[test]
    fn normalize_drops_the_native_error_frame_and_keeps_the_stderr_line() {
        let framed = "before\ncargo :     Finished dev in 1.06s\nNo linha:1 caractere:302\n\
                      + ... tch {} } }; cargo test -p slim-cli 2>&1\n\
                      +                 ~~~~~~~~~~~~~~~~~~~~~~\n    \
                      + CategoryInfo          : NotSpecified: (    Finished:String) [], RemoteException\n    \
                      + FullyQualifiedErrorId : NativeCommandError\n \nrunning 1 test\n";
        assert_eq!(
            normalize_shell_text(framed),
            "before\ncargo :     Finished dev in 1.06s\nrunning 1 test\n"
        );
        // Any other error record is evidence and stays whole.
        let cmdlet = "Get-Item : not found\nAt line:1 char:1\n+ Get-Item x\n+ ~~~~~~~~~~\n    \
                      + CategoryInfo          : ObjectNotFound: (x:String) [Get-Item], ItemNotFoundException\n    \
                      + FullyQualifiedErrorId : PathNotFound,Microsoft.PowerShell.Commands.GetItemCommand\n";
        assert_eq!(normalize_shell_text(cmdlet), cmdlet);
    }

    #[test]
    fn normalize_removes_the_setup_and_epilogue_that_powershell_quotes_in_errors() {
        // PowerShell prints `<whole script> : message`, wrapped at 120 columns.
        let script = format!("{SHELL_SETUP}Write-Error boom\n\n{SHELL_EPILOGUE}");
        let mut wrapped = String::new();
        let mut width = 0;
        for word in script.split_inclusive(' ') {
            if width + word.len() > 118 {
                wrapped.push_str("\r\n");
                width = 0;
            }
            wrapped.push_str(word);
            width += word.len();
        }
        let raw = format!(
            "{wrapped} : boom\r\n    + CategoryInfo          : NotSpecified: (:) [Write-Error], WriteErrorException\r\n    + FullyQualifiedErrorId : Microsoft.PowerShell.Commands.WriteErrorException\r\n \r\n"
        );
        let cleaned = normalize_shell_text(&raw);
        assert_eq!(
            cleaned,
            "Write-Error boom : boom\n    + CategoryInfo          : NotSpecified: (:) [Write-Error], WriteErrorException\n    + FullyQualifiedErrorId : Microsoft.PowerShell.Commands.WriteErrorException\n \n"
        );
        // The position echo of a first-line error shows the tail of the setup.
        let echo = "At line:1 char:339\n+ ... rue; $PSDefaultParameterValues['Out-File:Encoding'] = 'utf8'; throw x\n+                                                                     ~\n";
        assert_eq!(
            normalize_shell_text(echo),
            "At line:1 char:339\n+ ... throw x\n+                                                                     ~\n"
        );
        // Text that merely mentions a variable of that name is the model's.
        let own = "$utf8 = 1 is the model's own line\n+ ... = 'utf8'; no\n";
        assert_eq!(normalize_shell_text(own), own);
    }

    #[test]
    fn normalize_leaves_clean_text_byte_identical() {
        for text in [
            "",
            "\n",
            "plain",
            "line one\nline two\n",
            "no final newline\nlast",
            "  indented\n\ttabbed\n",
            "unicode: é ✓ — ok\n\n\nspaced\n",
            "a\na\nb\nb\n",
        ] {
            assert_eq!(normalize_shell_text(text), text);
        }
    }
}
