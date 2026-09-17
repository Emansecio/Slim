use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::Deserialize;
use slim_core::context::CompactionPolicy;

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
    #[serde(default)]
    pub lsp: Option<FileLspConfig>,
    #[serde(default)]
    pub mcp: Option<FileMcpConfig>,
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
}

impl Default for LspServerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: None,
        }
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
    pub url: Option<String>,
    #[serde(default)]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// Merged MCP configuration with defaults applied.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct McpConfig {
    pub servers: BTreeMap<String, McpServerConfig>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpServerConfig {
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub enabled: bool,
    pub timeout_ms: u64,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            enabled: true,
            timeout_ms: 30_000,
        }
    }
}

impl McpConfig {
    /// Fails loud on ambiguous server entries so a typo cannot silently
    /// disable or reroute a configured server.
    pub fn validate(&self) -> Result<(), String> {
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
    pub background: Option<bool>,
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
    pub lsp: LspConfig,
    pub mcp: McpConfig,
}

impl LayeredConfig {
    pub fn compaction_policy(&self) -> Result<CompactionPolicy, String> {
        let mut policy = CompactionPolicy::default();
        if let Some(value) = self.compaction.enabled {
            policy.enabled = value;
        }
        if let Some(value) = self.compaction.background {
            policy.background = value;
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

/// Writes or replaces `[mcp.servers.<name>]` in the TOML at `path`,
/// preserving all other keys. Only fields actually set on `server` are
/// written, so a re-add does not resurrect stale keys.
pub fn upsert_mcp_server_to(
    path: &Path,
    name: &str,
    server: &FileMcpServerConfig,
) -> Result<(), String> {
    let _write_guard = CONFIG_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut table = read_config_table_locked(path)?;
    let entry = mcp_servers_table(&mut table);
    let mut fields = toml::Table::new();
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
        fields.insert(
            "env".into(),
            toml::Value::Table(
                env.iter()
                    .map(|(k, v)| (k.clone(), v.clone().into()))
                    .collect(),
            ),
        );
    }
    if let Some(url) = &server.url {
        fields.insert("url".into(), url.clone().into());
    }
    if let Some(headers) = &server.headers {
        fields.insert(
            "headers".into(),
            toml::Value::Table(
                headers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone().into()))
                    .collect(),
            ),
        );
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
    entry.insert(name.to_owned(), toml::Value::Table(fields));
    write_config_table_locked(path, &table)
}

/// Deletes `[mcp.servers.<name>]` from the first layer that defines it
/// (project file first, then global). Returns the edited path, or `None`
/// when the server was not configured anywhere.
pub fn remove_mcp_server(name: &str) -> Result<Option<PathBuf>, String> {
    let _write_guard = CONFIG_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut paths = vec![PathBuf::from(PROJECT_CONFIG_FILE)];
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

/// Loads global first, then project. Later layers override keys they set
/// and fill remaining gaps (`project` wins over `global`).
pub fn load_layered() -> Result<LayeredConfig, String> {
    let mut paths = Vec::new();
    if let Some(global) = global_config_path() {
        paths.push(global);
    }
    paths.push(PathBuf::from(PROJECT_CONFIG_FILE));
    let mut config = load_layered_from(paths)?;
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

pub(crate) fn load_layered_from(
    paths: impl IntoIterator<Item = PathBuf>,
) -> Result<LayeredConfig, String> {
    let mut layered = LayeredConfig::default();
    for path in paths {
        if let Some(config) = FileConfig::load(&path)? {
            // Per-layer rejection: one file setting both transports is a
            // typo; merging then can no longer detect it (a layer's `command`
            // legitimately clears an inherited `url` and vice versa).
            if let Some(servers) = config.mcp.as_ref().and_then(|mcp| mcp.servers.as_ref()) {
                for (name, server) in servers {
                    if server.command.is_some() && server.url.is_some() {
                        return Err(format!(
                            "mcp.servers.{name}: set either command (stdio) or url (http), not both"
                        ));
                    }
                }
            }
            merge_layer(&mut layered, config);
        }
    }
    Ok(layered)
}

fn merge_layer(target: &mut LayeredConfig, source: FileConfig) {
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
                if let Some(enabled) = server.enabled {
                    entry.enabled = enabled;
                }
                if let Some(path) = server.path {
                    entry.path = Some(path);
                }
            }
        }
    }
    if let Some(mcp) = source.mcp {
        if let Some(servers) = mcp.servers {
            for (name, server) in servers {
                let entry = target.mcp.servers.entry(name).or_default();
                // A layer that picks one transport clears the other's keys so
                // `command`+`url` never coexist in the merged entry.
                if let Some(command) = server.command {
                    entry.command = Some(command);
                    entry.url = None;
                    entry.headers.clear();
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
                }
                if let Some(headers) = server.headers {
                    entry.headers.extend(headers);
                }
                if let Some(enabled) = server.enabled {
                    entry.enabled = enabled;
                }
                if let Some(timeout_ms) = server.timeout_ms {
                    entry.timeout_ms = timeout_ms;
                }
            }
        }
    }
    if let Some(compaction) = source.compaction {
        if compaction.enabled.is_some() {
            target.compaction.enabled = compaction.enabled;
        }
        if compaction.background.is_some() {
            target.compaction.background = compaction.background;
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
            "[compaction]\nenabled = true\nbackground = false\nkeep_recent_tokens = 12000\n",
        )
        .expect("global config");
        let project =
            FileConfig::parse("[compaction]\nbackground = true\nsummary_max_bytes = 32768\n")
                .expect("project config");
        let mut layered = LayeredConfig::default();
        merge_layer(&mut layered, global);
        merge_layer(&mut layered, project);
        let policy = layered.compaction_policy().expect("valid policy");
        assert!(policy.enabled);
        assert!(policy.background);
        assert_eq!(policy.keep_recent_tokens, 12_000);
        assert_eq!(policy.summary_max_bytes, 32_768);
        assert_eq!(policy.manual_instructions_max_bytes, 4 * 1024);
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
}
