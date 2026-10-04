//! Bounded disk snapshot for semantic queries; no watcher or idle polling.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::document::FileStamp;
use crate::transport::TransportError;

const MAX_WORKSPACE_ENTRIES: usize = 32_768;

pub(crate) type WorkspaceSnapshot = BTreeMap<PathBuf, FileStamp>;

fn typescript(server_id: &str) -> bool {
    server_id == "typescript-language-server"
}

fn matches_name(value: Option<&str>, names: &[&str]) -> bool {
    value.is_some_and(|value| names.iter().any(|name| name.eq_ignore_ascii_case(value)))
}

pub(crate) fn project_input(path: &Path, server_id: &str) -> bool {
    if typescript(server_id) {
        return matches_name(
            path.extension().and_then(|ext| ext.to_str()),
            &["json", "jsonc"],
        ) || matches_name(
            path.file_name().and_then(|name| name.to_str()),
            &["yarn.lock", "pnpm-lock.yaml", "bun.lock", "bun.lockb"],
        );
    }
    matches_name(
        path.file_name().and_then(|name| name.to_str()),
        &["Cargo.toml", "Cargo.lock", "rust-project.json"],
    ) || (matches_name(
        path.parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str()),
        &[".cargo"],
    ) && matches_name(
        path.file_name().and_then(|name| name.to_str()),
        &["config", "config.toml"],
    ))
}

pub(crate) fn snapshot(root: &Path, server_id: &str) -> Result<WorkspaceSnapshot, TransportError> {
    snapshot_bounded(root, server_id, MAX_WORKSPACE_ENTRIES)
}

fn snapshot_bounded(
    root: &Path,
    server_id: &str,
    limit: usize,
) -> Result<WorkspaceSnapshot, TransportError> {
    let mut files = BTreeMap::new();
    let mut directories = vec![root.to_path_buf()];
    let mut visited = 0;
    while let Some(directory) = directories.pop() {
        // Paths removed while the tree is walked are deletions the snapshot
        // records by omission; any other disk error still fails the refresh.
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound && directory.as_path() != root =>
            {
                continue
            }
            Err(error) => return Err(disk_error(error)),
        };
        for entry in entries {
            visited += 1;
            if visited > limit {
                return Err(TransportError::Protocol(format!(
                    "workspace refresh exceeded its {limit}-entry limit; freshness is unknown"
                )));
            }
            let entry = entry.map_err(disk_error)?;
            let kind = match entry.file_type() {
                Ok(kind) => kind,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(disk_error(error)),
            };
            let path = entry.path();
            if kind.is_dir() {
                let name = entry.file_name();
                let ignored = matches!(name.to_str(), Some(".git" | ".slim" | "node_modules"))
                    || (name == "target"
                        && (!typescript(server_id)
                            || path
                                .parent()
                                .is_some_and(|parent| parent.join("Cargo.toml").is_file())));
                if !ignored
                    && !kind.is_symlink()
                    && crate::path_policy::existing_workspace_path(root, &path).is_some()
                {
                    directories.push(path);
                }
            } else if kind.is_file()
                && (source_file(&path, server_id) || project_input(&path, server_id))
            {
                let Some(stamp) = FileStamp::for_path(&path) else {
                    if matches!(path.try_exists(), Ok(false)) {
                        continue;
                    }
                    return Err(TransportError::Protocol(
                        "workspace file metadata could not be read during refresh".into(),
                    ));
                };
                files.insert(path, stamp);
            }
        }
    }
    Ok(files)
}

fn source_file(path: &Path, server_id: &str) -> bool {
    let extension = path.extension().and_then(|ext| ext.to_str());
    if typescript(server_id) {
        matches_name(
            extension,
            &["js", "jsx", "mjs", "cjs", "ts", "tsx", "mts", "cts"],
        )
    } else {
        matches_name(extension, &["rs"])
    }
}

fn disk_error(error: std::io::Error) -> TransportError {
    TransportError::Protocol(format!("workspace refresh failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_inputs_and_sources_are_profile_specific() {
        let server = "typescript-language-server";
        for name in [
            "main.js",
            "main.jsx",
            "main.mjs",
            "main.cjs",
            "main.ts",
            "main.tsx",
            "main.mts",
            "main.cts",
            "types.d.ts",
            "types.d.mts",
            "types.d.cts",
            "main.TS",
            "main.JSX",
            "types.D.CTS",
        ] {
            assert!(source_file(Path::new(name), server), "{name}");
            assert!(!source_file(Path::new(name), "rust-analyzer"), "{name}");
        }
        for name in [
            "package.json",
            "arbitrary-extends.jsonc",
            "data.json",
            "package-lock.json",
            "npm-shrinkwrap.json",
            "yarn.lock",
            "pnpm-lock.yaml",
            "bun.lock",
            "bun.lockb",
            "config.JSONC",
            "data.JSON",
            "YARN.LOCK",
            "PNPM-LOCK.YAML",
            "BUN.LOCKB",
        ] {
            assert!(project_input(Path::new(name), server), "{name}");
        }
        assert!(!project_input(Path::new("Cargo.lock"), server));
        assert!(project_input(Path::new("Cargo.lock"), "rust-analyzer"));
        assert!(project_input(Path::new("CARGO.TOML"), "rust-analyzer"));
        assert!(source_file(Path::new("main.RS"), "rust-analyzer"));
        assert!(!source_file(Path::new("main.rs"), server));
        assert!(!source_file(Path::new("main.py"), server));
    }

    #[test]
    fn typescript_snapshot_preserves_importable_outputs_and_observes_external_changes() {
        let root = std::env::temp_dir().join(format!(
            "slim-typescript-snapshot-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for directory in [
            "src",
            "dist",
            "lib",
            "target",
            "node_modules",
            ".git",
            ".slim",
            "package/target",
        ] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        std::fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();
        for name in [
            "src/main.ts",
            "dist/types.d.ts",
            "lib/types.d.cts",
            "package/target/source.ts",
            "config.jsonc",
            "BUN.LOCKB",
            "src/case.TS",
            "uppercase.JSONC",
            "YARN.LOCK",
        ] {
            std::fs::write(root.join(name), "before").unwrap();
        }
        for name in [
            "target/generated.ts",
            "node_modules/ignored.ts",
            ".git/ignored.json",
            ".slim/ignored.json",
            "src/main.rs",
        ] {
            std::fs::write(root.join(name), "excluded").unwrap();
        }
        let server = "typescript-language-server";
        let before = snapshot(&root, server).unwrap();
        assert_eq!(
            before.len(),
            9,
            "Cargo target is excluded; ordinary target remains observable"
        );
        assert!(before.contains_key(&root.join("dist/types.d.ts")));
        assert!(before.contains_key(&root.join("lib/types.d.cts")));
        assert!(before.contains_key(&root.join("package/target/source.ts")));
        for name in ["src/case.TS", "uppercase.JSONC", "YARN.LOCK", "BUN.LOCKB"] {
            assert!(before.contains_key(&root.join(name)), "{name}");
        }
        assert!(snapshot_bounded(&root, server, 2)
            .unwrap_err()
            .to_string()
            .contains("freshness is unknown"));
        std::fs::write(root.join("src/main.ts"), "external change").unwrap();
        std::fs::remove_file(root.join("lib/types.d.cts")).unwrap();
        std::fs::write(root.join("src/created.mts"), "new module").unwrap();
        let after = snapshot(&root, server).unwrap();
        assert_ne!(
            before[&root.join("src/main.ts")],
            after[&root.join("src/main.ts")]
        );
        assert!(!after.contains_key(&root.join("lib/types.d.cts")));
        assert!(after.contains_key(&root.join("src/created.mts")));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_excludes_build_trees_and_never_returns_a_partial_success() {
        let root = std::env::temp_dir().join(format!(
            "slim-disk-snapshot-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();
        std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("target/ignored.rs"), "generated").unwrap();
        let before = snapshot(&root, "rust-analyzer").unwrap();
        assert_eq!(before.len(), 2);
        assert!(snapshot_bounded(&root, "rust-analyzer", 1)
            .unwrap_err()
            .to_string()
            .contains("freshness is unknown"));
        std::fs::write(root.join("new.rs"), "fn added() {}").unwrap();
        let after = snapshot(&root, "rust-analyzer").unwrap();
        assert_eq!(after.len(), 3);
        assert_ne!(before, after);
        std::fs::remove_dir_all(root).unwrap();
    }
}
