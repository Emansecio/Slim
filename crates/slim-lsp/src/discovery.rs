//! Server discovery: which server serves a workspace and where its binary
//! lives. Built-in profiles cover Rust and JavaScript/TypeScript. Discovery
//! never installs anything and
//! never spawns processes: a missing binary is reported, and the pool circuit
//! breaker handles a binary that exists but fails to start.

use std::path::{Path, PathBuf};

use serde_json::Value;

pub const MAX_ROOT_WALK_UP: usize = 12;
pub const RUST_ANALYZER: &str = "rust-analyzer";
pub const TYPESCRIPT_LANGUAGE_SERVER: &str = "typescript-language-server";

/// Optional overrides for a built-in profile. Absence retains its defaults.
#[derive(Clone, Debug)]
pub struct ServerOptions {
    pub enabled: bool,
    pub path: Option<PathBuf>,
    pub args: Option<Vec<String>>,
    pub initialization_options: Option<Value>,
    pub settings: Option<Value>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            path: None,
            args: None,
            initialization_options: None,
            settings: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerSpec {
    pub id: String,
    pub label: String,
    /// Resolved binary (absolute when found) or plain name when not found.
    pub command: String,
    pub args: Vec<String>,
    pub root_markers: Vec<String>,
    /// (file extension, LSP languageId) pairs used to sync documents.
    pub language_ids: Vec<(String, String)>,
    /// Section name used to answer workspace/configuration.
    pub settings_section: String,
}

impl ServerSpec {
    pub fn language_id_for(&self, path: &Path) -> Option<&str> {
        let ext = path.extension().and_then(|value| value.to_str())?;
        self.language_ids
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(ext))
            .map(|(_, language)| language.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryResult {
    /// Nearest directory containing a root marker (or the workspace itself).
    pub root: Option<PathBuf>,
    pub spec: Option<ServerSpec>,
    pub binary_missing: bool,
    pub unavailable_reason: Option<String>,
}

/// Walks up from the workspace looking for a directory holding any marker file.
pub fn find_root_marker(start: &Path, markers: &[&str]) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    for _ in 0..=MAX_ROOT_WALK_UP {
        if markers.iter().any(|marker| dir.join(marker).is_file()) {
            return Some(dir);
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// Resolves a server binary from an explicit configured path, then from a
/// set of PATH-style directories. Windows extensions are probed first so the
/// plain name also works on Unix.
pub fn resolve_binary_from_paths(
    configured: Option<&Path>,
    bin_name: &str,
    path_dirs: &[PathBuf],
) -> Option<PathBuf> {
    if let Some(path) = configured {
        return path.is_file().then(|| path.to_path_buf());
    }
    let extensions = ["", ".exe", ".cmd", ".bat"];
    // A relative or empty PATH entry would resolve against the process
    // directory (normally the workspace), letting a repository plant a binary.
    for dir in path_dirs.iter().filter(|dir| dir.is_absolute()) {
        for ext in extensions {
            let candidate = if ext.is_empty() {
                dir.join(bin_name)
            } else {
                dir.join(format!("{bin_name}{ext}"))
            };
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

pub fn resolve_binary_on_path(configured: Option<&Path>, bin_name: &str) -> Option<PathBuf> {
    let dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).collect())
        .unwrap_or_default();
    resolve_binary_from_paths(configured, bin_name, &dirs)
}

pub fn rust_analyzer_spec(command: String) -> ServerSpec {
    ServerSpec {
        id: RUST_ANALYZER.into(),
        label: "Rust Analyzer".into(),
        command,
        args: vec![],
        root_markers: vec!["Cargo.toml".into()],
        language_ids: vec![("rs".into(), "rust".into())],
        settings_section: "rust-analyzer".into(),
    }
}

pub fn typescript_language_server_spec(command: String, args: Vec<String>) -> ServerSpec {
    ServerSpec {
        id: TYPESCRIPT_LANGUAGE_SERVER.into(),
        label: "TypeScript / JavaScript".into(),
        command,
        args,
        root_markers: vec![
            "package.json".into(),
            "tsconfig.json".into(),
            "jsconfig.json".into(),
        ],
        language_ids: [
            ("js", "javascript"),
            ("jsx", "javascriptreact"),
            ("mjs", "javascript"),
            ("cjs", "javascript"),
            ("ts", "typescript"),
            ("tsx", "typescriptreact"),
            ("mts", "typescript"),
            ("cts", "typescript"),
        ]
        .into_iter()
        .map(|(extension, language)| (extension.into(), language.into()))
        .collect(),
        settings_section: "typescript".into(),
    }
}

/// Resolves discovery for the rust-analyzer server against a workspace dir.
pub fn discover_for_workspace(workspace: &Path, configured: Option<&Path>) -> DiscoveryResult {
    let markers = ["Cargo.toml"];
    let Some(root) = find_root_marker(workspace, &markers) else {
        return DiscoveryResult {
            root: None,
            spec: None,
            binary_missing: true,
            unavailable_reason: Some("no Cargo.toml found for the Rust workspace".into()),
        };
    };
    match resolve_binary_on_path(configured, "rust-analyzer") {
        Some(binary) => DiscoveryResult {
            root: Some(root),
            spec: Some(rust_analyzer_spec(
                binary
                    .canonicalize()
                    .unwrap_or(binary)
                    .to_string_lossy()
                    .into_owned(),
            )),
            binary_missing: false,
            unavailable_reason: None,
        },
        None => DiscoveryResult {
            root: Some(root),
            // Keep a spec so status can explain what is missing.
            spec: Some(rust_analyzer_spec("rust-analyzer".into())),
            binary_missing: true,
            unavailable_reason: Some(match configured {
                Some(_) => "configured rust-analyzer path does not exist".into(),
                None => "rust-analyzer was not found on PATH".into(),
            }),
        },
    }
}

/// Resolves one built-in profile without starting it. JS/TS always uses the
/// authorized workspace root; TypeScript chooses each document's project.
pub fn discover_server(
    workspace: &Path,
    server_id: &str,
    configured: Option<&Path>,
    args: Option<&[String]>,
) -> DiscoveryResult {
    // A relative configured path follows the workspace, like MCP `cwd`, not
    // the process directory (which differs when a session resumes elsewhere).
    let configured = configured.map(|path| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            workspace.join(path)
        }
    });
    let configured = configured.as_deref();
    if server_id == RUST_ANALYZER {
        let mut result = discover_for_workspace(workspace, configured);
        if let (Some(spec), Some(args)) = (&mut result.spec, args) {
            spec.args = args.to_vec();
        }
        return result;
    }
    let root = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    if server_id != TYPESCRIPT_LANGUAGE_SERVER {
        return DiscoveryResult {
            root: Some(root),
            spec: None,
            binary_missing: true,
            unavailable_reason: Some(format!("unknown built-in language server: {server_id}")),
        };
    }
    let path_dirs = std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_default();
    discover_typescript(&root, configured, args, &path_dirs)
}

fn discover_typescript(
    workspace: &Path,
    configured: Option<&Path>,
    args: Option<&[String]>,
    path_dirs: &[PathBuf],
) -> DiscoveryResult {
    let server_args = args
        .map(<[String]>::to_vec)
        .unwrap_or_else(|| vec!["--stdio".into()]);
    let resolved = resolve_typescript(workspace, configured, &server_args, path_dirs);
    let (spec, reason) = match resolved {
        Ok(spec) => (spec, None),
        Err(reason) => (
            typescript_language_server_spec(TYPESCRIPT_LANGUAGE_SERVER.into(), server_args),
            Some(reason),
        ),
    };
    DiscoveryResult {
        root: Some(workspace.to_path_buf()),
        spec: Some(spec),
        binary_missing: reason.is_some(),
        unavailable_reason: reason,
    }
}

fn resolve_typescript(
    workspace: &Path,
    configured: Option<&Path>,
    server_args: &[String],
    path_dirs: &[PathBuf],
) -> Result<ServerSpec, String> {
    if let Some(path) = configured {
        let launcher = path
            .canonicalize()
            .map_err(|_| "configured TypeScript Language Server path does not exist".to_owned())?;
        return resolve_typescript_launcher(&launcher, server_args, path_dirs);
    }
    let local_package = workspace
        .join("node_modules")
        .join(TYPESCRIPT_LANGUAGE_SERVER);
    if local_package.join("package.json").is_file() {
        return node_spec(&package_entrypoint(&local_package)?, server_args, path_dirs);
    }
    // An unsupported launcher layout early on PATH must not hide a usable
    // install later on PATH; report the first failure when none works.
    let mut first_error = None;
    for directory in path_dirs {
        let Some(launcher) = resolve_binary_from_paths(
            None,
            TYPESCRIPT_LANGUAGE_SERVER,
            std::slice::from_ref(directory),
        ) else {
            continue;
        };
        match resolve_typescript_launcher(&launcher, server_args, path_dirs) {
            Ok(spec) => return Ok(spec),
            Err(error) if first_error.is_none() => first_error = Some(error),
            Err(_) => {}
        }
    }
    Err(first_error.unwrap_or_else(|| {
        "TypeScript Language Server was not found in workspace node_modules or PATH".to_owned()
    }))
}

fn resolve_typescript_launcher(
    launcher: &Path,
    server_args: &[String],
    path_dirs: &[PathBuf],
) -> Result<ServerSpec, String> {
    let canonical = launcher
        .canonicalize()
        .map_err(|_| "TypeScript Language Server launcher is unavailable".to_owned())?;
    if !canonical.is_file() {
        return Err("TypeScript Language Server launcher must be a file".into());
    }
    if is_javascript(&canonical) {
        let package = canonical
            .ancestors()
            .skip(1)
            .take(4)
            .find(|directory| directory.join("package.json").is_file())
            .ok_or_else(|| {
                "TypeScript Language Server JavaScript entrypoint has no package metadata"
                    .to_owned()
            })?;
        let entrypoint = package_entrypoint(package)?;
        if entrypoint != canonical {
            return Err(
                "configured JavaScript path is not the declared TypeScript Language Server bin"
                    .into(),
            );
        }
        return node_spec(&entrypoint, server_args, path_dirs);
    }
    // npm's .bin and global launchers have a known package location. Resolve
    // package metadata rather than interpreting any cmd/bat/shebang script.
    if launcher.file_stem().and_then(|name| name.to_str()) == Some(TYPESCRIPT_LANGUAGE_SERVER) {
        if let Some(directory) = launcher.parent() {
            let package = if directory.file_name().and_then(|name| name.to_str()) == Some(".bin") {
                directory
                    .parent()
                    .map(|parent| parent.join(TYPESCRIPT_LANGUAGE_SERVER))
            } else {
                Some(
                    directory
                        .join("node_modules")
                        .join(TYPESCRIPT_LANGUAGE_SERVER),
                )
            };
            if let Some(package) = package.filter(|package| package.join("package.json").is_file())
            {
                return node_spec(&package_entrypoint(&package)?, server_args, path_dirs);
            }
        }
    }
    if is_native_executable(&canonical) {
        return Ok(typescript_language_server_spec(
            canonical.to_string_lossy().into_owned(),
            server_args.to_vec(),
        ));
    }
    Err("unsupported TypeScript Language Server launcher; use its installed package bin entrypoint or a native executable".into())
}

fn is_javascript(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["js", "mjs", "cjs"]
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}

fn is_native_executable(path: &Path) -> bool {
    #[cfg(windows)]
    {
        path.extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
    }
    #[cfg(not(windows))]
    {
        use std::io::Read;
        let mut magic = [0; 4];
        std::fs::File::open(path)
            .and_then(|mut file| file.read_exact(&mut magic))
            .is_ok()
            && matches!(
                magic,
                [0x7f, b'E', b'L', b'F']
                    | [0xfe, 0xed, 0xfa, 0xce]
                    | [0xfe, 0xed, 0xfa, 0xcf]
                    | [0xce, 0xfa, 0xed, 0xfe]
                    | [0xcf, 0xfa, 0xed, 0xfe]
                    | [0xca, 0xfe, 0xba, 0xbe]
            )
    }
}

fn package_entrypoint(package: &Path) -> Result<PathBuf, String> {
    use std::io::Read;
    let package = package
        .canonicalize()
        .map_err(|_| "TypeScript Language Server package is unavailable".to_owned())?;
    let mut metadata = String::new();
    std::fs::File::open(package.join("package.json"))
        .and_then(|file| file.take(1024 * 1024 + 1).read_to_string(&mut metadata))
        .map_err(|_| "cannot read TypeScript Language Server package metadata".to_owned())?;
    if metadata.len() > 1024 * 1024 {
        return Err("TypeScript Language Server package metadata exceeds its size limit".into());
    }
    let metadata: Value = serde_json::from_str(&metadata)
        .map_err(|_| "invalid TypeScript Language Server package metadata".to_owned())?;
    if metadata["name"].as_str() != Some(TYPESCRIPT_LANGUAGE_SERVER) {
        return Err("package name does not identify TypeScript Language Server".into());
    }
    let bin = metadata["bin"]
        .as_str()
        .or_else(|| metadata["bin"][TYPESCRIPT_LANGUAGE_SERVER].as_str())
        .ok_or_else(|| {
            "package metadata does not declare the TypeScript Language Server bin".to_owned()
        })?;
    let entrypoint = package
        .join(bin)
        .canonicalize()
        .map_err(|_| "TypeScript Language Server package bin does not exist".to_owned())?;
    if !entrypoint.starts_with(&package) || !entrypoint.is_file() || !is_javascript(&entrypoint) {
        return Err(
            "TypeScript Language Server bin must be a JavaScript file inside its package".into(),
        );
    }
    Ok(entrypoint)
}

fn node_spec(
    entrypoint: &Path,
    server_args: &[String],
    path_dirs: &[PathBuf],
) -> Result<ServerSpec, String> {
    let node = path_dirs
        .iter()
        .filter(|directory| directory.is_absolute())
        .find_map(|directory| {
            ["node", "node.exe"].iter().find_map(|name| {
                let candidate = directory.join(name);
                (candidate.is_file() && is_native_executable(&candidate))
                    .then(|| candidate.canonicalize().ok())
                    .flatten()
            })
        })
        .ok_or_else(|| {
            "Node executable was not found on PATH for TypeScript Language Server".to_owned()
        })?;
    let mut args = Vec::with_capacity(server_args.len() + 1);
    args.push(entrypoint.to_string_lossy().into_owned());
    args.extend_from_slice(server_args);
    Ok(typescript_language_server_spec(
        node.to_string_lossy().into_owned(),
        args,
    ))
}

/// Stable config hash for pooling: same root + server + config share a process.
pub fn config_hash(payload: &serde_json::Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    payload.to_string().hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "slim-lsp-typescript-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn package(&self, base: &Path) -> PathBuf {
            let package = base.join("node_modules").join(TYPESCRIPT_LANGUAGE_SERVER);
            std::fs::create_dir_all(package.join("lib")).unwrap();
            std::fs::write(
                package.join("package.json"),
                serde_json::json!({
                    "name": TYPESCRIPT_LANGUAGE_SERVER,
                    "bin": { TYPESCRIPT_LANGUAGE_SERVER: "./lib/cli.mjs" }
                })
                .to_string(),
            )
            .unwrap();
            std::fs::write(package.join("lib/cli.mjs"), "export {};").unwrap();
            package
        }

        fn node(&self) -> PathBuf {
            let directory = self.0.join("node-runtime");
            std::fs::create_dir_all(&directory).unwrap();
            let name = if cfg!(windows) { "node.exe" } else { "node" };
            std::fs::write(directory.join(name), b"\x7fELF").unwrap();
            directory
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn explicit_missing_binary_does_not_fall_back_to_path() {
        let directory = TestDirectory::new();
        std::fs::write(directory.0.join("rust-analyzer.exe"), "x").unwrap();
        assert!(resolve_binary_from_paths(
            Some(&directory.0.join("missing.exe")),
            RUST_ANALYZER,
            std::slice::from_ref(&directory.0)
        )
        .is_none());
    }

    #[test]
    fn relative_or_empty_path_entries_never_resolve_binaries() {
        assert!(resolve_binary_from_paths(
            None,
            RUST_ANALYZER,
            &[PathBuf::new(), PathBuf::from(".")]
        )
        .is_none());
    }

    #[test]
    fn typescript_unsupported_launcher_on_path_does_not_hide_a_later_install() {
        let directory = TestDirectory::new();
        let unsupported = directory.0.join("pnpm-global");
        std::fs::create_dir_all(&unsupported).unwrap();
        std::fs::write(
            unsupported.join("typescript-language-server.cmd"),
            "pnpm shim",
        )
        .unwrap();
        let global = directory.0.join("npm-global");
        let package = directory.package(&global);
        std::fs::write(global.join("typescript-language-server.cmd"), "npm shim").unwrap();
        let node = directory.node();
        let result = discover_typescript(&directory.0, None, None, &[unsupported, global, node]);
        assert!(!result.binary_missing, "{:?}", result.unavailable_reason);
        assert_eq!(
            result.spec.unwrap().args[0],
            package
                .join("lib/cli.mjs")
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        );
    }

    #[test]
    fn typescript_local_package_precedes_path_and_keeps_internal_entrypoint_args() {
        let directory = TestDirectory::new();
        let local = directory.package(&directory.0);
        let global = directory.0.join("npm-global");
        directory.package(&global);
        std::fs::write(
            global.join("typescript-language-server.cmd"),
            "untrusted shim contents",
        )
        .unwrap();
        let node = directory.node();
        let result = discover_typescript(&directory.0, None, Some(&[]), &[global, node.clone()]);
        assert!(!result.binary_missing, "{:?}", result.unavailable_reason);
        let spec = result.spec.unwrap();
        assert_eq!(
            spec.command,
            node.join(if cfg!(windows) { "node.exe" } else { "node" })
                .canonicalize()
                .unwrap()
                .to_string_lossy()
        );
        assert_eq!(
            spec.args,
            [local
                .join("lib/cli.mjs")
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned()]
        );
        assert_eq!(result.root, Some(directory.0.clone()));
        for (name, language) in [
            ("index.js", "javascript"),
            ("index.jsx", "javascriptreact"),
            ("index.mjs", "javascript"),
            ("index.cjs", "javascript"),
            ("index.ts", "typescript"),
            ("index.tsx", "typescriptreact"),
            ("index.mts", "typescript"),
            ("index.cts", "typescript"),
            ("index.d.ts", "typescript"),
        ] {
            assert_eq!(spec.language_id_for(Path::new(name)), Some(language));
        }
    }

    #[test]
    fn npm_shim_is_mapped_to_package_bin_without_executing_a_shell() {
        let directory = TestDirectory::new();
        let global = directory.0.join("npm-global");
        let package = directory.package(&global);
        std::fs::write(
            global.join("typescript-language-server.cmd"),
            "not even a valid cmd file",
        )
        .unwrap();
        let node = directory.node();
        let result = discover_typescript(&directory.0, None, None, &[global.clone(), node.clone()]);
        assert!(!result.binary_missing, "{:?}", result.unavailable_reason);
        let spec = result.spec.unwrap();
        assert_eq!(
            spec.args,
            [
                package
                    .join("lib/cli.mjs")
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                "--stdio".into()
            ]
        );
        assert!(spec
            .command
            .ends_with(if cfg!(windows) { "node.exe" } else { "node" }));
        let explicit = discover_typescript(
            &directory.0,
            Some(&global.join("typescript-language-server.cmd")),
            None,
            &[node],
        );
        assert_eq!(explicit.spec, Some(spec));
    }

    #[test]
    fn typescript_missing_node_or_explicit_entrypoint_is_explained() {
        let directory = TestDirectory::new();
        directory.package(&directory.0);
        let result = discover_typescript(&directory.0, None, None, &[]);
        assert!(result.binary_missing);
        assert!(result.unavailable_reason.unwrap().contains("Node"));
        let result = discover_typescript(
            &directory.0,
            Some(&directory.0.join("missing.mjs")),
            None,
            &[directory.node()],
        );
        assert!(result.binary_missing);
        assert!(result.unavailable_reason.unwrap().contains("configured"));
    }

    #[test]
    fn typescript_rejects_wrong_package_and_bin_outside_package() {
        let directory = TestDirectory::new();
        let package = directory.package(&directory.0);
        let entrypoint = package.join("lib/cli.mjs");
        std::fs::write(
            package.join("package.json"),
            r#"{"name":"other","bin":{"typescript-language-server":"./lib/cli.mjs"}}"#,
        )
        .unwrap();
        let result =
            discover_typescript(&directory.0, Some(&entrypoint), None, &[directory.node()]);
        assert!(result.unavailable_reason.unwrap().contains("package name"));
        std::fs::write(package.parent().unwrap().join("outside.mjs"), "export {};").unwrap();
        std::fs::write(package.join("package.json"), r#"{"name":"typescript-language-server","bin":{"typescript-language-server":"../outside.mjs"}}"#).unwrap();
        let result = discover_typescript(&directory.0, None, None, &[directory.node()]);
        assert!(result
            .unavailable_reason
            .unwrap()
            .contains("inside its package"));
    }

    #[test]
    fn typescript_rechecks_local_precedence_after_installation_appears() {
        let directory = TestDirectory::new();
        let global = directory.0.join("npm-global");
        let global_package = directory.package(&global);
        std::fs::write(global.join("typescript-language-server.cmd"), "shim").unwrap();
        let paths = [global, directory.node()];
        let before = discover_typescript(&directory.0, None, None, &paths)
            .spec
            .unwrap();
        assert_eq!(
            before.args[0],
            global_package
                .join("lib/cli.mjs")
                .canonicalize()
                .unwrap()
                .to_string_lossy()
        );
        let local = directory.package(&directory.0);
        let after = discover_typescript(&directory.0, None, None, &paths)
            .spec
            .unwrap();
        assert_eq!(
            after.args[0],
            local
                .join("lib/cli.mjs")
                .canonicalize()
                .unwrap()
                .to_string_lossy()
        );
    }

    #[test]
    fn finds_marker_at_workspace_and_walks_up() {
        let dir = std::env::temp_dir().join(format!(
            "slim-lsp-discovery-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(find_root_marker(&dir, &["Cargo.toml"]), Some(dir.clone()));
        assert_eq!(
            find_root_marker(&dir.join("sub"), &["Cargo.toml"]),
            Some(dir.clone())
        );
        assert_eq!(
            find_root_marker(&dir.join("sub"), &["pyproject.toml"]),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolves_binary_from_configured_first() {
        let dir = std::env::temp_dir().join(format!(
            "slim-lsp-bin-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let configured = dir.join("custom-ra.exe");
        std::fs::write(&configured, "x").unwrap();
        let resolved = resolve_binary_from_paths(
            Some(&configured),
            "rust-analyzer",
            &[PathBuf::from("C:\\does-not-exist")],
        )
        .expect("configured binary");
        assert_eq!(resolved, configured);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolves_binary_from_path_dirs_with_windows_extension() {
        let dir = std::env::temp_dir().join(format!(
            "slim-lsp-bin2-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("rust-analyzer.exe"), "x").unwrap();
        let resolved = resolve_binary_from_paths(None, "rust-analyzer", std::slice::from_ref(&dir))
            .expect("path binary");
        assert_eq!(resolved, dir.join("rust-analyzer.exe"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discovery_reports_missing_binary_without_root_change() {
        let dir = std::env::temp_dir().join(format!(
            "slim-lsp-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
        let result = discover_for_workspace(&dir, None);
        assert_eq!(result.root, Some(dir.clone()));
        assert!(result.spec.is_some());
        // binary_missing depends on whether rust-analyzer is on PATH; either
        // outcome is correct as long as root and spec are populated.
        assert!(
            result.binary_missing == (result.spec.as_ref().unwrap().command == "rust-analyzer")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_hash_changes_with_config() {
        use serde_json::json;
        let a = config_hash(&json!({ "checkOnSave": false }));
        let b = config_hash(&json!({ "checkOnSave": true }));
        assert_ne!(a, b);
        assert_eq!(a, config_hash(&json!({ "checkOnSave": false })));
    }
}
