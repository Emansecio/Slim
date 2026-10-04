//! Pi-style configuration value resolution for MCP `env`, `headers` and
//! `oauth.client_secret` values.
//!
//! - `$NAME` and `${NAME}` insert an environment variable. Unset or empty
//!   variables are errors, so a missing secret never becomes an empty header.
//! - `$$` is a literal `$` and `$!` a literal `!` (only meaningful in
//!   templates).
//! - A value that starts with `!` is a command: the rest runs in a shell with
//!   a 10 second timeout and its trimmed stdout is the value. It must make up
//!   the whole value; nothing is interpolated inside it.
//!
//! Error text names the failing variable or the failure kind, never a value
//! and never the command line (it may contain a credential).

use std::path::Path;
use std::time::Duration;

use slim_core::process::{ExecutableResolver, ProcessOutputBudget, ProcessRequest, ProcessRunner};

pub(crate) const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const COMMAND_OUTPUT_LIMIT: usize = 64 * 1024;

/// Where interpolation reads its inputs. Production uses [`SystemSource`];
/// tests inject fixed values.
pub(crate) trait ValueSource {
    fn env_var(&self, name: &str) -> Option<String>;
    /// Runs `command` (the text after the leading `!`) with `cwd` as the
    /// working directory and returns its raw stdout.
    fn run_command(&self, command: &str, cwd: &Path) -> Result<String, String>;
}

pub(crate) struct SystemSource;

impl ValueSource for SystemSource {
    fn env_var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn run_command(&self, command: &str, cwd: &Path) -> Result<String, String> {
        let runner = ProcessRunner::new(ExecutableResolver::default());
        let (program, args) = shell_invocation(&runner, command)?;
        let output = runner
            .run(ProcessRequest {
                cwd: cwd.to_path_buf(),
                program,
                args,
                timeout: COMMAND_TIMEOUT,
                cancellation: None,
                output_budget: ProcessOutputBudget::per_stream(COMMAND_OUTPUT_LIMIT),
            })
            .map_err(|error| format!("shell command could not be started ({})", error.kind()))?;
        if output.timed_out {
            return Err(format!(
                "shell command timed out after {} s",
                COMMAND_TIMEOUT.as_secs()
            ));
        }
        if !output.output.status.success() {
            return Err(match output.output.status.code() {
                Some(code) => format!("shell command exited with status {code}"),
                None => "shell command was terminated".to_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&output.output.stdout).into_owned())
    }
}

#[cfg(windows)]
fn shell_invocation(
    runner: &ProcessRunner,
    command: &str,
) -> Result<(std::path::PathBuf, Vec<std::ffi::OsString>), String> {
    let program = runner
        .resolve_powershell()
        .map_err(|error| format!("shell command could not be started ({})", error.kind()))?
        .ok_or_else(|| "neither pwsh nor powershell was found on PATH".to_owned())?;
    // Same shell, UTF-8 setup and exit status rules as the native shell tool.
    let script = format!(
        "$utf8 = [System.Text.UTF8Encoding]::new($false)\n\
         $OutputEncoding = $utf8\n\
         [Console]::InputEncoding = $utf8\n\
         [Console]::OutputEncoding = $utf8\n\
         {command}\n\
         if (-not $?) {{ if ($LASTEXITCODE) {{ exit $LASTEXITCODE }}; \
         if ($LASTEXITCODE -eq 0 -and $Error.Count -and \
         $Error[0].FullyQualifiedErrorId -eq 'NativeCommandError') {{ exit 0 }}; exit 1 }}"
    );
    Ok((
        program,
        [
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ]
        .into_iter()
        .map(std::ffi::OsString::from)
        .collect(),
    ))
}

#[cfg(not(windows))]
fn shell_invocation(
    runner: &ProcessRunner,
    command: &str,
) -> Result<(std::path::PathBuf, Vec<std::ffi::OsString>), String> {
    let program = runner
        .resolver()
        .resolve("sh")
        .map_err(|error| format!("shell command could not be started ({})", error.kind()))?
        .ok_or_else(|| "sh was not found on PATH".to_owned())?;
    Ok((
        program,
        vec!["-c".into(), std::ffi::OsString::from(command)],
    ))
}

enum Part {
    Literal(String),
    Env(String),
}

fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn push_literal(parts: &mut Vec<Part>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(Part::Literal(last)) = parts.last_mut() {
        last.push_str(text);
    } else {
        parts.push(Part::Literal(text.to_owned()));
    }
}

fn parse_template(config: &str) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut rest = config;
    while let Some(dollar) = rest.find('$') {
        push_literal(&mut parts, &rest[..dollar]);
        let after = &rest[dollar + 1..];
        let mut chars = after.chars();
        match chars.next() {
            Some(escaped @ ('$' | '!')) => {
                push_literal(&mut parts, &escaped.to_string());
                rest = &after[1..];
            }
            Some('{') => match after[1..].split_once('}') {
                Some((name, tail)) => {
                    if valid_env_name(name) {
                        parts.push(Part::Env(name.to_owned()));
                    } else {
                        push_literal(&mut parts, &format!("${{{name}}}"));
                    }
                    rest = tail;
                }
                None => {
                    push_literal(&mut parts, "$");
                    rest = after;
                }
            },
            _ => {
                let name_len = after
                    .char_indices()
                    .take_while(|(index, ch)| {
                        if *index == 0 {
                            ch.is_ascii_alphabetic() || *ch == '_'
                        } else {
                            ch.is_ascii_alphanumeric() || *ch == '_'
                        }
                    })
                    .count();
                if name_len == 0 {
                    push_literal(&mut parts, "$");
                    rest = after;
                } else {
                    parts.push(Part::Env(after[..name_len].to_owned()));
                    rest = &after[name_len..];
                }
            }
        }
    }
    push_literal(&mut parts, rest);
    parts
}

/// Resolves one configuration value. `cwd` is where a `!command` runs.
pub(crate) fn resolve_value(
    raw: &str,
    cwd: &Path,
    source: &dyn ValueSource,
) -> Result<String, String> {
    if let Some(command) = raw.strip_prefix('!') {
        if command.trim().is_empty() {
            return Err("shell command is empty".to_owned());
        }
        let output = source.run_command(command, cwd)?;
        let value = output.trim();
        if value.is_empty() {
            return Err("shell command produced no output".to_owned());
        }
        return Ok(value.to_owned());
    }
    let parts = parse_template(raw);
    let mut missing: Vec<&str> = Vec::new();
    let mut resolved = String::new();
    for part in &parts {
        match part {
            Part::Literal(text) => resolved.push_str(text),
            Part::Env(name) => match source.env_var(name).filter(|value| !value.is_empty()) {
                Some(value) => resolved.push_str(&value),
                None => {
                    if !missing.contains(&name.as_str()) {
                        missing.push(name);
                    }
                }
            },
        }
    }
    match missing.as_slice() {
        [] => Ok(resolved),
        [one] => Err(format!("environment variable {one} is not set")),
        many => Err(format!(
            "environment variables {} are not set",
            many.join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fixed {
        vars: BTreeMap<&'static str, &'static str>,
        command_result: Option<Result<String, String>>,
        ran: Mutex<Vec<String>>,
    }

    impl ValueSource for Fixed {
        fn env_var(&self, name: &str) -> Option<String> {
            self.vars.get(name).map(|value| (*value).to_owned())
        }

        fn run_command(&self, command: &str, _cwd: &Path) -> Result<String, String> {
            self.ran.lock().unwrap().push(command.to_owned());
            self.command_result
                .clone()
                .unwrap_or_else(|| Err("no command configured".into()))
        }
    }

    fn fixed(vars: &[(&'static str, &'static str)]) -> Fixed {
        Fixed {
            vars: vars.iter().copied().collect(),
            ..Fixed::default()
        }
    }

    fn resolve(raw: &str, source: &Fixed) -> Result<String, String> {
        resolve_value(raw, Path::new("."), source)
    }

    #[test]
    fn literals_variables_and_escapes_follow_pi_semantics() {
        let source = fixed(&[("LEFT", "left"), ("RIGHT", "right")]);
        assert_eq!(resolve("literal", &source).unwrap(), "literal");
        assert_eq!(resolve("$LEFT", &source).unwrap(), "left");
        assert_eq!(resolve("${LEFT}_$RIGHT", &source).unwrap(), "left_right");
        assert_eq!(
            resolve("Bearer ${RIGHT}!", &source).unwrap(),
            "Bearer right!"
        );
        assert_eq!(resolve("$$LEFT", &source).unwrap(), "$LEFT");
        assert_eq!(
            resolve("$!literal-$RIGHT", &source).unwrap(),
            "!literal-right"
        );
        // `$` that does not start a reference stays literal.
        assert_eq!(resolve("a$", &source).unwrap(), "a$");
        assert_eq!(
            resolve("$1 ${not valid} ${open", &source).unwrap(),
            "$1 ${not valid} ${open"
        );
        assert_eq!(resolve("100$ now", &source).unwrap(), "100$ now");
    }

    #[test]
    fn unset_or_empty_variables_fail_naming_only_the_variable() {
        let source = fixed(&[("EMPTY", ""), ("SET", "x")]);
        assert_eq!(
            resolve("$MISSING", &source).unwrap_err(),
            "environment variable MISSING is not set"
        );
        assert_eq!(
            resolve("a-${EMPTY}-$SET", &source).unwrap_err(),
            "environment variable EMPTY is not set"
        );
        assert_eq!(
            resolve("$ONE $TWO $ONE", &source).unwrap_err(),
            "environment variables ONE, TWO are not set"
        );
    }

    #[test]
    fn whole_value_command_runs_once_and_trims_output() {
        let mut source = fixed(&[("IGNORED", "no")]);
        source.command_result = Some(Ok("  token-value \r\n".into()));
        assert_eq!(resolve("!gh auth token", &source).unwrap(), "token-value");
        assert_eq!(
            *source.ran.lock().unwrap(),
            vec!["gh auth token".to_owned()]
        );
        // Nothing inside a command is interpolated: it is handed over verbatim.
        let mut source = fixed(&[("HOME_TOKEN", "secret")]);
        source.command_result = Some(Ok("v".into()));
        resolve("!echo $HOME_TOKEN", &source).unwrap();
        assert_eq!(
            *source.ran.lock().unwrap(),
            vec!["echo $HOME_TOKEN".to_owned()]
        );
    }

    #[test]
    fn command_failures_never_echo_the_command_line() {
        let mut source = Fixed {
            command_result: Some(Err("shell command exited with status 3".into())),
            ..Fixed::default()
        };
        let error = resolve("!echo hunter2-secret", &source).unwrap_err();
        assert_eq!(error, "shell command exited with status 3");
        assert!(!error.contains("hunter2"));
        source.command_result = Some(Ok("  \n".into()));
        assert_eq!(
            resolve("!anything", &source).unwrap_err(),
            "shell command produced no output"
        );
        assert_eq!(resolve("!", &source).unwrap_err(), "shell command is empty");
    }

    #[test]
    fn a_command_must_be_the_whole_value() {
        // The marker only counts at the start: elsewhere it is literal text.
        let source = fixed(&[]);
        assert_eq!(
            resolve("Bearer !not-a-command", &source).unwrap(),
            "Bearer !not-a-command"
        );
        assert!(source.ran.lock().unwrap().is_empty());
    }

    #[test]
    fn system_source_runs_a_real_command_and_reports_failures() {
        let source = SystemSource;
        let cwd = std::env::temp_dir();
        let echoed = source
            .run_command("echo slim-interp-ok", &cwd)
            .expect("echo runs");
        assert_eq!(echoed.trim(), "slim-interp-ok");
        let error = source.run_command("exit 7", &cwd).unwrap_err();
        assert_eq!(error, "shell command exited with status 7");
        // A native program that exits 0 after writing to a redirected stderr
        // succeeded, whatever Windows PowerShell makes of `$?`.
        #[cfg(windows)]
        assert!(source
            .run_command("cmd /c \"echo warn 1>&2 & echo value\" 2>&1", &cwd)
            .expect("stderr under 2>&1 is not a failure")
            .contains("value"));
    }

    #[test]
    fn system_source_reads_process_environment() {
        let name = "SLIM_INTERP_TEST_VAR_UNSET_93f1";
        assert!(SystemSource.env_var(name).is_none());
    }
}
