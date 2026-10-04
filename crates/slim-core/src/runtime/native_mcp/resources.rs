//! Gateway actions for MCP resources, in the shape Codex's resource tools
//! use:
//!
//! * `mcp {resources:true, server?, cursor?}` lists resources;
//! * `mcp {resource_templates:true, server?, cursor?}` lists templates;
//! * `mcp {server, uri}` reads one.
//!
//! With a `server` a listing is one page (`cursor` continues it). Without,
//! every connected server is listed whole and merged, with the servers that
//! failed reported next to the entries instead of failing the call. MCP App
//! entries (`ui://`) are left out. Like every `mcp` action these run in Auto
//! mode only.

use super::render::{render_read_resource_result, RenderEnv, Rendered};
use super::*;
use crate::mcp::McpResourcePage;

const MAX_URI_BYTES: usize = 4096;
const MAX_CURSOR_BYTES: usize = 4096;
/// A listing the model sees stays valid JSON within this size: entries that
/// do not fit are left out (counted in `omitted`) and the complete listing
/// is stored as an artifact.
const LISTING_INLINE_BYTES: usize = 12 * 1024;
/// Room kept for the `omitted` and `complete` fields.
const LISTING_RESERVE_BYTES: usize = 512;

/// What a resource request asks for.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum ResourceAction {
    List {
        templates: bool,
        server: Option<String>,
        cursor: Option<String>,
    },
    Read {
        server: String,
        uri: String,
    },
}

/// The resource fields of an `mcp` call, validated against the rest.
pub(super) struct ResourceFields<'a> {
    pub(super) resources: bool,
    pub(super) resource_templates: bool,
    pub(super) uri: Option<&'a str>,
    pub(super) cursor: Option<&'a str>,
    pub(super) server: Option<&'a str>,
    /// Any of `list`, `describe`, `tool`, `query`, `arguments`.
    pub(super) discovery_or_call: bool,
}

/// `Ok(None)`: not a resource request. `Err`: a malformed one.
pub(super) fn parse_action(fields: &ResourceFields<'_>) -> Result<Option<ResourceAction>, String> {
    let listing = fields.resources || fields.resource_templates;
    if !listing && fields.uri.is_none() && fields.cursor.is_none() {
        return Ok(None);
    }
    if fields.resources && fields.resource_templates {
        return Err("resources and resource_templates cannot be combined".into());
    }
    if fields.discovery_or_call {
        return Err(
            "resources, resource_templates and uri cannot be combined with list, describe, tool, query or arguments"
                .into(),
        );
    }
    if listing {
        if fields.uri.is_some() {
            return Err("uri reads a resource; it cannot be combined with a listing".into());
        }
        let server = fields.server.map(str::to_owned);
        if fields.cursor.is_some() && server.is_none() {
            return Err("cursor can only be used when a server is specified".into());
        }
        if fields
            .cursor
            .is_some_and(|cursor| cursor.is_empty() || cursor.len() > MAX_CURSOR_BYTES)
        {
            return Err(format!("cursor must contain 1-{MAX_CURSOR_BYTES} bytes"));
        }
        return Ok(Some(ResourceAction::List {
            templates: fields.resource_templates,
            server,
            cursor: fields.cursor.map(str::to_owned),
        }));
    }
    if fields.cursor.is_some() {
        return Err("cursor continues a resources or resource_templates listing".into());
    }
    let (Some(server), Some(uri)) = (fields.server, fields.uri) else {
        return Err("reading a resource needs both server and uri".into());
    };
    let uri = uri.trim();
    if uri.is_empty() || uri.len() > MAX_URI_BYTES {
        return Err(format!("uri must contain 1-{MAX_URI_BYTES} bytes"));
    }
    Ok(Some(ResourceAction::Read {
        server: server.to_owned(),
        uri: uri.to_owned(),
    }))
}

/// The listing as compact JSON the model can parse whatever its size: the
/// entries under `key` that exceed the inline budget are dropped from the
/// end, `omitted` counts them, and `complete` points at an artifact holding
/// the whole listing.
fn fit_listing(mut payload: Value, key: &str, env: &RenderEnv<'_>) -> Rendered {
    let full = payload.to_string();
    if full.len() <= LISTING_INLINE_BYTES {
        return Rendered::plain(full);
    }
    let mut blobs = Vec::new();
    let Some(entries) = payload.get_mut(key).and_then(Value::as_array_mut) else {
        return Rendered::plain(full);
    };
    let all = std::mem::take(entries);
    let base = payload.to_string().len();
    let mut used = base + LISTING_RESERVE_BYTES;
    let mut kept = Vec::new();
    for entry in &all {
        used += entry.to_string().len() + 1;
        if used > LISTING_INLINE_BYTES {
            break;
        }
        kept.push(entry.clone());
    }
    let omitted = all.len() - kept.len();
    payload[key] = Value::Array(kept);
    payload["omitted"] = json!(payload["omitted"].as_u64().unwrap_or(0) + omitted as u64);
    if let Some(store) = env.store {
        let stored = crate::runtime::redact_values(env.secrets, &full);
        if let Ok(handle) = store.put("mcp-listing", stored.as_bytes()) {
            payload["complete"] = json!(format!(
                "artifact id={} path={}",
                handle.id,
                handle.path.display()
            ));
            blobs.push(handle);
        }
    }
    let mut rendered = Rendered::plain(payload.to_string());
    rendered.blobs = blobs;
    rendered
}

fn page_payload(server: &str, templates: bool, page: McpResourcePage) -> Value {
    let key = if templates {
        "resourceTemplates"
    } else {
        "resources"
    };
    let mut payload = serde_json::Map::new();
    payload.insert("server".into(), json!(server));
    payload.insert(
        key.into(),
        Value::Array(
            page.items
                .iter()
                .map(|item| item.to_json(server, templates))
                .collect(),
        ),
    );
    if let Some(cursor) = page.next_cursor {
        payload.insert("nextCursor".into(), json!(cursor));
    }
    if page.omitted > 0 {
        payload.insert("omitted".into(), json!(page.omitted));
    }
    Value::Object(payload)
}

/// Every connected server's complete listing, merged, with the failures.
async fn list_merged(
    manager: &McpManager,
    templates: bool,
    cancellation: McpCancellation,
    env: &RenderEnv<'_>,
) -> McpRequestOutcome<Rendered> {
    let mut targets = manager.resource_targets();
    targets.listable.sort();
    targets.not_connected.sort();
    let requests = targets.listable.iter().map(|server| {
        let cancellation = cancellation.clone();
        async move {
            if templates {
                manager
                    .all_resource_templates_cancellable(server, cancellation)
                    .await
            } else {
                manager
                    .all_resources_cancellable(server, cancellation)
                    .await
            }
        }
    });
    let outcomes = futures_util::future::join_all(requests).await;
    let key = if templates {
        "resourceTemplates"
    } else {
        "resources"
    };
    let mut entries = Vec::new();
    let mut errors = Vec::new();
    let mut truncated = Vec::new();
    for (server, outcome) in targets.listable.iter().zip(outcomes) {
        match outcome {
            McpRequestOutcome::Completed(Ok(listing)) => {
                if listing.truncated {
                    truncated.push(json!(server));
                }
                entries.extend(
                    listing
                        .items
                        .iter()
                        .map(|item| item.to_json(server, templates)),
                );
            }
            McpRequestOutcome::Completed(Err(error)) => {
                errors.push(json!({"server": server, "error": error.to_string()}));
            }
            McpRequestOutcome::InterruptedBeforeSend {
                interruption,
                cleanup,
            } => {
                if cancellation.is_cancelled() {
                    return McpRequestOutcome::InterruptedBeforeSend {
                        interruption,
                        cleanup,
                    };
                }
                errors.push(
                    json!({"server": server, "error": format!("interrupted: {interruption:?}")}),
                );
            }
            McpRequestOutcome::OutcomeUncertain {
                interruption,
                cleanup,
            } => {
                if cancellation.is_cancelled() {
                    return McpRequestOutcome::OutcomeUncertain {
                        interruption,
                        cleanup,
                    };
                }
                errors.push(
                    json!({"server": server, "error": format!("interrupted: {interruption:?}")}),
                );
            }
        }
    }
    let mut payload = serde_json::Map::new();
    payload.insert(key.into(), Value::Array(entries));
    if !errors.is_empty() {
        payload.insert("errors".into(), Value::Array(errors));
    }
    if !truncated.is_empty() {
        payload.insert("truncated".into(), Value::Array(truncated));
    }
    if !targets.not_connected.is_empty() {
        payload.insert("notConnected".into(), json!(targets.not_connected));
    }
    McpRequestOutcome::Completed(Ok(fit_listing(Value::Object(payload), key, env)))
}

pub(super) async fn dispatch(
    manager: &McpManager,
    action: ResourceAction,
    cancellation: McpCancellation,
    env: &RenderEnv<'_>,
) -> McpRequestOutcome<Rendered> {
    match action {
        ResourceAction::List {
            templates,
            server: None,
            ..
        } => list_merged(manager, templates, cancellation, env).await,
        ResourceAction::List {
            templates,
            server: Some(server),
            cursor,
        } => {
            let page = if templates {
                manager
                    .resource_templates_page_cancellable(&server, cursor.as_deref(), cancellation)
                    .await
            } else {
                manager
                    .resources_page_cancellable(&server, cursor.as_deref(), cancellation)
                    .await
            };
            page.map(|page| {
                let key = if templates {
                    "resourceTemplates"
                } else {
                    "resources"
                };
                fit_listing(page_payload(&server, templates, page), key, env)
            })
        }
        ResourceAction::Read { server, uri } => manager
            .read_resource_cancellable(&server, &uri, cancellation)
            .await
            .map(|value| render_read_resource_result(&value, &uri, env)),
    }
}

/// Adds the resource actions to the `mcp` tool definition.
pub(super) fn extend_tool_definition(definition: &mut Value) {
    const NOTE: &str = " Resources: {resources:true,server?,cursor?} lists resources and {resource_templates:true,server?,cursor?} lists templates (without server: every connected server, with per-server errors); {server,uri} reads one resource. Images are shown; binary and oversized results are stored as artifacts.";
    if let Some(description) = definition.get_mut("description") {
        if let Some(text) = description.as_str() {
            *description = Value::String(format!("{text}{NOTE}"));
        }
    }
    if let Some(properties) = definition
        .pointer_mut("/input_schema/properties")
        .and_then(Value::as_object_mut)
    {
        properties.insert("resources".into(), json!({"type": "boolean"}));
        properties.insert("resource_templates".into(), json!({"type": "boolean"}));
        properties.insert(
            "uri".into(),
            json!({"type": "string", "minLength": 1, "maxLength": MAX_URI_BYTES}),
        );
        properties.insert(
            "cursor".into(),
            json!({"type": "string", "minLength": 1, "maxLength": MAX_CURSOR_BYTES}),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields<'a>() -> ResourceFields<'a> {
        ResourceFields {
            resources: false,
            resource_templates: false,
            uri: None,
            cursor: None,
            server: None,
            discovery_or_call: false,
        }
    }

    #[test]
    fn actions_are_recognized_and_validated() {
        assert_eq!(parse_action(&fields()), Ok(None));
        assert_eq!(
            parse_action(&ResourceFields {
                resources: true,
                ..fields()
            }),
            Ok(Some(ResourceAction::List {
                templates: false,
                server: None,
                cursor: None
            }))
        );
        assert_eq!(
            parse_action(&ResourceFields {
                resource_templates: true,
                server: Some("fs"),
                cursor: Some("c1"),
                ..fields()
            }),
            Ok(Some(ResourceAction::List {
                templates: true,
                server: Some("fs".into()),
                cursor: Some("c1".into())
            }))
        );
        assert_eq!(
            parse_action(&ResourceFields {
                server: Some("fs"),
                uri: Some("  file:///a "),
                ..fields()
            }),
            Ok(Some(ResourceAction::Read {
                server: "fs".into(),
                uri: "file:///a".into()
            }))
        );
    }

    #[test]
    fn malformed_resource_requests_are_rejected() {
        let bad: Vec<ResourceFields<'_>> = vec![
            ResourceFields {
                resources: true,
                resource_templates: true,
                ..fields()
            },
            ResourceFields {
                resources: true,
                cursor: Some("c"),
                ..fields()
            },
            ResourceFields {
                resources: true,
                uri: Some("x"),
                ..fields()
            },
            ResourceFields {
                resources: true,
                discovery_or_call: true,
                ..fields()
            },
            ResourceFields {
                server: Some("s"),
                uri: Some("x"),
                discovery_or_call: true,
                ..fields()
            },
            ResourceFields {
                uri: Some("x"),
                ..fields()
            },
            ResourceFields {
                server: Some("s"),
                uri: Some("   "),
                ..fields()
            },
            ResourceFields {
                cursor: Some("c"),
                ..fields()
            },
            ResourceFields {
                server: Some("s"),
                cursor: Some("c"),
                uri: Some("u"),
                ..fields()
            },
            ResourceFields {
                resources: true,
                server: Some("s"),
                cursor: Some(""),
                ..fields()
            },
        ];
        for (index, case) in bad.iter().enumerate() {
            assert!(parse_action(case).is_err(), "case {index} was accepted");
        }
        let long = "u".repeat(MAX_URI_BYTES + 1);
        assert!(parse_action(&ResourceFields {
            server: Some("s"),
            uri: Some(&long),
            ..fields()
        })
        .is_err());
    }

    #[test]
    fn the_tool_definition_gains_the_resource_properties_only() {
        let definition = mcp_tool_definition();
        let properties = definition.pointer("/input_schema/properties").unwrap();
        for key in [
            "list",
            "server",
            "tool",
            "describe",
            "query",
            "arguments",
            "offset",
            "resources",
            "resource_templates",
            "uri",
            "cursor",
        ] {
            assert!(properties.get(key).is_some(), "{key}");
        }
        assert_eq!(definition["input_schema"]["additionalProperties"], false);
        assert!(definition["description"]
            .as_str()
            .unwrap()
            .contains("{server,uri} reads one resource"));
        // Stable across calls: the definition is part of the cached prompt.
        assert_eq!(definition, mcp_tool_definition());
    }
}
