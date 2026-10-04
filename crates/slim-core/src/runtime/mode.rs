use crate::provider::{ProviderMessage, ProviderToolCall};

pub fn mode_name(mode: crate::OperatingMode) -> &'static str {
    match mode {
        crate::OperatingMode::Auto => "Auto",
        crate::OperatingMode::ReadOnly => "Read-only",
        crate::OperatingMode::Plan => "Plan",
    }
}

/// Suffix on the run's anchor user message ([`ChannelFrame`]). Durable JSONL stores the prompt only;
/// [`super::without_workspace_snapshot`] strips this with the workspace listing.
pub(super) const CHANNEL_MARKER: &str = "\n\nHarness channel:";

const PROJECTION_CACHE_BYTES: usize = 1024 * 1024;
const PROJECTION_CACHE_ENTRIES: usize = 16;
/// Smaller `write` arguments are not worth eliding.
const MIN_ELIDED_ARGUMENT_BYTES: usize = 4096;

struct CachedWrite {
    id: String,
    arguments: String,
    receipt: String,
    projected: String,
}

impl CachedWrite {
    fn bytes(&self) -> usize {
        self.id.len() + self.arguments.len() + self.receipt.len() + self.projected.len()
    }
}

#[derive(Default)]
pub(super) struct WriteProjectionCache {
    entries: Vec<CachedWrite>,
    bytes: usize,
}

impl WriteProjectionCache {
    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    fn insert(&mut self, id: &str, arguments: &str, receipt: String, projected: &str) {
        let bytes = id.len() + arguments.len() + receipt.len() + projected.len();
        // Keep entries reused by the next forward scan; FIFO would thrash when
        // the history exceeds the cap. Check capacity before copying arguments.
        if self.entries.len() >= PROJECTION_CACHE_ENTRIES
            || self.bytes + bytes > PROJECTION_CACHE_BYTES
        {
            return;
        }
        self.bytes += bytes;
        self.entries.push(CachedWrite {
            id: id.into(),
            arguments: arguments.into(),
            receipt,
            projected: projected.into(),
        });
    }
}

/// Where a run puts the channel overlay, fixed when the run starts (and again
/// when a compaction rewrites the history). The provider caches the request
/// prefix: a stanza that followed the newest user message would move onto
/// every message the loop injects (reviews, steers, notes, shell completions)
/// and change the message it left.
#[derive(Clone, Debug, Default)]
pub(super) struct ChannelFrame {
    /// The user message that carries the stanza: the one that started the run.
    anchor: Option<usize>,
    /// The MCP awareness block as it was when the frame was made; its status
    /// labels change while servers connect.
    mcp_servers: Option<String>,
}

impl ChannelFrame {
    pub(super) fn new(messages: &[ProviderMessage], mcp_servers: Option<String>) -> Self {
        Self {
            anchor: messages.iter().rposition(|message| message.role == "user"),
            mcp_servers,
        }
    }

    /// The frame for `messages` that replace the history this one described.
    /// The new prefix is cached from scratch anyway, so the MCP labels are
    /// read again (`mcp_servers`) instead of staying as they were at the start.
    pub(super) fn rebuilt_for(messages: &[ProviderMessage], mcp_servers: Option<String>) -> Self {
        Self::new(messages, mcp_servers)
    }
}

/// Facts that match advertised tools: `ask_question` only when Auto has a route.
pub(super) struct ChannelOverlay<'a> {
    messages: &'a mut [ProviderMessage],
    /// The user message carrying the stanza and its length without it.
    applied: Option<(usize, usize)>,
    original_arguments: Vec<(usize, usize, String)>,
}

impl<'a> ChannelOverlay<'a> {
    #[cfg(test)]
    pub(super) fn apply(
        messages: &'a mut [ProviderMessage],
        mode: crate::OperatingMode,
        can_ask: bool,
        cache: Option<&mut WriteProjectionCache>,
    ) -> Self {
        Self::apply_with_mcp(messages, mode, can_ask, cache, &ChannelFrame::default())
    }

    /// [`Self::apply`] at the frame's message, with its MCP server awareness
    /// block (Auto only), which follows the stanza and is stripped with it.
    /// Without a usable anchor the newest user message carries them.
    pub(super) fn apply_with_mcp(
        messages: &'a mut [ProviderMessage],
        mode: crate::OperatingMode,
        can_ask: bool,
        cache: Option<&mut WriteProjectionCache>,
        frame: &ChannelFrame,
    ) -> Self {
        let mcp_servers = frame.mcp_servers.as_deref();
        let original_arguments = project_completed_writes(messages, cache);
        let applied = frame
            .anchor
            .filter(|index| messages.get(*index).is_some_and(|m| m.role == "user"))
            .or_else(|| messages.iter().rposition(|message| message.role == "user"))
            .map(|index| {
                let content = &mut messages[index].content;
                if let Some(at) = content.find(CHANNEL_MARKER) {
                    content.truncate(at);
                }
                let original_len = content.len();
                content.push_str(channel_stanza(mode, can_ask));
                if let Some(block) = mcp_servers.filter(|_| mode.allows_mutation()) {
                    content.push_str("\n\n");
                    content.push_str(block);
                }
                (index, original_len)
            });
        Self {
            messages,
            applied,
            original_arguments,
        }
    }

    pub(super) fn view(&self) -> &[ProviderMessage] {
        self.messages
    }
}

impl Drop for ChannelOverlay<'_> {
    fn drop(&mut self) {
        for (message, call, original) in self.original_arguments.drain(..) {
            self.messages[message].tool_calls[call].arguments = original;
        }
        if let Some((index, original_len)) = self.applied {
            self.messages[index].content.truncate(original_len);
        }
    }
}

// Change only the provider view, never the transcript, journal or execution input.
// Require the native success receipt to match the payload; errors and incomplete
// calls must retain their content for recovery. Small writes are not worth eliding.
fn project_completed_writes(
    messages: &mut [ProviderMessage],
    mut cache: Option<&mut WriteProjectionCache>,
) -> Vec<(usize, usize, String)> {
    if let Some(cache) = &mut cache {
        reconcile_cache(cache, messages);
    }
    let mut replacements = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        if message.role != "assistant" {
            continue;
        }
        for (call_index, call) in message.tool_calls.iter().enumerate() {
            if let Some(projected) = project_call(messages, index, call, cache.as_deref_mut()) {
                replacements.push((index, call_index, projected));
            }
        }
    }
    for (message, call, arguments) in &mut replacements {
        std::mem::swap(
            &mut messages[*message].tool_calls[*call].arguments,
            arguments,
        );
    }
    replacements
}

/// Compaction or revised history must free capacity for current writes.
fn reconcile_cache(cache: &mut WriteProjectionCache, messages: &[ProviderMessage]) {
    cache.entries.retain(|entry| {
        messages.iter().any(|message| {
            message
                .tool_calls
                .iter()
                .any(|call| call.id == entry.id && call.arguments == entry.arguments)
        })
    });
    cache.bytes = cache.entries.iter().map(CachedWrite::bytes).sum();
}

/// The elided arguments for the `write` call `call` (in `messages[index]`), or
/// `None` when it must stay verbatim. A fresh projection is remembered in
/// `cache`.
fn project_call(
    messages: &[ProviderMessage],
    index: usize,
    call: &ProviderToolCall,
    cache: Option<&mut WriteProjectionCache>,
) -> Option<String> {
    use sha2::{Digest, Sha256};
    if call.name != "write" || call.arguments.len() <= MIN_ELIDED_ARGUMENT_BYTES {
        return None;
    }
    let succeeded = |receipt: &str| {
        messages[index + 1..].iter().any(|result| {
            result.role == "tool"
                && result.name.as_deref() == Some("write")
                && result.tool_call_id.as_deref() == Some(call.id.as_str())
                && result.content.starts_with("written ")
                && result.content.contains(receipt)
        })
    };
    if let Some(entry) = cache.as_deref().and_then(|cache| {
        cache
            .entries
            .iter()
            .find(|entry| entry.id == call.id && entry.arguments == call.arguments)
    }) {
        // A cached projection is not a cached success: check the receipt again.
        return succeeded(&entry.receipt).then(|| entry.projected.clone());
    }
    let mut arguments = serde_json::from_str::<serde_json::Value>(&call.arguments).ok()?;
    let content = arguments.get("content")?.as_str()?;
    let hash = format!("{:x}", Sha256::digest(content.as_bytes()));
    let receipt = format!(
        "; bytes={}; sha256={}; exists=true;",
        content.len(),
        &hash[..12]
    );
    if !succeeded(&receipt) {
        return None;
    }
    arguments["content"] = serde_json::Value::String(format!(
        "[successful write content elided; bytes={}; sha256={hash}; read the file if needed]",
        content.len()
    ));
    let projected = arguments.to_string();
    if projected.len() >= call.arguments.len() {
        return None;
    }
    if let Some(cache) = cache {
        cache.insert(&call.id, &call.arguments, receipt, &projected);
    }
    Some(projected)
}

/// Bytes of a call's arguments as the provider view sends them: a completed
/// large `write` goes out as the short projection, everything else whole.
pub(super) fn wire_argument_bytes(
    messages: &[ProviderMessage],
    index: usize,
    call: &ProviderToolCall,
) -> usize {
    project_call(messages, index, call, None)
        .map_or(call.arguments.len(), |projected| projected.len())
}

pub(super) fn channel_stanza(mode: crate::OperatingMode, can_ask: bool) -> &'static str {
    match (mode, can_ask) {
        (crate::OperatingMode::Auto, true) => {
            "\n\nHarness channel: Auto, interactive. Act on the request without asking. Use ask_question only before irreversible loss of data the request does not cover, production or external writes it does not cover, exposing secrets, or a material decision the request and code cannot settle. Shell runs PowerShell, not bash."
        }
        (crate::OperatingMode::Auto, false) => {
            "\n\nHarness channel: Auto, unattended. No interactive pause (ask_question is not available). Take the most reasonable reading and finish the work. Skip only irreversible loss of data the request does not cover, production or external writes it does not cover, and exposing secrets; report what you skipped and why. Shell runs PowerShell, not bash."
        }
        (crate::OperatingMode::Plan, _) => {
            "\n\nHarness channel: Plan. Inspect and report only. Workspace mutations, shell, todo, skill and ask_question are not available."
        }
        (crate::OperatingMode::ReadOnly, true) => {
            "\n\nHarness channel: Read-only, interactive. Inspect without mutating the workspace. Use ask_question for undiscoverable architecture/safety/data/behavior decisions. write, patch, shell, todo and skill are not available."
        }
        (crate::OperatingMode::ReadOnly, false) => {
            "\n\nHarness channel: Read-only. Inspect without mutating the workspace. write, patch, shell, todo, skill and ask_question are not available."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{channel_stanza, CHANNEL_MARKER};
    use crate::OperatingMode;

    fn cached_write_fixture(count: usize) -> Vec<crate::provider::ProviderMessage> {
        use crate::provider::{ProviderMessage, ProviderToolCall};
        use sha2::{Digest, Sha256};
        let mut messages = vec![ProviderMessage::user("implement")];
        for index in 0..count {
            let content = format!("source {index} ação 日本語\n").repeat(1400);
            let hash = format!("{:x}", Sha256::digest(content.as_bytes()));
            let id = format!("write-{index}");
            messages.push(ProviderMessage::assistant(
                "",
                vec![ProviderToolCall {
                    id: id.clone(),
                    name: "write".into(),
                    arguments: serde_json::json!({"path":format!("{index}.rs"),"content":content})
                        .to_string(),
                }],
            ));
            messages.push(ProviderMessage::tool(
                "write",
                id,
                format!(
                    "written {index}.rs; bytes={}; sha256={}; exists=true; do not re-read",
                    content.len(),
                    &hash[..12]
                ),
            ));
        }
        messages
    }

    #[test]
    fn write_projection_cache_rechecks_receipts_arguments_and_bounds() {
        let mut messages = cached_write_fixture(1);
        let original = messages.clone();
        let mut cache = super::WriteProjectionCache::default();
        for _ in 0..2 {
            let view = super::ChannelOverlay::apply(
                &mut messages,
                OperatingMode::Auto,
                false,
                Some(&mut cache),
            );
            assert!(view.view()[1].tool_calls[0]
                .arguments
                .contains("content elided"));
            drop(view);
            assert_eq!(messages, original);
            assert_eq!(cache.entries.len(), 1);
        }
        for failure in [
            "error: failed",
            "written 0.rs; bytes=1; sha256=bad; exists=true;",
        ] {
            messages[2].content = failure.into();
            let view = super::ChannelOverlay::apply(
                &mut messages,
                OperatingMode::Auto,
                false,
                Some(&mut cache),
            );
            assert_eq!(
                view.view()[1].tool_calls[0].arguments,
                original[1].tool_calls[0].arguments
            );
        }
        messages = original;
        messages[1].tool_calls[0].arguments = messages[1].tool_calls[0]
            .arguments
            .replace("source 0", "source 1");
        let view = super::ChannelOverlay::apply(
            &mut messages,
            OperatingMode::Auto,
            false,
            Some(&mut cache),
        );
        assert!(!view.view()[1].tool_calls[0]
            .arguments
            .contains("content elided"));
        drop(view);
        messages = cached_write_fixture(40);
        drop(super::ChannelOverlay::apply(
            &mut messages,
            OperatingMode::Auto,
            false,
            Some(&mut cache),
        ));
        assert!(cache.entries.len() <= super::PROJECTION_CACHE_ENTRIES);
        assert!(cache.bytes <= super::PROJECTION_CACHE_BYTES);
        let bytes = cache.bytes;
        cache.insert(
            "oversized",
            &"x".repeat(super::PROJECTION_CACHE_BYTES),
            "ok".into(),
            "short",
        );
        assert_eq!(cache.bytes, bytes);
        let retained: Vec<_> = cache.entries.iter().map(|entry| entry.id.clone()).collect();
        drop(super::ChannelOverlay::apply(
            &mut messages,
            OperatingMode::Auto,
            false,
            Some(&mut cache),
        ));
        assert_eq!(
            cache
                .entries
                .iter()
                .map(|entry| &entry.id)
                .collect::<Vec<_>>(),
            retained.iter().collect::<Vec<_>>()
        );
        // A compacted history releases old entries and admits the remaining ones.
        messages.drain(1..41);
        drop(super::ChannelOverlay::apply(
            &mut messages,
            OperatingMode::Auto,
            false,
            Some(&mut cache),
        ));
        assert!(cache
            .entries
            .iter()
            .all(|entry| !retained.contains(&entry.id)));
        assert!(!cache.entries.is_empty());
        cache.clear();
        assert!(cache.entries.is_empty());
        assert_eq!(cache.bytes, 0);
    }

    #[test]
    #[ignore = "manual paired preparation benchmark; no network"]
    fn write_projection_cache_measurement() {
        use crate::provider::{OpenAiCompatibleAdapter, ProviderAdapter, ProviderConfig};
        let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            "http://127.0.0.1:1",
            "fixture",
            "unused",
        ))
        .unwrap();
        for count in [0, 1, 8, 40] {
            let mut history = cached_write_fixture(count);
            let original = history.clone();
            let argument_bytes: usize = history
                .iter()
                .flat_map(|message| &message.tool_calls)
                .map(|call| call.arguments.len())
                .sum();
            let cache = std::sync::Mutex::new(super::WriteProjectionCache::default());
            let mut samples: [Vec<f64>; 3] = std::array::from_fn(|_| Vec::new());
            let mut expected = None;
            for sample in 0..33 {
                // Alternate baseline ordering to reduce warm-up/order bias.
                let order = if sample % 2 == 0 {
                    [0, 1, 2]
                } else {
                    [1, 2, 0]
                };
                for variant in order {
                    if variant == 1 {
                        cache.lock().unwrap().clear();
                    }
                    let started = std::time::Instant::now();
                    let view = if variant == 0 {
                        super::ChannelOverlay::apply(&mut history, OperatingMode::Auto, false, None)
                    } else {
                        super::ChannelOverlay::apply(
                            &mut history,
                            OperatingMode::Auto,
                            false,
                            Some(&mut cache.lock().unwrap()),
                        )
                    };
                    let request = adapter
                        .build_messages_request_with_tools_checked(view.view(), &[])
                        .unwrap();
                    drop(view);
                    let micros = started.elapsed().as_secs_f64() * 1_000_000.0;
                    if sample > 0 {
                        samples[variant].push(micros);
                    }
                    if let Some(body) = &expected {
                        assert_eq!(&request.body, body);
                    } else {
                        expected = Some(request.body.clone());
                    }
                    std::hint::black_box(request);
                    assert_eq!(history, original);
                }
            }
            for (variant, timings) in samples.iter_mut().enumerate() {
                timings.sort_by(f64::total_cmp);
                eprintln!("projection writes={count} variant={variant} n={} median_us={:.3} min_us={:.3} max_us={:.3} arguments_bytes={argument_bytes} cache_bytes={}",
                timings.len(), timings[timings.len()/2], timings[0], timings.last().unwrap(), cache.lock().unwrap().bytes);
            }
        }
    }

    #[test]
    fn completed_write_projection_preserves_canonical_history_and_recovery() {
        use crate::provider::{ProviderMessage, ProviderToolCall};
        use sha2::{Digest, Sha256};
        let content = "a日本語\n".repeat(3000);
        let hash = format!("{:x}", Sha256::digest(content.as_bytes()));
        let arguments = serde_json::json!({"path":"file.rs", "content":content}).to_string();
        let call = ProviderToolCall {
            id: "write-1".into(),
            name: "write".into(),
            arguments: arguments.clone(),
        };
        let mut messages = vec![
            ProviderMessage::user("implement"),
            ProviderMessage::assistant("", vec![call]),
            ProviderMessage::tool(
                "write",
                "write-1",
                format!(
                    "written file.rs; bytes={}; sha256={}; exists=true; do not re-read",
                    content.len(),
                    &hash[..12]
                ),
            ),
        ];
        let canonical = messages.clone();
        for _ in 0..2 {
            let overlay =
                super::ChannelOverlay::apply(&mut messages, OperatingMode::Auto, false, None);
            let projected = &overlay.view()[1].tool_calls[0].arguments;
            assert!(projected.len() < 400);
            assert!(projected.contains(&hash));
            assert!(!projected.contains(&content));
            drop(overlay);
            assert_eq!(messages, canonical);
        }
        for receipt in [
            "error: write failed",
            "written file.rs; bytes=1; sha256=invalid; exists=true;",
        ] {
            messages[2].content = receipt.into();
            let overlay =
                super::ChannelOverlay::apply(&mut messages, OperatingMode::Auto, false, None);
            assert_eq!(overlay.view()[1].tool_calls[0].arguments, arguments);
        }
        messages.pop();
        let overlay = super::ChannelOverlay::apply(&mut messages, OperatingMode::Auto, false, None);
        assert_eq!(overlay.view()[1].tool_calls[0].arguments, arguments);
    }

    #[test]
    fn requests_of_one_run_serialize_the_earlier_messages_identically() {
        use crate::provider::{
            OpenAiCompatibleAdapter, ProviderAdapter, ProviderConfig, ProviderMessage,
            ProviderToolCall,
        };
        let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            "http://127.0.0.1:1",
            "fixture",
            "unused",
        ))
        .unwrap();
        let call = ProviderToolCall {
            id: "call-1".into(),
            name: "read".into(),
            arguments: r#"{"path":"a.rs"}"#.into(),
        };
        let mut messages = vec![
            ProviderMessage::user("implement"),
            ProviderMessage::assistant("", vec![call]),
            ProviderMessage::tool("read", "call-1", "fn main() {}"),
        ];
        let frame = super::ChannelFrame::new(&messages, Some("MCP servers (1)\n- fs".into()));
        let serialized = |messages: &mut [ProviderMessage], frame: &super::ChannelFrame| {
            let overlay = super::ChannelOverlay::apply_with_mcp(
                messages,
                OperatingMode::Auto,
                false,
                None,
                frame,
            );
            let request = adapter
                .build_messages_request_with_tools_checked(overlay.view(), &[])
                .unwrap();
            let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
            body["messages"].as_array().unwrap().clone()
        };
        let first = serialized(&mut messages, &frame);
        assert!(first
            .iter()
            .any(|message| message.to_string().contains("Harness channel")));
        // The loop injects user messages: a review, a steer, a note.
        messages.push(ProviderMessage::assistant("step", Vec::new()));
        messages.push(ProviderMessage::user("Review the todo list."));
        messages.push(ProviderMessage::user("Diagnostics note."));
        let second = serialized(&mut messages, &frame);
        assert_eq!(second[..first.len()], first[..]);
        let stanzas = second
            .iter()
            .filter(|message| message.to_string().contains("Harness channel"))
            .count();
        assert_eq!(stanzas, 1);
        // Without a frame the newest user message carries it, which is what
        // changed the earlier message on every injection.
        let moved = serialized(&mut messages, &super::ChannelFrame::default());
        assert_ne!(moved[..first.len()], first[..]);
        // A compaction rewrites the history: the frame is rebuilt for it.
        let rebuilt = super::ChannelFrame::rebuilt_for(
            &messages[2..],
            Some("MCP servers (1)\n- fs (ready)".into()),
        );
        assert_eq!(
            rebuilt.mcp_servers.as_deref(),
            Some("MCP servers (1)\n- fs (ready)")
        );
        assert_ne!(rebuilt.mcp_servers, frame.mcp_servers);
        assert_eq!(rebuilt.anchor, Some(3));
    }

    #[test]
    fn channel_stanza_matches_advertised_ask_question() {
        let interactive = channel_stanza(OperatingMode::Auto, true);
        let unattended = channel_stanza(OperatingMode::Auto, false);
        assert!(interactive.starts_with(CHANNEL_MARKER));
        assert!(unattended.starts_with(CHANNEL_MARKER));
        assert!(interactive.contains("Auto, interactive"));
        assert!(interactive.contains("Use ask_question"));
        assert!(!interactive.contains("unattended"));
        assert!(unattended.contains("Auto, unattended"));
        assert!(unattended.contains("ask_question is not available"));
        assert!(!unattended.contains("Auto, interactive"));
        assert!(interactive.contains("Shell runs PowerShell"));
        assert!(unattended.contains("Shell runs PowerShell"));
        assert!(!channel_stanza(OperatingMode::Plan, true).contains("PowerShell"));
        assert!(!channel_stanza(OperatingMode::ReadOnly, true).contains("PowerShell"));
        assert!(channel_stanza(OperatingMode::Plan, true).contains("Plan."));
        assert!(!channel_stanza(OperatingMode::Plan, true).contains("Use ask_question"));
        let readonly = channel_stanza(OperatingMode::ReadOnly, true);
        assert!(readonly.contains("Read-only, interactive"));
        assert!(readonly.contains("Use ask_question"));
        assert!(!channel_stanza(OperatingMode::ReadOnly, false).contains("Use ask_question"));
    }
}
