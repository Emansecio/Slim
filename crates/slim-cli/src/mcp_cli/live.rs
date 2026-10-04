//! The `slim mcp` commands that talk to servers: `list`, `login`, `logout`.

use std::collections::BTreeMap;
use std::io::{BufRead, IsTerminal, Write as _};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use slim_core::mcp::{
    McpAuth, McpCancellation, McpError, McpManager, McpOAuthSpec, McpServerBlock, McpServerSpec,
    McpServerStatus,
};
use tokio::sync::{mpsc, watch};

use super::{err, fail, names_of, out, parse_options, Context, Kind, Scope};
use crate::auth::redact_with_secrets;
use crate::config::{project_config_path, McpOrigin, McpServerConfig};
use crate::mcp::{self, oauth, McpLoad};
use crate::oauth::{BrowserLauncher, OAuthError, SystemBrowser};

/// Tool names come from the server unbounded; one listing line shows this much
/// of each.
const MAX_TOOL_NAME_CHARS: usize = 128;

/// How long `list` waits for each server by default.
const DEFAULT_LIST_TIMEOUT: Duration = Duration::from_secs(15);

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|error| format!("cannot start the async runtime: {error}"))
}

fn parse_seconds(value: Option<&str>, default: Duration) -> Result<Duration, String> {
    match value {
        None => Ok(default),
        Some(text) => text
            .parse::<u64>()
            .ok()
            .filter(|seconds| *seconds > 0)
            .map(Duration::from_secs)
            .ok_or_else(|| "--timeout expects a positive number of seconds".to_owned()),
    }
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

/// What connecting to one server taught us.
#[derive(Default)]
struct Probe {
    error: Option<String>,
    /// `(resources, resource templates)` when the server offers resources.
    resources: Option<(usize, usize)>,
    resources_error: Option<String>,
}

async fn probe_server(manager: Arc<McpManager>, name: String, limit: Duration) -> Probe {
    let mut probe = Probe::default();
    match tokio::time::timeout(limit, manager.list_tools(&name)).await {
        Err(_) => {
            probe.error = Some(format!("no answer within {} s", limit.as_secs()));
            return probe;
        }
        Ok(Err(error)) => {
            probe.error = Some(error.to_string());
            return probe;
        }
        Ok(Ok(_)) => {}
    }
    let offers_resources = manager
        .statuses()
        .into_iter()
        .find(|info| info.name == name)
        .and_then(|info| info.handshake)
        .is_some_and(|handshake| handshake.capabilities.get("resources").is_some());
    if !offers_resources {
        return probe;
    }
    let counted = tokio::time::timeout(limit, async {
        let resources = manager
            .all_resources_cancellable(&name, McpCancellation::new())
            .await
            .into_result()?;
        let templates = manager
            .all_resource_templates_cancellable(&name, McpCancellation::new())
            .await
            .into_result()?;
        Ok::<_, McpError>((resources.items.len(), templates.items.len()))
    })
    .await;
    match counted {
        Ok(Ok(counts)) => probe.resources = Some(counts),
        Ok(Err(error)) => probe.resources_error = Some(error.to_string()),
        Err(_) => {
            probe.resources_error = Some(format!("no answer within {} s", limit.as_secs()));
        }
    }
    probe
}

fn clip(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut shortened: String = text.chars().take(max_chars).collect();
    shortened.push('…');
    shortened
}

struct Row {
    name: String,
    scope: &'static str,
    source: String,
    enabled: bool,
    transport: &'static str,
    target: String,
    exposure: &'static str,
    lazy: bool,
    description: Option<String>,
    status: &'static str,
    tools: Vec<String>,
    tool_exposure: BTreeMap<String, &'static str>,
    resources: Option<(usize, usize)>,
    resources_error: Option<String>,
    server: Option<Value>,
    error: Option<String>,
}

impl Row {
    /// Failed in the sense of the exit code: enabled and not running (an
    /// untrusted project server is reported, not a failure).
    fn failed(&self) -> bool {
        self.enabled && !matches!(self.status, "connected" | "untrusted")
    }

    fn to_json(&self) -> Value {
        let mut row = json!({
            "name": self.name,
            "scope": self.scope,
            "source": self.source,
            "enabled": self.enabled,
            "transport": self.transport,
            "target": self.target,
            "exposure": self.exposure,
            "lazy": self.lazy,
            "status": self.status,
            "tools": self.tools,
        });
        let fields = row.as_object_mut().expect("object");
        if let Some(description) = &self.description {
            fields.insert("description".into(), json!(description));
        }
        if !self.tool_exposure.is_empty() {
            fields.insert("tool_exposure".into(), json!(self.tool_exposure));
        }
        if let Some((resources, templates)) = self.resources {
            fields.insert("resources".into(), json!(resources));
            fields.insert("resource_templates".into(), json!(templates));
        }
        if let Some(error) = &self.resources_error {
            fields.insert("resources_error".into(), json!(error));
        }
        if let Some(server) = &self.server {
            fields.insert("server".into(), server.clone());
        }
        if let Some(error) = &self.error {
            fields.insert("error".into(), json!(error));
        }
        row
    }

    fn print(&self) {
        let state = match self.status {
            "connected" => format!(
                "connected, {} tool{}",
                self.tools.len(),
                if self.tools.len() == 1 { "" } else { "s" }
            ),
            "needs-auth" => "needs sign-in".to_owned(),
            "untrusted" => "not trusted, not started".to_owned(),
            other => other.to_owned(),
        };
        let mut facts = vec![self.transport, self.scope, self.exposure];
        if self.lazy {
            facts.push("lazy");
        }
        out!("{}: {state} ({})", self.name, facts.join(", "));
        out!("  {}", self.target);
        if let Some(description) = &self.description {
            out!("  {description}");
        }
        match self.status {
            "needs-auth" => out!("  sign in with: slim mcp login {}", self.name),
            "untrusted" => out!("  allow it with: slim mcp trust"),
            _ => {}
        }
        if !self.tools.is_empty() {
            let tools: Vec<String> = self
                .tools
                .iter()
                .map(|tool| {
                    let shown = clip(tool, MAX_TOOL_NAME_CHARS);
                    match self.tool_exposure.get(tool) {
                        Some(exposure) => format!("{shown} [{exposure}]"),
                        None => shown,
                    }
                })
                .collect();
            out!("  tools: {}", tools.join(", "));
        }
        if let Some((resources, templates)) = self.resources {
            out!("  resources: {resources}, URI templates: {templates}");
        }
        if let Some(error) = &self.resources_error {
            out!("  resources unavailable: {}", error.replace('\n', "\n  "));
        }
        if let Some(error) = &self.error {
            out!("  {}", error.replace('\n', "\n  "));
        }
    }
}

/// What every row is built from.
struct RowContext<'a> {
    load: &'a McpLoad,
    layered: &'a crate::config::LayeredConfig,
    workspace: &'a std::path::Path,
    redact: &'a dyn Fn(&str) -> String,
}

fn build_row(
    name: &str,
    spec: &McpServerSpec,
    context: &RowContext<'_>,
    probe: Option<Probe>,
    info: Option<&slim_core::mcp::McpServerInfo>,
) -> Row {
    let RowContext {
        load,
        layered,
        workspace,
        redact,
    } = *context;
    let scope = match layered.mcp.servers.get(name).map(|config| config.origin) {
        Some(McpOrigin::Project) => Scope::Project,
        _ => Scope::Global,
    };
    let source = match scope {
        Scope::Project => project_config_path(workspace).display().to_string(),
        Scope::Global => crate::config::global_config_path()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
    };
    let mut row = Row {
        name: name.to_owned(),
        scope: scope.label(),
        source,
        enabled: spec.enabled,
        transport: spec.transport.kind(),
        target: redact(&spec.transport.target()),
        exposure: spec.options.exposure.as_str(),
        lazy: spec.options.lazy,
        description: spec.options.description.as_deref().map(redact),
        status: "disabled",
        tools: Vec::new(),
        tool_exposure: BTreeMap::new(),
        resources: None,
        resources_error: None,
        server: None,
        error: None,
    };
    if let Some((_, reason)) = load.invalid.iter().find(|(invalid, _)| invalid == name) {
        row.status = "invalid";
        row.error = Some(redact(reason));
        return row;
    }
    if spec.options.block == Some(McpServerBlock::Untrusted) {
        row.status = "untrusted";
        return row;
    }
    let Some(probe) = probe else {
        // Disabled: never contacted.
        return row;
    };
    row.resources = probe.resources;
    row.resources_error = probe.resources_error.as_deref().map(redact);
    match info.map(|info| &info.status) {
        Some(McpServerStatus::NeedsAuth { reason }) => {
            row.status = "needs-auth";
            row.error = Some(redact(reason));
        }
        Some(McpServerStatus::Ready { tools }) if probe.error.is_none() => {
            row.status = "connected";
            row.tools = tools.iter().map(|tool| tool.name.clone()).collect();
            for tool in &row.tools {
                let exposure = spec.options.tool_exposure_for(tool);
                if exposure != spec.options.exposure {
                    row.tool_exposure.insert(tool.clone(), exposure.as_str());
                }
            }
            row.server = info
                .and_then(|info| info.handshake.as_ref())
                .map(|handshake| {
                    json!({
                        "name": handshake.server_name,
                        "version": handshake.server_version,
                        "protocol_version": handshake.protocol_version,
                    })
                });
        }
        _ => {
            row.status = "failed";
            let reason = probe.error.or_else(|| match info.map(|info| &info.status) {
                Some(McpServerStatus::Failed { error }) => Some(error.clone()),
                _ => None,
            });
            row.error = Some(redact(
                reason.as_deref().unwrap_or("the server did not connect"),
            ));
        }
    }
    row
}

pub(super) fn list(context: &Context, args: &[String]) -> i32 {
    let parsed = match parse_options(
        args,
        &[("json", Kind::Flag), ("timeout", Kind::Value)],
        usize::MAX,
    ) {
        Ok(parsed) => parsed,
        Err(error) => return fail(&error),
    };
    if !parsed.positional.is_empty() {
        return fail(&format!(
            "Usage: slim mcp list [--json] [--timeout SECONDS]\n{}",
            super::HINT
        ));
    }
    let limit = match parse_seconds(parsed.value("timeout"), DEFAULT_LIST_TIMEOUT) {
        Ok(limit) => limit,
        Err(error) => return fail(&error),
    };
    let (layered, invalid) = match context.load() {
        Ok(loaded) => loaded,
        Err(error) => return fail(&error),
    };
    let load = mcp::load_mcp_from(&layered, &context.workspace, false);
    let manager = mcp::build_mcp_manager(&load, &context.workspace);
    let rows = match runtime() {
        Ok(runtime) => runtime.block_on(collect_rows(
            &load,
            &layered,
            manager,
            &context.workspace,
            limit,
        )),
        Err(error) => return fail(&error),
    };

    let mut notes = Vec::new();
    if let Some(error) = &load.trust_error {
        notes.push(format!("{error}; project servers stay disabled"));
    }
    if !load.untrusted.is_empty() {
        notes.push(if load.denied {
            "Project servers are disabled for this workspace; run `slim mcp trust` to allow them."
                .to_owned()
        } else {
            format!(
                "Project servers not started (workspace not trusted): {}. Run `slim mcp trust` to allow them.",
                load.untrusted.join(", ")
            )
        });
    }
    let errors: Vec<String> = invalid.iter().map(|(_, error)| error.clone()).collect();
    let failed = !errors.is_empty() || rows.iter().any(Row::failed);

    if parsed.flag("json") {
        let document = json!({
            "workspace": context.workspace.display().to_string(),
            "servers": rows.iter().map(Row::to_json).collect::<Vec<_>>(),
            "errors": errors,
            "notes": notes,
        });
        out!(
            "{}",
            serde_json::to_string_pretty(&document).unwrap_or_else(|_| "{}".to_owned())
        );
    } else {
        if rows.is_empty() && errors.is_empty() {
            out!("No MCP servers configured. Add one with: slim mcp add <name> -- <command>");
        }
        for row in &rows {
            row.print();
        }
        for error in &errors {
            out!("config error: {error}");
        }
        for note in &notes {
            out!("{note}");
        }
    }
    i32::from(failed)
}

async fn collect_rows(
    load: &McpLoad,
    layered: &crate::config::LayeredConfig,
    manager: Option<Arc<McpManager>>,
    workspace: &std::path::Path,
    limit: Duration,
) -> Vec<Row> {
    let secrets = manager
        .as_ref()
        .map(|manager| manager.sensitive_values())
        .unwrap_or_default();
    let redact = |text: &str| redact_with_secrets(text, &secrets);
    let mut probes: BTreeMap<String, Probe> = BTreeMap::new();
    if let Some(manager) = &manager {
        let mut tasks = tokio::task::JoinSet::new();
        for (name, spec) in &load.specs {
            if spec.enabled && spec.options.block.is_none() {
                let manager = Arc::clone(manager);
                let name = name.clone();
                tasks.spawn(async move {
                    let probe = probe_server(manager, name.clone(), limit).await;
                    (name, probe)
                });
            }
        }
        while let Some(done) = tasks.join_next().await {
            if let Ok((name, probe)) = done {
                probes.insert(name, probe);
            }
        }
    }
    let infos = manager
        .as_ref()
        .map(|manager| manager.statuses())
        .unwrap_or_default();
    let context = RowContext {
        load,
        layered,
        workspace,
        redact: &redact,
    };
    let rows = load
        .specs
        .iter()
        .map(|(name, spec)| {
            build_row(
                name,
                spec,
                &context,
                probes.remove(name),
                infos.iter().find(|info| info.name == *name),
            )
        })
        .collect();
    if let Some(manager) = &manager {
        manager.disconnect_all().await;
    }
    rows
}

// ---------------------------------------------------------------------------
// login, logout
// ---------------------------------------------------------------------------

/// The configured server `name`, or why there is none.
fn find_server<'a>(
    layered: &'a crate::config::LayeredConfig,
    invalid: &[(String, String)],
    name: &str,
) -> Result<&'a McpServerConfig, String> {
    if let Some(server) = layered.mcp.servers.get(name) {
        return Ok(server);
    }
    if let Some((_, error)) = invalid.iter().find(|(entry, _)| entry == name) {
        return Err(format!("MCP server \"{name}\" is invalid: {error}"));
    }
    Err(format!(
        "No MCP server named \"{name}\". Configured: {}.",
        names_of(layered, invalid)
    ))
}

/// The URL of a server that signs in with OAuth: HTTP, with no
/// `Authorization` header of its own.
fn oauth_url<'a>(name: &str, server: &'a McpServerConfig) -> Result<&'a str, String> {
    match &server.url {
        Some(url)
            if !server
                .headers
                .keys()
                .any(|key| key.eq_ignore_ascii_case("authorization")) =>
        {
            Ok(url)
        }
        _ => Err(format!(
            "MCP server \"{name}\" does not use OAuth. Only HTTP servers without an Authorization header do."
        )),
    }
}

struct NoBrowser;

impl BrowserLauncher for NoBrowser {
    fn open(&self, _url: &str) -> Result<(), OAuthError> {
        Err(OAuthError::Browser)
    }
}

pub(super) fn login(context: &Context, args: &[String]) -> i32 {
    let parsed = match parse_options(
        args,
        &[("timeout", Kind::Value), ("no-browser", Kind::Flag)],
        usize::MAX,
    ) {
        Ok(parsed) => parsed,
        Err(error) => return fail(&error),
    };
    let name = match parsed.single("name", "login") {
        Ok(name) => name.to_owned(),
        Err(error) => return fail(&error),
    };
    let timeout = match parse_seconds(parsed.value("timeout"), oauth::LOGIN_TIMEOUT) {
        Ok(timeout) => timeout,
        Err(error) => return fail(&error),
    };
    let (layered, invalid) = match context.load() {
        Ok(loaded) => loaded,
        Err(error) => return fail(&error),
    };
    let server = match find_server(&layered, &invalid, &name) {
        Ok(server) => server,
        Err(error) => return fail(&error),
    };
    if let Err(error) = oauth_url(&name, server) {
        return fail(&error);
    }
    if !server.enabled {
        return fail(&format!(
            "MCP server \"{name}\" is disabled; run `slim mcp enable {name}` first."
        ));
    }
    let load = mcp::load_mcp_from(&layered, &context.workspace, false);
    match load.specs.get(&name).and_then(|spec| spec.options.block.as_ref()) {
        Some(McpServerBlock::Untrusted) => {
            return fail(&format!(
                "MCP server \"{name}\" is defined by this project's slim.toml, which is not trusted; run `slim mcp trust` first."
            ))
        }
        Some(McpServerBlock::Invalid(reason)) => {
            return fail(&format!("MCP server \"{name}\" cannot start: {reason}"))
        }
        None => {}
    }
    let Some(manager) = mcp::build_mcp_manager(&load, &context.workspace) else {
        return fail(&format!("No MCP server named \"{name}\"."));
    };
    let Some(auth) = manager.auth_handle(&name) else {
        return fail(&format!(
            "MCP server \"{name}\" cannot sign in: its url is not a valid http(s) URL."
        ));
    };
    let browser: Arc<dyn BrowserLauncher> = if parsed.flag("no-browser") {
        Arc::new(NoBrowser)
    } else {
        Arc::new(SystemBrowser)
    };
    match runtime() {
        Ok(runtime) => runtime.block_on(login_flow(manager, auth, name, timeout, browser)),
        Err(error) => fail(&error),
    }
}

/// Redirect URLs typed into a terminal (the browser could not reach this
/// machine, for example over SSH). Only a terminal can do this: a pipe could
/// not know the sign-in's `state` in advance.
fn paste_channel() -> Option<mpsc::UnboundedReceiver<String>> {
    if !std::io::stdin().is_terminal() {
        return None;
    }
    let (sender, receiver) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            match stdin.lock().read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let text = line.trim();
                    if !text.is_empty() && sender.send(text.to_owned()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    err!("If the browser cannot reach this machine, paste the URL it was redirected to and press Enter.");
    Some(receiver)
}

async fn login_flow(
    manager: Arc<McpManager>,
    auth: Arc<McpAuth>,
    name: String,
    timeout: Duration,
    browser: Arc<dyn BrowserLauncher>,
) -> i32 {
    let secrets = manager.sensitive_values();
    let redact = |text: &str| redact_with_secrets(text, &secrets);
    // Connecting first answers whether a sign-in is needed at all.
    let code = match manager.list_tools(&name).await {
        Ok(tools) => {
            out!(
                "Already connected to MCP server \"{name}\" ({} tools); no sign-in needed.",
                tools.len()
            );
            0
        }
        Err(McpError::AuthRequired(_)) => {
            sign_in(&manager, &auth, &name, timeout, browser, &redact).await
        }
        Err(error) => fail(&format!(
            "MCP server \"{name}\" failed to connect: {}",
            redact(&error.to_string())
        )),
    };
    manager.disconnect_all().await;
    code
}

async fn sign_in(
    manager: &Arc<McpManager>,
    auth: &Arc<McpAuth>,
    name: &str,
    timeout: Duration,
    browser: Arc<dyn BrowserLauncher>,
    redact: &dyn Fn(&str) -> String,
) -> i32 {
    // A dropped sender cancels the sign-in, so it lives until this returns.
    let (_cancel, cancelled) = watch::channel(false);
    let notice_name = name.to_owned();
    let options = oauth::LoginOptions {
        timeout,
        browser,
        notify: Arc::new(move |notice| match notice {
            oauth::LoginNotice::AuthorizationUrl {
                url,
                browser_opened,
                ..
            } => {
                out!("Sign in to MCP server \"{notice_name}\" in your browser:\n{url}");
                if browser_opened {
                    out!("(opened in your browser)");
                }
            }
            oauth::LoginNotice::Warning(text) => err!("{text}"),
        }),
        pasted: paste_channel(),
        cancel: cancelled,
    };
    match oauth::login(auth, options).await {
        Ok(_) => {}
        Err(oauth::LoginError::Timeout) => {
            return fail(&format!(
                "Sign-in to MCP server \"{name}\" was not completed within {} seconds.",
                timeout.as_secs()
            ))
        }
        Err(oauth::LoginError::Cancelled) => {
            return fail(&format!("Sign-in to MCP server \"{name}\" was cancelled."))
        }
        Err(oauth::LoginError::Failed(message)) => {
            return fail(&format!(
                "Sign-in to MCP server \"{name}\" failed: {}",
                redact(&message)
            ))
        }
    }
    if let Err(error) = manager.reconnect(name).await {
        return fail(&format!(
            "Signed in, but connecting failed: {}",
            redact(&error.to_string())
        ));
    }
    match manager.list_tools(name).await {
        Ok(tools) => {
            out!(
                "Signed in to MCP server \"{name}\" ({} tools).",
                tools.len()
            );
            0
        }
        Err(error) => fail(&format!(
            "Signed in, but listing tools failed: {}",
            redact(&error.to_string())
        )),
    }
}

pub(super) fn logout(context: &Context, args: &[String]) -> i32 {
    let parsed = match parse_options(args, &[], usize::MAX) {
        Ok(parsed) => parsed,
        Err(error) => return fail(&error),
    };
    let name = match parsed.single("name", "logout") {
        Ok(name) => name.to_owned(),
        Err(error) => return fail(&error),
    };
    let (layered, invalid) = match context.load() {
        Ok(loaded) => loaded,
        Err(error) => return fail(&error),
    };
    let server = match find_server(&layered, &invalid, &name) {
        Ok(server) => server,
        Err(error) => return fail(&error),
    };
    let url = match oauth_url(&name, server) {
        Ok(url) => url,
        Err(error) => return fail(&error),
    };
    // Credentials are keyed by name and URL; nothing connects or starts.
    let Some(auth) = oauth::build_auth(&name, url, McpOAuthSpec::default()) else {
        return fail(&format!(
            "MCP server \"{name}\" has no OAuth credentials: its url is not a valid http(s) URL."
        ));
    };
    let runtime = match runtime() {
        Ok(runtime) => runtime,
        Err(error) => return fail(&error),
    };
    match runtime.block_on(oauth::logout(&auth.0)) {
        Ok(true) => {
            out!("Signed out of MCP server \"{name}\"; stored credentials deleted.");
            0
        }
        Ok(false) => {
            out!("No stored credentials for MCP server \"{name}\".");
            0
        }
        Err(error) => fail(&format!(
            "Sign-out of MCP server \"{name}\" failed: {error}"
        )),
    }
}
