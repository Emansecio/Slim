use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::Deserialize;
use slim_core::context::CompactionPolicy;
use slim_core::mcp::McpExposure;

pub struct Config;

static CONFIG_WRITE_LOCK: Mutex<()> = Mutex::new(());
static CONFIG_TEMP_NONCE: AtomicU64 = AtomicU64::new(0);

impl Config {
    pub fn resolve<'a>(
        cli: Option<&'a str>,
        environment: Option<&'a str>,
        project: Option<&'a str>,
        global: Option<&'a str>,
    ) -> Option<&'a str> {
        cli.or(environment).or(project).or(global)
    }
}

/// Defaults loaded from TOML config files. Unknown keys are ignored so the
/// format stays forward-compatible.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize)]
pub struct FileConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    pub codex_fast: Option<bool>,
    #[serde(default)]
    pub max_mutating_tool_calls: Option<usize>,
    #[serde(default)]
    pub max_read_tool_calls: Option<usize>,
    #[serde(default)]
    pub max_total_tool_calls: Option<usize>,
    #[serde(default)]
    pub max_turns: Option<usize>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub max_result_bytes: Option<usize>,
    #[serde(default)]
    pub compaction: Option<FileCompactionConfig>,
    pub shell_jobs: Option<FileShellJobConfig>,
    #[serde(default)]
    pub lsp: Option<FileLspConfig>,
    #[serde(default)]
    pub mcp: Option<FileMcpConfig>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileShellJobConfig {
    pub max_running: Option<usize>,
    pub max_retained: Option<usize>,
    pub memory_bytes: Option<usize>,
    pub interrupt_grace_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize)]
pub struct FileLspConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub idle_shutdown_minutes: Option<u64>,
    #[serde(default)]
    pub max_servers: Option<usize>,
    #[serde(default)]
    pub request_timeout_ms: Option<u64>,
    #[serde(default)]
    pub max_open_documents: Option<usize>,
    #[serde(default)]
    pub servers: Option<BTreeMap<String, FileLspServerConfig>>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize)]
pub struct FileLspServerConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_optional_lsp_object")]
    pub initialization_options: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "deserialize_optional_lsp_object")]
    pub settings: Option<serde_json::Value>,
}

fn deserialize_optional_lsp_object<'de, D>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    if !value.is_object() {
        return Err(serde::de::Error::custom(
            "LSP configuration must be a table",
        ));
    }
    Ok(Some(value))
}

/// Merged LSP configuration with defaults applied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LspConfig {
    pub enabled: bool,
    pub idle_shutdown_minutes: u64,
    pub max_servers: usize,
    pub request_timeout_ms: u64,
    pub max_open_documents: usize,
    pub servers: BTreeMap<String, LspServerConfig>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LspServerConfig {
    pub enabled: bool,
    pub path: Option<String>,
    pub args: Option<Vec<String>>,
    pub initialization_options: Option<serde_json::Value>,
    pub settings: Option<serde_json::Value>,
    /// Values the workspace's `slim.toml` set. A project file can name any
    /// executable or launcher argument, so they apply only to a trusted
    /// workspace, like project MCP servers. A project `enabled = false` only
    /// restricts and is applied directly.
    pub project: LspProjectOverrides,
}

/// Launch-affecting LSP fields from the project layer; see
/// [`LspServerConfig::effective`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LspProjectOverrides {
    pub enabled: Option<bool>,
    pub path: Option<String>,
    pub args: Option<Vec<String>>,
    pub initialization_options: Option<serde_json::Value>,
    pub settings: Option<serde_json::Value>,
}

impl Default for LspServerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: None,
            args: None,
            initialization_options: None,
            settings: None,
            project: LspProjectOverrides::default(),
        }
    }
}

impl LspServerConfig {
    /// Whether the workspace's `slim.toml` changes how this server launches.
    pub fn has_project_overrides(&self) -> bool {
        self.project != LspProjectOverrides::default()
    }

    /// The configuration a manager may use: project overrides are layered on
    /// top only when the workspace is trusted.
    pub fn effective(&self, trusted: bool) -> LspServerConfig {
        let mut server = LspServerConfig {
            project: LspProjectOverrides::default(),
            ..self.clone()
        };
        if trusted {
            let project = &self.project;
            if let Some(enabled) = project.enabled {
                server.enabled = enabled;
            }
            if project.path.is_some() {
                server.path.clone_from(&project.path);
            }
            if project.args.is_some() {
                server.args.clone_from(&project.args);
            }
            if project.initialization_options.is_some() {
                server
                    .initialization_options
                    .clone_from(&project.initialization_options);
            }
            if project.settings.is_some() {
                server.settings.clone_from(&project.settings);
            }
        }
        server
    }
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            idle_shutdown_minutes: 15,
            max_servers: 4,
            request_timeout_ms: 30_000,
            max_open_documents: 64,
            servers: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize)]
pub struct FileMcpConfig {
    #[serde(default)]
    pub servers: Option<BTreeMap<String, FileMcpServerConfig>>,
    /// How long the first model request waits for direct-exposure servers
    /// that are still connecting (milliseconds).
    #[serde(default)]
    pub startup_wait_ms: Option<u64>,
}

/// One `[mcp.servers.<name>]` entry: stdio (`command`) or streamable HTTP
/// (`url`). Exactly one transport key must be set; enforced by validation.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize)]
pub struct FileMcpServerConfig {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub exposure: Option<McpExposure>,
    #[serde(default)]
    pub tool_exposure: Option<BTreeMap<String, McpExposure>>,
    #[serde(default)]
    pub lazy: Option<bool>,
    #[serde(default)]
    pub oauth: Option<FileMcpOAuthConfig>,
}

/// `[mcp.servers.<name>.oauth]`: pre-registered client and discovery
/// overrides for an HTTP server.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize)]
pub struct FileMcpOAuthConfig {
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub callback_port: Option<u16>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub client_name: Option<String>,
    #[serde(default)]
    pub auth_server_metadata_url: Option<String>,
}

/// Merged MCP configuration with defaults applied.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct McpConfig {
    pub servers: BTreeMap<String, McpServerConfig>,
    /// `[mcp] startup_wait_ms`; `None` keeps the default (10 s).
    pub startup_wait_ms: Option<u64>,
}

/// Which layer defines a merged server entry. Entries touched by the project
/// `slim.toml` are not started until the workspace is trusted.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum McpOrigin {
    #[default]
    Global,
    Project,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpServerConfig {
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub enabled: bool,
    pub timeout_ms: u64,
    pub description: Option<String>,
    pub exposure: McpExposure,
    pub tool_exposure: BTreeMap<String, McpExposure>,
    pub lazy: bool,
    pub oauth: Option<McpOAuthConfig>,
    pub origin: McpOrigin,
}

/// Merged OAuth settings; `client_secret` is the raw (uninterpolated) value.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct McpOAuthConfig {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub callback_port: Option<u16>,
    pub scope: Option<String>,
    pub client_name: Option<String>,
    pub auth_server_metadata_url: Option<String>,
}

impl std::fmt::Debug for McpOAuthConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpOAuthConfig")
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .field("callback_port", &self.callback_port)
            .field("scope", &self.scope)
            .field("client_name", &self.client_name)
            .field("auth_server_metadata_url", &self.auth_server_metadata_url)
            .finish()
    }
}

/// Default per-request timeout; progress notifications renew it.
pub const DEFAULT_MCP_TIMEOUT_MS: u64 = 60_000;
const MAX_MCP_DESCRIPTION_BYTES: usize = 2048;

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            url: None,
            headers: BTreeMap::new(),
            enabled: true,
            timeout_ms: DEFAULT_MCP_TIMEOUT_MS,
            description: None,
            exposure: McpExposure::Gateway,
            tool_exposure: BTreeMap::new(),
            lazy: false,
            oauth: None,
            origin: McpOrigin::Global,
        }
    }
}

/// `https://...`, or `http://` on a loopback host (`localhost`, `127.0.0.1`,
/// `[::1]`) - the only endpoints allowed to carry OAuth metadata.
fn https_or_loopback(url: &str) -> bool {
    if url.starts_with("https://") {
        return true;
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    ["localhost", "127.0.0.1", "[::1]"].iter().any(|host| {
        rest.strip_prefix(host)
            .is_some_and(|tail| tail.is_empty() || tail.starts_with([':', '/', '?', '#']))
    })
}

/// Largest accepted `[mcp] startup_wait_ms`.
const MAX_MCP_STARTUP_WAIT_MS: u64 = 120_000;

impl McpConfig {
    /// Time the first model request waits for direct-exposure servers that
    /// are still connecting in the background.
    pub fn startup_wait(&self) -> std::time::Duration {
        self.startup_wait_ms.map_or(
            slim_core::mcp::DEFAULT_MCP_STARTUP_WAIT,
            std::time::Duration::from_millis,
        )
    }

    /// Fails loud on ambiguous server entries so a typo cannot silently
    /// disable or reroute a configured server.
    pub fn validate(&self) -> Result<(), String> {
        if self
            .startup_wait_ms
            .is_some_and(|wait| wait > MAX_MCP_STARTUP_WAIT_MS)
        {
            return Err(format!(
                "mcp.startup_wait_ms must be at most {MAX_MCP_STARTUP_WAIT_MS}"
            ));
        }
        let mut normalized_names = BTreeMap::new();
        for (name, server) in &self.servers {
            if name.is_empty()
                || name.len() > 64
                || !name
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
            {
                return Err(format!(
                    "mcp.servers.{name}: name must match ^[A-Za-z0-9_-]{{1,64}}$"
                ));
            }
            // Tool names (`mcp__<server>__<tool>`) fold `-` into `_`, so two
            // servers differing only there would be indistinguishable.
            if let Some(other) = normalized_names.insert(name.replace('-', "_"), name) {
                return Err(format!(
                    "mcp.servers.{name}: name collides with mcp.servers.{other} (names that differ only in '-' and '_' are the same server)"
                ));
            }
            match (server.command.is_some(), server.url.is_some()) {
                (true, true) => {
                    return Err(format!(
                        "mcp.servers.{name}: set either command (stdio) or url (http), not both"
                    ))
                }
                (false, false) => {
                    return Err(format!(
                        "mcp.servers.{name}: missing command (stdio) or url (http)"
                    ))
                }
                _ => {}
            }
            if let Some(command) = &server.command {
                if command.trim().is_empty() {
                    return Err(format!("mcp.servers.{name}: command must not be empty"));
                }
            }
            if let Some(url) = &server.url {
                if !url.starts_with("http://") && !url.starts_with("https://") {
                    return Err(format!(
                        "mcp.servers.{name}: url must start with http:// or https://"
                    ));
                }
            }
            if let Some(cwd) = &server.cwd {
                if server.command.is_none() {
                    return Err(format!(
                        "mcp.servers.{name}: cwd applies to stdio servers (command) only"
                    ));
                }
                if cwd.trim().is_empty() || cwd.contains('\0') {
                    return Err(format!("mcp.servers.{name}: cwd must be a non-empty path"));
                }
            }
            if let Some(description) = &server.description {
                if description.trim().is_empty() {
                    return Err(format!("mcp.servers.{name}: description must not be empty"));
                }
                if description.len() > MAX_MCP_DESCRIPTION_BYTES {
                    return Err(format!(
                        "mcp.servers.{name}: description must be at most {MAX_MCP_DESCRIPTION_BYTES} bytes"
                    ));
                }
            }
            if server.tool_exposure.keys().any(String::is_empty) {
                return Err(format!(
                    "mcp.servers.{name}: tool_exposure keys must not be empty"
                ));
            }
            if let Some(oauth) = &server.oauth {
                if server.url.is_none() {
                    return Err(format!(
                        "mcp.servers.{name}: oauth applies to HTTP servers (url) only"
                    ));
                }
                if oauth.client_id.as_deref().is_some_and(str::is_empty) {
                    return Err(format!(
                        "mcp.servers.{name}: oauth.client_id must not be empty"
                    ));
                }
                if oauth.client_secret.is_some() && oauth.client_id.is_none() {
                    return Err(format!(
                        "mcp.servers.{name}: oauth.client_secret requires oauth.client_id"
                    ));
                }
                if oauth.callback_port == Some(0) {
                    return Err(format!(
                        "mcp.servers.{name}: oauth.callback_port must be between 1 and 65535"
                    ));
                }
                if let Some(url) = &oauth.auth_server_metadata_url {
                    if !https_or_loopback(url) {
                        return Err(format!(
                            "mcp.servers.{name}: oauth.auth_server_metadata_url must use https (http only on localhost, 127.0.0.1, or [::1])"
                        ));
                    }
                }
            }
            if !(1_000..=600_000).contains(&server.timeout_ms) {
                return Err(format!(
                    "mcp.servers.{name}: timeout_ms must be between 1000 and 600000"
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize)]
pub struct FileCompactionConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub reserve_tokens: Option<u64>,
    #[serde(default)]
    pub keep_recent_tokens: Option<u64>,
    #[serde(default)]
    pub summary_max_bytes: Option<usize>,
    #[serde(default)]
    pub manual_instructions_max_bytes: Option<usize>,
}

/// Merged view of every config layer (project wins over global).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LayeredConfig {
    pub model: Option<String>,
    pub endpoint: Option<String>,
    pub effort: Option<String>,
    pub codex_fast: Option<bool>,
    pub max_mutating_tool_calls: Option<usize>,
    pub max_read_tool_calls: Option<usize>,
    pub max_total_tool_calls: Option<usize>,
    pub max_turns: Option<usize>,
    pub max_output_tokens: Option<u32>,
    pub timeout_secs: Option<u64>,
    pub max_result_bytes: Option<usize>,
    pub compaction: FileCompactionConfig,
    pub shell_jobs: slim_core::runtime::ShellJobLimits,
    pub lsp: LspConfig,
    pub mcp: McpConfig,
}

impl LayeredConfig {
    pub fn compaction_policy(&self) -> Result<CompactionPolicy, String> {
        let mut policy = CompactionPolicy::default();
        if let Some(value) = self.compaction.enabled {
            policy.enabled = value;
        }
        if let Some(value) = self.compaction.reserve_tokens {
            if value == 0 {
                return Err("compaction.reserve_tokens must be positive".into());
            }
            policy.reserve_tokens = value;
        }
        if let Some(value) = self.compaction.keep_recent_tokens {
            if value == 0 {
                return Err("compaction.keep_recent_tokens must be positive".into());
            }
            policy.keep_recent_tokens = value;
        }
        if let Some(value) = self.compaction.summary_max_bytes {
            if value == 0 || value > 64 * 1024 {
                return Err("compaction.summary_max_bytes must be between 1 and 65536".into());
            }
            policy.summary_max_bytes = value;
        }
        if let Some(value) = self.compaction.manual_instructions_max_bytes {
            if value == 0 || value > 4 * 1024 {
                return Err(
                    "compaction.manual_instructions_max_bytes must be between 1 and 4096".into(),
                );
            }
            policy.manual_instructions_max_bytes = value;
        }
        Ok(policy)
    }
}

/// Project-level file looked up relative to the working directory.
pub const PROJECT_CONFIG_FILE: &str = "slim.toml";
const MAX_CONFIG_BYTES: usize = 1024 * 1024;

fn read_config(path: &Path) -> std::io::Result<String> {
    let file = open_config_read(path)?;
    let mut contents = String::new();
    file.take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_string(&mut contents)?;
    if contents.len() > MAX_CONFIG_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("config exceeds the {MAX_CONFIG_BYTES}-byte safety limit"),
        ));
    }
    Ok(contents)
}

#[cfg(windows)]
fn open_config_read(path: &Path) -> std::io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)
}

#[cfg(not(windows))]
fn open_config_read(path: &Path) -> std::io::Result<fs::File> {
    fs::File::open(path)
}

impl FileConfig {
    pub fn parse(contents: &str) -> Result<Self, String> {
        toml::from_str(contents).map_err(|error| error.to_string())
    }

    /// Missing file yields `Ok(None)`; present-but-invalid surfaces an error
    /// carrying the path so callers can point at the offending file.
    pub fn load(path: &Path) -> Result<Option<Self>, String> {
        match read_config(path) {
            Ok(contents) => Self::parse(&contents)
                .map(Some)
                .map_err(|error| format!("{}: {error}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }
}

/// OS config directory for Slim (e.g. `%APPDATA%\slim\config` on Windows).
pub fn global_config_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("SLIM_CONFIG_FILE").filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }
    #[cfg(test)]
    {
        Some(std::env::temp_dir().join(format!(
            "slim-test-global-config-{}.toml",
            std::process::id()
        )))
    }
    #[cfg(not(test))]
    {
        directories::ProjectDirs::from("", "", "slim")
            .map(|dirs| dirs.config_dir().join(PROJECT_CONFIG_FILE))
    }
}

/// Persists the selected model and effort into the global config TOML
/// (`%APPDATA%\slim\config\slim.toml`), preserving unknown keys (endpoint, future
/// fields) by editing a parsed table instead of rewriting from scratch.
/// Returns the path written on success so callers can surface it in toasts.
pub fn save_global_model(
    model: &str,
    effort: &str,
    codex_fast: Option<bool>,
) -> Result<PathBuf, String> {
    let path = global_config_path()
        .ok_or_else(|| "unable to resolve the global config directory".to_owned())?;
    save_global_model_to(&path, model, effort, codex_fast)?;
    Ok(path)
}

/// Writes `model`/`effort` into the TOML file at `path`, preserving unknown
/// keys. Exposed for hermetic tests; production uses [`save_global_model`].
fn save_global_model_to(
    path: &Path,
    model: &str,
    effort: &str,
    codex_fast: Option<bool>,
) -> Result<(), String> {
    let _write_guard = CONFIG_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    let existing = match read_config(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let mut table: toml::Table = existing
        .parse()
        .map_err(|error| format!("{}: {error}", path.display()))?;
    table.insert("model".into(), model.into());
    table.insert("effort".into(), effort.into());
    if let Some(fast) = codex_fast {
        table.insert("codex_fast".into(), fast.into());
    }
    let serialized =
        toml::to_string(&table).map_err(|error| format!("{}: {error}", path.display()))?;
    write_config_atomic(path, serialized.as_bytes())
}

/// Path of the project config layer for `workspace`.
pub fn project_config_path(workspace: &Path) -> PathBuf {
    workspace.join(PROJECT_CONFIG_FILE)
}

fn toml_string_table(entries: &BTreeMap<String, String>) -> toml::Table {
    entries
        .iter()
        .map(|(key, value)| (key.clone(), toml::Value::String(value.clone())))
        .collect()
}

/// Updates `fields[key]` (a table) key by key, creating it when missing or
/// when the existing value is not a table.
fn merge_into_table(fields: &mut toml::Table, key: &str, update: toml::Table) {
    let slot = fields
        .entry(key)
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if !slot.is_table() {
        *slot = toml::Value::Table(toml::Table::new());
    }
    if let Some(table) = slot.as_table_mut() {
        table.extend(update);
    }
}

/// Writes `[mcp.servers.<name>]` in the TOML at `path`, preserving all other
/// keys. An existing entry is merged, not replaced: only fields set on
/// `server` change; `env`, `headers`, `tool_exposure` and `oauth` update key
/// by key. A new `command` replaces the old one's `args` and `cwd`. Choosing
/// one transport removes the other transport's keys so the entry stays valid.
pub fn upsert_mcp_server_to(
    path: &Path,
    name: &str,
    server: &FileMcpServerConfig,
) -> Result<(), String> {
    write_mcp_server(path, name, server, false)
}

/// Replaces `[mcp.servers.<name>]` outright (no merge with an existing
/// entry); used by `import --force`.
pub fn replace_mcp_server_to(
    path: &Path,
    name: &str,
    server: &FileMcpServerConfig,
) -> Result<(), String> {
    write_mcp_server(path, name, server, true)
}

/// Whether the TOML at `path` already defines `[mcp.servers.<name>]`.
pub fn mcp_server_defined_in(path: &Path, name: &str) -> Result<bool, String> {
    let _write_guard = CONFIG_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut table = read_config_table_locked(path)?;
    Ok(mcp_servers_table(&mut table).contains_key(name))
}

fn write_mcp_server(
    path: &Path,
    name: &str,
    server: &FileMcpServerConfig,
    replace: bool,
) -> Result<(), String> {
    if server.command.is_some() && server.url.is_some() {
        return Err(format!(
            "mcp.servers.{name}: set either command (stdio) or url (http), not both"
        ));
    }
    let _write_guard = CONFIG_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut table = read_config_table_locked(path)?;
    let entry = mcp_servers_table(&mut table);
    let mut fields = match entry.remove(name) {
        Some(toml::Value::Table(existing)) if !replace => existing,
        _ => toml::Table::new(),
    };
    if server.command.is_some() {
        // A new command brings its own arguments and working directory: the
        // old ones belong to the command it replaces.
        for key in ["url", "headers", "oauth", "args", "cwd"] {
            fields.remove(key);
        }
    }
    if server.url.is_some() {
        for key in ["command", "args", "env", "cwd"] {
            fields.remove(key);
        }
    }
    if let Some(command) = &server.command {
        fields.insert("command".into(), command.clone().into());
    }
    if let Some(args) = &server.args {
        fields.insert(
            "args".into(),
            toml::Value::Array(args.iter().cloned().map(toml::Value::String).collect()),
        );
    }
    if let Some(env) = &server.env {
        merge_into_table(&mut fields, "env", toml_string_table(env));
    }
    if let Some(cwd) = &server.cwd {
        fields.insert("cwd".into(), cwd.clone().into());
    }
    if let Some(url) = &server.url {
        fields.insert("url".into(), url.clone().into());
    }
    if let Some(headers) = &server.headers {
        merge_into_table(&mut fields, "headers", toml_string_table(headers));
    }
    if let Some(enabled) = server.enabled {
        fields.insert("enabled".into(), enabled.into());
    }
    if let Some(timeout_ms) = server.timeout_ms {
        // TOML integers are i64; an unchecked `as` cast wraps huge values to
        // negatives and writes a file the next load cannot parse.
        fields.insert(
            "timeout_ms".into(),
            i64::try_from(timeout_ms).unwrap_or(i64::MAX).into(),
        );
    }
    if let Some(description) = &server.description {
        fields.insert("description".into(), description.clone().into());
    }
    if let Some(exposure) = server.exposure {
        fields.insert("exposure".into(), exposure.as_str().into());
    }
    if let Some(tool_exposure) = &server.tool_exposure {
        let update = tool_exposure
            .iter()
            .map(|(tool, exposure)| (tool.clone(), toml::Value::from(exposure.as_str())))
            .collect();
        merge_into_table(&mut fields, "tool_exposure", update);
    }
    if let Some(lazy) = server.lazy {
        fields.insert("lazy".into(), lazy.into());
    }
    if let Some(oauth) = &server.oauth {
        let mut update = toml::Table::new();
        for (key, value) in [
            ("client_id", &oauth.client_id),
            ("client_secret", &oauth.client_secret),
            ("scope", &oauth.scope),
            ("client_name", &oauth.client_name),
            ("auth_server_metadata_url", &oauth.auth_server_metadata_url),
        ] {
            if let Some(value) = value {
                update.insert(key.into(), value.clone().into());
            }
        }
        if let Some(port) = oauth.callback_port {
            update.insert("callback_port".into(), i64::from(port).into());
        }
        merge_into_table(&mut fields, "oauth", update);
    }
    entry.insert(name.to_owned(), toml::Value::Table(fields));
    write_config_table_locked(path, &table)
}

/// Deletes `[mcp.servers.<name>]` from the first layer that defines it
/// (the workspace's project file first, then global). Returns the edited
/// path, or `None` when the server was not configured anywhere.
pub fn remove_mcp_server(workspace: &Path, name: &str) -> Result<Option<PathBuf>, String> {
    let _write_guard = CONFIG_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut paths = vec![project_config_path(workspace)];
    if let Some(global) = global_config_path() {
        paths.push(global);
    }
    for path in paths {
        let mut table = match read_config_table_locked(&path) {
            Ok(table) => table,
            Err(_) if !path.exists() => continue,
            Err(error) => return Err(error),
        };
        if mcp_servers_table(&mut table).remove(name).is_some() {
            write_config_table_locked(&path, &table)?;
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// Deletes `[mcp.servers.<name>]` from the TOML at `path` only. `true` when it
/// was there.
pub fn remove_mcp_server_from(path: &Path, name: &str) -> Result<bool, String> {
    let _write_guard = CONFIG_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut table = read_config_table_locked(path)?;
    if mcp_servers_table(&mut table).remove(name).is_none() {
        return Ok(false);
    }
    write_config_table_locked(path, &table)?;
    Ok(true)
}

/// Undoes a project "disable-only" override: when `[mcp.servers.<name>]` in
/// the TOML at `path` holds nothing but `enabled = false`, the entry is
/// deleted (the server is then governed by the file that really defines it)
/// and `true` is returned. Any other entry is left alone and gives `false`.
/// Writing `enabled = true` instead would turn the override into a project
/// definition that needs the workspace to be trusted.
pub fn clear_mcp_disable_override_to(path: &Path, name: &str) -> Result<bool, String> {
    let _write_guard = CONFIG_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut table = read_config_table_locked(path)?;
    let servers = mcp_servers_table(&mut table);
    let only_disabled = servers
        .get(name)
        .and_then(toml::Value::as_table)
        .is_some_and(|entry| {
            entry.len() == 1 && entry.get("enabled").and_then(toml::Value::as_bool) == Some(false)
        });
    if !only_disabled {
        return Ok(false);
    }
    servers.remove(name);
    write_config_table_locked(path, &table)?;
    Ok(true)
}

fn read_config_table_locked(path: &Path) -> Result<toml::Table, String> {
    match read_config(path) {
        Ok(contents) => contents
            .parse()
            .map_err(|error| format!("{}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(toml::Table::new()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn mcp_servers_table(table: &mut toml::Table) -> &mut toml::Table {
    let mcp = table
        .entry("mcp")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if !mcp.is_table() {
        *mcp = toml::Value::Table(toml::Table::new());
    }
    let servers = mcp
        .as_table_mut()
        .expect("mcp table")
        .entry("servers")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if !servers.is_table() {
        *servers = toml::Value::Table(toml::Table::new());
    }
    servers.as_table_mut().expect("servers table")
}

fn write_config_table_locked(path: &Path, table: &toml::Table) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    let serialized =
        toml::to_string(table).map_err(|error| format!("{}: {error}", path.display()))?;
    write_config_atomic(path, serialized.as_bytes())
}

fn write_config_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{}: missing parent directory", path.display()))?;
    let temporary = parent.join(format!(
        ".slim-config-{}-{}.tmp",
        std::process::id(),
        CONFIG_TEMP_NONCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| format!("{}: {error}", temporary.display()))?;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|error| format!("{}: {error}", temporary.display()))?;
        drop(file);
        replace_config_file(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(windows)]
fn replace_config_file(source: &Path, destination: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source_wide = source
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let destination_wide = destination
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let deadline = Instant::now() + Duration::from_millis(250);
    loop {
        let success = unsafe {
            MoveFileExW(
                source_wide.as_ptr(),
                destination_wide.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if success != 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(5 | 32 | 33)) || Instant::now() >= deadline {
            return Err(format!("{}: {error}", destination.display()));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(not(windows))]
fn replace_config_file(source: &Path, destination: &Path) -> Result<(), String> {
    fs::rename(source, destination).map_err(|error| format!("{}: {error}", destination.display()))
}

/// Loads global first, then the project `slim.toml` in the current directory.
/// Later layers override keys they set and fill remaining gaps (`project`
/// wins over `global`).
pub fn load_layered() -> Result<LayeredConfig, String> {
    load_layered_with_project(PathBuf::from(PROJECT_CONFIG_FILE))
}

/// Like [`load_layered`] but reads the project layer from `workspace`, the
/// root MCP servers and sessions actually run in. It differs from the process
/// directory when a session is resumed from its recorded workspace.
pub fn load_layered_for(workspace: &Path) -> Result<LayeredConfig, String> {
    load_layered_with_project(project_config_path(workspace))
}

fn load_layered_with_project(project: PathBuf) -> Result<LayeredConfig, String> {
    let mut layers = Vec::new();
    if let Some(global) = global_config_path() {
        layers.push((global, McpOrigin::Global));
    }
    layers.push((project, McpOrigin::Project));
    let mut config = load_layers(layers)?;
    // A file value is a default, not an explicit ProviderRunOptions override.
    // Leave env-backed values unset so the shared resolvers validate them;
    // invalid/empty environment settings must not silently fall back to TOML.
    config.max_mutating_tool_calls = file_default(
        config.max_mutating_tool_calls,
        "SLIM_MAX_MUTATING_TOOL_CALLS",
    );
    config.max_read_tool_calls =
        file_default(config.max_read_tool_calls, "SLIM_MAX_READ_TOOL_CALLS");
    config.max_total_tool_calls =
        file_default(config.max_total_tool_calls, "SLIM_MAX_TOTAL_TOOL_CALLS");
    config.max_turns = file_default(config.max_turns, "SLIM_MAX_TURNS");
    config.max_output_tokens = file_default(config.max_output_tokens, "SLIM_MAX_OUTPUT_TOKENS");
    config.timeout_secs = file_default(config.timeout_secs, "SLIM_TIMEOUT_SECS");
    config.max_result_bytes = file_default(config.max_result_bytes, "SLIM_MAX_RESULT_BYTES");
    config.mcp.validate()?;
    Ok(config)
}

fn file_default<T>(value: Option<T>, env_key: &str) -> Option<T> {
    value.filter(|_| std::env::var_os(env_key).is_none())
}

/// Layered load where every path is a global-origin layer (hermetic tests).
#[cfg(test)]
pub(crate) fn load_layered_from(
    paths: impl IntoIterator<Item = PathBuf>,
) -> Result<LayeredConfig, String> {
    load_layers(paths.into_iter().map(|path| (path, McpOrigin::Global)))
}

fn load_layers(
    layers: impl IntoIterator<Item = (PathBuf, McpOrigin)>,
) -> Result<LayeredConfig, String> {
    let mut layered = LayeredConfig::default();
    for (path, origin) in layers {
        if let Some(config) = FileConfig::load(&path)? {
            // Per-layer rejection: one file setting both transports is a
            // typo; merging then can no longer detect it (a layer's `command`
            // legitimately clears an inherited `url` and vice versa).
            if let Some(servers) = config.mcp.as_ref().and_then(|mcp| mcp.servers.as_ref()) {
                for (name, server) in servers {
                    if let Some(error) = layer_server_error(name, server) {
                        return Err(error);
                    }
                }
            }
            merge_layer_as(&mut layered, config, origin);
        }
    }
    Ok(layered)
}

/// Per-layer rule for one server entry (see [`load_layers`]).
fn layer_server_error(name: &str, server: &FileMcpServerConfig) -> Option<String> {
    if server.command.is_some() && server.url.is_some() {
        return Some(format!(
            "mcp.servers.{name}: set either command (stdio) or url (http), not both"
        ));
    }
    if server.url.is_some() && server.cwd.is_some() {
        return Some(format!(
            "mcp.servers.{name}: cwd applies to stdio servers (command) only"
        ));
    }
    if server.command.is_some() && server.oauth.is_some() {
        return Some(format!(
            "mcp.servers.{name}: oauth applies to HTTP servers (url) only"
        ));
    }
    None
}

/// Like [`load_layered_for`] for the MCP CLI: one invalid server entry does not
/// fail the load. Invalid entries are dropped from the result and returned as
/// `(name, error)` so `slim mcp list` can report them next to the good ones.
/// A file that cannot be read or parsed is still an error.
pub(crate) fn load_layered_lenient_for(
    workspace: &Path,
) -> Result<(LayeredConfig, Vec<(String, String)>), String> {
    let mut layers = Vec::new();
    if let Some(global) = global_config_path() {
        layers.push((global, McpOrigin::Global));
    }
    layers.push((project_config_path(workspace), McpOrigin::Project));
    let mut layered = LayeredConfig::default();
    let mut invalid: BTreeMap<String, String> = BTreeMap::new();
    for (path, origin) in layers {
        let Some(mut config) = FileConfig::load(&path)? else {
            continue;
        };
        if let Some(servers) = config.mcp.as_mut().and_then(|mcp| mcp.servers.as_mut()) {
            servers.retain(|name, server| match layer_server_error(name, server) {
                Some(error) => {
                    invalid.entry(name.clone()).or_insert(error);
                    false
                }
                None => true,
            });
        }
        merge_layer_as(&mut layered, config, origin);
    }
    // An entry invalid in one layer is invalid as a whole: showing the other
    // layer's version would hide the error.
    layered
        .mcp
        .servers
        .retain(|name, _| !invalid.contains_key(name));
    // Merged entries are checked one at a time, then added together so a name
    // collision blames the later name only.
    let candidates = std::mem::take(&mut layered.mcp.servers);
    let mut accepted = McpConfig {
        startup_wait_ms: layered.mcp.startup_wait_ms,
        ..McpConfig::default()
    };
    accepted.validate()?;
    for (name, server) in candidates {
        accepted.servers.insert(name.clone(), server);
        if let Err(error) = accepted.validate() {
            accepted.servers.remove(&name);
            invalid.insert(name, error);
        }
    }
    layered.mcp = accepted;
    Ok((layered, invalid.into_iter().collect()))
}

/// A project entry that only turns a server off cannot start anything, so it
/// does not need the workspace to be trusted.
fn is_disable_only(server: &FileMcpServerConfig) -> bool {
    server.enabled == Some(false)
        && FileMcpServerConfig {
            enabled: None,
            ..server.clone()
        } == FileMcpServerConfig::default()
}

pub(crate) fn merge_layer(target: &mut LayeredConfig, source: FileConfig) {
    merge_layer_as(target, source, McpOrigin::Global);
}

pub(crate) fn merge_layer_as(target: &mut LayeredConfig, source: FileConfig, origin: McpOrigin) {
    if source.model.is_some() {
        target.model = source.model;
    }
    if source.endpoint.is_some() {
        target.endpoint = source.endpoint;
    }
    if source.effort.is_some() {
        target.effort = source.effort;
    }
    if source.codex_fast.is_some() {
        target.codex_fast = source.codex_fast;
    }
    if source.max_mutating_tool_calls.is_some() {
        target.max_mutating_tool_calls = source.max_mutating_tool_calls;
    }
    if source.max_read_tool_calls.is_some() {
        target.max_read_tool_calls = source.max_read_tool_calls;
    }
    if source.max_total_tool_calls.is_some() {
        target.max_total_tool_calls = source.max_total_tool_calls;
    }
    if source.max_turns.is_some() {
        target.max_turns = source.max_turns;
    }
    if source.max_output_tokens.is_some() {
        target.max_output_tokens = source.max_output_tokens;
    }
    if source.timeout_secs.is_some() {
        target.timeout_secs = source.timeout_secs;
    }
    if source.max_result_bytes.is_some() {
        target.max_result_bytes = source.max_result_bytes;
    }
    if let Some(jobs) = source.shell_jobs {
        if let Some(v) = jobs.max_running {
            target.shell_jobs.max_running = v;
        }
        if let Some(v) = jobs.max_retained {
            target.shell_jobs.max_retained = v;
        }
        if let Some(v) = jobs.memory_bytes {
            target.shell_jobs.memory_bytes = v;
        }
        if let Some(v) = jobs.interrupt_grace_ms {
            target.shell_jobs.interrupt_grace_ms = v;
        }
    }
    if let Some(lsp) = source.lsp {
        if let Some(enabled) = lsp.enabled {
            target.lsp.enabled = enabled;
        }
        if let Some(idle) = lsp.idle_shutdown_minutes {
            target.lsp.idle_shutdown_minutes = idle;
        }
        if let Some(max_servers) = lsp.max_servers {
            target.lsp.max_servers = max_servers;
        }
        if let Some(timeout) = lsp.request_timeout_ms {
            target.lsp.request_timeout_ms = timeout;
        }
        if let Some(max_open_documents) = lsp.max_open_documents {
            target.lsp.max_open_documents = max_open_documents;
        }
        if let Some(servers) = lsp.servers {
            for (name, server) in servers {
                let entry = target.lsp.servers.entry(name).or_default();
                if origin == McpOrigin::Project {
                    // Disabling only restricts; everything else waits for trust.
                    match server.enabled {
                        Some(false) => {
                            entry.enabled = false;
                            entry.project.enabled = None;
                        }
                        Some(true) => entry.project.enabled = Some(true),
                        None => {}
                    }
                    if server.path.is_some() {
                        entry.project.path = server.path;
                    }
                    if server.args.is_some() {
                        entry.project.args = server.args;
                    }
                    if server.initialization_options.is_some() {
                        entry.project.initialization_options = server.initialization_options;
                    }
                    if server.settings.is_some() {
                        entry.project.settings = server.settings;
                    }
                    continue;
                }
                if let Some(enabled) = server.enabled {
                    entry.enabled = enabled;
                }
                if let Some(path) = server.path {
                    entry.path = Some(path);
                }
                if let Some(args) = server.args {
                    entry.args = Some(args);
                }
                if let Some(options) = server.initialization_options {
                    entry.initialization_options = Some(options);
                }
                if let Some(settings) = server.settings {
                    entry.settings = Some(settings);
                }
            }
        }
    }
    if let Some(mcp) = source.mcp {
        if let Some(wait) = mcp.startup_wait_ms {
            target.mcp.startup_wait_ms = Some(wait);
        }
        if let Some(servers) = mcp.servers {
            for (name, server) in servers {
                let touches_beyond_disable = !is_disable_only(&server);
                let entry = target.mcp.servers.entry(name).or_default();
                // The project layer owns an entry once it sets anything but
                // `enabled = false` (which can only reduce what runs).
                if origin == McpOrigin::Project && touches_beyond_disable {
                    entry.origin = McpOrigin::Project;
                }
                // A layer that picks one transport clears the other's keys so
                // `command`+`url` never coexist in the merged entry.
                if let Some(command) = server.command {
                    entry.command = Some(command);
                    entry.url = None;
                    entry.headers.clear();
                    entry.oauth = None;
                }
                if let Some(args) = server.args {
                    entry.args = args;
                }
                if let Some(env) = server.env {
                    entry.env.extend(env);
                }
                if let Some(url) = server.url {
                    entry.url = Some(url);
                    entry.command = None;
                    entry.args.clear();
                    entry.env.clear();
                    entry.cwd = None;
                }
                if let Some(headers) = server.headers {
                    entry.headers.extend(headers);
                }
                if let Some(cwd) = server.cwd {
                    entry.cwd = Some(cwd);
                }
                if let Some(enabled) = server.enabled {
                    entry.enabled = enabled;
                }
                if let Some(timeout_ms) = server.timeout_ms {
                    entry.timeout_ms = timeout_ms;
                }
                if let Some(description) = server.description {
                    entry.description = Some(description);
                }
                if let Some(exposure) = server.exposure {
                    entry.exposure = exposure;
                }
                if let Some(tool_exposure) = server.tool_exposure {
                    entry.tool_exposure.extend(tool_exposure);
                }
                if let Some(lazy) = server.lazy {
                    entry.lazy = lazy;
                }
                if let Some(oauth) = server.oauth {
                    let merged = entry.oauth.get_or_insert_with(McpOAuthConfig::default);
                    if oauth.client_id.is_some() {
                        merged.client_id = oauth.client_id;
                    }
                    if oauth.client_secret.is_some() {
                        merged.client_secret = oauth.client_secret;
                    }
                    if oauth.callback_port.is_some() {
                        merged.callback_port = oauth.callback_port;
                    }
                    if oauth.scope.is_some() {
                        merged.scope = oauth.scope;
                    }
                    if oauth.client_name.is_some() {
                        merged.client_name = oauth.client_name;
                    }
                    if oauth.auth_server_metadata_url.is_some() {
                        merged.auth_server_metadata_url = oauth.auth_server_metadata_url;
                    }
                }
            }
        }
    }
    if let Some(compaction) = source.compaction {
        if compaction.enabled.is_some() {
            target.compaction.enabled = compaction.enabled;
        }
        if compaction.reserve_tokens.is_some() {
            target.compaction.reserve_tokens = compaction.reserve_tokens;
        }
        if compaction.keep_recent_tokens.is_some() {
            target.compaction.keep_recent_tokens = compaction.keep_recent_tokens;
        }
        if compaction.summary_max_bytes.is_some() {
            target.compaction.summary_max_bytes = compaction.summary_max_bytes;
        }
        if compaction.manual_instructions_max_bytes.is_some() {
            target.compaction.manual_instructions_max_bytes =
                compaction.manual_instructions_max_bytes;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_tool_budget_keys() {
        let config = FileConfig::parse(
            "max_mutating_tool_calls = 48\nmax_read_tool_calls = 120\nmax_turns = 64\nmax_output_tokens = 8192\ntimeout_secs = 180\nmax_result_bytes = 32768\n",
        )
        .expect("valid config parses");
        assert_eq!(config.max_mutating_tool_calls, Some(48));
        assert_eq!(config.max_read_tool_calls, Some(120));
        assert_eq!(config.max_turns, Some(64));
        assert_eq!(config.max_output_tokens, Some(8192));
        assert_eq!(config.timeout_secs, Some(180));
        assert_eq!(config.max_result_bytes, Some(32768));
    }

    #[test]
    fn removed_compaction_strategy_key_is_ignored_by_the_parser() {
        let config =
            FileConfig::parse("[compaction]\nstrategy = \"summary\"\nkeep_recent_tokens = 1000\n")
                .expect("legacy strategy key still parses");
        let mut layered = LayeredConfig::default();
        merge_layer(&mut layered, config);
        let policy = layered.compaction_policy().expect("policy");
        assert_eq!(policy.keep_recent_tokens, 1000);
    }

    #[test]
    fn merge_later_layer_overrides_max_turns() {
        let mut layered = LayeredConfig {
            max_turns: Some(8),
            ..LayeredConfig::default()
        };
        merge_layer(
            &mut layered,
            FileConfig {
                max_turns: Some(64),
                ..FileConfig::default()
            },
        );
        assert_eq!(layered.max_turns, Some(64));
    }

    #[test]
    fn parse_and_merge_nested_compaction_policy_field_by_field() {
        let global = FileConfig::parse(
            "[compaction]\nenabled = true\nreserve_tokens = 20000\nkeep_recent_tokens = 12000\n",
        )
        .expect("global config");
        let project =
            FileConfig::parse("[compaction]\nsummary_max_bytes = 32768\n").expect("project config");
        let mut layered = LayeredConfig::default();
        merge_layer(&mut layered, global);
        merge_layer(&mut layered, project);
        let policy = layered.compaction_policy().expect("valid policy");
        assert!(policy.enabled);
        assert_eq!(policy.reserve_tokens, 20_000);
        assert_eq!(policy.keep_recent_tokens, 12_000);
        assert_eq!(policy.summary_max_bytes, 32_768);
        assert_eq!(policy.manual_instructions_max_bytes, 4 * 1024);
    }

    #[test]
    fn compaction_reserve_tokens_defaults_to_pi_and_must_be_positive() {
        let defaults = LayeredConfig::default()
            .compaction_policy()
            .expect("default policy");
        assert_eq!(defaults.reserve_tokens, 16_384);
        assert_eq!(defaults.keep_recent_tokens, 20_000);

        let mut layered = LayeredConfig::default();
        merge_layer(
            &mut layered,
            FileConfig::parse("[compaction]\nreserve_tokens = 0\n").expect("parses"),
        );
        let error = layered.compaction_policy().expect_err("zero reserve");
        assert!(error.contains("compaction.reserve_tokens"), "{error}");
    }

    #[test]
    fn removed_compaction_background_key_is_ignored_by_the_parser() {
        let config = FileConfig::parse("[compaction]\nbackground = false\nreserve_tokens = 9000\n")
            .expect("legacy background key still parses");
        let mut layered = LayeredConfig::default();
        merge_layer(&mut layered, config);
        let policy = layered.compaction_policy().expect("policy");
        assert_eq!(policy.reserve_tokens, 9000);
    }

    #[test]
    fn shell_job_layers_merge_only_provided_fields_and_limits_are_checked() {
        let mut config = LayeredConfig::default();
        merge_layer(&mut config,FileConfig::parse("[shell_jobs]\nmax_running=2\nmax_retained=8\nmemory_bytes=4096\ninterrupt_grace_ms=200\n").unwrap());
        merge_layer(
            &mut config,
            FileConfig::parse("[shell_jobs]\nmax_running=1\n").unwrap(),
        );
        assert_eq!(
            config.shell_jobs,
            slim_core::runtime::ShellJobLimits {
                max_running: 1,
                max_retained: 8,
                memory_bytes: 4096,
                interrupt_grace_ms: 200
            }
        );
        config.shell_jobs.validate().unwrap();
        assert!(FileConfig::parse("[shell_jobs]\nmax_runing=1\n").is_err());
        for layer in [
            "max_running=0",
            "max_running=65",
            "max_retained=0",
            "max_retained=1025",
            "memory_bytes=4095",
            "memory_bytes=16777217",
            "interrupt_grace_ms=49",
            "interrupt_grace_ms=10001",
        ] {
            let mut invalid = config.clone();
            merge_layer(
                &mut invalid,
                FileConfig::parse(&format!("[shell_jobs]\n{layer}\n")).unwrap(),
            );
            assert!(invalid.shell_jobs.validate().is_err(), "{layer}");
        }
    }

    #[test]
    fn parse_reads_known_keys_and_ignores_unknown() {
        let config = FileConfig::parse(
            "model = \"gpt-4o-mini\"\nendpoint = \"http://localhost\"\neffort = \"low\"\nfuture_key = true\n",
        )
        .expect("valid config parses");
        assert_eq!(config.model.as_deref(), Some("gpt-4o-mini"));
        assert_eq!(config.endpoint.as_deref(), Some("http://localhost"));
        assert_eq!(config.effort.as_deref(), Some("low"));
    }

    #[test]
    fn parse_rejects_invalid_toml() {
        assert!(FileConfig::parse("model = ").is_err());
        assert!(FileConfig::parse("= broken").is_err());
    }

    #[test]
    fn empty_file_yields_defaults() {
        let config = FileConfig::parse("").expect("empty config parses");
        assert_eq!(config, FileConfig::default());
    }

    #[test]
    fn load_returns_none_for_missing_file_and_error_for_invalid() {
        let missing = Path::new("definitely-missing-slim-config.toml");
        assert_eq!(FileConfig::load(missing).expect("missing is ok"), None);

        let path =
            std::env::temp_dir().join(format!("slim-config-invalid-{}.toml", std::process::id()));
        fs::write(&path, "model = ").expect("write fixture");
        let error = FileConfig::load(&path).expect_err("invalid config errors");
        assert!(
            error.contains("slim-config-invalid"),
            "error names the file: {error}"
        );
        fs::remove_file(&path).ok();

        let path =
            std::env::temp_dir().join(format!("slim-config-valid-{}.toml", std::process::id()));
        fs::write(&path, "model = \"gpt-4o-mini\"\n").expect("write fixture");
        let config = FileConfig::load(&path)
            .expect("valid loads")
            .expect("file exists");
        assert_eq!(config.model.as_deref(), Some("gpt-4o-mini"));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn load_rejects_config_above_the_byte_budget() {
        let path =
            std::env::temp_dir().join(format!("slim-config-large-{}.toml", std::process::id()));
        fs::File::create(&path)
            .expect("create")
            .set_len(1024 * 1024 + 1)
            .expect("extend");

        let error = FileConfig::load(&path).expect_err("oversized config");

        assert!(error.contains("1048576-byte safety limit"));
        fs::remove_file(path).ok();
    }

    #[test]
    fn merge_later_layer_overrides_defined_keys_and_fills_gaps() {
        let mut layered = LayeredConfig {
            model: Some("from-global".into()),
            ..LayeredConfig::default()
        };
        merge_layer(
            &mut layered,
            FileConfig {
                model: Some("from-project".into()),
                endpoint: Some("http://project".into()),
                effort: Some("medium".into()),
                ..FileConfig::default()
            },
        );
        assert_eq!(layered.model.as_deref(), Some("from-project"));
        assert_eq!(layered.endpoint.as_deref(), Some("http://project"));
        assert_eq!(layered.effort.as_deref(), Some("medium"));
    }

    #[test]
    fn load_layered_from_lets_project_override_global() {
        let dir = std::env::temp_dir().join(format!(
            "slim-config-layers-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("mkdir");
        let global = dir.join("global.toml");
        let project = dir.join("project.toml");
        fs::write(
            &global,
            "model = \"from-global\"\nendpoint = \"http://global\"\n",
        )
        .expect("write global");
        fs::write(&project, "model = \"from-project\"\n").expect("write project");
        let layered = load_layered_from([global, project]).expect("load");
        assert_eq!(layered.model.as_deref(), Some("from-project"));
        assert_eq!(layered.endpoint.as_deref(), Some("http://global"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_global_model_writes_and_preserves_unknown_keys() {
        let dir = std::env::temp_dir().join(format!("slim-config-save-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join(PROJECT_CONFIG_FILE);
        fs::write(
            &path,
            "endpoint = \"http://localhost:8000\"\nfuture_key = true\n",
        )
        .expect("seed");

        super::save_global_model_to(&path, "gpt-6-astra", "max", Some(true)).expect("save");

        let reloaded = fs::read_to_string(&path).expect("read back");
        assert!(
            reloaded.contains("model = \"gpt-6-astra\""),
            "model written"
        );
        assert!(reloaded.contains("effort = \"max\""), "effort written");
        let loaded = super::load_layered_from([path.clone()]).expect("load fast config");
        assert_eq!(loaded.codex_fast, Some(true));
        super::save_global_model_to(&path, "gpt-6-astra", "max", Some(false)).expect("normal");
        let loaded = super::load_layered_from([path]).expect("load normal config");
        assert_eq!(loaded.codex_fast, Some(false));
        assert!(
            reloaded.contains("future_key = true"),
            "unknown keys survive"
        );
        assert!(
            reloaded.contains("http://localhost:8000"),
            "endpoint survives"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_global_model_creates_missing_file_and_dir() {
        let dir = std::env::temp_dir().join(format!("slim-config-save-new-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join(PROJECT_CONFIG_FILE);

        super::save_global_model_to(&path, "grok-4", "high", None).expect("save");
        let reloaded = fs::read_to_string(&path).expect("read back");
        assert!(reloaded.contains("model = \"grok-4\""));
        assert!(reloaded.contains("effort = \"high\""));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn concurrent_model_saves_never_expose_partial_toml() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Barrier};

        let dir = std::env::temp_dir().join(format!(
            "slim-config-concurrent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("mkdir");
        let path = Arc::new(dir.join(PROJECT_CONFIG_FILE));
        fs::write(&*path, "future_key = true\n").expect("seed");
        let barrier = Arc::new(Barrier::new(5));
        let done = Arc::new(AtomicBool::new(false));
        let mut writers = Vec::new();
        for index in 0..4 {
            let path = path.clone();
            let barrier = barrier.clone();
            writers.push(std::thread::spawn(move || {
                barrier.wait();
                for iteration in 0..8 {
                    save_global_model_to(
                        &path,
                        &format!("model-{index}-{iteration}"),
                        if iteration % 2 == 0 { "low" } else { "high" },
                        None,
                    )
                    .expect("atomic save");
                }
            }));
        }
        let reader_path = path.clone();
        let reader_done = done.clone();
        let reader = std::thread::spawn(move || {
            barrier.wait();
            while !reader_done.load(Ordering::Acquire) {
                FileConfig::load(&reader_path)
                    .expect("reader never observes partial TOML")
                    .expect("config remains present");
                std::thread::yield_now();
            }
        });
        for writer in writers {
            writer.join().expect("writer");
        }
        done.store(true, Ordering::Release);
        reader.join().expect("reader");

        let reloaded = fs::read_to_string(&*path).expect("read back");
        assert!(reloaded.contains("future_key = true"));
        assert!(FileConfig::parse(&reloaded).is_ok());
        assert_eq!(
            fs::read_dir(&dir)
                .expect("read dir")
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".slim-config-")
                })
                .count(),
            0
        );
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn resolve_keeps_cli_over_environment_over_project_over_global() {
        assert_eq!(
            Config::resolve(Some("cli"), Some("env"), Some("proj"), Some("glob")),
            Some("cli")
        );
        assert_eq!(
            Config::resolve(None, Some("env"), Some("proj"), Some("glob")),
            Some("env")
        );
        assert_eq!(
            Config::resolve(None, None, Some("proj"), Some("glob")),
            Some("proj")
        );
    }
    #[test]
    fn lsp_path_only_is_enabled_and_explicit_disable_survives_layering() {
        let mut layered = LayeredConfig::default();
        merge_layer(
            &mut layered,
            FileConfig::parse("[lsp.servers.rust-analyzer]\npath = 'custom-ra.exe'\n").unwrap(),
        );
        let server = &layered.lsp.servers["rust-analyzer"];
        assert!(
            server.enabled,
            "path-only configuration keeps enabled default"
        );
        assert_eq!(server.path.as_deref(), Some("custom-ra.exe"));
        merge_layer(
            &mut layered,
            FileConfig::parse("[lsp.servers.rust-analyzer]\nenabled = false\n").unwrap(),
        );
        merge_layer(
            &mut layered,
            FileConfig::parse("[lsp.servers.rust-analyzer]\npath = 'another-ra.exe'\n").unwrap(),
        );
        assert!(!layered.lsp.servers["rust-analyzer"].enabled);
        assert_eq!(
            layered.lsp.servers["rust-analyzer"].path.as_deref(),
            Some("another-ra.exe"),
        );
    }

    #[test]
    fn project_lsp_launch_overrides_wait_for_trust_but_disabling_applies() {
        let mut layered = LayeredConfig::default();
        merge_layer(
            &mut layered,
            FileConfig::parse(
                "[lsp.servers.typescript-language-server]\nargs = ['--stdio']\n[lsp.servers.rust-analyzer]\nenabled = false\n",
            )
            .unwrap(),
        );
        merge_layer_as(
            &mut layered,
            FileConfig::parse(
                "[lsp.servers.typescript-language-server]\npath = 'repo.mjs'\nargs = ['--stdio', '--log-level', '4']\ninitialization_options = { tsserver = { path = 'repo.js' } }\nsettings = { typescript = {} }\n[lsp.servers.rust-analyzer]\nenabled = true\n[lsp.servers.custom]\nenabled = false\n",
            )
            .unwrap(),
            McpOrigin::Project,
        );
        let typescript = &layered.lsp.servers["typescript-language-server"];
        assert!(typescript.has_project_overrides());
        let untrusted = typescript.effective(false);
        assert_eq!(untrusted.path, None);
        assert_eq!(untrusted.args.as_ref().unwrap(), &["--stdio"]);
        assert_eq!(untrusted.initialization_options, None);
        assert_eq!(untrusted.settings, None);
        assert!(!untrusted.has_project_overrides());
        let trusted = typescript.effective(true);
        assert_eq!(trusted.path.as_deref(), Some("repo.mjs"));
        assert_eq!(
            trusted.args.as_ref().unwrap(),
            &["--stdio", "--log-level", "4"]
        );
        assert!(trusted.initialization_options.is_some());
        assert!(trusted.settings.is_some());

        // Re-enabling a globally disabled server is a project decision too.
        let rust = &layered.lsp.servers["rust-analyzer"];
        assert!(!rust.effective(false).enabled);
        assert!(rust.effective(true).enabled);
        // Disabling only restricts, so it applies without trust.
        let custom = &layered.lsp.servers["custom"];
        assert!(!custom.has_project_overrides());
        assert!(!custom.effective(false).enabled);
    }

    #[test]
    fn lsp_overrides_replace_only_present_fields_including_empty_objects() {
        let mut layered = LayeredConfig::default();
        merge_layer(&mut layered, FileConfig::parse(
            "[lsp.servers.typescript-language-server]\nenabled = false\nargs = ['--stdio', '--log-level', '1']\ninitialization_options = { hostInfo = 'global' }\nsettings = { typescript = { preferences = { quotePreference = 'single' } } }\n"
        ).unwrap());
        merge_layer(&mut layered, FileConfig::parse(
            "[lsp.servers.typescript-language-server]\npath = 'lib/cli.mjs'\nsettings = { javascript = { preferences = { quotePreference = 'double' } } }\n"
        ).unwrap());
        let server = &layered.lsp.servers["typescript-language-server"];
        assert!(!server.enabled);
        assert_eq!(server.path.as_deref(), Some("lib/cli.mjs"));
        assert_eq!(
            server.args.as_ref().unwrap(),
            &["--stdio", "--log-level", "1"]
        );
        assert_eq!(
            server.initialization_options,
            Some(serde_json::json!({"hostInfo": "global"}))
        );
        assert!(server
            .settings
            .as_ref()
            .unwrap()
            .get("typescript")
            .is_none());
        assert_eq!(
            server.settings.as_ref().unwrap()["javascript"]["preferences"]["quotePreference"],
            "double"
        );
        merge_layer(&mut layered, FileConfig::parse(
            "[lsp.servers.typescript-language-server]\nargs = []\nsettings = {}\ninitialization_options = {}\n"
        ).unwrap());
        let server = &layered.lsp.servers["typescript-language-server"];
        assert_eq!(server.args, Some(vec![]));
        assert_eq!(server.settings, Some(serde_json::json!({})));
        assert_eq!(server.initialization_options, Some(serde_json::json!({})));
        assert!(!server.enabled);
    }

    #[test]
    fn lsp_known_override_types_are_validated_without_rejecting_unknown_keys() {
        for field in [
            "args = [1]",
            "initialization_options = 'invalid'",
            "settings = []",
        ] {
            assert!(FileConfig::parse(&format!(
                "[lsp.servers.typescript-language-server]\n{field}\n"
            ))
            .is_err());
        }
        assert!(FileConfig::parse(
            "[lsp.servers.typescript-language-server]\nfuture_option = 'ignored'\n"
        )
        .is_ok());
    }

    #[test]
    fn mcp_parse_and_project_layer_overrides_global_fields() {
        let mut layered = LayeredConfig::default();
        merge_layer(
            &mut layered,
            FileConfig::parse(
                "[mcp.servers.fs]\ncommand = \"npx\"\nargs = [\"-y\", \"fs-server\"]\ntimeout_ms = 5000\n[mcp.servers.web]\nurl = \"https://mcp.example.com\"\n[mcp.servers.web.headers]\nAuthorization = \"Bearer global\"\n",
            )
            .unwrap(),
        );
        merge_layer(
            &mut layered,
            FileConfig::parse(
                "[mcp.servers.fs]\nargs = [\"-y\", \"fs-server-v2\"]\n[mcp.servers.fs.env]\nKEY = \"v\"\n[mcp.servers.web]\nenabled = false\n",
            )
            .unwrap(),
        );
        let fs = &layered.mcp.servers["fs"];
        assert_eq!(fs.command.as_deref(), Some("npx"));
        assert_eq!(fs.args, vec!["-y", "fs-server-v2"]);
        assert_eq!(fs.env.get("KEY").map(String::as_str), Some("v"));
        assert_eq!(fs.timeout_ms, 5_000);
        let web = &layered.mcp.servers["web"];
        assert!(!web.enabled);
        assert_eq!(
            web.headers.get("Authorization").map(String::as_str),
            Some("Bearer global")
        );
    }

    #[test]
    fn mcp_validate_rejects_ambiguous_or_missing_transport() {
        let mut config = McpConfig::default();
        config.servers.insert(
            "both".into(),
            McpServerConfig {
                command: Some("npx".into()),
                url: Some("https://x".into()),
                ..McpServerConfig::default()
            },
        );
        assert!(config
            .validate()
            .expect_err("both transports")
            .contains("not both"));
        config.servers.clear();
        config
            .servers
            .insert("none".into(), McpServerConfig::default());
        assert!(config
            .validate()
            .expect_err("no transport")
            .contains("missing command"));
        config.servers.clear();
        config.servers.insert(
            "bad name!".into(),
            McpServerConfig {
                command: Some("npx".into()),
                ..McpServerConfig::default()
            },
        );
        assert!(config
            .validate()
            .expect_err("bad name")
            .contains("name must match"));
        config.servers.clear();
        config.servers.insert(
            "slow".into(),
            McpServerConfig {
                command: Some("npx".into()),
                timeout_ms: 10,
                ..McpServerConfig::default()
            },
        );
        assert!(config
            .validate()
            .expect_err("tiny timeout")
            .contains("timeout_ms"));
    }

    #[test]
    fn mcp_upsert_and_remove_preserve_unrelated_toml() {
        let path =
            std::env::temp_dir().join(format!("slim-mcp-upsert-{}.toml", std::process::id()));
        fs::write(&path, "model = \"keep-me\"\n[other]\nx = 1\n").expect("seed");
        let server = FileMcpServerConfig {
            command: Some("npx".into()),
            args: Some(vec!["-y".into(), "srv".into()]),
            ..FileMcpServerConfig::default()
        };
        upsert_mcp_server_to(&path, "fs", &server).expect("upsert");

        let contents = fs::read_to_string(&path).expect("read back");
        let parsed: toml::Table = contents.parse().expect("still valid toml");
        assert_eq!(
            parsed.get("model").and_then(toml::Value::as_str),
            Some("keep-me")
        );
        assert!(parsed.contains_key("other"));
        let mcp = parsed
            .get("mcp")
            .and_then(|m| m.get("servers"))
            .and_then(|s| s.get("fs"))
            .expect("mcp.servers.fs written");
        assert_eq!(
            mcp.get("command").and_then(toml::Value::as_str),
            Some("npx")
        );
        assert_eq!(
            mcp.get("args")
                .and_then(toml::Value::as_array)
                .map(Vec::len),
            Some(2)
        );

        // Re-add replaces the entry without resurrecting stale keys.
        let http = FileMcpServerConfig {
            url: Some("https://mcp.example.com".into()),
            ..FileMcpServerConfig::default()
        };
        upsert_mcp_server_to(&path, "fs", &http).expect("re-upsert");
        let parsed: toml::Table = fs::read_to_string(&path)
            .expect("read back")
            .parse()
            .expect("valid");
        let mcp = parsed["mcp"]["servers"]["fs"].as_table().expect("table");
        assert!(mcp.get("command").is_none());
        assert_eq!(
            mcp.get("url").and_then(toml::Value::as_str),
            Some("https://mcp.example.com")
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn startup_wait_merges_by_layer_defaults_to_ten_seconds_and_is_bounded() {
        let mut layered = LayeredConfig::default();
        assert_eq!(
            layered.mcp.startup_wait(),
            std::time::Duration::from_secs(10)
        );
        merge_layer(
            &mut layered,
            FileConfig::parse("[mcp]\nstartup_wait_ms = 3000\n").unwrap(),
        );
        assert_eq!(
            layered.mcp.startup_wait(),
            std::time::Duration::from_secs(3)
        );
        // A later layer that does not mention it leaves it alone; one that
        // does replaces it (0 turns the wait off).
        merge_layer(
            &mut layered,
            FileConfig::parse("[mcp.servers.a]\ncommand = \"x\"\n").unwrap(),
        );
        assert_eq!(layered.mcp.startup_wait_ms, Some(3000));
        merge_layer(
            &mut layered,
            FileConfig::parse("[mcp]\nstartup_wait_ms = 0\n").unwrap(),
        );
        assert_eq!(layered.mcp.startup_wait(), std::time::Duration::ZERO);
        layered.mcp.validate().expect("zero is allowed");
        layered.mcp.startup_wait_ms = Some(120_001);
        assert!(layered
            .mcp
            .validate()
            .unwrap_err()
            .contains("startup_wait_ms"));
        layered.mcp.startup_wait_ms = Some(120_000);
        layered.mcp.validate().expect("the maximum is allowed");
    }

    #[test]
    fn mcp_upsert_saturates_timeout_ms_beyond_i64() {
        let path =
            std::env::temp_dir().join(format!("slim-mcp-timeout-{}.toml", std::process::id()));
        let server = FileMcpServerConfig {
            command: Some("npx".into()),
            timeout_ms: Some(u64::MAX),
            ..FileMcpServerConfig::default()
        };
        upsert_mcp_server_to(&path, "fs", &server).expect("upsert");
        // The written file must round-trip: a wrapped negative i64 would fail
        // to parse back into Option<u64>.
        let parsed = FileConfig::load(&path)
            .expect("load")
            .expect("config present");
        assert_eq!(
            parsed
                .mcp
                .and_then(|mcp| mcp.servers)
                .and_then(|mut servers| servers.remove("fs"))
                .and_then(|server| server.timeout_ms),
            Some(i64::MAX as u64)
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn mcp_layer_can_switch_transport_without_coexistence() {
        // Global http → project stdio: url and http-only headers clear.
        let mut layered = LayeredConfig::default();
        merge_layer(
            &mut layered,
            FileConfig::parse(
                "[mcp.servers.srv]\nurl = \"https://mcp.example.com\"\n[mcp.servers.srv.headers]\nAuthorization = \"Bearer s\"\n",
            )
            .unwrap(),
        );
        merge_layer(
            &mut layered,
            FileConfig::parse("[mcp.servers.srv]\ncommand = \"npx\"\nargs = [\"srv\"]\n").unwrap(),
        );
        let srv = &layered.mcp.servers["srv"];
        assert_eq!(srv.command.as_deref(), Some("npx"));
        assert!(srv.url.is_none());
        assert!(srv.headers.is_empty());
        layered.mcp.validate().expect("merged config validates");

        // Global stdio → project http: command, args and env clear.
        let mut layered = LayeredConfig::default();
        merge_layer(
            &mut layered,
            FileConfig::parse(
                "[mcp.servers.srv]\ncommand = \"npx\"\nargs = [\"srv\"]\n[mcp.servers.srv.env]\nKEY = \"v\"\n",
            )
            .unwrap(),
        );
        merge_layer(
            &mut layered,
            FileConfig::parse("[mcp.servers.srv]\nurl = \"https://mcp.example.com\"\n").unwrap(),
        );
        let srv = &layered.mcp.servers["srv"];
        assert_eq!(srv.url.as_deref(), Some("https://mcp.example.com"));
        assert!(srv.command.is_none());
        assert!(srv.args.is_empty());
        assert!(srv.env.is_empty());
        layered.mcp.validate().expect("merged config validates");
    }

    #[test]
    fn mcp_single_layer_with_both_transports_is_rejected() {
        let path = std::env::temp_dir().join(format!("slim-mcp-both-{}.toml", std::process::id()));
        fs::write(
            &path,
            "[mcp.servers.bad]\ncommand = \"npx\"\nurl = \"https://x\"\n",
        )
        .expect("seed");
        let error = load_layered_from([path.clone()]).expect_err("both transports in one layer");
        assert!(error.contains("not both"), "{error}");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn mcp_validate_rejects_non_http_url() {
        let mut config = McpConfig::default();
        config.servers.insert(
            "ws".into(),
            McpServerConfig {
                url: Some("ftp://x".into()),
                ..McpServerConfig::default()
            },
        );
        assert!(config
            .validate()
            .expect_err("non-http url")
            .contains("http://"));
    }

    fn temp_path(label: &str, extension: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "slim-config-{label}-{}-{:?}.{extension}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_file(&path);
        path
    }

    #[test]
    fn mcp_new_fields_parse_and_merge_across_layers() {
        let mut layered = LayeredConfig::default();
        merge_layer(
            &mut layered,
            FileConfig::parse(
                "[mcp.servers.web]\nurl = \"https://mcp.example.com\"\ndescription = \"Docs\"\nexposure = \"direct\"\nlazy = true\n[mcp.servers.web.tool_exposure]\n\"get_*\" = \"hidden\"\nexact = \"gateway\"\n[mcp.servers.web.oauth]\nclient_id = \"cid\"\ncallback_port = 8765\nscope = \"read\"\n",
            )
            .unwrap(),
        );
        merge_layer_as(
            &mut layered,
            FileConfig::parse(
                "[mcp.servers.web]\nexposure = \"hidden\"\n[mcp.servers.web.tool_exposure]\nextra = \"direct\"\n[mcp.servers.web.oauth]\nclient_secret = \"$SECRET\"\nscope = \"write\"\n",
            )
            .unwrap(),
            McpOrigin::Project,
        );
        let web = &layered.mcp.servers["web"];
        assert_eq!(web.description.as_deref(), Some("Docs"));
        assert_eq!(web.exposure, McpExposure::Hidden);
        assert!(web.lazy);
        assert_eq!(web.timeout_ms, 60_000, "default timeout is 60 s");
        assert_eq!(web.tool_exposure.len(), 3);
        assert_eq!(web.tool_exposure["get_*"], McpExposure::Hidden);
        assert_eq!(web.tool_exposure["extra"], McpExposure::Direct);
        let oauth = web.oauth.as_ref().expect("oauth merged");
        assert_eq!(oauth.client_id.as_deref(), Some("cid"));
        assert_eq!(oauth.client_secret.as_deref(), Some("$SECRET"));
        assert_eq!(oauth.callback_port, Some(8765));
        assert_eq!(
            oauth.scope.as_deref(),
            Some("write"),
            "later layer wins per key"
        );
        assert_eq!(web.origin, McpOrigin::Project);
        layered.mcp.validate().expect("valid");
        // The client secret never appears in Debug output.
        assert!(!format!("{oauth:?}").contains("$SECRET"));
    }

    #[test]
    fn mcp_unknown_exposure_is_a_parse_error() {
        let error = FileConfig::parse("[mcp.servers.a]\ncommand = \"x\"\nexposure = \"diret\"\n")
            .expect_err("typo");
        assert!(error.contains("diret"), "{error}");
        assert!(FileConfig::parse(
            "[mcp.servers.a]\ncommand = \"x\"\n[mcp.servers.a.tool_exposure]\nt = \"maybe\"\n"
        )
        .is_err());
    }

    #[test]
    fn mcp_validate_checks_new_fields() {
        let base = |edit: &dyn Fn(&mut McpServerConfig)| {
            let mut server = McpServerConfig {
                command: Some("x".into()),
                ..McpServerConfig::default()
            };
            edit(&mut server);
            let mut config = McpConfig::default();
            config.servers.insert("s".into(), server);
            config.validate()
        };
        assert!(base(&|_| {}).is_ok());
        assert!(base(&|s| s.cwd = Some("  ".into()))
            .unwrap_err()
            .contains("cwd"));
        assert!(base(&|s| s.description = Some(" ".into()))
            .unwrap_err()
            .contains("description"));
        assert!(base(&|s| s.description = Some("x".repeat(3000)))
            .unwrap_err()
            .contains("at most"));
        assert!(base(&|s| {
            s.tool_exposure.insert(String::new(), McpExposure::Hidden);
        })
        .unwrap_err()
        .contains("tool_exposure"));
        // oauth is for HTTP servers only.
        assert!(base(&|s| s.oauth = Some(McpOAuthConfig::default()))
            .unwrap_err()
            .contains("HTTP"));
        let http = |edit: &dyn Fn(&mut McpOAuthConfig)| {
            let mut oauth = McpOAuthConfig::default();
            edit(&mut oauth);
            let mut config = McpConfig::default();
            config.servers.insert(
                "h".into(),
                McpServerConfig {
                    url: Some("https://mcp.example.com".into()),
                    oauth: Some(oauth),
                    ..McpServerConfig::default()
                },
            );
            config.validate()
        };
        assert!(http(&|_| {}).is_ok());
        assert!(http(&|o| o.client_secret = Some("s".into()))
            .unwrap_err()
            .contains("client_id"));
        assert!(http(&|o| {
            o.client_id = Some("id".into());
            o.callback_port = Some(0);
        })
        .unwrap_err()
        .contains("callback_port"));
        assert!(
            http(&|o| o.auth_server_metadata_url = Some("http://example.com/x".into()))
                .unwrap_err()
                .contains("https")
        );
        for ok in [
            "https://auth.example.com/.well-known/openid-configuration",
            "http://localhost:9000/meta",
            "http://127.0.0.1/meta",
            "http://[::1]:1/meta",
        ] {
            assert!(
                http(&|o| o.auth_server_metadata_url = Some(ok.into())).is_ok(),
                "{ok}"
            );
        }
        assert!(
            http(&|o| o.auth_server_metadata_url = Some("http://localhost.evil.com/x".into()))
                .is_err()
        );
    }

    #[test]
    fn mcp_validate_rejects_names_that_differ_only_in_dash_or_underscore() {
        let mut config = McpConfig::default();
        for name in ["my-server", "my_server"] {
            config.servers.insert(
                name.into(),
                McpServerConfig {
                    command: Some("x".into()),
                    ..McpServerConfig::default()
                },
            );
        }
        assert!(config.validate().unwrap_err().contains("collides"));
    }

    #[test]
    fn mcp_layer_rejects_cwd_with_url_and_oauth_with_command() {
        let path = temp_path("mcp-mixed", "toml");
        fs::write(
            &path,
            "[mcp.servers.a]\nurl = \"https://x\"\ncwd = \"sub\"\n",
        )
        .unwrap();
        assert!(load_layered_from([path.clone()])
            .unwrap_err()
            .contains("cwd applies to stdio"));
        fs::write(
            &path,
            "[mcp.servers.a]\ncommand = \"x\"\n[mcp.servers.a.oauth]\nclient_id = \"id\"\n",
        )
        .unwrap();
        assert!(load_layered_from([path.clone()])
            .unwrap_err()
            .contains("oauth applies to HTTP"));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn mcp_transport_switch_clears_cwd_and_oauth() {
        let mut layered = LayeredConfig::default();
        merge_layer(
            &mut layered,
            FileConfig::parse("[mcp.servers.s]\ncommand = \"x\"\ncwd = \"sub\"\n").unwrap(),
        );
        merge_layer(
            &mut layered,
            FileConfig::parse("[mcp.servers.s]\nurl = \"https://x/mcp\"\n[mcp.servers.s.oauth]\nclient_id = \"id\"\n")
                .unwrap(),
        );
        let s = &layered.mcp.servers["s"];
        assert!(s.cwd.is_none() && s.command.is_none() && s.oauth.is_some());
        merge_layer(
            &mut layered,
            FileConfig::parse("[mcp.servers.s]\ncommand = \"y\"\n").unwrap(),
        );
        let s = &layered.mcp.servers["s"];
        assert!(s.oauth.is_none() && s.url.is_none());
        layered.mcp.validate().expect("valid after switches");
    }

    #[test]
    fn project_layer_origin_is_tracked_and_disable_only_overrides_stay_global() {
        let global = temp_path("origin-global", "toml");
        let project = temp_path("origin-project", "toml");
        fs::write(
            &global,
            "[mcp.servers.g]\ncommand = \"g\"\n[mcp.servers.h]\ncommand = \"h\"\n[mcp.servers.k]\ncommand = \"k\"\n",
        )
        .unwrap();
        fs::write(
            &project,
            "[mcp.servers.g]\nenabled = false\n[mcp.servers.h]\nargs = [\"--x\"]\n[mcp.servers.p]\nurl = \"https://p/mcp\"\n",
        )
        .unwrap();
        let layered = load_layers([
            (global.clone(), McpOrigin::Global),
            (project.clone(), McpOrigin::Project),
        ])
        .unwrap();
        assert_eq!(layered.mcp.servers["g"].origin, McpOrigin::Global);
        assert!(!layered.mcp.servers["g"].enabled);
        assert_eq!(layered.mcp.servers["h"].origin, McpOrigin::Project);
        assert_eq!(layered.mcp.servers["k"].origin, McpOrigin::Global);
        assert_eq!(layered.mcp.servers["p"].origin, McpOrigin::Project);
        fs::remove_file(&global).ok();
        fs::remove_file(&project).ok();
    }

    #[test]
    fn project_layer_is_read_from_the_workspace_not_the_process_directory() {
        let workspace = std::env::temp_dir().join(format!(
            "slim-config-workspace-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&workspace);
        fs::create_dir_all(&workspace).unwrap();
        fs::write(
            project_config_path(&workspace),
            "[mcp.servers.in-workspace]\ncommand = \"w\"\n",
        )
        .unwrap();
        // The test process directory has no such file; only the workspace does.
        let layered = load_layered_for(&workspace).expect("loads");
        let server = &layered.mcp.servers["in-workspace"];
        assert_eq!(server.origin, McpOrigin::Project);
        assert_eq!(
            project_config_path(&workspace),
            workspace.join(PROJECT_CONFIG_FILE)
        );
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn mcp_upsert_merges_into_an_existing_table_and_keeps_unrelated_keys() {
        let path = temp_path("upsert-merge", "toml");
        fs::write(
            &path,
            "model = \"keep\"\n[mcp.servers.fs]\ncommand = \"npx\"\nargs = [\"-y\", \"old\"]\ntimeout_ms = 5000\nenabled = false\ndescription = \"mine\"\n[mcp.servers.fs.env]\nA = \"1\"\nB = \"2\"\n",
        )
        .unwrap();
        let update = FileMcpServerConfig {
            args: Some(vec!["new".into()]),
            env: Some(BTreeMap::from([
                ("B".into(), "3".into()),
                ("C".into(), "4".into()),
            ])),
            ..FileMcpServerConfig::default()
        };
        upsert_mcp_server_to(&path, "fs", &update).unwrap();
        let parsed: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(parsed["model"].as_str(), Some("keep"));
        let fs_table = parsed["mcp"]["servers"]["fs"].as_table().unwrap();
        assert_eq!(fs_table["command"].as_str(), Some("npx"));
        assert_eq!(fs_table["timeout_ms"].as_integer(), Some(5000));
        assert_eq!(fs_table["enabled"].as_bool(), Some(false));
        assert_eq!(fs_table["description"].as_str(), Some("mine"));
        assert_eq!(fs_table["args"].as_array().unwrap().len(), 1);
        let env = fs_table["env"].as_table().unwrap();
        assert_eq!(env["A"].as_str(), Some("1"));
        assert_eq!(env["B"].as_str(), Some("3"));
        assert_eq!(env["C"].as_str(), Some("4"));

        // Switching to HTTP drops the stdio keys and the oauth/headers of the
        // old transport but keeps shared settings.
        let http = FileMcpServerConfig {
            url: Some("https://x/mcp".into()),
            oauth: Some(FileMcpOAuthConfig {
                client_id: Some("cid".into()),
                callback_port: Some(9000),
                ..FileMcpOAuthConfig::default()
            }),
            exposure: Some(McpExposure::Direct),
            tool_exposure: Some(BTreeMap::from([("t".into(), McpExposure::Hidden)])),
            ..FileMcpServerConfig::default()
        };
        upsert_mcp_server_to(&path, "fs", &http).unwrap();
        let parsed: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        let fs_table = parsed["mcp"]["servers"]["fs"].as_table().unwrap();
        for gone in ["command", "args", "env", "cwd"] {
            assert!(fs_table.get(gone).is_none(), "{gone} must be cleared");
        }
        assert_eq!(fs_table["timeout_ms"].as_integer(), Some(5000));
        assert_eq!(fs_table["exposure"].as_str(), Some("direct"));
        assert_eq!(fs_table["tool_exposure"]["t"].as_str(), Some("hidden"));
        assert_eq!(fs_table["oauth"]["callback_port"].as_integer(), Some(9000));
        // The result parses and validates as a layer.
        let layered = load_layered_from([path.clone()]).expect("loads");
        assert_eq!(
            layered.mcp.servers["fs"].url.as_deref(),
            Some("https://x/mcp")
        );

        // Back to stdio drops url, headers and oauth.
        let stdio = FileMcpServerConfig {
            command: Some("again".into()),
            ..FileMcpServerConfig::default()
        };
        upsert_mcp_server_to(&path, "fs", &stdio).unwrap();
        let parsed: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        let fs_table = parsed["mcp"]["servers"]["fs"].as_table().unwrap();
        assert!(fs_table.get("url").is_none() && fs_table.get("oauth").is_none());
        load_layered_from([path.clone()]).expect("still valid");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn mcp_upsert_of_a_new_command_replaces_the_old_commands_args_and_cwd() {
        let path = temp_path("upsert-new-command", "toml");
        fs::write(
            &path,
            "[mcp.servers.fs]\ncommand = \"npx\"\nargs = [\"-y\", \"@scope/server\", \".\"]\ncwd = \"work\"\ntimeout_ms = 5000\n",
        )
        .unwrap();
        let update = FileMcpServerConfig {
            command: Some("node".into()),
            ..FileMcpServerConfig::default()
        };
        upsert_mcp_server_to(&path, "fs", &update).unwrap();
        let parsed: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        let fs_table = parsed["mcp"]["servers"]["fs"].as_table().unwrap();
        assert_eq!(fs_table["command"].as_str(), Some("node"));
        assert!(fs_table.get("args").is_none(), "{fs_table:?}");
        assert!(fs_table.get("cwd").is_none(), "{fs_table:?}");
        assert_eq!(fs_table["timeout_ms"].as_integer(), Some(5000));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn mcp_upsert_rejects_both_transports_and_replace_drops_old_keys() {
        let path = temp_path("upsert-replace", "toml");
        let both = FileMcpServerConfig {
            command: Some("x".into()),
            url: Some("https://x".into()),
            ..FileMcpServerConfig::default()
        };
        assert!(upsert_mcp_server_to(&path, "a", &both)
            .unwrap_err()
            .contains("not both"));
        assert!(!path.exists(), "nothing written on rejection");
        fs::write(
            &path,
            "[mcp.servers.a]\ncommand = \"old\"\nenabled = false\n",
        )
        .unwrap();
        assert!(mcp_server_defined_in(&path, "a").unwrap());
        assert!(!mcp_server_defined_in(&path, "b").unwrap());
        let fresh = FileMcpServerConfig {
            command: Some("new".into()),
            ..FileMcpServerConfig::default()
        };
        replace_mcp_server_to(&path, "a", &fresh).unwrap();
        let parsed: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        let table = parsed["mcp"]["servers"]["a"].as_table().unwrap();
        assert_eq!(table["command"].as_str(), Some("new"));
        assert!(table.get("enabled").is_none());
        fs::remove_file(&path).ok();
    }

    #[test]
    fn remove_mcp_server_edits_the_workspace_project_file() {
        let workspace = std::env::temp_dir().join(format!(
            "slim-config-remove-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&workspace);
        fs::create_dir_all(&workspace).unwrap();
        let project = project_config_path(&workspace);
        fs::write(
            &project,
            "model = \"keep\"\n[mcp.servers.gone-from-project]\ncommand = \"x\"\n",
        )
        .unwrap();
        let edited = remove_mcp_server(&workspace, "gone-from-project").unwrap();
        assert_eq!(edited.as_deref(), Some(project.as_path()));
        assert!(fs::read_to_string(&project).unwrap().contains("keep"));
        assert!(!fs::read_to_string(&project)
            .unwrap()
            .contains("gone-from-project"));
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn lenient_load_reports_invalid_entries_and_keeps_the_valid_ones() {
        let workspace = std::env::temp_dir().join(format!(
            "slim-config-lenient-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&workspace);
        fs::create_dir_all(&workspace).unwrap();
        fs::write(
            project_config_path(&workspace),
            "[mcp.servers.len-ok]
command = \"x\"
             [mcp.servers.len-both]
command = \"x\"
url = \"https://a.test\"
             [mcp.servers.len-slow]
command = \"x\"
timeout_ms = 5
             [mcp.servers.len-dup]
command = \"x\"
             [mcp.servers.len_dup]
command = \"x\"
",
        )
        .unwrap();
        let (layered, invalid) = load_layered_lenient_for(&workspace).unwrap();
        let names: Vec<&str> = invalid.iter().map(|(name, _)| name.as_str()).collect();
        assert!(layered.mcp.servers.contains_key("len-ok"));
        for bad in ["len-both", "len-slow"] {
            assert!(!layered.mcp.servers.contains_key(bad), "{bad}");
            assert!(names.contains(&bad), "{names:?}");
        }
        // The strict loader refuses the same file as a whole.
        assert!(load_layered_for(&workspace).is_err());
        // Of two names that only differ in `-`/`_`, the later one is rejected.
        assert!(layered.mcp.servers.contains_key("len-dup"));
        assert!(names.contains(&"len_dup"), "{names:?}");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn remove_mcp_server_from_edits_only_the_given_file() {
        let directory = std::env::temp_dir().join(format!(
            "slim-config-remove-from-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("slim.toml");
        fs::write(
            &path,
            "model = \"keep\"
[mcp.servers.a]
command = \"x\"
[mcp.servers.b]
command = \"y\"
",
        )
        .unwrap();
        assert!(remove_mcp_server_from(&path, "a").unwrap());
        assert!(!remove_mcp_server_from(&path, "a").unwrap());
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("keep") && text.contains("[mcp.servers.b]"));
        // A missing file is "not there", and is not created.
        let missing = directory.join("missing.toml");
        assert!(!remove_mcp_server_from(&missing, "a").unwrap());
        assert!(!missing.exists());
        let _ = fs::remove_dir_all(&directory);
    }
}
