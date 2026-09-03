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
    #[serde(default)]
    pub max_mutating_tool_calls: Option<usize>,
    #[serde(default)]
    pub max_read_tool_calls: Option<usize>,
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LspServerConfig {
    pub enabled: bool,
    pub path: Option<String>,
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
    pub max_mutating_tool_calls: Option<usize>,
    pub max_read_tool_calls: Option<usize>,
    pub max_turns: Option<usize>,
    pub max_output_tokens: Option<u32>,
    pub timeout_secs: Option<u64>,
    pub max_result_bytes: Option<usize>,
    pub compaction: FileCompactionConfig,
    pub lsp: LspConfig,
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

/// OS config directory for Slim (e.g. `%APPDATA%\slim` on Windows).
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
/// (`%APPDATA%\slim\slim.toml`), preserving unknown keys (endpoint, future
/// fields) by editing a parsed table instead of rewriting from scratch.
/// Returns the path written on success so callers can surface it in toasts.
pub fn save_global_model(model: &str, effort: &str) -> Result<PathBuf, String> {
    let path = global_config_path()
        .ok_or_else(|| "unable to resolve the global config directory".to_owned())?;
    save_global_model_to(&path, model, effort)?;
    Ok(path)
}

/// Writes `model`/`effort` into the TOML file at `path`, preserving unknown
/// keys. Exposed for hermetic tests; production uses [`save_global_model`].
fn save_global_model_to(path: &Path, model: &str, effort: &str) -> Result<(), String> {
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
    let serialized =
        toml::to_string(&table).map_err(|error| format!("{}: {error}", path.display()))?;
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
    load_layered_from(paths)
}

pub(crate) fn load_layered_from(
    paths: impl IntoIterator<Item = PathBuf>,
) -> Result<LayeredConfig, String> {
    let mut layered = LayeredConfig::default();
    for path in paths {
        if let Some(config) = FileConfig::load(&path)? {
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
    if source.max_mutating_tool_calls.is_some() {
        target.max_mutating_tool_calls = source.max_mutating_tool_calls;
    }
    if source.max_read_tool_calls.is_some() {
        target.max_read_tool_calls = source.max_read_tool_calls;
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

        super::save_global_model_to(&path, "gpt-5.6-luna", "low").expect("save");

        let reloaded = fs::read_to_string(&path).expect("read back");
        assert!(
            reloaded.contains("model = \"gpt-5.6-luna\""),
            "model written"
        );
        assert!(reloaded.contains("effort = \"low\""), "effort written");
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

        super::save_global_model_to(&path, "grok-4", "high").expect("save");
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
}
