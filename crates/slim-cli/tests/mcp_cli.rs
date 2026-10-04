//! `slim mcp ...` end to end: the real binary against temporary config,
//! trust and credential files, with a scripted stdio server (this test binary
//! re-spawned with `--ignored`) and the loopback OAuth/MCP mock.
//!
//! Every command runs without a TUI, a provider or credentials.

#[path = "../../../tests/support/mcp_oauth_mock.rs"]
mod mcp_oauth_mock;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

// ---------------------------------------------------------------------------
// Scripted stdio server
// ---------------------------------------------------------------------------

/// A stdio MCP server driven by `MCP_CLI_MODE`: `ok` (tools, resources and a
/// template), `reject` (answers `initialize` with an error that echoes
/// `$API_TOKEN`), `exit` (dies before the handshake), `escapes` (a tool name
/// carrying a terminal escape sequence). `MCP_CLI_MARKER` names
/// a file created when it starts.
#[test]
#[ignore = "subprocess fixture"]
fn mcp_cli_stdio_fixture() {
    use serde_json::json;
    if let Some(marker) = std::env::var_os("MCP_CLI_MARKER") {
        fs::write(marker, "started").expect("marker");
    }
    let mode = std::env::var("MCP_CLI_MODE").unwrap_or_default();
    if mode == "exit" {
        std::process::exit(3);
    }
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        if mode == "reject" {
            let mut out = stdout.lock();
            writeln!(
                out,
                "{}",
                json!({"jsonrpc": "2.0", "id": id, "error": {
                    "code": -32000,
                    "message": format!("rejected token {}", std::env::var("API_TOKEN").unwrap_or_default()),
                }})
            )
            .expect("write");
            out.flush().expect("flush");
            continue;
        }
        let result = match message["method"].as_str().unwrap_or("") {
            "initialize" => json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {"tools": {}, "resources": {}},
                "serverInfo": {"name": "cli-fixture", "version": "1.2.3"},
            }),
            "tools/list" if mode == "escapes" => json!({"tools": [
                {"name": "bad\u{1b}[2Jname", "inputSchema": {"type": "object"}},
            ]}),
            "tools/list" => json!({"tools": [
                {"name": "alpha", "description": "first", "inputSchema": {"type": "object"}},
                {"name": "beta", "description": "second", "inputSchema": {"type": "object"}},
            ]}),
            "resources/list" => json!({"resources": [
                {"uri": "file:///a.txt", "name": "a"},
                {"uri": "file:///b.txt", "name": "b"},
            ]}),
            "resources/templates/list" => {
                json!({"resourceTemplates": [{"uriTemplate": "file:///{path}", "name": "any"}]})
            }
            _ => json!({}),
        };
        let mut out = stdout.lock();
        writeln!(
            out,
            "{}",
            json!({"jsonrpc": "2.0", "id": id, "result": result})
        )
        .expect("write");
        out.flush().expect("flush");
    }
}

// ---------------------------------------------------------------------------
// Sandbox
// ---------------------------------------------------------------------------

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|error| panic!("stdout is not JSON ({error}): {}", self.stdout))
    }
}

/// A workspace plus its own global config, trust store and credential store,
/// so no test touches the real ones.
struct Sandbox {
    root: PathBuf,
    workspace: PathBuf,
}

impl Sandbox {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "slim-mcp-cli-{label}-{}-{unique}",
            std::process::id()
        ));
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::create_dir_all(root.join("config")).expect("config dir");
        Self { root, workspace }
    }

    fn global(&self) -> PathBuf {
        self.root.join("config").join("slim.toml")
    }

    fn trust_file(&self) -> PathBuf {
        self.root.join("config").join("mcp-trust.json")
    }

    fn auth_file(&self) -> PathBuf {
        self.root.join("config").join("mcp-auth.json")
    }

    fn project(&self) -> PathBuf {
        self.workspace.join("slim.toml")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_slim"));
        command
            .arg("mcp")
            .args(args)
            .current_dir(&self.workspace)
            .env("SLIM_CONFIG_FILE", self.global())
            .env("SLIM_MCP_TRUST_FILE", self.trust_file())
            .env("SLIM_MCP_AUTH_FILE", self.auth_file())
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Run {
        let output = self.command(args).output().expect("spawn slim mcp");
        Run {
            code: output.status.code().expect("exit code"),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    fn ok(&self, args: &[&str]) -> Run {
        let run = self.run(args);
        assert_eq!(
            run.code, 0,
            "{args:?}\nstdout: {}\nstderr: {}",
            run.stdout, run.stderr
        );
        run
    }

    fn marker(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// `slim mcp add <name> <options> -- <this test binary as a server>`.
    fn add_fixture(&self, name: &str, options: &[&str]) -> Run {
        let exe = std::env::current_exe().expect("exe");
        let exe = exe.to_str().expect("utf-8 path");
        let mut args = vec!["add", name];
        args.extend_from_slice(options);
        args.extend_from_slice(&["--", exe, "--exact", "mcp_cli_stdio_fixture", "--ignored"]);
        self.ok(&args)
    }

    fn toml(&self, path: &Path) -> toml::Table {
        fs::read_to_string(path)
            .unwrap_or_default()
            .parse()
            .expect("valid toml")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn server<'a>(document: &'a Value, name: &str) -> &'a Value {
    document["servers"]
        .as_array()
        .expect("servers")
        .iter()
        .find(|server| server["name"] == name)
        .unwrap_or_else(|| panic!("no server {name} in {document}"))
}

// ---------------------------------------------------------------------------
// Help and usage
// ---------------------------------------------------------------------------

#[test]
fn help_describes_every_command_and_needs_no_command_word() {
    let sandbox = Sandbox::new("help");
    for args in [&[][..], &["--help"], &["help"], &["list", "--help"]] {
        let run = sandbox.ok(args);
        for word in [
            "add",
            "remove",
            "enable",
            "disable",
            "list",
            "login",
            "logout",
            "import",
            "trust",
            "untrust",
            "--project",
            "--global",
            "--bearer-token-env-var",
            "--oauth-client-id",
            "--exposure",
            "--json",
        ] {
            assert!(
                run.stdout.contains(word),
                "{args:?} lacks {word}: {}",
                run.stdout
            );
        }
    }
    let top = Command::new(env!("CARGO_BIN_EXE_slim"))
        .arg("--help")
        .output()
        .expect("top-level help");
    assert!(String::from_utf8_lossy(&top.stdout).contains("Slim mcp COMMAND"));
}

#[test]
fn unknown_commands_options_and_missing_arguments_exit_1_with_a_hint() {
    let sandbox = Sandbox::new("usage");
    for args in [
        &["frobnicate"][..],
        &["add"],
        &["add", "only-a-name"],
        &["add", "x", "--bogus", "--", "cmd"],
        &["add", "x", "--url"],
        &["add", "x", "--project", "--global", "--", "cmd"],
        &["remove"],
        &["enable"],
        &["list", "extra"],
        &["list", "--timeout", "0"],
        &["login"],
        &["logout"],
        &["import"],
        &["trust", "extra"],
    ] {
        let run = sandbox.run(args);
        assert_eq!(run.code, 1, "{args:?}: {} {}", run.stdout, run.stderr);
        assert!(run.stdout.is_empty(), "{args:?}: {}", run.stdout);
        assert!(!run.stderr.trim().is_empty(), "{args:?}");
    }
    assert!(
        !sandbox.project().exists(),
        "failed commands must not write"
    );
}

// ---------------------------------------------------------------------------
// add, remove, enable, disable
// ---------------------------------------------------------------------------

#[test]
fn add_writes_the_project_file_by_default_and_merges_into_an_existing_entry() {
    let sandbox = Sandbox::new("add");
    let run = sandbox.ok(&[
        "add",
        "files",
        "--env",
        "ROOT=/data",
        "--cwd",
        "work",
        "--timeout-ms",
        "5000",
        "--exposure",
        "direct",
        "--description",
        "Local files",
        "--lazy",
        "--",
        "node",
        "server.js",
        "--flag",
    ]);
    assert!(
        run.stdout.contains("Added project MCP server \"files\""),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("slim mcp trust"),
        "an untrusted project says so: {}",
        run.stdout
    );
    let table = sandbox.toml(&sandbox.project());
    let entry = &table["mcp"]["servers"]["files"];
    assert_eq!(entry["command"].as_str(), Some("node"));
    assert_eq!(
        entry["args"].as_array().expect("args"),
        &[toml::Value::from("server.js"), toml::Value::from("--flag")]
    );
    assert_eq!(entry["env"]["ROOT"].as_str(), Some("/data"));
    assert_eq!(entry["cwd"].as_str(), Some("work"));
    assert_eq!(entry["timeout_ms"].as_integer(), Some(5000));
    assert_eq!(entry["exposure"].as_str(), Some("direct"));
    assert_eq!(entry["description"].as_str(), Some("Local files"));
    assert_eq!(entry["lazy"].as_bool(), Some(true));
    assert!(!sandbox.global().exists(), "the global file is untouched");

    // Updating keeps what the command line did not mention.
    let run = sandbox.ok(&["add", "files", "--env", "EXTRA=1", "--", "node", "other.js"]);
    assert!(
        run.stdout.contains("Updated project MCP server"),
        "{}",
        run.stdout
    );
    let table = sandbox.toml(&sandbox.project());
    let entry = &table["mcp"]["servers"]["files"];
    assert_eq!(entry["args"].as_array().expect("args").len(), 1);
    assert_eq!(entry["env"]["ROOT"].as_str(), Some("/data"));
    assert_eq!(entry["env"]["EXTRA"].as_str(), Some("1"));
    assert_eq!(entry["description"].as_str(), Some("Local files"));
}

#[test]
fn add_http_global_maps_headers_bearer_and_oauth_options() {
    let sandbox = Sandbox::new("add-http");
    let run = sandbox.ok(&[
        "add",
        "docs",
        "--global",
        "--url",
        "https://mcp.example.com/mcp",
        "--header",
        "X-Team=blue",
        "--oauth-client-id",
        "client-1",
        "--oauth-client-secret",
        "${DOCS_SECRET}",
        "--oauth-callback-port",
        "8765",
        "--oauth-scope",
        "read",
        "--oauth-client-name",
        "Slim",
    ]);
    assert!(
        run.stdout.contains("Added global MCP server \"docs\""),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("slim mcp login docs"),
        "sign-in hint: {}",
        run.stdout
    );
    assert!(
        run.stderr.is_empty(),
        "interpolated secrets draw no warning: {}",
        run.stderr
    );
    assert!(!sandbox.project().exists());
    let table = sandbox.toml(&sandbox.global());
    let entry = &table["mcp"]["servers"]["docs"];
    assert_eq!(entry["url"].as_str(), Some("https://mcp.example.com/mcp"));
    assert_eq!(entry["headers"]["X-Team"].as_str(), Some("blue"));
    assert_eq!(entry["oauth"]["client_id"].as_str(), Some("client-1"));
    assert_eq!(
        entry["oauth"]["client_secret"].as_str(),
        Some("${DOCS_SECRET}")
    );
    assert_eq!(entry["oauth"]["callback_port"].as_integer(), Some(8765));
    assert_eq!(entry["oauth"]["scope"].as_str(), Some("read"));
    assert_eq!(entry["oauth"]["client_name"].as_str(), Some("Slim"));

    let run = sandbox.ok(&[
        "add",
        "api",
        "--global",
        "--url",
        "https://api.example.com/mcp",
        "--bearer-token-env-var",
        "API_TOKEN",
    ]);
    assert!(
        !run.stdout.contains("slim mcp login"),
        "an Authorization header skips OAuth: {}",
        run.stdout
    );
    let table = sandbox.toml(&sandbox.global());
    assert_eq!(
        table["mcp"]["servers"]["api"]["headers"]["Authorization"].as_str(),
        Some("Bearer ${API_TOKEN}")
    );
}

#[test]
fn add_warns_about_plain_credentials_without_echoing_them() {
    let sandbox = Sandbox::new("add-warn");
    let run = sandbox.ok(&[
        "add",
        "api",
        "--global",
        "--url",
        "https://api.example.com/mcp",
        "--header",
        "Authorization=Bearer hunter2-plain",
    ]);
    assert!(run.stderr.contains("plain text"), "{}", run.stderr);
    assert!(!run.stdout.contains("hunter2") && !run.stderr.contains("hunter2"));
}

#[test]
fn add_rejects_invalid_requests_without_touching_the_file() {
    let sandbox = Sandbox::new("add-invalid");
    sandbox.ok(&["add", "alpha-one", "--", "node"]);
    let before = fs::read_to_string(sandbox.project()).expect("project file");
    for args in [
        // names that differ only in - versus _ are the same server
        &["add", "alpha_one", "--", "node"][..],
        &["add", "bad name", "--", "node"],
        &["add", "x", "--header", "A=b", "--", "node"],
        &["add", "x", "--url", "https://x.test", "--cwd", "d"],
        &["add", "x", "--url", "ftp://x.test"],
        &["add", "x", "--timeout-ms", "10", "--", "node"],
        &["add", "x", "--exposure", "wide", "--", "node"],
        &[
            "add",
            "x",
            "--url",
            "https://x.test",
            "--oauth-callback-port",
            "0",
        ],
        &[
            "add",
            "x",
            "--url",
            "https://x.test",
            "--bearer-token-env-var",
            "bad-name",
        ],
    ] {
        let run = sandbox.run(args);
        assert_eq!(run.code, 1, "{args:?}: {}", run.stderr);
        assert!(!run.stderr.trim().is_empty(), "{args:?}");
    }
    assert_eq!(
        fs::read_to_string(sandbox.project()).expect("project file"),
        before
    );
}

#[test]
fn add_fails_cleanly_on_a_config_file_that_is_not_toml() {
    let sandbox = Sandbox::new("add-corrupt");
    fs::write(sandbox.project(), "this is = = not toml").expect("write");
    let run = sandbox.run(&["add", "x", "--", "node"]);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("slim.toml"), "{}", run.stderr);
    assert_eq!(
        fs::read_to_string(sandbox.project()).expect("kept"),
        "this is = = not toml"
    );
}

#[test]
fn remove_deletes_from_the_requested_or_defining_file() {
    let sandbox = Sandbox::new("remove");
    sandbox.ok(&["add", "both", "--", "node", "project.js"]);
    sandbox.ok(&["add", "both", "--global", "--", "node", "global.js"]);
    sandbox.ok(&["add", "only-global", "--global", "--", "node"]);

    // Without a flag: the project file first, and it says the global one remains.
    let run = sandbox.ok(&["remove", "both"]);
    assert!(
        run.stdout.contains("Removed project MCP server \"both\""),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("still defined in the global"),
        "{}",
        run.stdout
    );
    assert!(sandbox.toml(&sandbox.project())["mcp"]["servers"]
        .as_table()
        .expect("servers")
        .is_empty());

    // A flag restricts the search to that file.
    let run = sandbox.run(&["remove", "only-global", "--project"]);
    assert_eq!(run.code, 1);
    assert!(
        run.stderr.contains("--global"),
        "points at the right flag: {}",
        run.stderr
    );
    sandbox.ok(&["remove", "only-global", "--global"]);
    sandbox.ok(&["remove", "both", "--global"]);

    let run = sandbox.run(&["remove", "both"]);
    assert_eq!(run.code, 1);
    assert!(
        run.stderr.contains("No MCP server named \"both\""),
        "{}",
        run.stderr
    );
}

#[test]
fn enable_and_disable_edit_the_defining_file_and_keep_everything_else() {
    let sandbox = Sandbox::new("enable");
    sandbox.ok(&[
        "add",
        "gone",
        "--global",
        "--env",
        "KEEP=1",
        "--description",
        "kept",
        "--",
        "node",
    ]);
    sandbox.ok(&["add", "local", "--", "node"]);

    let run = sandbox.ok(&["disable", "gone"]);
    assert!(
        run.stdout.contains("Disabled MCP server \"gone\""),
        "{}",
        run.stdout
    );
    let table = sandbox.toml(&sandbox.global());
    let entry = &table["mcp"]["servers"]["gone"];
    assert_eq!(entry["enabled"].as_bool(), Some(false));
    assert_eq!(entry["env"]["KEEP"].as_str(), Some("1"));
    assert_eq!(entry["description"].as_str(), Some("kept"));
    assert_eq!(entry["command"].as_str(), Some("node"));
    assert!(
        sandbox.toml(&sandbox.project())["mcp"]["servers"]["local"]
            .get("enabled")
            .is_none(),
        "the project file is not touched for a global server"
    );

    let run = sandbox.ok(&["disable", "gone"]);
    assert!(run.stdout.contains("already disabled"), "{}", run.stdout);
    sandbox.ok(&["enable", "gone"]);
    assert_eq!(
        sandbox.toml(&sandbox.global())["mcp"]["servers"]["gone"]["enabled"].as_bool(),
        Some(true)
    );
    sandbox.ok(&["disable", "local"]);
    assert_eq!(
        sandbox.toml(&sandbox.project())["mcp"]["servers"]["local"]["enabled"].as_bool(),
        Some(false)
    );

    let run = sandbox.run(&["enable", "missing"]);
    assert_eq!(run.code, 1);
    assert!(
        run.stderr.contains("Configured: gone, local"),
        "{}",
        run.stderr
    );
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

#[test]
fn list_with_nothing_configured_succeeds() {
    let sandbox = Sandbox::new("list-empty");
    let run = sandbox.ok(&["list"]);
    assert!(
        run.stdout.contains("No MCP servers configured"),
        "{}",
        run.stdout
    );
    let document = sandbox.ok(&["list", "--json"]).json();
    assert_eq!(document["servers"].as_array().expect("servers").len(), 0);
    assert_eq!(document["errors"].as_array().expect("errors").len(), 0);
}

#[test]
fn list_connects_a_global_stdio_server_and_reports_tools_resources_and_exposure() {
    let sandbox = Sandbox::new("list-ok");
    sandbox.add_fixture(
        "stub",
        &[
            "--global",
            "--description",
            "A stub",
            "--exposure",
            "direct",
            "--env",
            "MCP_CLI_MODE=ok",
        ],
    );
    let run = sandbox.ok(&["list"]);
    assert!(
        run.stdout
            .contains("stub: connected, 2 tools (stdio, global, direct)"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("tools: alpha, beta"), "{}", run.stdout);
    assert!(
        run.stdout.contains("resources: 2, URI templates: 1"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("A stub"), "{}", run.stdout);

    let document = sandbox.ok(&["list", "--json"]).json();
    let row = server(&document, "stub");
    assert_eq!(row["status"], "connected");
    assert_eq!(row["scope"], "global");
    assert_eq!(row["enabled"], true);
    assert_eq!(row["transport"], "stdio");
    assert_eq!(row["exposure"], "direct");
    assert_eq!(row["lazy"], false);
    assert_eq!(row["description"], "A stub");
    assert_eq!(row["tools"], serde_json::json!(["alpha", "beta"]));
    assert_eq!(row["resources"], 2);
    assert_eq!(row["resource_templates"], 1);
    assert_eq!(row["server"]["name"], "cli-fixture");
    assert_eq!(row["server"]["version"], "1.2.3");
    assert_eq!(row["server"]["protocol_version"], "2025-11-25");
    assert!(row["source"]
        .as_str()
        .expect("source")
        .ends_with("slim.toml"));
    assert!(row.get("error").is_none());
}

#[test]
fn list_shows_per_tool_exposure_overrides() {
    let sandbox = Sandbox::new("list-tools");
    sandbox.add_fixture("stub", &["--global"]);
    fs::write(
        sandbox.global(),
        format!(
            "{}\n[mcp.servers.stub.tool_exposure]\nbeta = \"hidden\"\n",
            fs::read_to_string(sandbox.global()).expect("global")
        ),
    )
    .expect("write");
    let document = sandbox.ok(&["list", "--json"]).json();
    assert_eq!(
        server(&document, "stub")["tool_exposure"],
        serde_json::json!({"beta": "hidden"})
    );
    let run = sandbox.ok(&["list"]);
    assert!(run.stdout.contains("beta [hidden]"), "{}", run.stdout);
}

#[test]
fn list_reports_untrusted_project_servers_without_starting_them() {
    let sandbox = Sandbox::new("list-untrusted");
    let marker = sandbox.marker("started.txt");
    sandbox.add_fixture(
        "proj",
        &["--env", &format!("MCP_CLI_MARKER={}", marker.display())],
    );
    let run = sandbox.ok(&["list"]);
    assert!(
        run.stdout.contains("proj: not trusted, not started"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("slim mcp trust"), "{}", run.stdout);
    let document = sandbox.ok(&["list", "--json"]).json();
    assert_eq!(server(&document, "proj")["status"], "untrusted");
    assert_eq!(server(&document, "proj")["scope"], "project");
    assert!(
        !marker.exists(),
        "an untrusted project server must not be started"
    );

    // Trusting starts it.
    let run = sandbox.ok(&["trust"]);
    assert!(run.stdout.contains("Trusted"), "{}", run.stdout);
    assert!(
        run.stdout.contains("proj"),
        "lists what may now start: {}",
        run.stdout
    );
    assert!(sandbox.trust_file().exists(), "the decision is stored");
    let document = sandbox.ok(&["list", "--json"]).json();
    assert_eq!(server(&document, "proj")["status"], "connected");
    assert!(marker.exists(), "a trusted project server starts");

    // Untrusting stops it again; the decision is per workspace.
    fs::remove_file(&marker).expect("reset marker");
    let run = sandbox.ok(&["untrust"]);
    assert!(run.stdout.contains("stay disabled"), "{}", run.stdout);
    let document = sandbox.ok(&["list", "--json"]).json();
    assert_eq!(server(&document, "proj")["status"], "untrusted");
    assert!(!marker.exists());
    let other = Sandbox::new("list-untrusted-other");
    fs::copy(sandbox.project(), other.project()).expect("copy project file");
    let document = Command::new(env!("CARGO_BIN_EXE_slim"))
        .args(["mcp", "list", "--json"])
        .current_dir(&other.workspace)
        .env("SLIM_CONFIG_FILE", other.global())
        .env("SLIM_MCP_TRUST_FILE", sandbox.trust_file())
        .env("SLIM_MCP_AUTH_FILE", other.auth_file())
        .output()
        .expect("spawn");
    let document: Value = serde_json::from_slice(&document.stdout).expect("json");
    assert_eq!(
        server(&document, "proj")["status"],
        "untrusted",
        "another workspace does not inherit the decision"
    );
}

#[test]
fn list_does_not_start_disabled_servers_and_exits_0() {
    let sandbox = Sandbox::new("list-disabled");
    let marker = sandbox.marker("started.txt");
    sandbox.add_fixture(
        "off",
        &[
            "--global",
            "--env",
            &format!("MCP_CLI_MARKER={}", marker.display()),
        ],
    );
    sandbox.ok(&["disable", "off"]);
    let document = sandbox.ok(&["list", "--json"]).json();
    let row = server(&document, "off");
    assert_eq!(row["status"], "disabled");
    assert_eq!(row["enabled"], false);
    assert!(!marker.exists());
    let run = sandbox.ok(&["list"]);
    assert!(run.stdout.contains("off: disabled"), "{}", run.stdout);
}

#[test]
fn list_exits_1_for_a_failed_server_and_never_prints_its_secrets() {
    let sandbox = Sandbox::new("list-failed");
    sandbox.add_fixture(
        "dies",
        &[
            "--global",
            "--env",
            "MCP_CLI_MODE=reject",
            "--env",
            "API_TOKEN=s3cr3t-value-123",
        ],
    );
    sandbox.add_fixture("fine", &["--global"]);
    let run = sandbox.run(&["list"]);
    assert_eq!(run.code, 1, "{} {}", run.stdout, run.stderr);
    assert!(
        run.stdout.contains("fine: connected"),
        "good servers are still listed: {}",
        run.stdout
    );
    assert!(run.stdout.contains("dies: failed"), "{}", run.stdout);
    assert!(!run.stdout.contains("s3cr3t-value-123") && !run.stderr.contains("s3cr3t-value-123"));
    assert!(
        run.stdout.contains("rejected token [REDACTED]"),
        "the server's own diagnostic is shown, redacted: {}",
        run.stdout
    );

    let run = sandbox.run(&["list", "--json"]);
    assert_eq!(run.code, 1);
    assert!(!run.stdout.contains("s3cr3t-value-123"));
    let document = run.json();
    assert_eq!(server(&document, "dies")["status"], "failed");
    assert!(server(&document, "dies")["error"]
        .as_str()
        .is_some_and(|error| !error.is_empty()));
    assert_eq!(server(&document, "fine")["status"], "connected");
}

#[test]
fn list_explains_a_server_that_exits_before_the_handshake() {
    let sandbox = Sandbox::new("list-exit");
    sandbox.add_fixture("quits", &["--global", "--env", "MCP_CLI_MODE=exit"]);
    let run = sandbox.run(&["list"]);
    assert_eq!(run.code, 1);
    assert!(run.stdout.contains("quits: failed"), "{}", run.stdout);
    assert!(run.stdout.contains("connection closed"), "{}", run.stdout);
}

#[test]
fn list_reports_invalid_entries_next_to_the_valid_ones_and_exits_1() {
    let sandbox = Sandbox::new("list-invalid");
    sandbox.add_fixture("fine", &["--global"]);
    let mut global = fs::read_to_string(sandbox.global()).expect("global");
    global.push_str(
        "\n[mcp.servers.both]\ncommand = \"node\"\nurl = \"https://x.test\"\n\n[mcp.servers.\"bad name\"]\ncommand = \"node\"\n",
    );
    fs::write(sandbox.global(), global).expect("write");
    let run = sandbox.run(&["list", "--json"]);
    assert_eq!(run.code, 1);
    let document = run.json();
    assert_eq!(server(&document, "fine")["status"], "connected");
    let errors = document["errors"].as_array().expect("errors");
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(errors
        .iter()
        .any(|error| error.as_str().is_some_and(|e| e.contains("both"))));
    assert!(errors
        .iter()
        .any(|error| error.as_str().is_some_and(|e| e.contains("bad name"))));
    let text = sandbox.run(&["list"]);
    assert_eq!(text.code, 1);
    assert!(
        text.stdout.contains("config error: mcp.servers.both"),
        "{}",
        text.stdout
    );
}

#[test]
fn list_marks_a_server_whose_variable_is_missing_as_invalid() {
    let sandbox = Sandbox::new("list-var");
    sandbox.add_fixture(
        "needs-var",
        &[
            "--global",
            "--env",
            "TOKEN_X=${SLIM_CLI_TEST_MISSING_VARIABLE}",
        ],
    );
    let run = sandbox.run(&["list", "--json"]);
    assert_eq!(run.code, 1);
    let document = run.json();
    assert_eq!(server(&document, "needs-var")["status"], "invalid");
    assert!(server(&document, "needs-var")["error"]
        .as_str()
        .expect("error")
        .contains("SLIM_CLI_TEST_MISSING_VARIABLE"));
}

#[test]
fn list_timeout_bounds_a_server_that_never_answers() {
    let sandbox = Sandbox::new("list-timeout");
    // A listener that accepts and stays silent.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/mcp", listener.local_addr().expect("addr"));
    sandbox.ok(&["add", "silent", "--global", "--url", &url]);
    let begun = Instant::now();
    let run = sandbox.run(&["list", "--timeout", "1", "--json"]);
    assert!(
        begun.elapsed() < Duration::from_secs(20),
        "{:?}",
        begun.elapsed()
    );
    assert_eq!(run.code, 1);
    assert_eq!(server(&run.json(), "silent")["status"], "failed");
    drop(listener);
}

// ---------------------------------------------------------------------------
// import
// ---------------------------------------------------------------------------

const IMPORT_JSON: &str = r#"{
  // VS Code style comment
  "mcpServers": {
    "files": {"command": "npx", "args": ["-y", "server-files"], "env": {"ROOT": "${env:HOME}"}},
    "remote": {"type": "http", "url": "https://remote.example/mcp", "headers": {"Authorization": "Bearer ${REMOTE_TOKEN}"}},
    "legacy": {"type": "sse", "url": "https://old.example/sse"},
  }
}"#;

#[test]
fn import_converts_other_clients_json_and_reports_what_it_skipped() {
    let sandbox = Sandbox::new("import");
    let file = sandbox.root.join("mcp.json");
    fs::write(&file, IMPORT_JSON).expect("write");
    let file = file.to_str().expect("utf-8");

    let run = sandbox.ok(&["import", file]);
    assert!(
        run.stdout
            .contains("Imported 2 servers into the project config"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("files, remote"), "{}", run.stdout);
    assert!(run.stderr.contains("skipped legacy"), "{}", run.stderr);
    let table = sandbox.toml(&sandbox.project());
    let files = &table["mcp"]["servers"]["files"];
    assert_eq!(files["command"].as_str(), Some("npx"));
    assert_eq!(
        files["env"]["ROOT"].as_str(),
        Some("${HOME}"),
        "${{env:X}} becomes ${{X}}"
    );
    assert_eq!(
        table["mcp"]["servers"]["remote"]["headers"]["Authorization"].as_str(),
        Some("Bearer ${REMOTE_TOKEN}")
    );
    assert!(table["mcp"]["servers"].get("legacy").is_none());

    // Existing entries are kept without --force.
    sandbox.ok(&["add", "files", "--description", "mine", "--", "other"]);
    let run = sandbox.ok(&["import", file]);
    assert!(
        run.stdout
            .contains("Kept existing (use --force to replace): files, remote"),
        "{}",
        run.stdout
    );
    assert_eq!(
        sandbox.toml(&sandbox.project())["mcp"]["servers"]["files"]["description"].as_str(),
        Some("mine")
    );
    let run = sandbox.ok(&["import", file, "--force"]);
    assert!(run.stdout.contains("Imported 2 servers"), "{}", run.stdout);
    let table = sandbox.toml(&sandbox.project());
    assert_eq!(
        table["mcp"]["servers"]["files"]["command"].as_str(),
        Some("npx")
    );
    assert!(
        table["mcp"]["servers"]["files"]
            .get("description")
            .is_none(),
        "--force replaces the entry"
    );

    // --global writes the global file.
    sandbox.ok(&["import", file, "--global"]);
    assert!(sandbox.toml(&sandbox.global())["mcp"]["servers"]["remote"].is_table());
}

#[test]
fn import_fails_for_unreadable_invalid_or_fully_rejected_files_and_skips_name_collisions() {
    let sandbox = Sandbox::new("import-fail");
    let run = sandbox.run(&["import", "does-not-exist.json"]);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("cannot read"), "{}", run.stderr);

    let bad = sandbox.root.join("bad.json");
    fs::write(&bad, "{not json").expect("write");
    let run = sandbox.run(&["import", bad.to_str().expect("utf-8")]);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("not valid JSON"), "{}", run.stderr);

    let none = sandbox.root.join("none.json");
    fs::write(
        &none,
        r#"{"mcpServers": {"only": {"type": "sse", "url": "https://x.test/sse"}}}"#,
    )
    .expect("write");
    let run = sandbox.run(&["import", none.to_str().expect("utf-8")]);
    assert_eq!(
        run.code, 1,
        "every server rejected is a failure: {}",
        run.stderr
    );
    assert!(run.stderr.contains("skipped only"), "{}", run.stderr);
    assert!(!sandbox.project().exists());

    // A name that differs only in - versus _ from an existing server.
    sandbox.ok(&["add", "my-server", "--", "node"]);
    let clash = sandbox.root.join("clash.json");
    fs::write(
        &clash,
        r#"{"mcpServers": {"my_server": {"command": "node"}}}"#,
    )
    .expect("write");
    let run = sandbox.run(&["import", clash.to_str().expect("utf-8")]);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("collides"), "{}", run.stderr);
    assert!(sandbox.toml(&sandbox.project())["mcp"]["servers"]
        .get("my_server")
        .is_none());
}

// ---------------------------------------------------------------------------
// login, logout
// ---------------------------------------------------------------------------

#[test]
fn login_and_logout_refuse_what_cannot_sign_in() {
    let sandbox = Sandbox::new("login-refuse");
    sandbox.add_fixture("stdio", &["--global"]);
    sandbox.ok(&[
        "add",
        "keyed",
        "--global",
        "--url",
        "https://x.test/mcp",
        "--bearer-token-env-var",
        "K",
    ]);
    sandbox.ok(&["add", "off", "--global", "--url", "https://y.test/mcp"]);
    sandbox.ok(&["disable", "off"]);
    sandbox.ok(&["add", "proj", "--url", "https://z.test/mcp"]);

    for (command, name, needle) in [
        (
            "login",
            "missing",
            "No MCP server named \"missing\". Configured:",
        ),
        ("logout", "missing", "No MCP server named"),
        ("login", "stdio", "does not use OAuth"),
        ("logout", "stdio", "does not use OAuth"),
        ("login", "keyed", "does not use OAuth"),
        ("login", "off", "disabled"),
        ("login", "proj", "not trusted"),
    ] {
        let run = sandbox.run(&[command, name]);
        assert_eq!(run.code, 1, "{command} {name}: {}", run.stderr);
        assert!(
            run.stderr.contains(needle),
            "{command} {name}: {}",
            run.stderr
        );
    }
    let run = sandbox.run(&["login", "off", "--timeout", "soon"]);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("--timeout"), "{}", run.stderr);
}

#[test]
fn logout_without_stored_credentials_is_a_no_op_that_succeeds() {
    let sandbox = Sandbox::new("logout-empty");
    sandbox.ok(&["add", "docs", "--global", "--url", "https://x.test/mcp"]);
    let run = sandbox.ok(&["logout", "docs"]);
    assert!(
        run.stdout.contains("No stored credentials"),
        "{}",
        run.stdout
    );
}

fn http_get(address: &str, target: &str) -> String {
    let mut stream = TcpStream::connect(address).expect("connect callback");
    write!(
        stream,
        "GET {target} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )
    .expect("request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("response");
    response
}

#[test]
fn login_signs_in_through_the_loopback_callback_and_logout_deletes_the_credentials() {
    use mcp_oauth_mock::{decode_pairs, World};
    let world = World::new();
    let sandbox = Sandbox::new("login");
    sandbox.ok(&["add", "oa", "--global", "--url", &world.mcp_url()]);

    // Needs sign-in: reported with a hint, exit 1, the browser untouched.
    let run = sandbox.run(&["list", "--json"]);
    assert_eq!(run.code, 1, "{} {}", run.stdout, run.stderr);
    assert_eq!(server(&run.json(), "oa")["status"], "needs-auth");
    let run = sandbox.run(&["list"]);
    assert!(run.stdout.contains("slim mcp login oa"), "{}", run.stdout);

    // `login`: prints the URL; the "browser" (this test) hits the callback.
    let mut child = sandbox
        .command(&["login", "oa", "--no-browser", "--timeout", "60"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn login");
    let stdout = child.stdout.take().expect("stdout");
    let mut lines = BufReader::new(stdout).lines();
    let mut url = None;
    for line in lines.by_ref() {
        let line = line.expect("line");
        if line.starts_with("http://") {
            url = Some(line);
            break;
        }
    }
    let url = url.expect("the authorization URL is printed");
    let query = url.split_once('?').expect("query").1;
    let pairs = decode_pairs(query);
    assert_eq!(pairs["response_type"], "code");
    assert_eq!(pairs["code_challenge_method"], "S256");
    let redirect = pairs["redirect_uri"].clone();
    let address = redirect
        .strip_prefix("http://")
        .and_then(|rest| rest.split_once('/'))
        .expect("loopback redirect")
        .0
        .to_owned();
    let response = http_get(
        &address,
        &format!("/callback?code=code-1&state={}", pairs["state"]),
    );
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let rest: Vec<String> = lines.map(|line| line.expect("line")).collect();
    let status = child.wait().expect("login exits");
    assert_eq!(status.code(), Some(0), "{rest:?}");
    assert!(
        rest.iter()
            .any(|line| line.contains("Signed in to MCP server \"oa\" (1 tools)")),
        "{rest:?}"
    );
    let auth = fs::read_to_string(sandbox.auth_file()).expect("credentials stored");
    assert!(
        auth.contains("at-1"),
        "the token is stored in the credential file"
    );

    // Connected now, and a second login has nothing to do.
    let document = sandbox.ok(&["list", "--json"]).json();
    assert_eq!(server(&document, "oa")["status"], "connected");
    assert_eq!(
        server(&document, "oa")["tools"],
        serde_json::json!(["echo"])
    );
    let run = sandbox.ok(&["login", "oa", "--no-browser"]);
    assert!(run.stdout.contains("no sign-in needed"), "{}", run.stdout);

    // Logout deletes the credentials and the server needs sign-in again.
    let run = sandbox.ok(&["logout", "oa"]);
    assert!(
        run.stdout.contains("stored credentials deleted"),
        "{}",
        run.stdout
    );
    assert!(!fs::read_to_string(sandbox.auth_file())
        .expect("store")
        .contains("at-1"));
    let run = sandbox.run(&["list", "--json"]);
    assert_eq!(run.code, 1);
    assert_eq!(server(&run.json(), "oa")["status"], "needs-auth");
    assert!(
        !run.stdout.contains("at-1") && !run.stdout.contains("rt-1"),
        "tokens never reach the output"
    );
}

#[test]
fn login_times_out_when_nobody_completes_it() {
    let world = mcp_oauth_mock::World::new();
    let sandbox = Sandbox::new("login-timeout");
    sandbox.ok(&["add", "oa", "--global", "--url", &world.mcp_url()]);
    let begun = Instant::now();
    let run = sandbox.run(&["login", "oa", "--no-browser", "--timeout", "1"]);
    assert_eq!(run.code, 1, "{} {}", run.stdout, run.stderr);
    assert!(
        run.stderr.contains("not completed within 1 seconds"),
        "{}",
        run.stderr
    );
    assert!(
        run.stdout.contains("Sign in to MCP server \"oa\""),
        "{}",
        run.stdout
    );
    assert!(begun.elapsed() < Duration::from_secs(30));
}

// ---------------------------------------------------------------------------
// Terminal safety, help, command updates, enabling a global server
// ---------------------------------------------------------------------------

#[test]
fn terminal_escapes_from_projects_and_servers_never_reach_the_terminal() {
    let sandbox = Sandbox::new("escapes");
    sandbox.add_fixture("proj", &["--env", "MCP_CLI_MODE=escapes"]);
    // A cloned repository's entry that tries to overwrite its own command line.
    sandbox.ok(&[
        "add",
        "spoof",
        "--",
        "node",
        "server.js",
        "\u{1b}[1A\u{1b}[2K  echo harmless",
    ]);
    let run = sandbox.ok(&["list"]);
    assert!(!run.stdout.contains('\u{1b}'), "{:?}", run.stdout);
    assert!(run.stdout.contains("echo harmless"), "{}", run.stdout);
    let run = sandbox.ok(&["trust"]);
    assert!(
        !run.stdout.contains('\u{1b}') && !run.stderr.contains('\u{1b}'),
        "{:?}",
        run.stdout
    );
    // A server's tool name is cleaned for the terminal, not for the JSON.
    let run = sandbox.run(&["list"]);
    assert!(!run.stdout.contains('\u{1b}'), "{:?}", run.stdout);
    assert!(run.stdout.contains("badname"), "{}", run.stdout);
    let run = sandbox.run(&["list", "--json"]);
    assert_eq!(server(&run.json(), "proj")["tools"][0], "bad\u{1b}[2Jname");
    assert!(!run.stdout.contains('\u{1b}'), "JSON stays escaped");
}

#[test]
fn help_is_recognized_only_where_an_option_may_stand() {
    let sandbox = Sandbox::new("help-position");
    sandbox.ok(&["add", "tool", "--", "some-server", "--help"]);
    let table = sandbox.toml(&sandbox.project());
    assert_eq!(
        table["mcp"]["servers"]["tool"]["args"]
            .as_array()
            .expect("args"),
        &[toml::Value::from("--help")]
    );
    // Without `--`, anything after the command word is the command's too.
    let run = sandbox.ok(&["add", "other", "cmd", "-h"]);
    assert!(!run.stdout.contains("Usage:"), "{}", run.stdout);
    let table = sandbox.toml(&sandbox.project());
    assert_eq!(
        table["mcp"]["servers"]["other"]["args"]
            .as_array()
            .expect("args"),
        &[toml::Value::from("-h")]
    );
    // As an option it is still help, for every command.
    for command in [
        "add", "remove", "enable", "disable", "list", "login", "logout", "import", "trust",
        "untrust",
    ] {
        let run = sandbox.ok(&[command, "--help"]);
        assert!(run.stdout.contains("Usage:"), "{command}: {}", run.stdout);
    }
    // A value that looks like the flag is a value.
    sandbox.ok(&["add", "described", "--description", "-h", "--", "cmd"]);
    let table = sandbox.toml(&sandbox.project());
    assert_eq!(
        table["mcp"]["servers"]["described"]["description"].as_str(),
        Some("-h")
    );
}

#[test]
fn a_new_command_replaces_the_old_commands_arguments() {
    let sandbox = Sandbox::new("add-new-command");
    sandbox.ok(&[
        "add",
        "files",
        "--",
        "npx",
        "-y",
        "@modelcontextprotocol/server-filesystem",
        ".",
    ]);
    let run = sandbox.ok(&["add", "files", "--", "node"]);
    assert!(run.stdout.contains("replaces"), "{}", run.stdout);
    let table = sandbox.toml(&sandbox.project());
    let entry = &table["mcp"]["servers"]["files"];
    assert_eq!(entry["command"].as_str(), Some("node"));
    assert!(entry.get("args").is_none(), "{entry:?}");
}

#[test]
fn enabling_a_server_a_project_only_disabled_does_not_make_it_a_project_server() {
    let sandbox = Sandbox::new("enable-override");
    sandbox.add_fixture("docs", &["--global"]);
    fs::write(sandbox.project(), "[mcp.servers.docs]\nenabled = false\n").expect("override");
    let document = sandbox.ok(&["list", "--json"]).json();
    assert_eq!(server(&document, "docs")["enabled"], false);

    sandbox.ok(&["enable", "docs"]);
    let table = sandbox.toml(&sandbox.project());
    assert!(
        table
            .get("mcp")
            .and_then(|mcp| mcp.get("servers"))
            .and_then(|servers| servers.get("docs"))
            .is_none(),
        "the override is lifted, not turned into a definition: {table:?}"
    );
    // The workspace was never trusted, and the global server still runs.
    let document = sandbox.ok(&["list", "--json"]).json();
    assert_eq!(server(&document, "docs")["status"], "connected");
    assert_eq!(server(&document, "docs")["scope"], "global");
}
