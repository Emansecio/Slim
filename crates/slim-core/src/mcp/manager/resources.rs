//! Resources of one MCP server: `resources/list`, `resources/templates/list`
//! and `resources/read`, with paging, bounded server text, MCP App filtering
//! and a per-server cache of the complete listing that
//! `notifications/resources/list_changed` (or a reconnect) invalidates.
//!
//! Resource requests are idempotent reads, so they carry no replay hazard,
//! and the HTTP transport already renews an expired session for them once.
//! Cancelling one still tears the connection down like any other cancelled
//! request, because it may already have reached the server.

use std::collections::HashSet;

use super::*;

/// Pages fetched for one complete listing.
const MAX_RESOURCE_PAGES: usize = 16;
/// Entries kept for one server's complete listing.
const MAX_RESOURCES_PER_SERVER: usize = 1000;
/// Entries kept from one page; a hostile page cannot grow memory further.
const MAX_ITEMS_PER_PAGE: usize = 500;
/// Longest resource URI (or template) kept or sent.
const MAX_URI_BYTES: usize = 4096;
const MAX_NAME_CHARS: usize = 256;
const MAX_DESCRIPTION_CHARS: usize = 512;
const MAX_MIME_CHARS: usize = 128;
const MAX_CURSOR_BYTES: usize = 4096;

/// JSON-RPC "method not found": a server without templates answers so.
const METHOD_NOT_FOUND: i64 = -32601;

/// One listed resource or resource template. For a template `uri` holds the
/// RFC 6570 `uriTemplate`. Server text is single-line and length-capped.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpResourceItem {
    pub uri: String,
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub mime_type: Option<String>,
    pub size: Option<u64>,
}

impl McpResourceItem {
    /// Listing entry in the shape Codex's resource tools use:
    /// `{server, uri|uriTemplate, name, title?, description?, mimeType?,
    /// size?}`. `_meta` and icons are not carried.
    pub fn to_json(&self, server: &str, template: bool) -> Value {
        let mut entry = serde_json::Map::new();
        entry.insert("server".into(), json!(server));
        entry.insert(
            if template { "uriTemplate" } else { "uri" }.into(),
            json!(self.uri),
        );
        entry.insert("name".into(), json!(self.name));
        if let Some(title) = &self.title {
            entry.insert("title".into(), json!(title));
        }
        if let Some(description) = &self.description {
            entry.insert("description".into(), json!(description));
        }
        if let Some(mime_type) = &self.mime_type {
            entry.insert("mimeType".into(), json!(mime_type));
        }
        if let Some(size) = self.size {
            entry.insert("size".into(), json!(size));
        }
        Value::Object(entry)
    }
}

/// One page of a listing.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct McpResourcePage {
    pub items: Vec<McpResourceItem>,
    pub next_cursor: Option<String>,
    /// Entries the server listed beyond the per-page cap.
    pub omitted: usize,
}

/// Every page of a listing, bounded.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct McpResourceListing {
    pub items: Vec<McpResourceItem>,
    /// The page, entry or cursor-loop bound cut the listing short.
    pub truncated: bool,
}

/// Cached listing sizes for status surfaces; `None` until fetched.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct McpResourceCounts {
    pub resources: Option<usize>,
    pub templates: Option<usize>,
}

/// Servers a merged resource listing covers.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct McpResourceTargets {
    /// Connected (or connecting) servers that may offer resources.
    pub listable: Vec<String>,
    /// Enabled servers that are not connected; a merged listing does not
    /// start them (naming the server does).
    pub not_connected: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ListingKind {
    Resources,
    Templates,
}

impl ListingKind {
    fn method(self) -> &'static str {
        match self {
            Self::Resources => "resources/list",
            Self::Templates => "resources/templates/list",
        }
    }

    fn array_key(self) -> &'static str {
        match self {
            Self::Resources => "resources",
            Self::Templates => "resourceTemplates",
        }
    }

    fn uri_key(self) -> &'static str {
        match self {
            Self::Resources => "uri",
            Self::Templates => "uriTemplate",
        }
    }
}

#[derive(Default)]
struct CachedListings {
    connection: Option<Weak<dyn McpConnection>>,
    resources: Option<Arc<McpResourceListing>>,
    templates: Option<Arc<McpResourceListing>>,
}

impl CachedListings {
    fn belongs_to(&self, connection: &Arc<dyn McpConnection>) -> bool {
        self.connection
            .as_ref()
            .is_some_and(|cached| Weak::ptr_eq(cached, &Arc::downgrade(connection)))
    }
}

/// Complete listings per server, valid for one connection only.
#[derive(Default)]
pub(super) struct ResourceCache {
    servers: Mutex<BTreeMap<String, CachedListings>>,
}

impl ResourceCache {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, CachedListings>> {
        self.servers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn get(
        &self,
        server: &str,
        connection: &Arc<dyn McpConnection>,
        kind: ListingKind,
    ) -> Option<Arc<McpResourceListing>> {
        let servers = self.lock();
        let cached = servers.get(server)?;
        if !cached.belongs_to(connection) {
            return None;
        }
        match kind {
            ListingKind::Resources => cached.resources.clone(),
            ListingKind::Templates => cached.templates.clone(),
        }
    }

    fn put(
        &self,
        server: &str,
        connection: &Arc<dyn McpConnection>,
        kind: ListingKind,
        listing: Arc<McpResourceListing>,
    ) {
        let mut servers = self.lock();
        let cached = servers.entry(server.to_owned()).or_default();
        if !cached.belongs_to(connection) {
            *cached = CachedListings {
                connection: Some(Arc::downgrade(connection)),
                ..CachedListings::default()
            };
        }
        match kind {
            ListingKind::Resources => cached.resources = Some(listing),
            ListingKind::Templates => cached.templates = Some(listing),
        }
    }

    fn invalidate(&self, server: &str) {
        self.lock().remove(server);
    }

    fn counts(&self, server: &str, connection: &Arc<dyn McpConnection>) -> McpResourceCounts {
        let servers = self.lock();
        let Some(cached) = servers
            .get(server)
            .filter(|cached| cached.belongs_to(connection))
        else {
            return McpResourceCounts::default();
        };
        McpResourceCounts {
            resources: cached.resources.as_ref().map(|listing| listing.items.len()),
            templates: cached.templates.as_ref().map(|listing| listing.items.len()),
        }
    }
}

/// A connection ready to carry resource requests.
struct Live {
    entry: Arc<ServerEntry>,
    connection: Arc<dyn McpConnection>,
    generation: u64,
}

/// Moves a non-`Ok` outcome to another payload type.
fn passthrough<T, U>(outcome: McpRequestOutcome<T>) -> Result<T, McpRequestOutcome<U>> {
    match outcome {
        McpRequestOutcome::Completed(Ok(value)) => Ok(value),
        McpRequestOutcome::Completed(Err(error)) => Err(McpRequestOutcome::Completed(Err(error))),
        McpRequestOutcome::InterruptedBeforeSend {
            interruption,
            cleanup,
        } => Err(McpRequestOutcome::InterruptedBeforeSend {
            interruption,
            cleanup,
        }),
        McpRequestOutcome::OutcomeUncertain {
            interruption,
            cleanup,
        } => Err(McpRequestOutcome::OutcomeUncertain {
            interruption,
            cleanup,
        }),
    }
}

fn offers_resources(entry: &ServerEntry) -> bool {
    // Test hooks insert connections without a handshake: capability unknown.
    entry
        .handshake
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .is_none_or(|handshake| handshake.has_resources())
}

/// Single-line, length-capped form of server text.
pub(crate) fn clean_text(text: &str, max_chars: usize) -> String {
    let cleaned: String = text
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.chars().count() > max_chars {
        let mut shortened: String = cleaned.chars().take(max_chars).collect();
        shortened.push('…');
        shortened
    } else {
        cleaned.to_owned()
    }
}

fn clean_optional(value: Option<&Value>, max_chars: usize) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(|text| clean_text(text, max_chars))
        .filter(|text| !text.is_empty())
}

/// MCP App user interfaces (`ui://` URIs, `profile=mcp-app` HTML) are for
/// hosts that render them, not for a model.
pub fn is_mcp_app_resource(uri: &str, mime_type: Option<&str>) -> bool {
    if uri.trim_start().to_ascii_lowercase().starts_with("ui://") {
        return true;
    }
    let Some(mime_type) = mime_type else {
        return false;
    };
    mime_type
        .split(';')
        .skip(1)
        .filter_map(|parameter| parameter.split_once('='))
        .any(|(key, value)| {
            key.trim().eq_ignore_ascii_case("profile")
                && value
                    .trim()
                    .trim_matches('"')
                    .eq_ignore_ascii_case("mcp-app")
        })
}

fn parse_page(kind: ListingKind, response: &Value) -> Result<McpResourcePage, McpError> {
    let entries = response
        .get(kind.array_key())
        .and_then(Value::as_array)
        .ok_or_else(|| {
            McpError::Protocol(format!(
                "{} result missing {} array",
                kind.method(),
                kind.array_key()
            ))
        })?;
    let mut items = Vec::new();
    let mut omitted = 0;
    for entry in entries {
        let Some(uri) = entry
            .get(kind.uri_key())
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|uri| !uri.is_empty() && uri.len() <= MAX_URI_BYTES)
        else {
            continue;
        };
        let mime_type = clean_optional(entry.get("mimeType"), MAX_MIME_CHARS);
        if is_mcp_app_resource(uri, mime_type.as_deref()) {
            continue;
        }
        if items.len() >= MAX_ITEMS_PER_PAGE {
            omitted += 1;
            continue;
        }
        let name = clean_optional(entry.get("name"), MAX_NAME_CHARS)
            .unwrap_or_else(|| clean_text(uri, MAX_NAME_CHARS));
        items.push(McpResourceItem {
            uri: uri.to_owned(),
            name,
            title: clean_optional(entry.get("title"), MAX_NAME_CHARS),
            description: clean_optional(entry.get("description"), MAX_DESCRIPTION_CHARS),
            mime_type,
            size: entry.get("size").and_then(Value::as_u64),
        });
    }
    let next_cursor = response
        .get("nextCursor")
        .and_then(Value::as_str)
        .filter(|cursor| !cursor.is_empty() && cursor.len() <= MAX_CURSOR_BYTES)
        .map(str::to_owned);
    Ok(McpResourcePage {
        items,
        next_cursor,
        omitted,
    })
}

fn interrupted(interruption: McpInterruption) -> McpRequestOutcome<Value> {
    McpRequestOutcome::InterruptedBeforeSend {
        interruption,
        cleanup: McpCleanupStatus::NotRequired,
    }
}

impl McpManager {
    /// Which servers a merged listing covers: enabled, startable, not hidden
    /// servers that are connected (or connecting) and may offer resources.
    /// Servers that are not connected are reported separately; a merged
    /// listing never starts one, like a global tool search.
    pub fn resource_targets(&self) -> McpResourceTargets {
        let mut targets = McpResourceTargets::default();
        for info in self.statuses() {
            if !info.enabled || info.exposure == McpExposure::Hidden {
                continue;
            }
            match info.status {
                McpServerStatus::Ready { .. } => {
                    if info
                        .handshake
                        .as_ref()
                        .is_none_or(|handshake| handshake.has_resources())
                    {
                        targets.listable.push(info.name);
                    }
                }
                McpServerStatus::Connecting => targets.listable.push(info.name),
                McpServerStatus::Disconnected
                | McpServerStatus::Failed { .. }
                | McpServerStatus::NeedsAuth { .. } => {
                    targets.not_connected.push(info.name);
                }
                McpServerStatus::Disabled | McpServerStatus::Untrusted => {}
            }
        }
        targets
    }

    /// Sizes of the cached complete listings of `server`; `None` fields were
    /// never fetched (or were invalidated by `resources/list_changed`).
    pub fn cached_resource_counts(&self, server: &str) -> McpResourceCounts {
        let Ok(entry) = self.entry(server) else {
            return McpResourceCounts::default();
        };
        // A server that is mid-connect or not connected has no cache.
        let Ok(slot) = entry.connection.try_lock() else {
            return McpResourceCounts::default();
        };
        slot.as_ref()
            .map(|connection| self.resource_cache.counts(server, connection))
            .unwrap_or_default()
    }

    /// One page of `server`'s resources. Not cached: the cursor is the
    /// caller's.
    pub async fn resources_page_cancellable(
        &self,
        server: &str,
        cursor: Option<&str>,
        cancellation: McpCancellation,
    ) -> McpRequestOutcome<McpResourcePage> {
        self.listing_page(server, ListingKind::Resources, cursor, cancellation)
            .await
    }

    /// One page of `server`'s resource templates; a server without template
    /// support has none.
    pub async fn resource_templates_page_cancellable(
        &self,
        server: &str,
        cursor: Option<&str>,
        cancellation: McpCancellation,
    ) -> McpRequestOutcome<McpResourcePage> {
        self.listing_page(server, ListingKind::Templates, cursor, cancellation)
            .await
    }

    /// Every page of `server`'s resources, cached until the server reports
    /// a change or the connection is replaced.
    pub async fn all_resources_cancellable(
        &self,
        server: &str,
        cancellation: McpCancellation,
    ) -> McpRequestOutcome<Arc<McpResourceListing>> {
        self.listing_all(server, ListingKind::Resources, cancellation)
            .await
    }

    /// Every page of `server`'s resource templates, cached like resources.
    pub async fn all_resource_templates_cancellable(
        &self,
        server: &str,
        cancellation: McpCancellation,
    ) -> McpRequestOutcome<Arc<McpResourceListing>> {
        self.listing_all(server, ListingKind::Templates, cancellation)
            .await
    }

    /// `resources/read`: the raw `{contents: [...]}` result of the server.
    pub async fn read_resource_cancellable(
        &self,
        server: &str,
        uri: &str,
        cancellation: McpCancellation,
    ) -> McpRequestOutcome<Value> {
        let uri = uri.trim();
        if uri.is_empty() || uri.len() > MAX_URI_BYTES {
            return McpRequestOutcome::Completed(Err(McpError::Protocol(format!(
                "resource uri must contain 1-{MAX_URI_BYTES} bytes"
            ))));
        }
        let live = match passthrough(self.live_connection(server, &cancellation).await) {
            Ok(live) => live,
            Err(outcome) => return outcome,
        };
        let response = match passthrough(
            self.live_request(&live, "resources/read", json!({"uri": uri}), cancellation)
                .await,
        ) {
            Ok(response) => response,
            Err(outcome) => return outcome,
        };
        if !response.get("contents").is_some_and(Value::is_array) {
            return McpRequestOutcome::Completed(Err(McpError::Protocol(
                "resources/read result missing contents array".into(),
            )));
        }
        McpRequestOutcome::Completed(Ok(response))
    }

    /// Connects `server` if needed (joining a connect in flight), refuses
    /// hidden servers and servers without the resources capability, and
    /// drops the cached listings the server announced as changed.
    async fn live_connection(
        &self,
        server: &str,
        cancellation: &McpCancellation,
    ) -> McpRequestOutcome<Live> {
        let entry = match self.entry(server) {
            Ok(entry) => entry,
            Err(error) => return McpRequestOutcome::Completed(Err(error)),
        };
        if entry.spec.options.exposure == McpExposure::Hidden {
            return McpRequestOutcome::Completed(Err(McpError::Blocked(format!(
                "MCP server {server} is hidden (exposure = \"hidden\")"
            ))));
        }
        let outcome = self
            .ensure_connected_cancellable(&entry, cancellation.clone())
            .await
            .map(|(connection, generation)| Live {
                entry: Arc::clone(&entry),
                connection,
                generation,
            });
        let McpRequestOutcome::Completed(Ok(live)) = &outcome else {
            return outcome;
        };
        if !offers_resources(&entry) {
            return McpRequestOutcome::Completed(Err(McpError::Protocol(format!(
                "MCP server {server} does not offer resources"
            ))));
        }
        if live.connection.take_resources_stale() {
            self.resource_cache.invalidate(server);
        }
        outcome
    }

    /// One request on a live connection, with the transport clean-up every
    /// manager request performs when the connection is lost or the request
    /// was cancelled after it may have been sent.
    async fn live_request(
        &self,
        live: &Live,
        method: &str,
        params: Value,
        cancellation: McpCancellation,
    ) -> McpRequestOutcome<Value> {
        if cancellation.is_cancelled() {
            return interrupted(McpInterruption::Cancelled);
        }
        let outcome = live
            .connection
            .request_cancellable(method, params, cancellation)
            .await;
        match outcome {
            McpRequestOutcome::OutcomeUncertain {
                interruption,
                cleanup,
            } => {
                let cleanup = merge_cleanup(cleanup, live.connection.close_for_cleanup().await);
                self.disconnect_if_generation(&live.entry, live.generation, &live.connection)
                    .await;
                McpRequestOutcome::OutcomeUncertain {
                    interruption,
                    cleanup,
                }
            }
            McpRequestOutcome::Completed(Err(error)) if live.connection.is_closed() => {
                let _ = live.connection.close_for_cleanup().await;
                self.disconnect_if_generation(&live.entry, live.generation, &live.connection)
                    .await;
                McpRequestOutcome::Completed(Err(error))
            }
            McpRequestOutcome::InterruptedBeforeSend {
                interruption,
                cleanup,
            } if live.connection.is_closed() => {
                let cleanup = merge_cleanup(cleanup, live.connection.close_for_cleanup().await);
                self.disconnect_if_generation(&live.entry, live.generation, &live.connection)
                    .await;
                McpRequestOutcome::InterruptedBeforeSend {
                    interruption,
                    cleanup,
                }
            }
            // The server needs a sign-in: show it and let the next use start a
            // fresh connection (see `call_with_progress`).
            McpRequestOutcome::Completed(Err(McpError::AuthRequired(reason))) => {
                self.disconnect_if_generation(&live.entry, live.generation, &live.connection)
                    .await;
                Self::set_status(
                    &live.entry,
                    McpServerStatus::NeedsAuth {
                        reason: reason.clone(),
                    },
                );
                self.bump();
                McpRequestOutcome::Completed(Err(McpError::AuthRequired(reason)))
            }
            outcome => outcome,
        }
    }

    async fn listing_page(
        &self,
        server: &str,
        kind: ListingKind,
        cursor: Option<&str>,
        cancellation: McpCancellation,
    ) -> McpRequestOutcome<McpResourcePage> {
        if cursor.is_some_and(|cursor| cursor.is_empty() || cursor.len() > MAX_CURSOR_BYTES) {
            return McpRequestOutcome::Completed(Err(McpError::Protocol(format!(
                "cursor must contain 1-{MAX_CURSOR_BYTES} bytes"
            ))));
        }
        let live = match passthrough(self.live_connection(server, &cancellation).await) {
            Ok(live) => live,
            Err(outcome) => return outcome,
        };
        let params = match cursor {
            Some(cursor) => json!({"cursor": cursor}),
            None => json!({}),
        };
        let response = match passthrough(
            self.live_request(&live, kind.method(), params, cancellation)
                .await,
        ) {
            Ok(response) => response,
            Err(McpRequestOutcome::Completed(Err(McpError::Server {
                code: METHOD_NOT_FOUND,
                ..
            }))) if kind == ListingKind::Templates => {
                return McpRequestOutcome::Completed(Ok(McpResourcePage::default()));
            }
            Err(outcome) => return outcome,
        };
        McpRequestOutcome::Completed(parse_page(kind, &response))
    }

    async fn listing_all(
        &self,
        server: &str,
        kind: ListingKind,
        cancellation: McpCancellation,
    ) -> McpRequestOutcome<Arc<McpResourceListing>> {
        let live = match passthrough(self.live_connection(server, &cancellation).await) {
            Ok(live) => live,
            Err(outcome) => return outcome,
        };
        if let Some(cached) = self.resource_cache.get(server, &live.connection, kind) {
            return McpRequestOutcome::Completed(Ok(cached));
        }
        let mut listing = McpResourceListing::default();
        let mut cursor: Option<String> = None;
        let mut seen_cursors: HashSet<String> = HashSet::new();
        let mut exhausted = false;
        for page_index in 0..MAX_RESOURCE_PAGES {
            let params = match &cursor {
                Some(cursor) => json!({"cursor": cursor}),
                None => json!({}),
            };
            let response = match passthrough(
                self.live_request(&live, kind.method(), params, cancellation.clone())
                    .await,
            ) {
                Ok(response) => response,
                Err(McpRequestOutcome::Completed(Err(McpError::Server {
                    code: METHOD_NOT_FOUND,
                    ..
                }))) if kind == ListingKind::Templates && page_index == 0 => {
                    exhausted = true;
                    break;
                }
                Err(outcome) => return outcome,
            };
            let page = match parse_page(kind, &response) {
                Ok(page) => page,
                Err(error) => return McpRequestOutcome::Completed(Err(error)),
            };
            if page.omitted > 0 {
                listing.truncated = true;
            }
            let room = MAX_RESOURCES_PER_SERVER.saturating_sub(listing.items.len());
            if page.items.len() > room {
                listing.items.extend(page.items.into_iter().take(room));
                listing.truncated = true;
                exhausted = true;
                break;
            }
            listing.items.extend(page.items);
            if listing.items.len() >= MAX_RESOURCES_PER_SERVER {
                listing.truncated |= page.next_cursor.is_some();
                exhausted = true;
                break;
            }
            match page.next_cursor {
                None => {
                    exhausted = true;
                    break;
                }
                // A cursor seen before would loop forever.
                Some(next) if !seen_cursors.insert(next.clone()) => {
                    listing.truncated = true;
                    exhausted = true;
                    break;
                }
                Some(next) => cursor = Some(next),
            }
        }
        if !exhausted {
            listing.truncated = true;
        }
        let listing = Arc::new(listing);
        self.resource_cache
            .put(server, &live.connection, kind, Arc::clone(&listing));
        McpRequestOutcome::Completed(Ok(listing))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_app_resources_are_recognized_by_scheme_and_profile() {
        assert!(is_mcp_app_resource("ui://widget/main", None));
        assert!(is_mcp_app_resource("UI://widget", Some("text/html")));
        assert!(is_mcp_app_resource(
            "file:///a.html",
            Some("text/html;profile=mcp-app")
        ));
        assert!(is_mcp_app_resource(
            "file:///a.html",
            Some("text/html; Profile = \"MCP-App\"")
        ));
        assert!(!is_mcp_app_resource("file:///a.txt", Some("text/plain")));
        assert!(!is_mcp_app_resource(
            "file:///a.html",
            Some("text/html;profile=other")
        ));
        assert!(!is_mcp_app_resource("https://ui://x", None));
    }

    #[test]
    fn pages_keep_bounded_single_line_text_and_drop_app_entries() {
        let page = parse_page(
            ListingKind::Resources,
            &json!({
                "resources": [
                    {"uri": "file:///a", "name": "a\nline\u{1b}[31m", "title": " T ",
                     "description": "d".repeat(2000), "mimeType": "text/plain",
                     "size": 12, "_meta": {"x": 1}, "icons": [{"src": "x"}]},
                    {"uri": "ui://app", "name": "app"},
                    {"uri": "file:///b", "name": "b", "mimeType": "text/html;profile=mcp-app"},
                    {"name": "no uri"},
                    {"uri": "", "name": "empty"},
                    {"uri": "file:///c"},
                ],
                "nextCursor": "page-2"
            }),
        )
        .unwrap();
        assert_eq!(page.next_cursor.as_deref(), Some("page-2"));
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].name, "a line [31m");
        assert_eq!(page.items[0].title.as_deref(), Some("T"));
        assert_eq!(
            page.items[0].description.as_ref().unwrap().chars().count(),
            513
        );
        assert_eq!(page.items[0].size, Some(12));
        // A missing name falls back to the URI.
        assert_eq!(page.items[1].name, "file:///c");
        let listed = page.items[0].to_json("fs", false);
        assert_eq!(listed["server"], "fs");
        assert_eq!(listed["uri"], "file:///a");
        assert!(listed.get("_meta").is_none() && listed.get("icons").is_none());
        let template = page.items[1].to_json("fs", true);
        assert_eq!(template["uriTemplate"], "file:///c");
        assert!(template.get("uri").is_none());
    }

    #[test]
    fn pages_cap_entries_and_reject_a_missing_array() {
        let many: Vec<Value> = (0..MAX_ITEMS_PER_PAGE + 7)
            .map(|index| json!({"uri": format!("file:///{index}"), "name": "n"}))
            .collect();
        let page = parse_page(ListingKind::Resources, &json!({"resources": many})).unwrap();
        assert_eq!(page.items.len(), MAX_ITEMS_PER_PAGE);
        assert_eq!(page.omitted, 7);
        let error = parse_page(ListingKind::Templates, &json!({"resources": []}))
            .expect_err("wrong array key");
        assert!(error.to_string().contains("resourceTemplates"), "{error}");
        let templates = parse_page(
            ListingKind::Templates,
            &json!({"resourceTemplates": [{"uriTemplate": "file:///{p}", "name": "t"}]}),
        )
        .unwrap();
        assert_eq!(templates.items[0].uri, "file:///{p}");
    }
}
