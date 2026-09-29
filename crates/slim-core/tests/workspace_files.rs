use slim_core::runtime::CancellationToken;
use slim_core::workspace_files::{
    is_sensitive_file_name, list_workspace_files, load_mention_file, mention_paths_in_prompt,
    MentionError,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_WORKSPACE: AtomicU64 = AtomicU64::new(0);

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let nonce = NEXT_WORKSPACE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "slim-workspace-files-{}-{stamp}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&root).expect("workspace");
        Self(root)
    }

    fn root(&self) -> &Path {
        &self.0
    }

    fn write(&self, relative: &str, content: impl AsRef<[u8]>) {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().expect("parent")).expect("directories");
        fs::write(path, content).expect("file");
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn listing_respects_gitignore_skip_dirs_and_includes_dotfiles() {
    let workspace = Workspace::new();
    workspace.write(".gitignore", "ignored.txt\nbuild-output/\n");
    workspace.write("ignored.txt", "x");
    workspace.write("build-output/a.txt", "x");
    workspace.write("src/lib.rs", "x");
    workspace.write("src/nested/mod.rs", "x");
    workspace.write("README.md", "x");
    workspace.write(".github/workflows/ci.yml", "x");
    workspace.write("node_modules/pkg/index.js", "x");
    workspace.write("target/debug/out.o", "x");
    workspace.write("dist/bundle.js", "x");
    workspace.write(".git/config", "x");
    workspace.write(".slim/state.json", "x");
    workspace.write(".pi/x", "x");
    workspace.write(".venv/bin/python", "x");

    let files = list_workspace_files(workspace.root(), 1000, None).expect("list");
    // Shallower first, then lexicographic ('.' sorts before 'R').
    assert_eq!(
        files,
        [
            ".gitignore",
            "README.md",
            "src/lib.rs",
            ".github/workflows/ci.yml",
            "src/nested/mod.rs",
        ]
    );
}

#[test]
fn listing_honors_limit_and_is_deterministic() {
    let workspace = Workspace::new();
    for index in 0..30 {
        workspace.write(&format!("dir{}/file{index:02}.txt", index % 3), "x");
    }
    let first = list_workspace_files(workspace.root(), 10, None).expect("list");
    let second = list_workspace_files(workspace.root(), 10, None).expect("list");
    assert_eq!(first.len(), 10);
    assert_eq!(first, second);
    let mut sorted = first.clone();
    sorted.sort();
    assert_eq!(first, sorted);
    assert!(list_workspace_files(workspace.root(), 0, None)
        .expect("list")
        .is_empty());
    assert_eq!(
        list_workspace_files(workspace.root(), usize::MAX, None)
            .expect("list")
            .len(),
        30
    );
}

#[test]
fn listing_stops_when_cancelled() {
    let workspace = Workspace::new();
    workspace.write("a.txt", "x");
    let token = CancellationToken::new();
    token.cancel();
    let error = list_workspace_files(workspace.root(), 10, Some(&token)).expect_err("cancelled");
    assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
}

#[test]
fn listing_skips_symlinked_files_and_directories() {
    let workspace = Workspace::new();
    workspace.write("real.txt", "x");
    workspace.write("real_dir/inner.txt", "x");
    let file_link = workspace.root().join("link.txt");
    let dir_link = workspace.root().join("link_dir");
    #[cfg(unix)]
    let created = std::os::unix::fs::symlink(workspace.root().join("real.txt"), &file_link)
        .and_then(|()| std::os::unix::fs::symlink(workspace.root().join("real_dir"), &dir_link));
    #[cfg(windows)]
    let created = std::os::windows::fs::symlink_file(workspace.root().join("real.txt"), &file_link)
        .and_then(|()| {
            std::os::windows::fs::symlink_dir(workspace.root().join("real_dir"), &dir_link)
        });
    if created.is_err() {
        eprintln!("symlink creation unavailable; skipping symlink assertions");
        return;
    }
    let files = list_workspace_files(workspace.root(), 100, None).expect("list");
    assert_eq!(files, ["real.txt", "real_dir/inner.txt"]);
}

#[cfg(windows)]
#[test]
fn listing_skips_directory_junctions() {
    let workspace = Workspace::new();
    workspace.write("real_dir/inner.txt", "x");
    let junction = workspace.root().join("junction_dir");
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&junction)
        .arg(workspace.root().join("real_dir"))
        .output();
    if !status.is_ok_and(|output| output.status.success()) {
        eprintln!("junction creation unavailable; skipping");
        return;
    }
    let files = list_workspace_files(workspace.root(), 100, None).expect("list");
    assert_eq!(files, ["real_dir/inner.txt"]);
}

#[test]
fn loader_reads_a_workspace_file() {
    let workspace = Workspace::new();
    workspace.write("src/lib.rs", "fn main() {}\r\n");
    let file = load_mention_file(workspace.root(), "src/lib.rs", 1024).expect("load");
    assert_eq!(file.path, "src/lib.rs");
    assert_eq!(file.text, "fn main() {}\r\n");
    assert_eq!(file.bytes, file.text.len());
    assert!(!file.truncated);
    let dotted = load_mention_file(workspace.root(), "./src/../src/lib.rs", 1024).expect("load");
    assert_eq!(dotted.path, "src/lib.rs");
}

#[test]
fn loader_rejects_escapes_missing_and_non_files() {
    let workspace = Workspace::new();
    workspace.write("src/lib.rs", "x");
    let outside = workspace.root().parent().expect("parent");
    assert!(matches!(
        load_mention_file(workspace.root(), "", 10),
        Err(MentionError::Empty)
    ));
    assert!(matches!(
        load_mention_file(workspace.root(), "   ", 10),
        Err(MentionError::Empty)
    ));
    assert!(matches!(
        load_mention_file(workspace.root(), "../outside.txt", 10),
        Err(MentionError::OutsideWorkspace)
    ));
    assert!(matches!(
        load_mention_file(workspace.root(), "src/../../outside.txt", 10),
        Err(MentionError::OutsideWorkspace)
    ));
    assert!(matches!(
        load_mention_file(workspace.root(), outside.to_str().expect("utf8"), 10),
        Err(MentionError::OutsideWorkspace)
    ));
    let absolute = workspace.root().join("src/lib.rs");
    assert!(matches!(
        load_mention_file(workspace.root(), absolute.to_str().expect("utf8"), 10),
        Err(MentionError::OutsideWorkspace)
    ));
    assert!(matches!(
        load_mention_file(workspace.root(), "nope.txt", 10),
        Err(MentionError::NotFound)
    ));
    assert!(matches!(
        load_mention_file(workspace.root(), "src", 10),
        Err(MentionError::NotRegularFile)
    ));
}

#[test]
fn loader_rejects_symlinks_pointing_outside() {
    let workspace = Workspace::new();
    let outside = Workspace::new();
    outside.write("secret.txt", "outside");
    let link = workspace.root().join("escape.txt");
    #[cfg(unix)]
    let created = std::os::unix::fs::symlink(outside.root().join("secret.txt"), &link);
    #[cfg(windows)]
    let created = std::os::windows::fs::symlink_file(outside.root().join("secret.txt"), &link);
    if created.is_err() {
        eprintln!("symlink creation unavailable; skipping");
        return;
    }
    assert!(matches!(
        load_mention_file(workspace.root(), "escape.txt", 100),
        Err(MentionError::OutsideWorkspace)
    ));
}

#[test]
fn loader_rejects_binary_and_non_utf8() {
    let workspace = Workspace::new();
    workspace.write("bin.dat", [b'a', 0, b'b']);
    workspace.write("latin1.txt", [b'c', b'a', b'f', 0xE9]);
    assert!(matches!(
        load_mention_file(workspace.root(), "bin.dat", 100),
        Err(MentionError::Binary)
    ));
    assert!(matches!(
        load_mention_file(workspace.root(), "latin1.txt", 100),
        Err(MentionError::Binary)
    ));
}

#[test]
fn loader_refuses_secret_names_but_allows_env_examples() {
    let workspace = Workspace::new();
    for name in [
        ".env",
        ".env.local",
        ".ENV",
        "certs/server.pem",
        "certs/server.KEY",
        "a.p12",
        "a.pfx",
        ".ssh/id_rsa",
        ".ssh/id_ed25519.pub",
        "vault.kdbx",
        "credentials.json",
        ".npmrc",
        ".pypirc",
        "auth.json",
    ] {
        workspace.write(name, "secret");
        assert!(
            matches!(
                load_mention_file(workspace.root(), name, 100),
                Err(MentionError::Sensitive)
            ),
            "{name}"
        );
    }
    for name in [".env.example", ".env.sample", ".env.template", "notes.txt"] {
        workspace.write(name, "KEY=value\n");
        let file = load_mention_file(workspace.root(), name, 100).expect(name);
        assert_eq!(file.text, "KEY=value\n");
    }
    assert!(is_sensitive_file_name("Credentials.JSON"));
    assert!(!is_sensitive_file_name("environment.rs"));
}

#[test]
fn loader_truncates_on_a_char_boundary_and_strips_bom() {
    let workspace = Workspace::new();
    // 'é' is two bytes; a cap of 4 lands inside the second 'é'.
    workspace.write("text.txt", "aébé-tail");
    let file = load_mention_file(workspace.root(), "text.txt", 4).expect("load");
    assert_eq!(file.text, "aéb");
    assert_eq!(file.bytes, 3 + 1);
    assert!(file.truncated);
    let exact = load_mention_file(workspace.root(), "text.txt", "aébé-tail".len()).expect("load");
    assert_eq!(exact.text, "aébé-tail");
    assert!(!exact.truncated);
    let zero = load_mention_file(workspace.root(), "text.txt", 0).expect("load");
    assert!(zero.text.is_empty() && zero.truncated);

    let mut bom = vec![0xEF, 0xBB, 0xBF];
    bom.extend_from_slice(b"hello");
    workspace.write("bom.txt", bom);
    let file = load_mention_file(workspace.root(), "bom.txt", 100).expect("load");
    assert_eq!(file.text, "hello");
    assert_eq!(file.bytes, 5);
}

#[test]
fn prompt_scanner_picks_existing_files_and_ignores_the_rest() {
    let workspace = Workspace::new();
    workspace.write("src/lib.rs", "x");
    workspace.write("docs/notes.md", "x");
    let picked = mention_paths_in_prompt(
        workspace.root(),
        "look at @src/lib.rs, then (@docs/notes.md) and \"@src/lib.rs\". \
         Also @override, mail user@host.com, @missing.txt, @src, @docs/notes.md!",
    );
    assert_eq!(picked, ["src/lib.rs", "docs/notes.md"]);
    assert!(mention_paths_in_prompt(workspace.root(), "@").is_empty());
    assert!(mention_paths_in_prompt(workspace.root(), "").is_empty());
    assert_eq!(
        mention_paths_in_prompt(workspace.root(), "@src/lib.rs\n@docs/notes.md"),
        ["src/lib.rs", "docs/notes.md"]
    );
    assert!(mention_paths_in_prompt(workspace.root(), "@../outside.txt @/etc/passwd").is_empty());
}

// Windows strips trailing dots from file names, so this only exists on Unix.
#[cfg(unix)]
#[test]
fn prompt_scanner_keeps_a_trailing_dot_when_the_file_exists() {
    let workspace = Workspace::new();
    workspace.write("dir/file.", "x");
    assert_eq!(
        mention_paths_in_prompt(workspace.root(), "see @dir/file."),
        ["dir/file."]
    );
}
