//! Converts other MCP clients' server JSON into `slim.toml` entries.
//!
//! Accepted shapes (first match wins): `{"mcpServers": {...}}` (Claude
//! Desktop, Claude Code `.mcp.json`, Cursor, Pi `mcp.json`), `{"servers":
//! {...}}` (VS Code `mcp.json`) and `{"mcp": {"servers": {...}}}` (VS Code
//! settings). JSON with comments and trailing commas (VS Code) is accepted.
//!
//! Conversion never guesses: a server that needs something Slim cannot
//! provide (SSE transport, `${input:...}` prompts, variables in fields Slim
//! does not interpolate, invalid names) is skipped with a reason; fields Slim
//! has no equivalent for are dropped with a warning.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Map, Value};
use slim_core::mcp::McpExposure;

use crate::config::{
    mcp_server_defined_in, replace_mcp_server_to, FileConfig, FileMcpConfig, FileMcpOAuthConfig,
    FileMcpServerConfig, LayeredConfig,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ImportIssue {
    pub server: String,
    pub message: String,
}

#[derive(Debug, Default)]
pub(crate) struct ImportReport {
    pub servers: BTreeMap<String, FileMcpServerConfig>,
    /// Servers that could not be converted, with the reason.
    pub skipped: Vec<ImportIssue>,
    /// Converted servers that lost something in translation.
    pub warnings: Vec<ImportIssue>,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct ApplyOutcome {
    pub written: Vec<String>,
    /// Already defined in the target file and left untouched (no `--force`).
    pub kept_existing: Vec<String>,
}

fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Removes `//` and `/* */` comments and trailing commas outside strings.
fn strip_jsonc(text: &str) -> String {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    let mut in_string = false;
    while index < chars.len() {
        let ch = chars[index];
        if in_string {
            out.push(ch);
            if ch == '\\' {
                if let Some(next) = chars.get(index + 1) {
                    out.push(*next);
                    index += 1;
                }
            } else if ch == '"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                out.push(ch);
                index += 1;
            }
            '/' if chars.get(index + 1) == Some(&'/') => {
                while index < chars.len() && chars[index] != '\n' {
                    index += 1;
                }
            }
            '/' if chars.get(index + 1) == Some(&'*') => {
                index += 2;
                while index < chars.len()
                    && !(chars[index] == '*' && chars.get(index + 1) == Some(&'/'))
                {
                    index += 1;
                }
                index += 2;
            }
            ',' => {
                // Drop the comma when only whitespace/comments precede a
                // closing bracket.
                let mut lookahead = index + 1;
                loop {
                    match chars.get(lookahead) {
                        Some(c) if c.is_whitespace() => lookahead += 1,
                        Some('/') if chars.get(lookahead + 1) == Some(&'/') => {
                            while lookahead < chars.len() && chars[lookahead] != '\n' {
                                lookahead += 1;
                            }
                        }
                        Some('/') if chars.get(lookahead + 1) == Some(&'*') => {
                            lookahead += 2;
                            while lookahead < chars.len()
                                && !(chars[lookahead] == '*'
                                    && chars.get(lookahead + 1) == Some(&'/'))
                            {
                                lookahead += 1;
                            }
                            lookahead += 2;
                        }
                        _ => break,
                    }
                }
                if !matches!(chars.get(lookahead), Some('}' | ']')) {
                    out.push(',');
                }
                index += 1;
            }
            _ => {
                out.push(ch);
                index += 1;
            }
        }
    }
    out
}

/// Maps `${env:NAME}` / `${NAME}` to `${NAME}` and escapes everything else
/// that Slim's interpolation would otherwise treat as special, so the value
/// keeps the meaning it had in the source client.
fn convert_interpolated(text: &str) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(dollar) = rest.find('$') {
        out.push_str(&rest[..dollar]);
        let after = &rest[dollar + 1..];
        if let Some(braced) = after.strip_prefix('{') {
            if let Some(end) = braced.find('}') {
                let inner = &braced[..end];
                let name = inner.strip_prefix("env:").unwrap_or(inner);
                if !valid_env_name(name) {
                    return Err(format!(
                        "uses ${{{inner}}}, which Slim cannot expand (only ${{NAME}} environment variables)"
                    ));
                }
                out.push_str("${");
                out.push_str(name);
                out.push('}');
                rest = &braced[end + 1..];
                continue;
            }
        }
        // A bare `$` was literal in the source; `$$` keeps it literal here.
        out.push_str("$$");
        rest = after;
    }
    out.push_str(rest);
    if out.starts_with('!') {
        out.insert(0, '$');
    }
    Ok(out)
}

/// How Pi's `mcp.json` would have read `text` when it differs from the
/// literal Slim keeps: Pi expands a bare `$NAME` as an environment variable
/// and runs a whole value that starts with `!` as a command, while Slim reads
/// `${NAME}` only (after the conversion above) and treats both as text.
fn pi_reading(text: &str) -> Option<&'static str> {
    if text.starts_with('!') {
        return Some("starts with `!`, which Pi runs as a command");
    }
    let mut rest = text;
    while let Some(dollar) = rest.find('$') {
        let after = &rest[dollar + 1..];
        if after
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        {
            return Some("contains `$NAME`, which Pi expands as an environment variable");
        }
        rest = after.strip_prefix('{').unwrap_or(after);
    }
    None
}

/// Fields Slim does not interpolate must not carry variable syntax.
fn require_plain(field: &str, text: &str) -> Result<(), String> {
    if text.contains("${") {
        return Err(format!(
            "{field} uses a ${{...}} variable; Slim expands variables only in env, headers, and oauth.client_secret"
        ));
    }
    Ok(())
}

fn string_field<'a>(object: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

fn scalar_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn string_map(
    object: &Map<String, Value>,
    key: &str,
    interpolate: bool,
    differences: &mut Vec<String>,
) -> Result<Option<BTreeMap<String, String>>, String> {
    let Some(value) = object.get(key).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let Some(map) = value.as_object() else {
        return Err(format!("{key} must be an object"));
    };
    let mut converted = BTreeMap::new();
    for (name, value) in map {
        let text =
            scalar_to_string(value).ok_or_else(|| format!("{key}.{name} must be a string"))?;
        let text = if interpolate {
            if let Some(reading) = pi_reading(&text) {
                differences.push(format!(
                    "{key}.{name} {reading}; it was kept as literal text (write ${{NAME}} for a variable)"
                ));
            }
            convert_interpolated(&text).map_err(|reason| format!("{key}.{name} {reason}"))?
        } else {
            text
        };
        converted.insert(name.clone(), text);
    }
    Ok(Some(converted))
}

fn exposure_from(text: &str) -> Option<(McpExposure, Option<&'static str>)> {
    match text {
        "gateway" | "codemode" | "codemode-deferred" => Some((McpExposure::Gateway, None)),
        "direct" => Some((McpExposure::Direct, None)),
        "hidden" => Some((McpExposure::Hidden, None)),
        "deferred" => Some((
            McpExposure::Gateway,
            Some("exposure \"deferred\" has no Slim equivalent; using gateway"),
        )),
        _ => None,
    }
}

const KNOWN_KEYS: &[&str] = &[
    "type",
    "transport",
    "command",
    "args",
    "env",
    "cwd",
    "url",
    "serverUrl",
    "headers",
    "timeout",
    "enabled",
    "disabled",
    "description",
    "exposure",
    "toolExposure",
    "oauth",
];

fn convert_server(
    name: &str,
    value: &Value,
    warnings: &mut Vec<ImportIssue>,
) -> Result<FileMcpServerConfig, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "entry must be an object".to_owned())?;
    let mut warn = |message: String| {
        warnings.push(ImportIssue {
            server: name.to_owned(),
            message,
        });
    };
    for key in object.keys() {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            warn(format!("ignored unsupported field \"{key}\""));
        }
    }

    let command = string_field(object, "command")?;
    let url = match string_field(object, "url")? {
        Some(url) => Some(url),
        None => string_field(object, "serverUrl")?,
    };
    let declared = match string_field(object, "type")? {
        Some(kind) => Some(kind),
        None => string_field(object, "transport")?,
    }
    .map(str::to_ascii_lowercase);
    let http = match declared.as_deref() {
        Some("stdio") | Some("local") => false,
        Some("http" | "streamable-http" | "streamablehttp" | "streamable_http" | "remote") => true,
        Some("sse") => {
            return Err("legacy SSE transport is not supported; use the server's streamable HTTP endpoint (often /mcp instead of /sse)".to_owned());
        }
        Some(other) => return Err(format!("unsupported transport type \"{other}\"")),
        None => match (command.is_some(), url.is_some()) {
            (true, false) => false,
            (false, true) => true,
            (true, true) => return Err("sets both command and url".to_owned()),
            (false, false) => return Err("has neither command nor url".to_owned()),
        },
    };

    let mut server = FileMcpServerConfig::default();
    if http {
        if command.is_some() {
            return Err("an HTTP server must not set command".to_owned());
        }
        let url = url.ok_or_else(|| "HTTP server is missing url".to_owned())?;
        require_plain("url", url)?;
        server.url = Some(url.to_owned());
        let mut differences = Vec::new();
        server.headers = string_map(object, "headers", true, &mut differences)?;
        differences.into_iter().for_each(&mut warn);
        if object.get("env").is_some_and(|env| !env.is_null()) {
            warn("ignored env (HTTP servers have no environment)".to_owned());
        }
        if object.get("cwd").is_some_and(|cwd| !cwd.is_null()) {
            warn("ignored cwd (HTTP servers have no working directory)".to_owned());
        }
    } else {
        let command = command.ok_or_else(|| "stdio server is missing command".to_owned())?;
        require_plain("command", command)?;
        server.command = Some(command.to_owned());
        if let Some(args) = object.get("args").filter(|args| !args.is_null()) {
            let list = args
                .as_array()
                .ok_or_else(|| "args must be an array".to_owned())?;
            let mut converted = Vec::with_capacity(list.len());
            for (index, arg) in list.iter().enumerate() {
                let text = scalar_to_string(arg)
                    .ok_or_else(|| format!("args[{index}] must be a string"))?;
                require_plain(&format!("args[{index}]"), &text)?;
                converted.push(text);
            }
            server.args = Some(converted);
        }
        let mut differences = Vec::new();
        server.env = string_map(object, "env", true, &mut differences)?;
        differences.into_iter().for_each(&mut warn);
        if let Some(cwd) = string_field(object, "cwd")? {
            require_plain("cwd", cwd)?;
            server.cwd = Some(cwd.to_owned());
        }
        if object.get("headers").is_some_and(|value| !value.is_null()) {
            warn("ignored headers (stdio servers have no HTTP headers)".to_owned());
        }
    }

    if let Some(timeout) = object.get("timeout").filter(|value| !value.is_null()) {
        match timeout.as_f64().filter(|seconds| *seconds > 0.0) {
            Some(seconds) => {
                // Pi and most clients express the request timeout in seconds.
                let millis = (seconds * 1000.0).round();
                if (1_000.0..=600_000.0).contains(&millis) {
                    server.timeout_ms = Some(millis as u64);
                } else {
                    warn(format!(
                        "ignored timeout {seconds} (must be 1-600 seconds; read as seconds)"
                    ));
                }
            }
            None => warn("ignored timeout (must be a positive number of seconds)".to_owned()),
        }
    }
    if let Some(enabled) = object.get("enabled").and_then(Value::as_bool) {
        server.enabled = Some(enabled);
    } else if let Some(disabled) = object.get("disabled").and_then(Value::as_bool) {
        server.enabled = Some(!disabled);
    }
    if let Some(description) = string_field(object, "description")? {
        if !description.trim().is_empty() {
            server.description = Some(description.to_owned());
        }
    }
    if let Some(exposure) = string_field(object, "exposure")? {
        match exposure_from(exposure) {
            Some((mapped, note)) => {
                server.exposure = Some(mapped);
                if let Some(note) = note {
                    warn(note.to_owned());
                }
            }
            None => warn(format!("ignored unknown exposure \"{exposure}\"")),
        }
    }
    if let Some(tools) = object.get("toolExposure").filter(|value| !value.is_null()) {
        let map = tools
            .as_object()
            .ok_or_else(|| "toolExposure must be an object".to_owned())?;
        let mut converted = BTreeMap::new();
        for (pattern, exposure) in map {
            match exposure.as_str().and_then(exposure_from) {
                Some((mapped, note)) => {
                    converted.insert(pattern.clone(), mapped);
                    if let Some(note) = note {
                        warn(format!("toolExposure.{pattern}: {note}"));
                    }
                }
                None => warn(format!("ignored toolExposure.{pattern} (unknown exposure)")),
            }
        }
        if !converted.is_empty() {
            server.tool_exposure = Some(converted);
        }
    }
    if let Some(oauth) = object.get("oauth").filter(|value| !value.is_null()) {
        if !http {
            warn("ignored oauth (only HTTP servers use OAuth)".to_owned());
        } else {
            let map = oauth
                .as_object()
                .ok_or_else(|| "oauth must be an object".to_owned())?;
            let mut converted = FileMcpOAuthConfig::default();
            for (key, value) in map {
                let text = scalar_to_string(value);
                match (key.as_str(), text) {
                    ("clientId", Some(text)) => converted.client_id = Some(text),
                    ("clientSecret", Some(text)) => {
                        if let Some(reading) = pi_reading(&text) {
                            warn(format!(
                                "oauth.clientSecret {reading}; it was kept as literal text (write ${{NAME}} for a variable)"
                            ));
                        }
                        converted.client_secret = Some(
                            convert_interpolated(&text)
                                .map_err(|reason| format!("oauth.clientSecret {reason}"))?,
                        );
                    }
                    ("callbackPort", Some(text)) => match text.parse::<u16>() {
                        Ok(port) if port != 0 => converted.callback_port = Some(port),
                        _ => warn("ignored oauth.callbackPort (not a valid port)".to_owned()),
                    },
                    ("scope", Some(text)) => converted.scope = Some(text),
                    ("clientName", Some(text)) => converted.client_name = Some(text),
                    ("authServerMetadataUrl", Some(text)) => {
                        converted.auth_server_metadata_url = Some(text);
                    }
                    (other, _) => warn(format!("ignored unsupported field \"oauth.{other}\"")),
                }
            }
            server.oauth = Some(converted);
        }
    }
    Ok(server)
}

/// Converts the JSON text of another client's MCP configuration.
pub(crate) fn convert_mcp_json(text: &str) -> Result<ImportReport, String> {
    let document: Value = serde_json::from_str(&strip_jsonc(text))
        .map_err(|error| format!("not valid JSON: {error}"))?;
    let root = document
        .as_object()
        .ok_or_else(|| "expected a JSON object".to_owned())?;
    let servers = root
        .get("mcpServers")
        .or_else(|| root.get("servers"))
        .or_else(|| root.get("mcp").and_then(|mcp| mcp.get("servers")))
        .and_then(Value::as_object)
        .ok_or_else(|| "no \"mcpServers\" (or VS Code \"servers\") object found".to_owned())?;

    let mut report = ImportReport::default();
    let mut normalized: BTreeMap<String, String> = BTreeMap::new();
    for (name, entry) in servers {
        let mut skip = |message: String| {
            report.skipped.push(ImportIssue {
                server: name.clone(),
                message,
            });
        };
        let valid_name = !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');
        if !valid_name {
            skip("name must match ^[A-Za-z0-9_-]{1,64}$; rename it and import again".to_owned());
            continue;
        }
        if let Some(other) = normalized.get(&name.replace('-', "_")) {
            skip(format!(
                "name collides with \"{other}\" (names that differ only in '-' and '_' are the same server)"
            ));
            continue;
        }
        let mut warnings = Vec::new();
        let server = match convert_server(name, entry, &mut warnings) {
            Ok(server) => server,
            Err(reason) => {
                skip(reason);
                continue;
            }
        };
        // Same rules a loaded config goes through.
        let mut layered = LayeredConfig::default();
        crate::config::merge_layer(
            &mut layered,
            FileConfig {
                mcp: Some(FileMcpConfig {
                    servers: Some(BTreeMap::from([(name.clone(), server.clone())])),
                    ..FileMcpConfig::default()
                }),
                ..FileConfig::default()
            },
        );
        if let Err(error) = layered.mcp.validate() {
            skip(error);
            continue;
        }
        normalized.insert(name.replace('-', "_"), name.clone());
        report.warnings.extend(warnings);
        report.servers.insert(name.clone(), server);
    }
    Ok(report)
}

/// Writes converted servers into the TOML at `path`. Existing entries are
/// kept unless `force`.
pub(crate) fn apply_import(
    path: &Path,
    report: &ImportReport,
    force: bool,
) -> Result<ApplyOutcome, String> {
    let mut outcome = ApplyOutcome::default();
    for (name, server) in &report.servers {
        if !force && mcp_server_defined_in(path, name)? {
            outcome.kept_existing.push(name.clone());
            continue;
        }
        replace_mcp_server_to(path, name, server)?;
        outcome.written.push(name.clone());
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(json: &str) -> ImportReport {
        convert_mcp_json(json).expect("converts")
    }

    #[test]
    fn claude_desktop_stdio_entry_maps_directly() {
        let report = convert(
            r#"{"mcpServers":{"filesystem":{"command":"npx","args":["-y","@modelcontextprotocol/server-filesystem","."],"env":{"API_KEY":"abc"}}}}"#,
        );
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        let server = &report.servers["filesystem"];
        assert_eq!(server.command.as_deref(), Some("npx"));
        assert_eq!(
            server.args.as_deref(),
            Some(
                &[
                    "-y".to_owned(),
                    "@modelcontextprotocol/server-filesystem".into(),
                    ".".into()
                ][..]
            )
        );
        assert_eq!(server.env.as_ref().unwrap()["API_KEY"], "abc");
        assert_eq!(server.url, None);
    }

    #[test]
    fn claude_code_and_cursor_http_entries_map_url_and_headers() {
        let report = convert(
            r#"{"mcpServers":{
                "docs":{"type":"http","url":"https://example.com/mcp","headers":{"Authorization":"Bearer ${DOCS_TOKEN}"}},
                "cursor":{"url":"https://cursor.example/mcp"}
            }}"#,
        );
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        let docs = &report.servers["docs"];
        assert_eq!(docs.url.as_deref(), Some("https://example.com/mcp"));
        assert_eq!(
            docs.headers.as_ref().unwrap()["Authorization"],
            "Bearer ${DOCS_TOKEN}"
        );
        assert_eq!(
            report.servers["cursor"].url.as_deref(),
            Some("https://cursor.example/mcp")
        );
    }

    #[test]
    fn vscode_servers_with_comments_and_env_variables() {
        let report = convert(
            r#"{
              // VS Code allows comments
              "servers": {
                "gh": {
                  "type": "stdio",
                  "command": "gh-mcp",
                  "env": { "GITHUB_TOKEN": "${env:GITHUB_TOKEN}", "NOTE": "a $5 fee", },
                },
              },
            }"#,
        );
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        let env = report.servers["gh"].env.as_ref().unwrap();
        assert_eq!(env["GITHUB_TOKEN"], "${GITHUB_TOKEN}");
        // A bare `$` was literal in the source and stays literal.
        assert_eq!(env["NOTE"], "a $$5 fee");
    }

    #[test]
    fn pi_variable_and_command_values_are_kept_literal_with_a_warning() {
        let report = convert(
            r#"{"mcpServers":{
                "api":{"url":"https://x.test/mcp","headers":{
                    "Authorization":"!echo Bearer $(gh auth token)",
                    "X-Token":"$GITHUB_TOKEN",
                    "X-Fine":"Bearer ${DOCS_TOKEN}",
                    "X-Fee":"a $5 fee"}},
                "local":{"command":"x","env":{"TOKEN":"$HOME/bin"}}
            }}"#,
        );
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        let headers = report.servers["api"].headers.as_ref().unwrap();
        assert_eq!(headers["X-Token"], "$$GITHUB_TOKEN");
        assert_eq!(headers["Authorization"], "$!echo Bearer $$(gh auth token)");
        let warned = |server: &str, needle: &str| {
            report
                .warnings
                .iter()
                .any(|issue| issue.server == server && issue.message.contains(needle))
        };
        assert!(warned("api", "headers.Authorization starts with `!`"));
        assert!(warned("api", "headers.X-Token contains `$NAME`"));
        assert!(warned("local", "env.TOKEN contains `$NAME`"));
        assert!(!warned("api", "X-Fine") && !warned("api", "X-Fee"));
    }

    #[test]
    fn vscode_settings_nested_servers_are_found() {
        let report = convert(r#"{"mcp":{"servers":{"a":{"command":"x"}}}}"#);
        assert!(report.servers.contains_key("a"));
    }

    #[test]
    fn input_prompts_and_unsupported_variables_skip_the_server_with_a_reason() {
        let report = convert(
            r#"{"servers":{
                "needs-input":{"command":"x","env":{"TOKEN":"${input:api-key}"}},
                "ws":{"command":"x","args":["${workspaceFolder}"]},
                "ok":{"command":"y"}
            }}"#,
        );
        assert_eq!(report.servers.keys().collect::<Vec<_>>(), ["ok"]);
        let reasons: BTreeMap<_, _> = report
            .skipped
            .iter()
            .map(|issue| (issue.server.as_str(), issue.message.as_str()))
            .collect();
        assert!(
            reasons["needs-input"].contains("${input:api-key}"),
            "{reasons:?}"
        );
        assert!(reasons["ws"].contains("args[0]"), "{reasons:?}");
    }

    #[test]
    fn sse_and_invalid_entries_are_skipped_not_guessed() {
        let report = convert(
            r#"{"mcpServers":{
                "legacy":{"type":"sse","url":"https://x/sse"},
                "both":{"command":"a","url":"https://x"},
                "neither":{},
                "bad name":{"command":"a"},
                "a-b":{"command":"a"},
                "a_b":{"command":"b"}
            }}"#,
        );
        let skipped: Vec<&str> = report.skipped.iter().map(|i| i.server.as_str()).collect();
        for expected in ["legacy", "both", "neither", "bad name", "a_b"] {
            assert!(
                skipped.contains(&expected),
                "{expected} not skipped: {skipped:?}"
            );
        }
        assert!(report.servers.contains_key("a-b"));
        let legacy = report
            .skipped
            .iter()
            .find(|i| i.server == "legacy")
            .unwrap();
        assert!(legacy.message.contains("SSE"), "{}", legacy.message);
    }

    #[test]
    fn pi_fields_map_and_unsupported_ones_warn() {
        let report = convert(
            r#"{"mcpServers":{"s":{
                "url":"https://example.com/mcp",
                "timeout": 90,
                "disabled": true,
                "description": "Docs search",
                "exposure": "deferred",
                "toolExposure": {"search_*": "direct", "danger": "hidden"},
                "oauth": {"clientId":"id","clientSecret":"${SECRET}","callbackPort":8765,"callbackUrl":"http://127.0.0.1/x"},
                "alwaysAllow": ["x"]
            }}}"#,
        );
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        let server = &report.servers["s"];
        assert_eq!(server.timeout_ms, Some(90_000));
        assert_eq!(server.enabled, Some(false));
        assert_eq!(server.description.as_deref(), Some("Docs search"));
        assert_eq!(server.exposure, Some(McpExposure::Gateway));
        let tools = server.tool_exposure.as_ref().unwrap();
        assert_eq!(tools["search_*"], McpExposure::Direct);
        assert_eq!(tools["danger"], McpExposure::Hidden);
        let oauth = server.oauth.as_ref().unwrap();
        assert_eq!(oauth.client_id.as_deref(), Some("id"));
        assert_eq!(oauth.client_secret.as_deref(), Some("${SECRET}"));
        assert_eq!(oauth.callback_port, Some(8765));
        let warnings: Vec<&str> = report.warnings.iter().map(|w| w.message.as_str()).collect();
        assert!(
            warnings.iter().any(|w| w.contains("alwaysAllow")),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("oauth.callbackUrl")),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("deferred")),
            "{warnings:?}"
        );
    }

    #[test]
    fn leading_bang_is_escaped_so_it_stays_a_literal() {
        let report = convert(r#"{"mcpServers":{"s":{"command":"x","env":{"FLAG":"!on"}}}}"#);
        assert_eq!(report.servers["s"].env.as_ref().unwrap()["FLAG"], "$!on");
    }

    #[test]
    fn converted_entries_pass_config_validation() {
        let report = convert(
            r#"{"mcpServers":{
                "bad-oauth":{"command":"x","oauth":{"clientId":"id"}},
                "bad-cwd":{"url":"https://x","cwd":"/tmp"}
            }}"#,
        );
        // oauth on stdio and cwd on http are dropped with warnings, not
        // written as invalid entries.
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert!(report.servers["bad-oauth"].oauth.is_none());
        assert!(report.servers["bad-cwd"].cwd.is_none());
        assert_eq!(report.warnings.len(), 2, "{:?}", report.warnings);
    }

    #[test]
    fn missing_server_object_and_bad_json_are_errors() {
        assert!(convert_mcp_json("{").is_err());
        assert!(convert_mcp_json("[]").is_err());
        assert!(convert_mcp_json(r#"{"other":{}}"#)
            .unwrap_err()
            .contains("mcpServers"));
    }

    #[test]
    fn apply_never_overwrites_without_force() {
        let path = std::env::temp_dir().join(format!(
            "slim-mcp-import-apply-{}-{:?}.toml",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        std::fs::write(
            &path,
            "model = \"keep\"\n[mcp.servers.fs]\ncommand = \"old\"\nenabled = false\n",
        )
        .unwrap();
        let report = convert(
            r#"{"mcpServers":{"fs":{"command":"new"},"web":{"url":"https://example.com/mcp"}}}"#,
        );
        let outcome = apply_import(&path, &report, false).unwrap();
        assert_eq!(outcome.written, ["web"]);
        assert_eq!(outcome.kept_existing, ["fs"]);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("command = \"old\"") && text.contains("model = \"keep\""));

        let outcome = apply_import(&path, &report, true).unwrap();
        assert_eq!(outcome.written, ["fs", "web"]);
        let parsed: toml::Table = std::fs::read_to_string(&path).unwrap().parse().unwrap();
        let fs_table = parsed["mcp"]["servers"]["fs"].as_table().unwrap();
        assert_eq!(fs_table["command"].as_str(), Some("new"));
        assert!(
            fs_table.get("enabled").is_none(),
            "force replaces the entry"
        );
        assert_eq!(parsed["model"].as_str(), Some("keep"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn jsonc_stripper_leaves_string_contents_alone() {
        let cleaned = strip_jsonc(r#"{"u":"http://x//y /* z */", /* c */ "a":[1,2,],}"#);
        let value: Value = serde_json::from_str(&cleaned).unwrap();
        assert_eq!(value["u"], "http://x//y /* z */");
        assert_eq!(value["a"], serde_json::json!([1, 2]));
    }
}
