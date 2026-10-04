//! `slim mcp ...`: configure, check and sign in to MCP servers without a TUI
//! session or provider credentials.
//!
//! This file parses arguments and edits configuration (`add`, `remove`,
//! `enable`, `disable`, `import`, `trust`, `untrust`). The commands that talk
//! to servers (`list`, `login`, `logout`) live in [`live`]. Everything reuses
//! the library the session uses: config upsert/replace/remove, the import
//! converter, the trust store, the OAuth login and the manager, so a command
//! and a session always agree on what a server is.
//!
//! Exit codes: 0 on success, 1 on any failure (usage errors, invalid entries,
//! servers that failed to connect, a sign-in that did not complete).

mod live;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use slim_core::mcp::McpExposure;

use crate::config::{
    clear_mcp_disable_override_to, global_config_path, load_layered_lenient_for,
    mcp_server_defined_in, merge_layer_as, project_config_path, remove_mcp_server,
    remove_mcp_server_from, upsert_mcp_server_to, FileConfig, FileMcpConfig, FileMcpOAuthConfig,
    FileMcpServerConfig, LayeredConfig, McpOrigin,
};
use crate::mcp::import;
use crate::mcp::trust::{TrustDecision, TrustStore};

/// Prints to stdout; a closed pipe is not an error worth a panic. Everything
/// printed goes through `strip_terminal_controls`: command lines, tool names
/// and error texts come from project files and servers, and an escape
/// sequence in them could rewrite what the person reads before deciding.
macro_rules! out {
    ($($arg:tt)*) => {{
        let _ = writeln!(
            std::io::stdout(),
            "{}",
            slim_core::mcp::strip_terminal_controls(&format!($($arg)*))
        );
    }};
}

macro_rules! err {
    ($($arg:tt)*) => {{
        let _ = writeln!(
            std::io::stderr(),
            "{}",
            slim_core::mcp::strip_terminal_controls(&format!($($arg)*))
        );
    }};
}

pub(crate) use err;
pub(crate) use out;

const HELP: &str = "Slim MCP servers: configure, check and sign in without starting a session

Usage:
  slim mcp add <name> [OPTIONS] -- <command> [args...]
  slim mcp add <name> [OPTIONS] --url <url>
  slim mcp remove <name> [--project|--global]
  slim mcp enable <name>
  slim mcp disable <name>
  slim mcp list [--json] [--timeout SECONDS]
  slim mcp login <name> [--timeout SECONDS] [--no-browser]
  slim mcp logout <name>
  slim mcp import <file> [--project|--global] [--force]
  slim mcp trust
  slim mcp untrust

Commands:
  add        Add a server to slim.toml, or update it when it already exists
  remove     Remove a server (the project file first, then the global one)
  enable     Turn a configured server on, in the file that defines it
  disable    Turn a configured server off, in the file that defines it
  list       Connect to the enabled servers and show status, tools and errors
             (exit 1 when an entry is invalid or an enabled server fails)
  login      Sign in to an HTTP server with OAuth through the browser
  logout     Delete the stored OAuth credentials of a server
  import     Convert an mcpServers/servers JSON file (Claude Desktop, Claude
             Code .mcp.json, Cursor, VS Code, Pi) into slim.toml entries
  trust      Allow this workspace's slim.toml to start MCP servers
  untrust    Keep this workspace's project servers disabled

Where servers live:
  --project  ./slim.toml in the current directory (default for add and import)
  --global   the global slim.toml in the Slim config directory
  Servers from a project file start only after `slim mcp trust` (or a run with
  --trust-project). Global servers are always trusted.

Options for add:
  --url URL                    Streamable HTTP server (instead of a command)
  --env KEY=VALUE              Environment variable for a stdio server (repeatable)
  --cwd DIR                    Working directory for a stdio server
  --header KEY=VALUE           HTTP header (repeatable)
  --bearer-token-env-var NAME  Send \"Authorization: Bearer ${NAME}\"
  --timeout-ms N               Per-request timeout (1000-600000, default 60000)
  --exposure MODE              gateway (default), direct or hidden
  --description TEXT           What the server offers, shown to the model
  --lazy                       Connect on first use instead of at session start
  --oauth-client-id ID         Pre-registered OAuth client id
  --oauth-client-secret S      OAuth client secret (use ${NAME} or !command)
  --oauth-callback-port PORT   Fixed loopback port for the OAuth callback
  --oauth-scope SCOPE          OAuth scope to request
  --oauth-client-name NAME     Client name used for dynamic registration
  Values may use $VAR, ${VAR} or a whole-value !command in --env, --header and
  --oauth-client-secret (resolved when the server starts).

Options for list and login:
  --json                       Print the list as JSON
  --timeout SECONDS            list: wait per server (default 15); login: wait
                               for the browser (default 300)
  --no-browser                 login: only print the URL, do not open a browser

Examples:
  slim mcp add files -- npx -y @modelcontextprotocol/server-filesystem .
  slim mcp add docs --url https://mcp.example.com/mcp --global
  slim mcp add api --url https://api.example.com/mcp --bearer-token-env-var API_TOKEN
  slim mcp list
  slim mcp login docs
";

const HINT: &str = "Use \"slim mcp --help\" for usage.";

/// What `parse_options` returns when `--help` or `-h` stands where an option
/// may: `fail` answers it with the usage text and exit code 0. Arguments after
/// `--`, or past a command's positionals, belong to the server command and
/// are never taken for help.
const HELP_REQUESTED: &str = "\0help";

/// Failure exit code of every `slim mcp` command.
const FAILED: i32 = 1;

/// Runs `slim mcp <args>` (`args` excludes the `mcp` word) and returns the
/// exit code. Output goes to stdout/stderr as it happens.
pub fn run_mcp_cli(args: &[String]) -> i32 {
    let Some(command) = args.first().map(String::as_str) else {
        out!("{}", HELP.trim_end());
        return 0;
    };
    if matches!(command, "help" | "--help" | "-h") {
        out!("{}", HELP.trim_end());
        return 0;
    }
    let rest = &args[1..];
    let context = match Context::current() {
        Ok(context) => context,
        Err(error) => return fail(&error),
    };
    match command {
        "add" => add(&context, rest),
        "remove" | "rm" => remove(&context, rest),
        "enable" => set_enabled(&context, rest, true),
        "disable" => set_enabled(&context, rest, false),
        "list" | "ls" => live::list(&context, rest),
        "login" => live::login(&context, rest),
        "logout" => live::logout(&context, rest),
        "import" => import_file(&context, rest),
        "trust" => trust(&context, rest, true),
        "untrust" => trust(&context, rest, false),
        other => fail(&format!("unknown mcp command \"{other}\". {HINT}")),
    }
}

fn fail(message: &str) -> i32 {
    if message == HELP_REQUESTED {
        out!("{}", HELP.trim_end());
        return 0;
    }
    err!("{message}");
    FAILED
}

// ---------------------------------------------------------------------------
// Context and scopes
// ---------------------------------------------------------------------------

/// The workspace every command is anchored to: the current directory.
pub(super) struct Context {
    pub workspace: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Scope {
    Project,
    Global,
}

impl Scope {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Global => "global",
        }
    }

    fn origin(self) -> McpOrigin {
        match self {
            Self::Project => McpOrigin::Project,
            Self::Global => McpOrigin::Global,
        }
    }
}

impl Context {
    fn current() -> Result<Self, String> {
        std::env::current_dir()
            .map(|workspace| Self { workspace })
            .map_err(|error| format!("cannot determine the current directory: {error}"))
    }

    pub(super) fn path(&self, scope: Scope) -> Result<PathBuf, String> {
        match scope {
            Scope::Project => Ok(project_config_path(&self.workspace)),
            Scope::Global => global_config_path()
                .ok_or_else(|| "cannot resolve the global config path".to_owned()),
        }
    }

    /// Merged config of both layers; invalid entries are reported apart.
    pub(super) fn load(&self) -> Result<(LayeredConfig, Vec<(String, String)>), String> {
        load_layered_lenient_for(&self.workspace)
    }

    /// The recorded trust decision for this workspace (`None`: never decided).
    fn trust_decision(&self) -> Result<Option<TrustDecision>, String> {
        TrustStore::default_store().and_then(|store| store.decision(&self.workspace))
    }

    /// A sentence for commands that touched the project file: whether its
    /// servers can start yet.
    fn project_trust_note(&self) -> Option<String> {
        match self.trust_decision() {
            Ok(Some(TrustDecision::Trusted)) => None,
            Ok(Some(TrustDecision::Denied)) => Some(
                "This workspace is set to keep project servers disabled; run `slim mcp trust` to allow them."
                    .to_owned(),
            ),
            Ok(None) => Some(
                "Project servers do not start until this workspace is trusted: run `slim mcp trust`."
                    .to_owned(),
            ),
            Err(error) => Some(format!(
                "{error}; project servers stay disabled until the trust store is readable."
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Kind {
    Flag,
    Value,
    List,
}

#[derive(Debug, Default)]
pub(super) struct Parsed {
    pub positional: Vec<String>,
    flags: BTreeSet<&'static str>,
    values: BTreeMap<&'static str, String>,
    lists: BTreeMap<&'static str, Vec<String>>,
}

impl Parsed {
    pub(super) fn flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }

    pub(super) fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    fn list(&self, name: &str) -> &[String] {
        self.lists.get(name).map_or(&[], Vec::as_slice)
    }

    fn has(&self, name: &str) -> bool {
        self.flag(name) || self.values.contains_key(name) || self.lists.contains_key(name)
    }

    /// Exactly one positional argument (a server name or a file).
    pub(super) fn single(&self, what: &str, command: &str) -> Result<&str, String> {
        match self.positional.as_slice() {
            [one] => Ok(one),
            _ => Err(format!("Usage: slim mcp {command} <{what}> ...\n{HINT}")),
        }
    }
}

/// Parses `--name value`, `--name=value` and flags. `--` ends the options, as
/// does reaching `max_positionals` positional arguments: the rest is
/// positional, so `add <name> <command> --flag` passes `--flag` to the
/// command.
pub(super) fn parse_options(
    args: &[String],
    known: &[(&'static str, Kind)],
    max_positionals: usize,
) -> Result<Parsed, String> {
    let mut parsed = Parsed::default();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            parsed.positional.extend(args[index + 1..].iter().cloned());
            break;
        }
        if parsed.positional.len() >= max_positionals {
            parsed.positional.extend(args[index..].iter().cloned());
            break;
        }
        if arg == "--help" || arg == "-h" {
            return Err(HELP_REQUESTED.to_owned());
        }
        if let Some(rest) = arg.strip_prefix("--") {
            let (name, inline) = match rest.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (rest, None),
            };
            let Some(&(canonical, kind)) = known.iter().find(|(known, _)| *known == name) else {
                return Err(format!("unknown option --{name}. {HINT}"));
            };
            if kind == Kind::Flag {
                if inline.is_some() {
                    return Err(format!("--{name} does not take a value"));
                }
                parsed.flags.insert(canonical);
            } else {
                let value = match inline {
                    Some(value) => value,
                    None => {
                        index += 1;
                        args.get(index)
                            .cloned()
                            .ok_or_else(|| format!("--{name} needs a value"))?
                    }
                };
                if kind == Kind::List {
                    parsed.lists.entry(canonical).or_default().push(value);
                } else {
                    parsed.values.insert(canonical, value);
                }
            }
        } else if arg.len() > 1 && arg.starts_with('-') {
            return Err(format!("unknown option {arg}. {HINT}"));
        } else {
            parsed.positional.push(arg.clone());
        }
        index += 1;
    }
    Ok(parsed)
}

/// `--project` / `--global`: `None` when neither was given.
pub(super) fn scope_option(parsed: &Parsed) -> Result<Option<Scope>, String> {
    match (parsed.flag("project"), parsed.flag("global")) {
        (true, true) => Err("--project and --global are mutually exclusive".to_owned()),
        (true, false) => Ok(Some(Scope::Project)),
        (false, true) => Ok(Some(Scope::Global)),
        (false, false) => Ok(None),
    }
}

fn parse_pairs(option: &str, pairs: &[String]) -> Result<BTreeMap<String, String>, String> {
    let mut map = BTreeMap::new();
    for pair in pairs {
        // The text is not echoed: a malformed pair may itself be a secret.
        match pair.split_once('=') {
            Some((key, value)) if !key.is_empty() => {
                map.insert(key.to_owned(), value.to_owned());
            }
            _ => return Err(format!("--{option} expects KEY=VALUE")),
        }
    }
    Ok(map)
}

fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn names_of(config: &LayeredConfig, invalid: &[(String, String)]) -> String {
    let mut names: BTreeSet<&str> = config.mcp.servers.keys().map(String::as_str).collect();
    names.extend(invalid.iter().map(|(name, _)| name.as_str()));
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.into_iter().collect::<Vec<_>>().join(", ")
    }
}

// ---------------------------------------------------------------------------
// add
// ---------------------------------------------------------------------------

const ADD_OPTIONS: &[(&str, Kind)] = &[
    ("project", Kind::Flag),
    ("global", Kind::Flag),
    ("url", Kind::Value),
    ("env", Kind::List),
    ("header", Kind::List),
    ("bearer-token-env-var", Kind::Value),
    ("cwd", Kind::Value),
    ("timeout-ms", Kind::Value),
    ("exposure", Kind::Value),
    ("description", Kind::Value),
    ("lazy", Kind::Flag),
    ("oauth-client-id", Kind::Value),
    ("oauth-client-secret", Kind::Value),
    ("oauth-callback-port", Kind::Value),
    ("oauth-scope", Kind::Value),
    ("oauth-client-name", Kind::Value),
];

const HTTP_ONLY: &[&str] = &[
    "header",
    "bearer-token-env-var",
    "oauth-client-id",
    "oauth-client-secret",
    "oauth-callback-port",
    "oauth-scope",
    "oauth-client-name",
];
const STDIO_ONLY: &[&str] = &["env", "cwd"];

fn secret_like(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "authorization",
        "token",
        "secret",
        "password",
        "api-key",
        "api_key",
        "apikey",
        "cookie",
        "credential",
    ]
    .iter()
    .any(|marker| key.contains(marker))
}

/// A value that reads the environment or runs a command is not a plain secret.
fn indirect(value: &str) -> bool {
    value.contains('$') || value.starts_with('!')
}

#[derive(Debug)]
struct NewServer {
    name: String,
    config: FileMcpServerConfig,
    warnings: Vec<String>,
}

fn build_server(parsed: &Parsed) -> Result<NewServer, String> {
    let url = parsed.value("url");
    let usage = || {
        format!(
            "Usage: slim mcp add <name> [OPTIONS] (--url <url> | -- <command> [args...])\n{HINT}"
        )
    };
    let (name, command) = match parsed.positional.split_first() {
        Some((name, command))
            if (url.is_some() && command.is_empty()) || (url.is_none() && !command.is_empty()) =>
        {
            (name.clone(), command)
        }
        _ => return Err(usage()),
    };
    let misplaced = if url.is_some() { STDIO_ONLY } else { HTTP_ONLY };
    if let Some(option) = misplaced.iter().find(|option| parsed.has(option)) {
        return Err(format!(
            "--{option} only applies to {}",
            if url.is_some() {
                "stdio servers (a command)"
            } else {
                "HTTP servers (--url)"
            }
        ));
    }
    let mut warnings = Vec::new();
    let mut config = FileMcpServerConfig::default();
    if let Some(url) = url {
        config.url = Some(url.to_owned());
        let mut headers = parse_pairs("header", parsed.list("header"))?;
        if let Some(variable) = parsed.value("bearer-token-env-var") {
            if !valid_env_name(variable) {
                return Err(
                    "--bearer-token-env-var expects an environment variable name".to_owned(),
                );
            }
            if headers
                .keys()
                .any(|key| key.eq_ignore_ascii_case("authorization"))
            {
                return Err(
                    "--bearer-token-env-var conflicts with an Authorization --header".to_owned(),
                );
            }
            headers.insert(
                "Authorization".to_owned(),
                format!("Bearer ${{{variable}}}"),
            );
        }
        for (key, value) in &headers {
            if key.is_empty() || key.contains(char::is_whitespace) || key.contains(':') {
                return Err(format!("invalid header name \"{key}\""));
            }
            if secret_like(key) && !indirect(value) {
                warnings.push(format!(
                    "header {key} stores a credential in plain text in the config file; prefer ${{VAR}} or --bearer-token-env-var"
                ));
            }
        }
        if !headers.is_empty() {
            config.headers = Some(headers);
        }
        let mut oauth = FileMcpOAuthConfig {
            client_id: parsed.value("oauth-client-id").map(str::to_owned),
            client_secret: parsed.value("oauth-client-secret").map(str::to_owned),
            scope: parsed.value("oauth-scope").map(str::to_owned),
            client_name: parsed.value("oauth-client-name").map(str::to_owned),
            ..FileMcpOAuthConfig::default()
        };
        if let Some(port) = parsed.value("oauth-callback-port") {
            oauth.callback_port = Some(
                port.parse::<u16>()
                    .ok()
                    .filter(|port| *port != 0)
                    .ok_or_else(|| {
                        "--oauth-callback-port expects a port number (1-65535)".to_owned()
                    })?,
            );
        }
        if oauth
            .client_secret
            .as_deref()
            .is_some_and(|secret| !indirect(secret))
        {
            warnings.push(
                "the OAuth client secret is stored in plain text in the config file; prefer ${VAR} or !command"
                    .to_owned(),
            );
        }
        if oauth != FileMcpOAuthConfig::default() {
            config.oauth = Some(oauth);
        }
    } else {
        let (executable, arguments) = command.split_first().ok_or_else(usage)?;
        config.command = Some(executable.clone());
        if !arguments.is_empty() {
            config.args = Some(arguments.to_vec());
        }
        let env = parse_pairs("env", parsed.list("env"))?;
        for (key, value) in &env {
            if secret_like(key) && !indirect(value) {
                warnings.push(format!(
                    "env {key} stores a credential in plain text in the config file; prefer ${{VAR}}"
                ));
            }
        }
        if !env.is_empty() {
            config.env = Some(env);
        }
        config.cwd = parsed.value("cwd").map(str::to_owned);
    }
    if let Some(timeout) = parsed.value("timeout-ms") {
        config.timeout_ms = Some(
            timeout
                .parse::<u64>()
                .map_err(|_| "--timeout-ms expects a number of milliseconds".to_owned())?,
        );
    }
    if let Some(exposure) = parsed.value("exposure") {
        config.exposure = Some(
            McpExposure::parse(exposure)
                .ok_or_else(|| "--exposure expects gateway, direct or hidden".to_owned())?,
        );
    }
    config.description = parsed.value("description").map(str::to_owned);
    if parsed.flag("lazy") {
        config.lazy = Some(true);
    }
    Ok(NewServer {
        name,
        config,
        warnings,
    })
}

fn add(context: &Context, args: &[String]) -> i32 {
    let parsed = match parse_options(args, ADD_OPTIONS, 2) {
        Ok(parsed) => parsed,
        Err(error) => return fail(&error),
    };
    let scope = match scope_option(&parsed) {
        Ok(scope) => scope.unwrap_or(Scope::Project),
        Err(error) => return fail(&error),
    };
    let NewServer {
        name,
        config,
        warnings,
    } = match build_server(&parsed) {
        Ok(server) => server,
        Err(error) => return fail(&error),
    };
    let (layered, _) = match context.load() {
        Ok(loaded) => loaded,
        Err(error) => return fail(&error),
    };
    // What the next load will see: this entry merged over the existing layers.
    let mut merged = layered;
    merge_layer_as(
        &mut merged,
        FileConfig {
            mcp: Some(FileMcpConfig {
                servers: Some(BTreeMap::from([(name.clone(), config.clone())])),
                startup_wait_ms: None,
            }),
            ..FileConfig::default()
        },
        scope.origin(),
    );
    if let Err(error) = merged.mcp.validate() {
        return fail(&error);
    }
    let path = match context.path(scope) {
        Ok(path) => path,
        Err(error) => return fail(&error),
    };
    let existed = match mcp_server_defined_in(&path, &name) {
        Ok(existed) => existed,
        Err(error) => return fail(&error),
    };
    if let Err(error) = upsert_mcp_server_to(&path, &name, &config) {
        return fail(&format!("Could not update {}: {error}", path.display()));
    }
    // The file may have held a broken entry the merge above did not see.
    match context.load() {
        Ok((_, invalid)) => {
            if let Some((_, error)) = invalid.iter().find(|(entry, _)| *entry == name) {
                return fail(&format!(
                    "{} was updated but \"{name}\" is still invalid: {error}",
                    path.display()
                ));
            }
        }
        Err(error) => return fail(&error),
    }
    for warning in &warnings {
        err!("warning: {warning}");
    }
    out!(
        "{} {} MCP server \"{name}\" in {}.",
        if existed { "Updated" } else { "Added" },
        scope.label(),
        path.display()
    );
    if existed {
        out!(
            "Fields you did not pass were kept, except that a new command replaces the old one's args and cwd; env, header and oauth values merge key by key."
        );
    }
    if scope == Scope::Project {
        if let Some(note) = context.project_trust_note() {
            out!("{note}");
        }
    }
    let mut next = "Check it with: slim mcp list".to_owned();
    let signs_in = config.url.is_some()
        && !config.headers.as_ref().is_some_and(|headers| {
            headers
                .keys()
                .any(|key| key.eq_ignore_ascii_case("authorization"))
        });
    if signs_in {
        next.push_str(&format!(". If it requires sign-in: slim mcp login {name}"));
    }
    out!("{next}");
    0
}

// ---------------------------------------------------------------------------
// remove, enable, disable
// ---------------------------------------------------------------------------

fn remove(context: &Context, args: &[String]) -> i32 {
    let parsed = match parse_options(
        args,
        &[("project", Kind::Flag), ("global", Kind::Flag)],
        usize::MAX,
    ) {
        Ok(parsed) => parsed,
        Err(error) => return fail(&error),
    };
    let name = match parsed.single("name", "remove") {
        Ok(name) => name.to_owned(),
        Err(error) => return fail(&error),
    };
    let scope = match scope_option(&parsed) {
        Ok(scope) => scope,
        Err(error) => return fail(&error),
    };
    let (layered, invalid) = match context.load() {
        Ok(loaded) => loaded,
        Err(error) => return fail(&error),
    };
    let removed: Result<Option<(PathBuf, Scope)>, String> = match scope {
        Some(scope) => context.path(scope).and_then(|path| {
            remove_mcp_server_from(&path, &name).map(|removed| removed.then_some((path, scope)))
        }),
        None => remove_mcp_server(&context.workspace, &name).map(|removed| {
            removed.map(|path| {
                let scope = if path == project_config_path(&context.workspace) {
                    Scope::Project
                } else {
                    Scope::Global
                };
                (path, scope)
            })
        }),
    };
    match removed {
        Ok(Some((path, scope))) => {
            out!(
                "Removed {} MCP server \"{name}\" from {}.",
                scope.label(),
                path.display()
            );
            // Another layer may still define it.
            let other = match scope {
                Scope::Project => Scope::Global,
                Scope::Global => Scope::Project,
            };
            if let Ok(other_path) = context.path(other) {
                if mcp_server_defined_in(&other_path, &name).unwrap_or(false) {
                    out!(
                        "It is still defined in the {} file {}.",
                        other.label(),
                        other_path.display()
                    );
                }
            }
            0
        }
        Ok(None) => {
            let known = names_of(&layered, &invalid);
            match scope {
                Some(scope) => {
                    let location = context
                        .path(scope)
                        .map(|path| format!(" in {}", path.display()))
                        .unwrap_or_default();
                    let mut message = format!(
                        "No {} MCP server named \"{name}\"{location}.",
                        scope.label()
                    );
                    let other = match scope {
                        Scope::Project => Scope::Global,
                        Scope::Global => Scope::Project,
                    };
                    if context
                        .path(other)
                        .ok()
                        .and_then(|path| mcp_server_defined_in(&path, &name).ok())
                        == Some(true)
                    {
                        message.push_str(&format!(
                            " It is defined in the {} file; use --{}.",
                            other.label(),
                            other.label()
                        ));
                    }
                    fail(&message)
                }
                None => fail(&format!(
                    "No MCP server named \"{name}\". Configured: {known}."
                )),
            }
        }
        Err(error) => fail(&format!("Could not update the config: {error}")),
    }
}

/// The file that defines `name`: the project file when it does (it wins when
/// both do), else the global one.
fn defining_file(context: &Context, name: &str) -> Result<Option<(Scope, PathBuf)>, String> {
    for scope in [Scope::Project, Scope::Global] {
        let Ok(path) = context.path(scope) else {
            continue;
        };
        if mcp_server_defined_in(&path, name)? {
            return Ok(Some((scope, path)));
        }
    }
    Ok(None)
}

fn set_enabled(context: &Context, args: &[String], enabled: bool) -> i32 {
    let command = if enabled { "enable" } else { "disable" };
    let parsed = match parse_options(args, &[], usize::MAX) {
        Ok(parsed) => parsed,
        Err(error) => return fail(&error),
    };
    let name = match parsed.single("name", command) {
        Ok(name) => name.to_owned(),
        Err(error) => return fail(&error),
    };
    let (layered, invalid) = match context.load() {
        Ok(loaded) => loaded,
        Err(error) => return fail(&error),
    };
    let (scope, path) = match defining_file(context, &name) {
        Ok(Some(found)) => found,
        Ok(None) => {
            return fail(&format!(
                "No MCP server named \"{name}\". Configured: {}.",
                names_of(&layered, &invalid)
            ))
        }
        Err(error) => return fail(&error),
    };
    if layered
        .mcp
        .servers
        .get(&name)
        .is_some_and(|server| server.enabled == enabled)
    {
        out!(
            "MCP server \"{name}\" is already {}.",
            if enabled { "enabled" } else { "disabled" }
        );
        return 0;
    }
    // A project entry that only disables a global server is lifted by
    // deleting it: `enabled = true` would turn it into a project definition
    // that needs the workspace to be trusted.
    let lifted = if enabled && scope == Scope::Project {
        match clear_mcp_disable_override_to(&path, &name) {
            Ok(lifted) => lifted,
            Err(error) => return fail(&format!("Could not update {}: {error}", path.display())),
        }
    } else {
        false
    };
    // Merging into the existing table changes `enabled` and nothing else.
    let update = FileMcpServerConfig {
        enabled: Some(enabled),
        ..FileMcpServerConfig::default()
    };
    if !lifted {
        if let Err(error) = upsert_mcp_server_to(&path, &name, &update) {
            return fail(&format!("Could not update {}: {error}", path.display()));
        }
    }
    out!(
        "{} MCP server \"{name}\" in {} ({}).",
        if enabled { "Enabled" } else { "Disabled" },
        path.display(),
        scope.label()
    );
    if enabled && scope == Scope::Project {
        if let Some(note) = context.project_trust_note() {
            out!("{note}");
        }
    }
    0
}

// ---------------------------------------------------------------------------
// import
// ---------------------------------------------------------------------------

/// Largest file `import` reads.
const MAX_IMPORT_BYTES: u64 = 4 * 1024 * 1024;

fn import_file(context: &Context, args: &[String]) -> i32 {
    let parsed = match parse_options(
        args,
        &[
            ("project", Kind::Flag),
            ("global", Kind::Flag),
            ("force", Kind::Flag),
        ],
        usize::MAX,
    ) {
        Ok(parsed) => parsed,
        Err(error) => return fail(&error),
    };
    let file = match parsed.single("file", "import") {
        Ok(file) => PathBuf::from(file),
        Err(error) => return fail(&error),
    };
    let scope = match scope_option(&parsed) {
        Ok(scope) => scope.unwrap_or(Scope::Project),
        Err(error) => return fail(&error),
    };
    let force = parsed.flag("force");
    let text = match read_import_file(&file) {
        Ok(text) => text,
        Err(error) => return fail(&error),
    };
    let mut report = match import::convert_mcp_json(&text) {
        Ok(report) => report,
        Err(error) => return fail(&format!("{}: {error}", file.display())),
    };
    let (layered, invalid) = match context.load() {
        Ok(loaded) => loaded,
        Err(error) => return fail(&error),
    };
    // A name that only differs in `-` versus `_` from another server would
    // make the whole configuration unloadable.
    let existing: Vec<String> = layered
        .mcp
        .servers
        .keys()
        .cloned()
        .chain(invalid.iter().map(|(name, _)| name.clone()))
        .collect();
    let colliding: Vec<(String, String)> = report
        .servers
        .keys()
        .filter_map(|name| {
            existing
                .iter()
                .find(|other| *other != name && other.replace('-', "_") == name.replace('-', "_"))
                .map(|other| (name.clone(), other.clone()))
        })
        .collect();
    for (name, other) in colliding {
        report.servers.remove(&name);
        report.skipped.push(import::ImportIssue {
            server: name,
            message: format!(
                "name collides with the existing server \"{other}\" (names that differ only in '-' and '_' are the same server)"
            ),
        });
    }
    let path = match context.path(scope) {
        Ok(path) => path,
        Err(error) => return fail(&error),
    };
    let outcome = match import::apply_import(&path, &report, force) {
        Ok(outcome) => outcome,
        Err(error) => return fail(&format!("Could not update {}: {error}", path.display())),
    };
    if !outcome.written.is_empty() {
        out!(
            "Imported {} server{} into the {} config {}: {}.",
            outcome.written.len(),
            if outcome.written.len() == 1 { "" } else { "s" },
            scope.label(),
            path.display(),
            outcome.written.join(", ")
        );
    }
    if !outcome.kept_existing.is_empty() {
        out!(
            "Kept existing (use --force to replace): {}.",
            outcome.kept_existing.join(", ")
        );
    }
    if outcome.written.is_empty() && outcome.kept_existing.is_empty() && report.skipped.is_empty() {
        out!("No servers to import in {}.", file.display());
    }
    for issue in &report.skipped {
        err!("skipped {}: {}", issue.server, issue.message);
    }
    for issue in &report.warnings {
        err!("warning {}: {}", issue.server, issue.message);
    }
    if !outcome.written.is_empty() {
        if scope == Scope::Project {
            if let Some(note) = context.project_trust_note() {
                out!("{note}");
            }
        }
        out!("Check them with: slim mcp list");
    }
    // Nothing imported because every server was rejected is a failure.
    if outcome.written.is_empty() && outcome.kept_existing.is_empty() && !report.skipped.is_empty()
    {
        FAILED
    } else {
        0
    }
}

fn read_import_file(file: &Path) -> Result<String, String> {
    let metadata = std::fs::metadata(file)
        .map_err(|error| format!("cannot read {}: {error}", file.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a file", file.display()));
    }
    if metadata.len() > MAX_IMPORT_BYTES {
        return Err(format!(
            "{} is larger than {MAX_IMPORT_BYTES} bytes",
            file.display()
        ));
    }
    std::fs::read_to_string(file)
        .map_err(|error| format!("cannot read {}: {error}", file.display()))
}

// ---------------------------------------------------------------------------
// trust, untrust
// ---------------------------------------------------------------------------

fn trust(context: &Context, args: &[String], trusted: bool) -> i32 {
    let command = if trusted { "trust" } else { "untrust" };
    match parse_options(args, &[], usize::MAX) {
        Ok(parsed) if parsed.positional.is_empty() => {}
        Ok(_) => return fail(&format!("Usage: slim mcp {command}\n{HINT}")),
        Err(error) => return fail(&error),
    }
    let (layered, _) = match context.load() {
        Ok(loaded) => loaded,
        Err(error) => return fail(&error),
    };
    let decision = if trusted {
        TrustDecision::Trusted
    } else {
        TrustDecision::Denied
    };
    let store = match crate::mcp::set_project_trust(&context.workspace, Some(decision)) {
        Ok(store) => store,
        Err(error) => return fail(&error),
    };
    let project: Vec<String> = layered
        .mcp
        .servers
        .iter()
        .filter(|(_, server)| server.origin == McpOrigin::Project)
        .map(|(name, server)| {
            let target = server
                .command
                .iter()
                .chain(server.args.iter())
                .cloned()
                .collect::<Vec<_>>()
                .join(" ");
            let target = if target.is_empty() {
                server.url.clone().unwrap_or_default()
            } else {
                target
            };
            format!("  {name}: {}", crate::redact(&target))
        })
        .collect();
    if trusted {
        out!(
            "Trusted {} for MCP (decision stored in {}).",
            context.workspace.display(),
            store.display()
        );
        if project.is_empty() {
            out!("Its slim.toml defines no MCP servers yet.");
        } else {
            out!("Its slim.toml servers may now start; they run with your permissions:");
            for line in &project {
                out!("{line}");
            }
        }
    } else {
        out!(
            "Project MCP servers stay disabled for {} (decision stored in {}).",
            context.workspace.display(),
            store.display()
        );
        out!("Allow them again with: slim mcp trust");
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn options_values_and_the_command_after_the_double_dash_are_separated() {
        let parsed = parse_options(
            &strings(&[
                "files",
                "--env",
                "A=1",
                "--env=B=2",
                "--lazy",
                "--",
                "npx",
                "-y",
                "--flag",
            ]),
            ADD_OPTIONS,
            2,
        )
        .expect("parses");
        assert_eq!(parsed.positional, ["files", "npx", "-y", "--flag"]);
        assert_eq!(parsed.list("env"), ["A=1", "B=2"]);
        assert!(parsed.flag("lazy"));
    }

    #[test]
    fn a_second_positional_ends_option_parsing_so_the_commands_own_flags_pass_through() {
        let parsed = parse_options(&strings(&["files", "npx", "--yes", "pkg"]), ADD_OPTIONS, 2)
            .expect("parses");
        assert_eq!(parsed.positional, ["files", "npx", "--yes", "pkg"]);
    }

    #[test]
    fn unknown_options_and_missing_values_are_usage_errors() {
        let error = parse_options(&strings(&["x", "--bogus"]), ADD_OPTIONS, 2).unwrap_err();
        assert!(error.contains("unknown option --bogus"), "{error}");
        let error = parse_options(&strings(&["x", "--url"]), ADD_OPTIONS, 2).unwrap_err();
        assert!(error.contains("--url needs a value"), "{error}");
        let error = parse_options(&strings(&["x", "--lazy=yes"]), ADD_OPTIONS, 2).unwrap_err();
        assert!(error.contains("does not take a value"), "{error}");
    }

    #[test]
    fn add_builds_stdio_and_http_entries_and_rejects_misplaced_options() {
        let stdio = |args: &[&str]| {
            build_server(&parse_options(&strings(args), ADD_OPTIONS, 2).expect("parses"))
        };
        let server =
            stdio(&["fs", "--env", "K=v", "--cwd", "work", "--", "node", "a.js"]).expect("stdio");
        assert_eq!(server.name, "fs");
        assert_eq!(server.config.command.as_deref(), Some("node"));
        assert_eq!(server.config.args, Some(vec!["a.js".to_owned()]));
        assert_eq!(server.config.cwd.as_deref(), Some("work"));
        assert!(stdio(&["fs", "--header", "A=b", "--", "node"])
            .unwrap_err()
            .contains("only applies to HTTP"));

        let http = |args: &[&str]| {
            build_server(&parse_options(&strings(args), ADD_OPTIONS, 2).expect("parses"))
        };
        let server = http(&[
            "web",
            "--url",
            "https://x.test/mcp",
            "--bearer-token-env-var",
            "TOKEN",
            "--oauth-callback-port",
            "8123",
        ])
        .expect("http");
        assert_eq!(
            server.config.headers.as_ref().expect("headers")["Authorization"],
            "Bearer ${TOKEN}"
        );
        assert_eq!(
            server.config.oauth.as_ref().expect("oauth").callback_port,
            Some(8123)
        );
        assert!(server.warnings.is_empty());
        assert!(http(&["web", "--url", "https://x.test", "--cwd", "d"])
            .unwrap_err()
            .contains("only applies to stdio"));
        assert!(http(&[
            "web",
            "--url",
            "https://x.test",
            "--bearer-token-env-var",
            "a-b"
        ])
        .unwrap_err()
        .contains("environment variable name"));
        assert!(
            http(&["web", "--url", "https://x.test", "--exposure", "wide"])
                .unwrap_err()
                .contains("gateway, direct or hidden")
        );
        assert!(http(&["web"]).unwrap_err().contains("Usage"));
        assert!(http(&["web", "--url", "https://x.test", "--", "cmd"])
            .unwrap_err()
            .contains("Usage"));
    }

    #[test]
    fn plain_credentials_on_the_command_line_are_warned_about() {
        let parsed = parse_options(
            &strings(&[
                "web",
                "--url",
                "https://x.test",
                "--header",
                "X-Api-Key=plain",
                "--header",
                "X-Token=${T}",
                "--oauth-client-secret",
                "hunter2",
            ]),
            ADD_OPTIONS,
            2,
        )
        .expect("parses");
        let server = build_server(&parsed).expect("builds");
        assert_eq!(server.warnings.len(), 2, "{:?}", server.warnings);
        assert!(server
            .warnings
            .iter()
            .any(|warning| warning.contains("X-Api-Key")));
        assert!(server
            .warnings
            .iter()
            .any(|warning| warning.contains("OAuth client secret")));
        assert!(server
            .warnings
            .iter()
            .all(|warning| !warning.contains("X-Token")));
        assert!(server
            .warnings
            .iter()
            .all(|warning| !warning.contains("hunter2")));
    }
}
