//! Builds the application-scoped MCP manager from layered `[mcp]` config.
//! The TUI owns one manager for its lifetime; headless runs own one per turn.
//! Connections stay lazy: building a manager never spawns a process or
//! opens a socket.
//!
//! Turning config into runtime specs is where the trust gate and value
//! interpolation live: servers defined by an untrusted project `slim.toml`
//! become `Untrusted` placeholders (no environment read, no command run), and
//! `env`/`headers`/`oauth.client_secret` values are resolved here so a
//! missing variable or failing command blocks only that server.

pub(crate) mod import;
pub(crate) mod interpolate;
pub(crate) mod oauth;
pub(crate) mod trust;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use slim_core::mcp::{
    McpManager, McpOAuthSpec, McpServerBlock, McpServerOptions, McpServerSpec, McpTransport,
};
use slim_core::process::ExecutableResolver;

use crate::config::{McpConfig, McpOrigin, McpServerConfig};
use interpolate::{resolve_value, SystemSource, ValueSource};
use trust::{TrustDecision, TrustStore};

/// Result of turning merged config into runtime specs.
#[derive(Debug, Default)]
pub struct McpLoad {
    pub specs: BTreeMap<String, McpServerSpec>,
    /// Project-defined servers held back until the workspace is trusted.
    pub untrusted: Vec<String>,
    /// Servers whose values could not be resolved: `(name, reason)`.
    pub invalid: Vec<(String, String)>,
    /// The user chose "never" for this workspace; do not nag.
    pub denied: bool,
    /// The trust store could not be read (treated as not trusted).
    pub trust_error: Option<String>,
    /// `[mcp] startup_wait_ms`: how long the first model request waits for
    /// direct-exposure servers that are still connecting.
    pub startup_wait: Duration,
}

fn home_directory() -> Option<PathBuf> {
    let variable = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// `~` and `~/...` name the home directory (command, arguments and cwd).
fn expand_home(text: &str, home: Option<&Path>) -> String {
    let Some(home) = home else {
        return text.to_owned();
    };
    if text == "~" {
        return home.to_string_lossy().into_owned();
    }
    match text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
        Some(rest) => home.join(rest).to_string_lossy().into_owned(),
        None => text.to_owned(),
    }
}

fn resolve_map(
    owner: &str,
    values: &BTreeMap<String, String>,
    workspace: &Path,
    source: &dyn ValueSource,
) -> Result<BTreeMap<String, String>, String> {
    values
        .iter()
        .map(|(key, raw)| {
            resolve_value(raw, workspace, source)
                .map(|value| (key.clone(), value))
                .map_err(|error| format!("{owner} \"{key}\": {error}"))
        })
        .collect()
}

struct Resolved {
    env: BTreeMap<String, String>,
    headers: BTreeMap<String, String>,
    client_secret: Option<String>,
}

fn resolve_server(
    server: &McpServerConfig,
    workspace: &Path,
    source: &dyn ValueSource,
) -> Result<Resolved, String> {
    let env = resolve_map("env", &server.env, workspace, source)?;
    let headers = resolve_map("header", &server.headers, workspace, source)?;
    let client_secret = server
        .oauth
        .as_ref()
        .and_then(|oauth| oauth.client_secret.as_deref())
        .map(|raw| {
            resolve_value(raw, workspace, source)
                .map_err(|error| format!("oauth.client_secret: {error}"))
        })
        .transpose()?;
    Ok(Resolved {
        env,
        headers,
        client_secret,
    })
}

fn build_spec(
    name: &str,
    server: &McpServerConfig,
    workspace: &Path,
    trusted: bool,
    source: &dyn ValueSource,
    home: Option<&Path>,
) -> Option<(McpServerSpec, Option<McpServerBlock>)> {
    let (block, resolved) = if server.origin == McpOrigin::Project && !trusted {
        (Some(McpServerBlock::Untrusted), None)
    } else if !server.enabled {
        // Disabled servers never read the environment or run commands.
        (None, None)
    } else {
        match resolve_server(server, workspace, source) {
            Ok(resolved) => (None, Some(resolved)),
            Err(error) => (Some(McpServerBlock::Invalid(error)), None),
        }
    };
    let (env, headers, client_secret) = match resolved {
        Some(resolved) => (resolved.env, resolved.headers, resolved.client_secret),
        None => (BTreeMap::new(), BTreeMap::new(), None),
    };
    let transport = match (&server.command, &server.url) {
        (Some(command), None) => McpTransport::Stdio {
            command: expand_home(command, home),
            args: server
                .args
                .iter()
                .map(|arg| expand_home(arg, home))
                .collect(),
            env,
        },
        (None, Some(url)) => McpTransport::Http {
            url: url.clone(),
            headers,
        },
        _ => return None,
    };
    let oauth = server.oauth.as_ref().map(|oauth| McpOAuthSpec {
        client_id: oauth.client_id.clone(),
        client_secret,
        callback_port: oauth.callback_port,
        scope: oauth.scope.clone(),
        client_name: oauth.client_name.clone(),
        auth_server_metadata_url: oauth.auth_server_metadata_url.clone(),
    });
    // HTTP servers without an Authorization header of their own sign in with
    // OAuth when they ask for it (never automatically: `/mcp login`).
    let auth = match &transport {
        McpTransport::Http { url, headers }
            if server.enabled
                && block.is_none()
                && !headers
                    .keys()
                    .any(|key| key.eq_ignore_ascii_case("authorization")) =>
        {
            oauth::build_auth(name, url, oauth.clone().unwrap_or_default())
        }
        _ => None,
    };
    let spec = McpServerSpec {
        name: name.to_owned(),
        transport,
        enabled: server.enabled,
        timeout: Duration::from_millis(server.timeout_ms),
        options: McpServerOptions {
            cwd: server
                .cwd
                .as_deref()
                .map(|cwd| PathBuf::from(expand_home(cwd, home))),
            description: server.description.clone(),
            exposure: server.exposure,
            tool_exposure: server.tool_exposure.clone(),
            lazy: server.lazy,
            oauth,
            block: block.clone(),
            auth,
        },
    };
    Some((spec, block))
}

/// Converts merged config into runtime specs. `trusted` says whether
/// project-defined servers may start. `load_layered` validates before this
/// runs; a server with neither transport is skipped defensively.
pub(crate) fn specs_from_config(
    config: &McpConfig,
    workspace: &Path,
    trusted: bool,
    source: &dyn ValueSource,
) -> McpLoad {
    let home = home_directory();
    let mut load = McpLoad {
        startup_wait: config.startup_wait(),
        ..McpLoad::default()
    };
    for (name, server) in &config.servers {
        let Some((spec, block)) =
            build_spec(name, server, workspace, trusted, source, home.as_deref())
        else {
            continue;
        };
        match block {
            Some(McpServerBlock::Untrusted) => load.untrusted.push(name.clone()),
            Some(McpServerBlock::Invalid(error)) => load.invalid.push((name.clone(), error)),
            None => {}
        }
        load.specs.insert(name.clone(), spec);
    }
    load
}

/// Loads layered config for `workspace`, applies the trust decision and
/// builds specs. `session_trust` is `--trust-project` (this run only).
/// Errors are configuration errors the caller must surface.
pub(crate) fn load_mcp(workspace: &Path, session_trust: bool) -> Result<McpLoad, String> {
    let layered = crate::config::load_layered_for(workspace)?;
    Ok(load_mcp_from(&layered, workspace, session_trust))
}

/// [`load_mcp`] for config that is already loaded (the `slim mcp` commands load
/// leniently so one invalid entry does not hide the rest).
pub(crate) fn load_mcp_from(
    layered: &crate::config::LayeredConfig,
    workspace: &Path,
    session_trust: bool,
) -> McpLoad {
    let needs_trust = layered
        .mcp
        .servers
        .values()
        .any(|server| server.origin == McpOrigin::Project);
    let mut trust_error = None;
    let mut denied = false;
    let trusted = if !needs_trust || session_trust {
        true
    } else {
        match TrustStore::default_store().and_then(|store| store.decision(workspace)) {
            Ok(Some(TrustDecision::Trusted)) => true,
            Ok(Some(TrustDecision::Denied)) => {
                denied = true;
                false
            }
            Ok(None) => false,
            Err(error) => {
                trust_error = Some(error);
                false
            }
        }
    };
    let mut load = specs_from_config(&layered.mcp, workspace, trusted, &SystemSource);
    load.denied = denied;
    load.trust_error = trust_error;
    load
}

/// Records the user's decision for the workspace's project MCP servers
/// (`None` forgets it). Returns the store path for messages.
pub(crate) fn set_project_trust(
    workspace: &Path,
    decision: Option<TrustDecision>,
) -> Result<PathBuf, String> {
    let store = TrustStore::default_store()?;
    store.set(workspace, decision)?;
    Ok(store.path().to_path_buf())
}

/// Human-readable lines for servers that did not start, for stderr and
/// startup notices. Never contains configuration values.
pub(crate) fn load_diagnostics(load: &McpLoad, trust_hint: &str) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(error) = &load.trust_error {
        lines.push(format!("{error}; project MCP servers stay disabled"));
    }
    if !load.untrusted.is_empty() && !load.denied {
        lines.push(format!(
            "project MCP servers not started (workspace not trusted): {}. {trust_hint}",
            load.untrusted.join(", ")
        ));
    }
    for (name, error) in &load.invalid {
        lines.push(format!("MCP server {name} not started: {error}"));
    }
    lines
}

/// Creates the shared manager, or `None` when no server is configured.
/// `cwd` is the workspace root stdio servers are spawned in. Building spawns
/// nothing; [`McpManager::start_background_connect`] starts the connections.
pub(crate) fn build_mcp_manager(load: &McpLoad, cwd: &Path) -> Option<Arc<McpManager>> {
    if load.specs.is_empty() {
        return None;
    }
    let mut manager = McpManager::new(
        load.specs.clone(),
        cwd.to_path_buf(),
        ExecutableResolver::default(),
    );
    if let Some(path) = mcp_log_path() {
        manager = manager.with_log_path(path);
    }
    manager.set_startup_wait(load.startup_wait);
    Some(Arc::new(manager))
}

/// Before the first model request of a session: waits (bounded by
/// `[mcp] startup_wait_ms`) for the direct-exposure servers that are still
/// connecting in the background. Returns the notice to show when some did not
/// make it in time; the run then proceeds without them. `None` when nothing
/// had to be waited for, the wait is disabled, or the run was cancelled.
pub(crate) async fn await_direct_startup(
    manager: &McpManager,
    cancellation: Option<&slim_core::runtime::CancellationToken>,
) -> Option<String> {
    let wait = manager.startup_wait();
    if wait.is_zero() {
        return None;
    }
    let report = match cancellation {
        Some(token) => tokio::select! {
            report = manager.wait_for_direct_servers(wait) => report,
            () = token.cancelled() => return None,
        },
        None => manager.wait_for_direct_servers(wait).await,
    };
    if report.still_connecting.is_empty() {
        return None;
    }
    Some(format!(
        "MCP servers still connecting after {}s, continuing without them: {}. Their tools appear once they are ready.",
        wait.as_secs().max(1),
        report.still_connecting.join(", ")
    ))
}

/// `<Slim config dir>/logs/mcp.log`: where servers' log notifications go.
fn mcp_log_path() -> Option<PathBuf> {
    let config = crate::config::global_config_path()?;
    Some(config.parent()?.join("logs").join("mcp.log"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FileConfig, LayeredConfig};
    use slim_core::mcp::{McpExposure, McpServerStatus};
    use std::sync::Mutex;

    /// A manager with one direct-exposure server that never answers its
    /// handshake (the listener accepts and stays silent), connecting in the
    /// background.
    async fn manager_with_a_silent_direct_server(
        wait: Duration,
    ) -> (Arc<McpManager>, std::net::TcpListener) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let mut spec = McpServerSpec::new(
            "slow",
            McpTransport::Http {
                url: format!("http://{}", listener.local_addr().expect("addr")),
                headers: BTreeMap::new(),
            },
        );
        spec.options.exposure = slim_core::mcp::McpExposure::Direct;
        let load = McpLoad {
            specs: BTreeMap::from([("slow".to_owned(), spec)]),
            startup_wait: wait,
            ..McpLoad::default()
        };
        let manager = build_mcp_manager(&load, Path::new(".")).expect("manager");
        assert_eq!(manager.startup_wait(), wait);
        assert_eq!(manager.start_background_connect(), 1);
        (manager, listener)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_first_request_wait_reports_direct_servers_that_are_not_ready() {
        let (manager, _listener) =
            manager_with_a_silent_direct_server(Duration::from_millis(150)).await;
        let notice = await_direct_startup(&manager, None)
            .await
            .expect("a notice when the wait runs out");
        assert!(
            notice.contains("slow") && notice.contains("continuing without"),
            "{notice}"
        );
        // Spent: the next run proceeds without waiting or repeating it.
        assert_eq!(await_direct_startup(&manager, None).await, None);
        manager.disconnect_all().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_zero_wait_disables_the_wait_and_cancellation_ends_it() {
        let (manager, _listener) = manager_with_a_silent_direct_server(Duration::ZERO).await;
        assert_eq!(await_direct_startup(&manager, None).await, None);
        manager.set_startup_wait(Duration::from_secs(30));
        let token = slim_core::runtime::CancellationToken::new();
        let trigger = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.cancel();
        });
        let begun = std::time::Instant::now();
        assert_eq!(await_direct_startup(&manager, Some(&token)).await, None);
        assert!(begun.elapsed() < Duration::from_secs(5));
        manager.disconnect_all().await;
    }

    #[test]
    fn startup_wait_comes_from_config_with_a_ten_second_default() {
        let none = config_from("", "");
        assert_eq!(
            specs_from_config(&none, Path::new("."), true, &SystemSource).startup_wait,
            Duration::from_secs(10)
        );
        let mut layered = LayeredConfig::default();
        crate::config::merge_layer(
            &mut layered,
            FileConfig::parse("[mcp]\nstartup_wait_ms = 2500\n").unwrap(),
        );
        layered.mcp.validate().expect("valid");
        assert_eq!(
            specs_from_config(&layered.mcp, Path::new("."), true, &SystemSource).startup_wait,
            Duration::from_millis(2500)
        );
    }

    #[derive(Default)]
    struct Source {
        vars: BTreeMap<&'static str, &'static str>,
        command_output: Option<&'static str>,
        commands: Mutex<Vec<String>>,
        env_reads: Mutex<Vec<String>>,
    }

    impl ValueSource for Source {
        fn env_var(&self, name: &str) -> Option<String> {
            self.env_reads.lock().unwrap().push(name.to_owned());
            self.vars.get(name).map(|value| (*value).to_owned())
        }

        fn run_command(&self, command: &str, _cwd: &Path) -> Result<String, String> {
            self.commands.lock().unwrap().push(command.to_owned());
            self.command_output
                .map(str::to_owned)
                .ok_or_else(|| "shell command exited with status 1".to_owned())
        }
    }

    fn config_from(global: &str, project: &str) -> McpConfig {
        let mut layered = LayeredConfig::default();
        crate::config::merge_layer(&mut layered, FileConfig::parse(global).unwrap());
        crate::config::merge_layer_as(
            &mut layered,
            FileConfig::parse(project).unwrap(),
            McpOrigin::Project,
        );
        layered.mcp.validate().expect("valid");
        layered.mcp
    }

    #[test]
    fn untrusted_project_servers_are_placeholders_that_read_nothing() {
        let config = config_from(
            "[mcp.servers.global]\ncommand = \"g\"\n",
            "[mcp.servers.proj]\ncommand = \"p\"\n[mcp.servers.proj.env]\nTOKEN = \"!secret-command\"\nOTHER = \"$SOME_VAR\"\n",
        );
        let source = Source::default();
        let load = specs_from_config(&config, Path::new("."), false, &source);
        assert_eq!(load.untrusted, ["proj"]);
        assert!(
            source.commands.lock().unwrap().is_empty(),
            "no command may run"
        );
        assert!(source.env_reads.lock().unwrap().is_empty(), "no env read");
        let proj = &load.specs["proj"];
        assert_eq!(proj.options.block, Some(McpServerBlock::Untrusted));
        assert!(matches!(&proj.transport, McpTransport::Stdio { env, .. } if env.is_empty()));
        assert_eq!(load.specs["global"].options.block, None);

        // The manager lists it, refuses to start it, and keeps the status.
        let manager = McpManager::new(load.specs.clone(), PathBuf::from("."), Default::default());
        let status = manager
            .statuses()
            .into_iter()
            .find(|info| info.name == "proj")
            .unwrap();
        assert!(matches!(status.status, McpServerStatus::Untrusted));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let error = runtime.block_on(manager.test("proj")).unwrap_err();
        assert!(error.to_string().contains("not trusted"), "{error}");
    }

    #[test]
    fn trusted_project_resolves_values_and_registers_credentials_as_sensitive() {
        let config = config_from(
            "",
            "[mcp.servers.api]\nurl = \"https://example.com/mcp\"\n[mcp.servers.api.headers]\nAuthorization = \"Bearer ${API_TOKEN}\"\n\"X-Region\" = \"eu-$REGION\"\n[mcp.servers.api.oauth]\nclient_id = \"cid\"\nclient_secret = \"!vault read secret\"\n",
        );
        let source = Source {
            vars: BTreeMap::from([("API_TOKEN", "tok-123"), ("REGION", "west")]),
            command_output: Some("cs-456"),
            ..Source::default()
        };
        let load = specs_from_config(&config, Path::new("."), true, &source);
        assert!(load.invalid.is_empty() && load.untrusted.is_empty());
        let spec = &load.specs["api"];
        let McpTransport::Http { headers, .. } = &spec.transport else {
            panic!("http");
        };
        assert_eq!(headers["Authorization"], "Bearer tok-123");
        assert_eq!(headers["X-Region"], "eu-west");
        let oauth = spec.options.oauth.as_ref().unwrap();
        assert_eq!(oauth.client_secret.as_deref(), Some("cs-456"));
        assert_eq!(
            *source.commands.lock().unwrap(),
            vec!["vault read secret".to_owned()]
        );

        // Credential-like header values and the OAuth client secret are
        // registered for redaction; the non-credential region value is not.
        let manager = McpManager::new(load.specs.clone(), PathBuf::from("."), Default::default());
        let secrets = manager.sensitive_values();
        assert!(
            secrets.contains(&"Bearer tok-123".to_owned()),
            "{secrets:?}"
        );
        assert!(secrets.contains(&"cs-456".to_owned()), "{secrets:?}");
        assert!(!secrets.contains(&"eu-west".to_owned()), "{secrets:?}");
        // The secret never shows in Debug output of the spec options.
        assert!(!format!("{:?}", spec.options).contains("cs-456"));
    }

    #[test]
    fn global_servers_are_always_trusted_and_project_disable_needs_no_trust() {
        let config = config_from(
            "[mcp.servers.g]\ncommand = \"g\"\n[mcp.servers.g.env]\nTOKEN = \"!cmd\"\n",
            "[mcp.servers.g]\nenabled = false\n",
        );
        assert_eq!(config.servers["g"].origin, McpOrigin::Global);
        let source = Source {
            command_output: Some("v"),
            ..Source::default()
        };
        let load = specs_from_config(&config, Path::new("."), false, &source);
        assert!(load.untrusted.is_empty());
        assert!(!load.specs["g"].enabled);
        // A disabled server does not even run its credential command.
        assert!(source.commands.lock().unwrap().is_empty());

        // Any other project override of a global server needs trust.
        let config = config_from(
            "[mcp.servers.g]\ncommand = \"g\"\n",
            "[mcp.servers.g]\nargs = [\"--evil\"]\n",
        );
        assert_eq!(config.servers["g"].origin, McpOrigin::Project);
        let load = specs_from_config(&config, Path::new("."), false, &Source::default());
        assert_eq!(load.untrusted, ["g"]);
    }

    #[test]
    fn unresolvable_values_block_only_that_server_with_a_redacted_reason() {
        let config = config_from(
            "[mcp.servers.bad]\ncommand = \"b\"\n[mcp.servers.bad.env]\nTOKEN = \"$NOT_SET\"\n[mcp.servers.good]\ncommand = \"g\"\n[mcp.servers.cmd]\nurl = \"https://x/mcp\"\n[mcp.servers.cmd.headers]\nAuthorization = \"!echo hunter2\"\n",
            "",
        );
        let load = specs_from_config(&config, Path::new("."), true, &Source::default());
        let invalid: BTreeMap<_, _> = load.invalid.iter().cloned().collect();
        assert_eq!(
            invalid["bad"],
            "env \"TOKEN\": environment variable NOT_SET is not set"
        );
        assert_eq!(
            invalid["cmd"],
            "header \"Authorization\": shell command exited with status 1"
        );
        assert!(!invalid["cmd"].contains("hunter2"));
        assert_eq!(load.specs["good"].options.block, None);
        let manager = McpManager::new(load.specs, PathBuf::from("."), Default::default());
        let status = manager
            .statuses()
            .into_iter()
            .find(|info| info.name == "bad")
            .unwrap();
        assert!(
            matches!(&status.status, McpServerStatus::Failed { error } if error.contains("NOT_SET")),
            "{:?}",
            status.status
        );
    }

    #[test]
    fn new_fields_reach_the_spec_and_home_is_expanded() {
        let config = config_from(
            "[mcp.servers.s]\ncommand = \"~/bin/tool\"\nargs = [\"~/data\", \"plain\"]\ncwd = \"~/work\"\ndescription = \"Does things\"\nexposure = \"direct\"\nlazy = true\ntimeout_ms = 5000\n[mcp.servers.s.tool_exposure]\n\"get_*\" = \"hidden\"\n",
            "",
        );
        let home = Path::new("/h");
        let (spec, _) = build_spec(
            "s",
            &config.servers["s"],
            Path::new("."),
            true,
            &Source::default(),
            Some(home),
        )
        .unwrap();
        let McpTransport::Stdio { command, args, .. } = &spec.transport else {
            panic!("stdio");
        };
        assert_eq!(Path::new(command), home.join("bin/tool"));
        assert_eq!(Path::new(&args[0]), home.join("data"));
        assert_eq!(args[1], "plain");
        assert_eq!(
            spec.options.cwd.as_deref(),
            Some(home.join("work").as_path())
        );
        assert_eq!(spec.options.description.as_deref(), Some("Does things"));
        assert_eq!(spec.options.exposure, McpExposure::Direct);
        assert!(spec.options.lazy);
        assert_eq!(spec.timeout, Duration::from_millis(5000));
        assert_eq!(spec.options.tool_exposure["get_*"], McpExposure::Hidden);
    }

    #[test]
    fn default_timeout_is_sixty_seconds() {
        let config = config_from("[mcp.servers.s]\ncommand = \"x\"\n", "");
        assert_eq!(config.servers["s"].timeout_ms, 60_000);
        let load = specs_from_config(&config, Path::new("."), true, &Source::default());
        assert_eq!(load.specs["s"].timeout, Duration::from_secs(60));
    }

    #[test]
    fn diagnostics_describe_untrusted_and_invalid_servers_without_values() {
        let load = McpLoad {
            untrusted: vec!["a".into(), "b".into()],
            invalid: vec![(
                "c".into(),
                "env \"K\": environment variable X is not set".into(),
            )],
            ..McpLoad::default()
        };
        let lines = load_diagnostics(&load, "Trust it with /mcp trust.");
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("a, b") && lines[0].contains("/mcp trust"));
        assert!(lines[1].contains("MCP server c not started"));
        let denied = McpLoad {
            untrusted: vec!["a".into()],
            denied: true,
            ..McpLoad::default()
        };
        assert!(
            load_diagnostics(&denied, "x").is_empty(),
            "denied: no nagging"
        );
    }

    #[test]
    fn cwd_is_validated_by_the_manager_before_spawning() {
        let mut spec = McpServerSpec::new(
            "s",
            McpTransport::Stdio {
                command: "definitely-not-a-real-binary".into(),
                args: Vec::new(),
                env: BTreeMap::new(),
            },
        );
        spec.options.cwd = Some(PathBuf::from("no-such-subdirectory"));
        let manager = McpManager::new(
            BTreeMap::from([("s".to_owned(), spec)]),
            std::env::temp_dir(),
            Default::default(),
        );
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let error = runtime.block_on(manager.test("s")).unwrap_err();
        assert!(
            error.to_string().contains("cwd is not a directory"),
            "{error}"
        );
    }

    #[test]
    fn oauth_is_for_enabled_trusted_http_servers_without_an_authorization_header() {
        let config = config_from(
            "",
            "[mcp.servers.oauth]\nurl = \"https://a.example/mcp\"\n\
             [mcp.servers.keyed]\nurl = \"https://b.example/mcp\"\n\
             [mcp.servers.keyed.headers]\nAuthorization = \"Bearer x\"\n\
             [mcp.servers.lower]\nurl = \"https://c.example/mcp\"\n\
             [mcp.servers.lower.headers]\nauthorization = \"Bearer y\"\n\
             [mcp.servers.other]\nurl = \"https://d.example/mcp\"\n\
             [mcp.servers.other.headers]\n\"X-Tenant\" = \"t\"\n\
             [mcp.servers.local]\ncommand = \"c\"\n\
             [mcp.servers.off]\nurl = \"https://e.example/mcp\"\nenabled = false\n",
        );
        let load = specs_from_config(&config, Path::new("."), true, &Source::default());
        let has_auth = |name: &str| load.specs[name].options.auth.is_some();
        assert!(has_auth("oauth"));
        assert!(
            has_auth("other"),
            "headers other than Authorization keep OAuth"
        );
        assert!(
            !has_auth("keyed") && !has_auth("lower"),
            "a configured Authorization wins"
        );
        assert!(!has_auth("local"), "stdio servers never use OAuth");
        assert!(!has_auth("off"));
        // An untrusted project's servers carry nothing, OAuth included.
        let untrusted = specs_from_config(&config, Path::new("."), false, &Source::default());
        assert!(untrusted
            .specs
            .values()
            .all(|spec| spec.options.auth.is_none()));
    }

    #[test]
    fn the_oauth_handle_does_not_make_identical_specs_differ() {
        // `reconcile` and `upsert` keep a warm connection (and its token
        // cache) when a reload produces an equal spec.
        let config = config_from("", "[mcp.servers.oauth]\nurl = \"https://a.example/mcp\"\n");
        let first = specs_from_config(&config, Path::new("."), true, &Source::default());
        let second = specs_from_config(&config, Path::new("."), true, &Source::default());
        assert_eq!(first.specs, second.specs);
        let debug = format!("{:?}", first.specs["oauth"]);
        assert!(debug.contains("McpAuthHandle"), "{debug}");
    }
}
