use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub struct Config;

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
}

/// Merged view of every config layer (project wins over global).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LayeredConfig {
    pub model: Option<String>,
    pub endpoint: Option<String>,
    pub effort: Option<String>,
}

/// Project-level file looked up relative to the working directory.
pub const PROJECT_CONFIG_FILE: &str = "slim.toml";

impl FileConfig {
    pub fn parse(contents: &str) -> Result<Self, String> {
        toml::from_str(contents).map_err(|error| error.to_string())
    }

    /// Missing file yields `Ok(None)`; present-but-invalid surfaces an error
    /// carrying the path so callers can point at the offending file.
    pub fn load(path: &Path) -> Result<Option<Self>, String> {
        match fs::read_to_string(path) {
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
    directories::ProjectDirs::from("", "", "slim")
        .map(|dirs| dirs.config_dir().join(PROJECT_CONFIG_FILE))
}

/// Loads global first, then project; later layers fill earlier gaps.
pub fn load_layered() -> Result<LayeredConfig, String> {
    let mut paths = Vec::new();
    if let Some(global) = global_config_path() {
        paths.push(global);
    }
    paths.push(PathBuf::from(PROJECT_CONFIG_FILE));
    let mut layered = LayeredConfig::default();
    for path in paths {
        if let Some(config) = FileConfig::load(&path)? {
            merge_layer(&mut layered, config);
        }
    }
    Ok(layered)
}

fn merge_layer(target: &mut LayeredConfig, source: FileConfig) {
    if target.model.is_none() {
        target.model = source.model;
    }
    if target.endpoint.is_none() {
        target.endpoint = source.endpoint;
    }
    if target.effort.is_none() {
        target.effort = source.effort;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let path = std::env::temp_dir().join(format!("slim-config-invalid-{}.toml", std::process::id()));
        fs::write(&path, "model = ").expect("write fixture");
        let error = FileConfig::load(&path).expect_err("invalid config errors");
        assert!(error.contains("slim-config-invalid"), "error names the file: {error}");
        fs::remove_file(&path).ok();

        let path = std::env::temp_dir().join(format!("slim-config-valid-{}.toml", std::process::id()));
        fs::write(&path, "model = \"gpt-4o-mini\"\n").expect("write fixture");
        let config = FileConfig::load(&path).expect("valid loads").expect("file exists");
        assert_eq!(config.model.as_deref(), Some("gpt-4o-mini"));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn merge_fills_gaps_without_overriding_earlier_layers() {
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
            },
        );
        assert_eq!(layered.model.as_deref(), Some("from-global"));
        assert_eq!(layered.endpoint.as_deref(), Some("http://project"));
        assert_eq!(layered.effort.as_deref(), Some("medium"));
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
