//! What the model sees of an MCP result: `tools/call` results and
//! `resources/read` contents become text, images and artifact pointers.
//!
//! * text, embedded text resources and text-like blobs (`text/*`,
//!   `application/json`, `+json`, `+xml`) become text;
//! * images that every wire accepts become image content blocks on the tool
//!   message ([`ToolResult::media`]); others, and any other binary blob, go
//!   to the artifact store and the text names the artifact;
//! * audio becomes `[audio <mime> omitted]`, resource links one line that
//!   says how to read them;
//! * `structuredContent` is shown only when the result has no content
//!   blocks (codemode keeps the full result); `isError` fails the call;
//! * text over the inline budget is stored whole as an artifact and the
//!   model sees its head and tail with a marker ([`limit_text`]).
//!
//! Server text is untrusted: it is cut at character boundaries and the
//! one-line forms are stripped of control characters.

use super::*;
use crate::context::ArtifactHandle;
use crate::mcp::clean_text;
use crate::provider::{image_limits, ProviderContentBlock};

/// Text the model sees inline per result. The runtime's own per-result
/// allowance (16 KiB by default) applies on top, plus the artifact pointer.
pub(in crate::runtime) const MCP_INLINE_TEXT_BYTES: usize = 14 * 1024;
/// Largest text stored as an artifact; the rest is cut (marked).
const MCP_FULL_TEXT_BYTES: usize = 8 * 1024 * 1024;
/// Images sent inline per result, and their base64 size. Small on purpose:
/// the context accounting counts every base64 character of a request as
/// text, so one large image could push a request past the context window,
/// and a result stays in the history until compaction removes it.
const MAX_INLINE_IMAGES: usize = 2;
const MAX_INLINE_IMAGE_BASE64: usize = 192 * 1024;
const MAX_INLINE_IMAGE_BASE64_TOTAL: usize = 320 * 1024;
/// Binary parts stored as artifacts per result.
const MAX_STORED_BLOBS: usize = 8;
const MAX_URI_CHARS: usize = 1024;
const MAX_LABEL_CHARS: usize = 256;
const MAX_DESCRIPTION_CHARS: usize = 512;
const MAX_MIME_CHARS: usize = 128;

/// Where binary parts go; without a store they are only described.
#[derive(Default)]
pub(in crate::runtime) struct RenderEnv<'a> {
    pub(in crate::runtime) store: Option<&'a ArtifactStore>,
    /// Registered secrets, removed from any text stored here.
    pub(in crate::runtime) secrets: &'a [String],
}

/// One rendered MCP result.
pub(in crate::runtime) struct Rendered {
    pub(in crate::runtime) text: String,
    pub(in crate::runtime) success: bool,
    pub(in crate::runtime) media: Vec<ProviderContentBlock>,
    /// Artifacts created for binary parts.
    pub(in crate::runtime) blobs: Vec<ArtifactHandle>,
}

impl Rendered {
    /// A successful, text-only result.
    pub(in crate::runtime) fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            success: true,
            media: Vec::new(),
            blobs: Vec::new(),
        }
    }
}

struct Collector<'a> {
    parts: Vec<String>,
    media: Vec<ProviderContentBlock>,
    blobs: Vec<ArtifactHandle>,
    inline_base64: usize,
    env: &'a RenderEnv<'a>,
}

fn format_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if value < KIB * KIB {
        format!("{:.1} KB", value / KIB)
    } else {
        format!("{:.1} MB", value / (KIB * KIB))
    }
}

/// Media type for display: its text, or a stand-in.
fn mime_label(mime: Option<&str>, fallback: &str) -> String {
    mime.map(|mime| clean_text(mime, MAX_MIME_CHARS))
        .filter(|mime| !mime.is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

/// `text/*`, JSON and XML-based types: blobs of these are shown as text.
fn is_text_mime(mime: Option<&str>) -> bool {
    let Some(mime) = mime else {
        return false;
    };
    let kind = mime
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    kind.starts_with("text/")
        || kind == "application/json"
        || kind.ends_with("+json")
        || kind.ends_with("+xml")
}

/// The line for a `resource_link` block, naming the way to read it.
fn resource_link_line(server: &str, block: &Value) -> String {
    let uri = clean_text(
        block.get("uri").and_then(Value::as_str).unwrap_or_default(),
        MAX_URI_CHARS,
    );
    let label = ["title", "name"]
        .iter()
        .filter_map(|key| block.get(*key).and_then(Value::as_str))
        .map(|text| clean_text(text, MAX_LABEL_CHARS))
        .find(|text| !text.is_empty())
        .unwrap_or_default();
    let mut details = Vec::new();
    if let Some(mime) = block.get("mimeType").and_then(Value::as_str) {
        let mime = clean_text(mime, MAX_MIME_CHARS);
        if !mime.is_empty() {
            details.push(mime);
        }
    }
    if let Some(size) = block.get("size").and_then(Value::as_u64) {
        details.push(format_size(size));
    }
    let details = if details.is_empty() {
        String::new()
    } else {
        format!(" ({})", details.join(", "))
    };
    let description = block
        .get("description")
        .and_then(Value::as_str)
        .map(|text| clean_text(text, MAX_DESCRIPTION_CHARS))
        .filter(|text| !text.is_empty())
        .map(|text| format!(": {text}"))
        .unwrap_or_default();
    let read = if server.is_empty() {
        String::new()
    } else {
        format!(
            ". Read it with mcp {{server:{}, uri:{}}}",
            Value::String(server.to_owned()),
            Value::String(uri.clone())
        )
    };
    format!("[Resource {uri} \"{label}\"{details}{description}{read}]")
}

impl<'a> Collector<'a> {
    fn new(env: &'a RenderEnv<'a>) -> Self {
        Self {
            parts: Vec::new(),
            media: Vec::new(),
            blobs: Vec::new(),
            inline_base64: 0,
            env,
        }
    }

    /// Stores a binary part as an artifact; the pointer names id and path.
    fn spill(&mut self, label: &str, bytes: &[u8]) -> Result<String, String> {
        if self.blobs.len() >= MAX_STORED_BLOBS {
            return Err("too many binary parts in one result".into());
        }
        let Some(store) = self.env.store else {
            return Err("no artifact storage is available".into());
        };
        let handle = store
            .put(label, bytes)
            .map_err(|error| format!("the artifact store failed ({:?})", error.kind()))?;
        let pointer = format!("artifact id={} path={}", handle.id, handle.path.display());
        self.blobs.push(handle);
        Ok(pointer)
    }

    /// An image: inline when every wire accepts it, else stored.
    fn image(&mut self, uri: Option<&str>, mime: Option<&str>, data: &str) {
        let mime = mime_label(mime, "image/*");
        let subject = match uri {
            Some(uri) => format!("image {} ({mime})", clean_text(uri, MAX_URI_CHARS)),
            None => format!("image {mime}"),
        };
        let reason = if self.media.len() >= MAX_INLINE_IMAGES {
            Err("too many images in one result")
        } else if self.inline_base64.saturating_add(data.len()) > MAX_INLINE_IMAGE_BASE64_TOTAL {
            Err("the images of this result are too large to send inline")
        } else {
            image_limits::inline_image(&mime, data, MAX_INLINE_IMAGE_BASE64)
        };
        match reason {
            Ok(block) => {
                self.inline_base64 += data.len();
                let size = image_limits::decode_base64(data).map_or(0, |bytes| bytes.len());
                self.media.push(block);
                self.parts
                    .push(format!("[{subject}, {}]", format_size(size as u64)));
            }
            Err(reason) => {
                let Some(bytes) = image_limits::decode_base64(data) else {
                    self.parts
                        .push(format!("[{subject} omitted: invalid base64]"));
                    return;
                };
                let size = format_size(bytes.len() as u64);
                match self.spill("mcp-image", &bytes) {
                    Ok(pointer) => self.parts.push(format!(
                        "[{subject}, {size}, not sent inline ({reason}); saved to {pointer}]"
                    )),
                    Err(error) => self.parts.push(format!(
                        "[{subject}, {size}, omitted: {reason}; not saved: {error}]"
                    )),
                }
            }
        }
    }

    /// The contents of an embedded or read resource.
    fn resource_contents(&mut self, resource: &Value) {
        let uri = resource.get("uri").and_then(Value::as_str);
        let mime = resource.get("mimeType").and_then(Value::as_str);
        if let Some(text) = resource.get("text").and_then(Value::as_str) {
            self.parts.push(text.to_owned());
            return;
        }
        let Some(blob) = resource.get("blob").and_then(Value::as_str) else {
            self.parts.push(format!(
                "[resource {} has no readable content]",
                clean_text(uri.unwrap_or("(no uri)"), MAX_URI_CHARS)
            ));
            return;
        };
        if mime.is_some_and(|mime| mime.trim().to_ascii_lowercase().starts_with("image/")) {
            self.image(uri, mime, blob);
            return;
        }
        let shown_uri = clean_text(uri.unwrap_or("(no uri)"), MAX_URI_CHARS);
        let Some(bytes) = image_limits::decode_base64(blob) else {
            self.parts.push(format!(
                "[Binary resource {shown_uri} omitted: invalid base64]"
            ));
            return;
        };
        if is_text_mime(mime) {
            self.parts
                .push(String::from_utf8_lossy(&bytes).into_owned());
            return;
        }
        let kind = format!(
            "{}, {}",
            mime_label(mime, "unknown type"),
            format_size(bytes.len() as u64)
        );
        match self.spill("mcp-resource", &bytes) {
            Ok(pointer) => self.parts.push(format!(
                "[Binary resource {shown_uri} ({kind}) saved to {pointer}]"
            )),
            Err(error) => self.parts.push(format!(
                "[Binary resource {shown_uri} ({kind}) could not be saved: {error}]"
            )),
        }
    }

    fn content_block(&mut self, server: &str, block: &Value) {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => self.parts.push(
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            ),
            Some("image") => self.image(
                None,
                block.get("mimeType").and_then(Value::as_str),
                block
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ),
            Some("audio") => self.parts.push(format!(
                "[audio {} omitted]",
                mime_label(
                    block.get("mimeType").and_then(Value::as_str),
                    "unknown type"
                )
            )),
            Some("resource_link") => self.parts.push(resource_link_line(server, block)),
            Some("resource") => match block.get("resource") {
                Some(resource) => self.resource_contents(resource),
                None => self.parts.push("[resource content missing]".to_owned()),
            },
            other => self.parts.push(format!(
                "[unsupported MCP content {}]",
                clean_text(other.unwrap_or("(no type)"), 64)
            )),
        }
    }

    fn finish(self, success: bool) -> Rendered {
        Rendered {
            text: self.parts.join("\n"),
            success,
            media: self.media,
            blobs: self.blobs,
        }
    }
}

/// Renders a `tools/call` result of `server`/`tool` (empty names: generic
/// labels). `isError` is a failed result; its message is the content, or a
/// line naming the tool when the content has no text.
pub(in crate::runtime) fn render_call_tool_result(
    value: &Value,
    server: &str,
    tool: &str,
    env: &RenderEnv<'_>,
) -> Rendered {
    let is_error = value
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut collector = Collector::new(env);
    let structured = value.get("structuredContent").filter(|v| !v.is_null());
    match (value.get("content").and_then(Value::as_array), structured) {
        (Some(content), _) if !content.is_empty() => {
            for block in content {
                collector.content_block(server, block);
            }
        }
        // No content blocks (empty, or the member is missing: some servers
        // return only structured content): structured content is the result.
        (_, Some(structured)) => {
            collector
                .parts
                .push(serde_json::to_string_pretty(structured).unwrap_or_default());
        }
        (Some(_), None) => {}
        (None, None) => {
            let mut whole = value.clone();
            if let Some(object) = whole.as_object_mut() {
                object.remove("_meta");
            }
            collector
                .parts
                .push(serde_json::to_string_pretty(&whole).unwrap_or_default());
        }
    }
    let mut rendered = collector.finish(!is_error);
    if is_error && rendered.text.trim().is_empty() {
        rendered.text = if server.is_empty() && tool.is_empty() {
            "MCP tool returned an error".to_owned()
        } else {
            format!(
                "MCP tool {}/{} returned an error",
                clean_text(server, MAX_LABEL_CHARS),
                clean_text(tool, MAX_LABEL_CHARS)
            )
        };
    } else if rendered.text.is_empty() && rendered.media.is_empty() {
        rendered.text = String::from("(empty result)");
    }
    rendered
}

/// Renders the result of `resources/read`: several contents are labelled by
/// their URIs; an empty result says so.
pub(in crate::runtime) fn render_read_resource_result(
    value: &Value,
    uri: &str,
    env: &RenderEnv<'_>,
) -> Rendered {
    let contents = value
        .get("contents")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut collector = Collector::new(env);
    for resource in contents {
        if contents.len() > 1 {
            let label = resource
                .get("uri")
                .and_then(Value::as_str)
                .map(|uri| clean_text(uri, MAX_URI_CHARS))
                .unwrap_or_default();
            collector.parts.push(format!("{label}:"));
        }
        collector.resource_contents(resource);
    }
    let mut rendered = collector.finish(true);
    if rendered.text.is_empty() && rendered.media.is_empty() {
        rendered.text = format!("Resource {} is empty.", clean_text(uri, MAX_URI_CHARS));
    }
    rendered
}

/// Results for a provider whose wire takes no images in tool results: the
/// images are dropped and the text says so.
pub(in crate::runtime) fn strip_unaccepted_media(results: &mut [ToolResult]) {
    for result in results {
        let images = result
            .media
            .iter()
            .filter(|block| matches!(block, ProviderContentBlock::Image { .. }))
            .count();
        result.media.clear();
        if images > 0 {
            result.output.push_str(&format!(
                "\n[{images} image(s) not shown: this provider does not accept images in tool results]"
            ));
        }
    }
}

/// Text that fits the inline budget, and the artifact holding it whole when
/// it did not.
pub(in crate::runtime) struct LimitedText {
    pub(in crate::runtime) inline: String,
    pub(in crate::runtime) artifact: Option<ArtifactHandle>,
}

/// Keeps the head and the tail of `text` within `limit` bytes around a
/// marker, cutting at character boundaries only.
pub(in crate::runtime) fn middle_truncate(text: &str, limit: usize, note: &str) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let marker_for =
        |omitted: usize| format!("\n[… {omitted} bytes omitted from the middle; {note}]\n");
    let marker_len = marker_for(text.len()).len();
    let budget = limit.saturating_sub(marker_len);
    let mut head_end = budget / 2 + budget % 2;
    while !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = text.len().saturating_sub(budget / 2);
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = tail_start.saturating_sub(head_end);
    format!(
        "{}{}{}",
        &text[..head_end],
        marker_for(omitted),
        &text[tail_start..]
    )
}

/// Applies the inline budget to a result's text. `text` must already be
/// redacted: the whole of it is stored.
pub(in crate::runtime) fn limit_text(text: String, store: Option<&ArtifactStore>) -> LimitedText {
    if text.len() <= MCP_INLINE_TEXT_BYTES {
        return LimitedText {
            inline: text,
            artifact: None,
        };
    }
    let (stored, cut) = if text.len() > MCP_FULL_TEXT_BYTES {
        let mut end = MCP_FULL_TEXT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        (
            format!(
                "{}\n[output cut at {} MiB]",
                &text[..end],
                MCP_FULL_TEXT_BYTES / (1024 * 1024)
            ),
            true,
        )
    } else {
        (text.clone(), false)
    };
    let artifact = store.and_then(|store| store.put("mcp-output", stored.as_bytes()).ok());
    // The pointer is part of the text: the runtime allots a result exactly
    // its own length, so a suffix added later would cost the tail.
    let note = match (&artifact, cut) {
        (Some(handle), false) => format!(
            "the complete output is in artifact id={} path={}",
            handle.id,
            handle.path.display()
        ),
        (Some(handle), true) => format!(
            "artifact id={} path={} holds the output cut at 8 MiB",
            handle.id,
            handle.path.display()
        ),
        (None, _) => "the complete output could not be stored".to_owned(),
    };
    LimitedText {
        inline: middle_truncate(&text, MCP_INLINE_TEXT_BYTES, &note),
        artifact,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> String {
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend(width.to_be_bytes());
        bytes.extend(height.to_be_bytes());
        encode(&bytes)
    }

    fn encode(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let mut value = 0_u32;
            for (index, byte) in chunk.iter().enumerate() {
                value |= u32::from(*byte) << (16 - 8 * index);
            }
            for index in 0..4 {
                if index <= chunk.len() {
                    out.push(ALPHABET[((value >> (18 - 6 * index)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    fn store() -> (std::path::PathBuf, ArtifactStore) {
        let root = std::env::temp_dir().join(format!(
            "slim-mcp-render-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = ArtifactStore::new(&root).unwrap();
        (root, store)
    }

    fn render(value: Value) -> Rendered {
        render_call_tool_result(&value, "srv", "tool", &RenderEnv::default())
    }

    #[test]
    fn base64_round_trips_and_rejects_malformed_input() {
        for len in 0..40_usize {
            let bytes: Vec<u8> = (0..len).map(|index| (index * 37 + 11) as u8).collect();
            assert_eq!(
                image_limits::decode_base64(&encode(&bytes)).as_deref(),
                Some(bytes.as_slice()),
                "{len}"
            );
        }
        assert_eq!(
            image_limits::decode_base64("aGk").as_deref(),
            Some(&b"hi"[..])
        );
        assert_eq!(
            image_limits::decode_base64("aG\nk=").as_deref(),
            Some(&b"hi"[..])
        );
        assert!(image_limits::decode_base64("a").is_none());
        assert!(image_limits::decode_base64("aG!k").is_none());
        assert!(image_limits::decode_base64("aG=k").is_none());
    }

    #[test]
    fn text_blocks_join_and_unknown_blocks_are_labelled() {
        let rendered = render(json!({"content": [
            {"type": "text", "text": "one"},
            {"type": "text", "text": "two"},
            {"type": "hologram\u{1b}[0m"},
        ]}));
        assert!(rendered.success);
        assert_eq!(
            rendered.text,
            "one\ntwo\n[unsupported MCP content hologram [0m]"
        );
    }

    #[test]
    fn audio_is_replaced_by_a_placeholder() {
        let rendered = render(json!({"content": [
            {"type": "audio", "mimeType": "audio/wav", "data": "AAAA"},
            {"type": "audio", "data": "AAAA"},
        ]}));
        assert_eq!(
            rendered.text,
            "[audio audio/wav omitted]\n[audio unknown type omitted]"
        );
        assert!(rendered.media.is_empty());
    }

    #[test]
    fn images_become_content_blocks_with_a_text_line() {
        let data = png(32, 16);
        let rendered = render(json!({"content": [
            {"type": "text", "text": "shot"},
            {"type": "image", "mimeType": "image/png", "data": data},
        ]}));
        assert_eq!(rendered.media.len(), 1);
        assert!(matches!(
            &rendered.media[0],
            ProviderContentBlock::Image { media_type, .. } if media_type == "image/png"
        ));
        assert!(
            rendered.text.starts_with("shot\n[image image/png, "),
            "{}",
            rendered.text
        );
        assert!(
            !rendered.text.contains(&data),
            "image data never enters the text"
        );
    }

    #[test]
    fn unusable_images_are_stored_or_described_never_sent() {
        let (root, store) = store();
        let env = RenderEnv {
            store: Some(&store),
            secrets: &[],
        };
        let value = json!({"content": [
            {"type": "image", "mimeType": "image/png", "data": png(4096, 4096)},
            {"type": "image", "mimeType": "image/svg+xml", "data": "PHN2Zy8+"},
            {"type": "image", "mimeType": "image/png", "data": "not base64!"},
        ]});
        let rendered = render_call_tool_result(&value, "s", "t", &env);
        assert!(rendered.media.is_empty());
        assert_eq!(rendered.blobs.len(), 2, "{}", rendered.text);
        assert!(rendered.text.contains("dimensions exceed the inline limit"));
        assert!(rendered.text.contains("unsupported image type"));
        assert!(rendered.text.contains("invalid base64"));
        assert!(rendered.text.contains("artifact id=mcp-image-"));
        let without_store = render(value);
        assert!(without_store.blobs.is_empty());
        assert!(without_store.text.contains("no artifact storage"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn inline_images_are_capped_per_result() {
        let data = png(8, 8);
        let blocks: Vec<Value> = (0..MAX_INLINE_IMAGES + 2)
            .map(|_| json!({"type": "image", "mimeType": "image/png", "data": data}))
            .collect();
        let rendered = render(json!({"content": blocks}));
        assert_eq!(rendered.media.len(), MAX_INLINE_IMAGES);
        assert_eq!(
            rendered
                .text
                .matches("too many images in one result")
                .count(),
            2
        );
    }

    #[test]
    fn embedded_resources_follow_their_content_type() {
        let (root, store) = store();
        let env = RenderEnv {
            store: Some(&store),
            secrets: &[],
        };
        let value = json!({"content": [
            {"type": "resource", "resource": {"uri": "file:///a.txt", "text": "plain"}},
            {"type": "resource", "resource": {"uri": "file:///b.json",
                "mimeType": "application/json; charset=utf-8", "blob": encode(b"{\"a\":1}")}},
            {"type": "resource", "resource": {"uri": "file:///c.xml",
                "mimeType": "application/atom+xml", "blob": encode("<é/>".as_bytes())}},
            {"type": "resource", "resource": {"uri": "file:///d.png",
                "mimeType": "image/png", "blob": png(5, 5)}},
            {"type": "resource", "resource": {"uri": "file:///e.bin",
                "mimeType": "application/octet-stream", "blob": encode(&[0, 1, 2, 255])}},
        ]});
        let rendered = render_call_tool_result(&value, "s", "t", &env);
        let lines: Vec<&str> = rendered.text.lines().collect();
        assert_eq!(lines[0], "plain");
        assert_eq!(lines[1], "{\"a\":1}");
        assert_eq!(lines[2], "<é/>");
        assert!(
            lines[3].starts_with("[image file:///d.png (image/png), "),
            "{}",
            lines[3]
        );
        assert!(
            lines[4].starts_with(
                "[Binary resource file:///e.bin (application/octet-stream, 4 B) saved to artifact id=mcp-resource-"
            ),
            "{}",
            lines[4]
        );
        assert_eq!(rendered.media.len(), 1);
        assert_eq!(rendered.blobs.len(), 1);
        assert_eq!(
            std::fs::read(&rendered.blobs[0].path).unwrap(),
            vec![0, 1, 2, 255]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn binary_blobs_without_storage_are_described_not_dropped_silently() {
        let rendered = render(json!({"content": [
            {"type": "resource", "resource": {"uri": "file:///e.bin", "blob": encode(&[9; 2048])}},
            {"type": "resource", "resource": {"uri": "file:///f.bin", "blob": "***"}},
        ]}));
        assert!(rendered.text.contains(
            "[Binary resource file:///e.bin (unknown type, 2.0 KB) could not be saved: no artifact storage is available]"
        ), "{}", rendered.text);
        assert!(rendered
            .text
            .contains("[Binary resource file:///f.bin omitted: invalid base64]"));
    }

    #[test]
    fn resource_links_say_how_to_read_them() {
        let rendered = render_call_tool_result(
            &json!({"content": [{
                "type": "resource_link", "uri": "file:///r.md", "name": "r", "title": "Readme",
                "mimeType": "text/markdown", "size": 2048, "description": "Docs\nhere"
            }, {"type": "resource_link", "uri": "x://y", "name": "bare"}]}),
            "docs",
            "search",
            &RenderEnv::default(),
        );
        assert_eq!(
            rendered.text,
            "[Resource file:///r.md \"Readme\" (text/markdown, 2.0 KB): Docs here. Read it with mcp {server:\"docs\", uri:\"file:///r.md\"}]\n\
             [Resource x://y \"bare\". Read it with mcp {server:\"docs\", uri:\"x://y\"}]"
        );
    }

    #[test]
    fn structured_content_is_shown_only_without_content_blocks() {
        let with_content = render(json!({
            "content": [{"type": "text", "text": "human view"}],
            "structuredContent": {"rows": [1, 2]}
        }));
        assert_eq!(with_content.text, "human view");
        let only_structured = render(json!({"content": [], "structuredContent": {"rows": [1, 2]}}));
        assert!(only_structured.success);
        assert_eq!(
            serde_json::from_str::<Value>(&only_structured.text).unwrap(),
            json!({"rows": [1, 2]})
        );
        assert!(only_structured.text.contains('\n'), "pretty printed");
        // A server may omit `content` and return only the structured result.
        let missing_content = render(json!({"structuredContent": {"rows": [1, 2]}}));
        assert!(missing_content.success);
        assert_eq!(
            serde_json::from_str::<Value>(&missing_content.text).unwrap(),
            json!({"rows": [1, 2]})
        );
        let no_array = render(json!({"weird": true, "_meta": {"x": 1}}));
        assert_eq!(
            serde_json::from_str::<Value>(&no_array.text).unwrap(),
            json!({"weird": true})
        );
        assert_eq!(render(json!({"content": []})).text, "(empty result)");
    }

    #[test]
    fn errors_fail_the_call_and_name_the_tool_when_they_carry_no_text() {
        let with_text =
            render(json!({"content": [{"type": "text", "text": "disk full"}], "isError": true}));
        assert!(!with_text.success);
        assert_eq!(with_text.text, "disk full");
        let bare = render(json!({"content": [], "isError": true}));
        assert!(!bare.success);
        assert_eq!(bare.text, "MCP tool srv/tool returned an error");
        let image_only = render(json!({
            "content": [{"type": "image", "mimeType": "image/png", "data": png(2, 2)}],
            "isError": true
        }));
        assert!(!image_only.success);
        assert!(image_only.text.contains("[image image/png"));
        assert_eq!(
            render_call_tool_result(
                &json!({"isError": true, "content": []}),
                "",
                "",
                &RenderEnv::default()
            )
            .text,
            "MCP tool returned an error"
        );
    }

    #[test]
    fn read_results_label_several_contents_and_report_empty_ones() {
        let one = render_read_resource_result(
            &json!({"contents": [{"uri": "file:///a", "text": "only"}]}),
            "file:///a",
            &RenderEnv::default(),
        );
        assert_eq!(one.text, "only");
        let several = render_read_resource_result(
            &json!({"contents": [
                {"uri": "file:///d/a", "text": "A"},
                {"uri": "file:///d/b", "mimeType": "application/json", "blob": encode(b"[1]")},
            ]}),
            "file:///d",
            &RenderEnv::default(),
        );
        assert_eq!(several.text, "file:///d/a:\nA\nfile:///d/b:\n[1]");
        let empty = render_read_resource_result(
            &json!({"contents": []}),
            "file:///z",
            &RenderEnv::default(),
        );
        assert_eq!(empty.text, "Resource file:///z is empty.");
    }

    #[test]
    fn middle_truncation_keeps_both_ends_within_the_limit_on_char_boundaries() {
        let text: String = ('a'..='z').cycle().take(5000).collect::<String>() + &"é".repeat(5000);
        for limit in [200, 201, 999, 4096] {
            let view = middle_truncate(&text, limit, "see artifact");
            assert!(view.len() <= limit, "{} > {limit}", view.len());
            assert!(view.starts_with("abcdef"));
            assert!(view.ends_with('é'));
            assert!(view.contains("bytes omitted from the middle; see artifact"));
        }
        assert_eq!(middle_truncate("short", 100, "n"), "short");
        // A limit below the marker still yields valid text.
        let tiny = middle_truncate(&text, 10, "n");
        assert!(tiny.contains("omitted"));
    }

    #[test]
    fn middle_truncation_never_splits_multibyte_characters_at_either_end() {
        for unit in ["é", "日", "😀"] {
            let text = unit.repeat(4000);
            for limit in 120..=260 {
                // Slicing off a character boundary would panic.
                let view = middle_truncate(&text, limit, "n");
                assert!(view.len() <= limit, "{unit} {limit}: {}", view.len());
                assert!(
                    view.starts_with(unit) && view.ends_with(unit),
                    "{unit} {limit}"
                );
            }
        }
    }

    #[test]
    fn oversized_text_is_stored_whole_and_shown_as_head_and_tail() {
        let (root, store) = store();
        let big = format!("HEAD{}TAIL", "é".repeat(MCP_INLINE_TEXT_BYTES * 2));
        let limited = limit_text(big.clone(), Some(&store));
        assert!(limited.inline.len() <= MCP_INLINE_TEXT_BYTES);
        assert!(limited.inline.starts_with("HEAD") && limited.inline.ends_with("TAIL"));
        let handle = limited.artifact.expect("stored");
        assert!(limited.inline.contains(&format!(
            "the complete output is in artifact id={} path=",
            handle.id
        )));
        assert_eq!(store.read(&handle).unwrap(), big.as_bytes());
        let unstored = limit_text(big, None);
        assert!(unstored.artifact.is_none());
        assert!(unstored.inline.contains("could not be stored"));
        let small = limit_text("fits".into(), Some(&store));
        assert_eq!(small.inline, "fits");
        assert!(small.artifact.is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_pretty_json_result_over_the_budget_is_stored_as_valid_json() {
        let (root, store) = store();
        let rows: Vec<Value> = (0..2000)
            .map(|index| json!({"row": index, "text": "é".repeat(40)}))
            .collect();
        let rendered = render(json!({"content": [], "structuredContent": {"rows": rows}}));
        let limited = limit_text(rendered.text, Some(&store));
        let handle = limited.artifact.expect("stored");
        let stored: Value =
            serde_json::from_slice(&store.read(&handle).unwrap()).expect("valid JSON");
        assert_eq!(stored["rows"].as_array().unwrap().len(), 2000);
        assert!(limited.inline.len() <= MCP_INLINE_TEXT_BYTES);
        let _ = std::fs::remove_dir_all(root);
    }
}
