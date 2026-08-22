mod list;
mod patch;
mod read;
mod search;
mod shell;
mod write;

use crate::context::ArtifactHandle;
use crate::runtime::CancellationToken;
use crate::OperatingMode;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub use list::list_directory;
pub use patch::apply_exact_patch;
pub use read::{read_file, read_file_range};
pub use search::{search_literal, SearchHit};
pub use shell::{run_shell, run_shell_timeout, run_shell_timeout_cancellable, TimedShellOutput};
pub use write::{write_file, FilePrecondition};

#[derive(Debug, Eq, PartialEq)]
pub enum ToolError {
    Io { message: String },
    StaleRead { path: String },
    PreconditionRequired { path: String },
    MatchCount { count: usize },
    InvalidInput { message: String },
}

impl From<std::io::Error> for ToolError {
    fn from(error: std::io::Error) -> Self {
        Self::Io {
            message: error.to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ToolSpec {
    name: &'static str,
    mutates: bool,
}

#[derive(Debug)]
pub struct ToolRegistry {
    specs: &'static [ToolSpec],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolResult {
    pub name: String,
    pub success: bool,
    pub output: String,
    pub artifact: Option<ArtifactHandle>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self {
            specs: &[
                ToolSpec {
                    name: "read",
                    mutates: false,
                },
                ToolSpec {
                    name: "list",
                    mutates: false,
                },
                ToolSpec {
                    name: "search",
                    mutates: false,
                },
                ToolSpec {
                    name: "write",
                    mutates: true,
                },
                ToolSpec {
                    name: "patch",
                    mutates: true,
                },
                ToolSpec {
                    name: "shell",
                    mutates: true,
                },
            ],
        }
    }
}

impl ToolRegistry {
    pub fn names_for_mode(&self, mode: OperatingMode) -> Vec<&'static str> {
        self.specs
            .iter()
            .filter(|spec| mode == OperatingMode::Auto || !spec.mutates)
            .map(|spec| spec.name)
            .collect()
    }

    pub fn definitions_for_mode(&self, mode: OperatingMode) -> Vec<Value> {
        self.names_for_mode(mode)
            .into_iter()
            .map(tool_definition)
            .collect()
    }

    pub fn execute(
        &self,
        mode: OperatingMode,
        cwd: impl AsRef<Path>,
        name: &str,
        arguments: &str,
    ) -> ToolResult {
        self.execute_with_cancellation(mode, cwd, name, arguments, None)
    }

    pub fn execute_with_cancellation(
        &self,
        mode: OperatingMode,
        cwd: impl AsRef<Path>,
        name: &str,
        arguments: &str,
        cancellation: Option<&CancellationToken>,
    ) -> ToolResult {
        if !self.names_for_mode(mode).contains(&name) {
            return ToolResult {
                name: name.into(),
                success: false,
                output: format!(
                    "tool unavailable in {} mode",
                    crate::runtime::mode_name(mode)
                ),
                artifact: None,
            };
        }
        let args: Value = match serde_json::from_str(arguments) {
            Ok(args) => args,
            Err(error) => {
                return ToolResult {
                    name: name.into(),
                    success: false,
                    output: format!("invalid tool arguments: {error}"),
                    artifact: None,
                }
            }
        };
        let result = match name {
            "read" => self.execute_read(cwd.as_ref(), &args),
            "list" => self.execute_list(cwd.as_ref(), &args),
            "search" => self.execute_search(cwd.as_ref(), &args),
            "write" => self.execute_write(cwd.as_ref(), &args),
            "patch" => self.execute_patch(cwd.as_ref(), &args),
            "shell" => self.execute_shell(cwd.as_ref(), &args, cancellation),
            _ => Err(ToolError::InvalidInput {
                message: format!("unknown tool: {name}"),
            }),
        };
        match result {
            Ok(output) => ToolResult {
                name: name.into(),
                success: true,
                output,
                artifact: None,
            },
            Err(error) => ToolResult {
                name: name.into(),
                success: false,
                output: tool_error_message(error),
                artifact: None,
            },
        }
    }

    fn execute_read(&self, cwd: &Path, args: &Value) -> Result<String, ToolError> {
        let path = resolve_path(cwd, required_string(args, "path")?);
        let max_lines = args.get("max_lines").and_then(Value::as_u64).unwrap_or(200) as usize;
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(1) as usize;
        read_file_range(path, offset, max_lines)
    }

    fn execute_list(&self, cwd: &Path, args: &Value) -> Result<String, ToolError> {
        let path = resolve_path(cwd, args.get("path").and_then(Value::as_str).unwrap_or("."));
        Ok(list_directory(path)?
            .into_iter()
            .map(|entry| entry.display().to_string())
            .collect::<Vec<_>>()
            .join("\n"))
    }

    fn execute_search(&self, cwd: &Path, args: &Value) -> Result<String, ToolError> {
        let path = resolve_path(cwd, args.get("path").and_then(Value::as_str).unwrap_or("."));
        let query = required_string(args, "query")?;
        Ok(search_literal(path, query)?
            .into_iter()
            .map(|hit| format!("{}:{}: {}", hit.path.display(), hit.line, hit.text))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    fn execute_write(&self, cwd: &Path, args: &Value) -> Result<String, ToolError> {
        let path = resolve_path(cwd, required_string(args, "path")?);
        let content = required_string(args, "content")?;
        let precondition = args
            .get("expected")
            .and_then(Value::as_str)
            .map(|expected| FilePrecondition::ExactText(expected.into()));
        write_file(path, content, precondition)?;
        Ok("written".into())
    }

    fn execute_patch(&self, cwd: &Path, args: &Value) -> Result<String, ToolError> {
        let path = resolve_path(cwd, required_string(args, "path")?);
        let expected = required_string(args, "expected")?;
        let replacement = required_string(args, "replacement")?;
        apply_exact_patch(path, expected, replacement)?;
        Ok("patched".into())
    }

    fn execute_shell(
        &self,
        cwd: &Path,
        args: &Value,
        cancellation: Option<&CancellationToken>,
    ) -> Result<String, ToolError> {
        let command = required_string(args, "command")?;
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(30_000);
        let result = run_shell_timeout_cancellable(
            cwd,
            command,
            std::time::Duration::from_millis(timeout_ms),
            cancellation,
        )?;
        let stdout = String::from_utf8_lossy(&result.output.stdout);
        let stderr = String::from_utf8_lossy(&result.output.stderr);
        Ok(format!(
            "exit_code={:?} timed_out={} cancelled={}\nstdout:\n{}stderr:\n{}",
            result.output.status.code(),
            result.timed_out,
            result.cancelled,
            cap_shell_stream(&stdout),
            cap_shell_stream(&stderr),
        ))
    }
}

/// Maximum bytes of one stdout/stderr stream placed into model context.
/// Larger streams are truncated with an explicit marker; the full output is
/// still visible in the TUI event log.
const SHELL_STREAM_CAP_BYTES: usize = 8 * 1024;

fn cap_shell_stream(raw: &str) -> String {
    if raw.len() <= SHELL_STREAM_CAP_BYTES {
        return raw.to_owned();
    }
    let mut end = SHELL_STREAM_CAP_BYTES;
    while !raw.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated {} bytes]", &raw[..end], raw.len() - end)
}

fn tool_definition(name: &str) -> Value {
    let (description, properties, required) = match name {
        "read" => (
            "Read a UTF-8 text file",
            json!({"path": {"type": "string"}, "max_lines": {"type": "integer", "minimum": 1}, "offset": {"type": "integer", "minimum": 1}}),
            json!(["path"]),
        ),
        "list" => (
            "List directory entries",
            json!({"path": {"type": "string"}}),
            json!([]),
        ),
        "search" => (
            "Search files for literal text",
            json!({"path": {"type": "string"}, "query": {"type": "string"}}),
            json!(["query"]),
        ),
        "write" => (
            "Write a UTF-8 text file",
            json!({"path": {"type": "string"}, "content": {"type": "string"}, "expected": {"type": "string"}}),
            json!(["path", "content"]),
        ),
        "patch" => (
            "Replace one exact text occurrence in a file",
            json!({"path": {"type": "string"}, "expected": {"type": "string"}, "replacement": {"type": "string"}}),
            json!(["path", "expected", "replacement"]),
        ),
        "shell" => (
            "Run a shell command in the workspace",
            json!({"command": {"type": "string"}, "timeout_ms": {"type": "integer", "minimum": 1}}),
            json!(["command"]),
        ),
        _ => ("Slim tool", json!({}), json!([])),
    };
    json!({
        "name": name,
        "description": description,
        "input_schema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false
        }
    })
}

fn required_string<'a>(args: &'a Value, name: &str) -> Result<&'a str, ToolError> {
    args.get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ToolError::InvalidInput {
            message: format!("missing string argument: {name}"),
        })
}

fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn tool_error_message(error: ToolError) -> String {
    match error {
        ToolError::Io { message } => format!("io error: {message}"),
        ToolError::StaleRead { path } => format!("stale read: {path}"),
        ToolError::PreconditionRequired { path } => format!("precondition required: {path}"),
        ToolError::MatchCount { count } => format!("expected exactly one match, got {count}"),
        ToolError::InvalidInput { message } => message,
    }
}
