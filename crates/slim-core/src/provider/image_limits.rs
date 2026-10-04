use super::{
    base64_value, normalize_base64, normalize_media_type, ProviderContentBlock, ProviderError,
    ProviderMessage,
};

// Anthropic's request limits apply to images retained from earlier turns too.
// https://platform.claude.com/docs/en/build-with-claude/vision
pub(super) fn validate_anthropic_images(messages: &[ProviderMessage]) -> Result<(), ProviderError> {
    let count = messages
        .iter()
        .flat_map(|message| &message.content_blocks)
        .filter(|block| matches!(block, ProviderContentBlock::Image { .. }))
        .count();
    let limit = if count > 20 { 2000 } else { 8000 };
    for block in messages.iter().flat_map(|message| &message.content_blocks) {
        let ProviderContentBlock::Image { media_type, data } = block else {
            continue;
        };
        if data.len() > 10 * 1024 * 1024 {
            return Err(invalid("Anthropic image exceeds the 10 MiB base64 limit"));
        }
        let media_type = normalize_media_type(media_type)?;
        let encoded = normalize_base64(data)?;
        let mut bytes = Vec::with_capacity(encoded.len() / 4 * 3);
        for chunk in encoded.as_bytes().as_chunks::<4>().0 {
            let value = chunk.iter().fold(0_u32, |value, byte| {
                (value << 6) | u32::from(base64_value(*byte).unwrap_or(0))
            });
            bytes.push((value >> 16) as u8);
            if chunk[2] != b'=' {
                bytes.push((value >> 8) as u8);
            }
            if chunk[3] != b'=' {
                bytes.push(value as u8);
            }
        }
        let (width, height) = dimensions(&media_type, &bytes)
            .filter(|(width, height)| *width > 0 && *height > 0)
            .ok_or_else(|| {
                invalid("Anthropic image has an unsupported or incomplete dimension header")
            })?;
        if width > limit || height > limit {
            return Err(invalid(&format!("Anthropic image is {width}x{height}; maximum is {limit}x{limit} pixels for this request")));
        }
    }
    Ok(())
}

fn invalid(message: &str) -> ProviderError {
    ProviderError::InvalidResponse {
        message: message.into(),
    }
}

// Read metadata only: no pixel allocation, decompression or automatic resizing.
fn dimensions(media_type: &str, bytes: &[u8]) -> Option<(u32, u32)> {
    match media_type {
        "image/png"
            if bytes.starts_with(b"\x89PNG\r\n\x1a\n") && bytes.get(8..16)? == b"\0\0\0\rIHDR" =>
        {
            Some((
                u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?),
                u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?),
            ))
        }
        "image/gif" if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") => Some((
            u16::from_le_bytes(bytes.get(6..8)?.try_into().ok()?).into(),
            u16::from_le_bytes(bytes.get(8..10)?.try_into().ok()?).into(),
        )),
        "image/jpeg" if bytes.starts_with(&[0xff, 0xd8]) => jpeg_dimensions(&bytes[2..]),
        "image/webp" if bytes.starts_with(b"RIFF") && bytes.get(8..12)? == b"WEBP" => {
            let length = u32::from_le_bytes(bytes.get(4..8)?.try_into().ok()?) as usize;
            webp_dimensions(bytes.get(12..length.checked_add(8)?)?)
        }
        _ => None,
    }
}

fn jpeg_dimensions(mut bytes: &[u8]) -> Option<(u32, u32)> {
    while !bytes.is_empty() {
        if bytes[0] != 0xff {
            return None;
        }
        while bytes.first() == Some(&0xff) {
            bytes = &bytes[1..];
        }
        let marker = *bytes.first()?;
        bytes = &bytes[1..];
        if matches!(marker, 0xd9 | 0xda | 0x00) {
            return None;
        }
        if matches!(marker, 0x01 | 0xd0..=0xd8) {
            continue;
        }
        let length = u16::from_be_bytes(bytes.get(..2)?.try_into().ok()?) as usize;
        if length < 2 {
            return None;
        }
        let segment = bytes.get(..length)?;
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            return Some((
                u16::from_be_bytes(segment.get(5..7)?.try_into().ok()?).into(),
                u16::from_be_bytes(segment.get(3..5)?.try_into().ok()?).into(),
            ));
        }
        bytes = &bytes[length..];
    }
    None
}

fn webp_dimensions(mut bytes: &[u8]) -> Option<(u32, u32)> {
    while !bytes.is_empty() {
        let kind = bytes.get(..4)?;
        let length = u32::from_le_bytes(bytes.get(4..8)?.try_into().ok()?) as usize;
        let end = 8_usize.checked_add(length)?;
        let chunk = bytes.get(8..end)?;
        match kind {
            b"VP8X" => {
                let width = chunk.get(4..7)?;
                let height = chunk.get(7..10)?;
                return Some((
                    u32::from_le_bytes([width[0], width[1], width[2], 0]) + 1,
                    u32::from_le_bytes([height[0], height[1], height[2], 0]) + 1,
                ));
            }
            b"VP8 " if chunk.get(3..6)? == [0x9d, 0x01, 0x2a] => {
                return Some((
                    (u16::from_le_bytes(chunk.get(6..8)?.try_into().ok()?) & 0x3fff).into(),
                    (u16::from_le_bytes(chunk.get(8..10)?.try_into().ok()?) & 0x3fff).into(),
                ))
            }
            b"VP8L" if chunk.first() == Some(&0x2f) => {
                let bits = u32::from_le_bytes(chunk.get(1..5)?.try_into().ok()?);
                return Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1));
            }
            _ => {}
        }
        bytes = bytes.get(end.checked_add(length % 2)?..)?;
    }
    None
}

/// Largest side of a tool-result image sent inline. Anthropic lowers its
/// limit from 8000 to 2000 pixels once a request carries more than 20
/// images, and a rejected image stays in the history for every later request.
pub(crate) const INLINE_IMAGE_MAX_DIMENSION: u32 = 2000;

/// Decodes standard base64: padding is optional and ASCII whitespace is
/// ignored. Any other character, data after padding, or a dangling sextet
/// is `None`.
pub(crate) fn decode_base64(encoded: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(encoded.len() / 4 * 3 + 3);
    let mut accumulator = 0_u32;
    let mut bits = 0_u32;
    let mut padded = false;
    for &byte in encoded.as_bytes() {
        if byte.is_ascii_whitespace() {
            continue;
        }
        if byte == b'=' {
            padded = true;
            continue;
        }
        if padded {
            return None;
        }
        accumulator = (accumulator << 6) | u32::from(base64_value(byte)?);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
            accumulator &= (1 << bits) - 1;
        }
    }
    (bits < 6).then_some(out)
}

/// A tool-result image that every provider wire accepts, or why it cannot
/// travel inline: a supported type, well-formed base64 within
/// `max_base64_bytes`, readable dimensions, and a side of at most
/// [`INLINE_IMAGE_MAX_DIMENSION`] pixels.
pub(crate) fn inline_image(
    media_type: &str,
    base64: &str,
    max_base64_bytes: usize,
) -> Result<ProviderContentBlock, &'static str> {
    let media_type = normalize_media_type(media_type).map_err(|_| "invalid media type")?;
    if !matches!(
        media_type.as_str(),
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    ) {
        return Err("unsupported image type");
    }
    if base64.len() > max_base64_bytes {
        return Err("image is too large to send inline");
    }
    let encoded = normalize_base64(base64).map_err(|_| "invalid base64")?;
    let bytes = decode_base64(&encoded).ok_or("invalid base64")?;
    let (width, height) = dimensions(&media_type, &bytes)
        .filter(|(width, height)| *width > 0 && *height > 0)
        .ok_or("image dimensions are unreadable")?;
    if width > INLINE_IMAGE_MAX_DIMENSION || height > INLINE_IMAGE_MAX_DIMENSION {
        return Err("image dimensions exceed the inline limit");
    }
    Ok(ProviderContentBlock::Image {
        media_type,
        data: encoded,
    })
}

#[cfg(test)]
mod tests {
    use super::super::{encode_standard_base64, AnthropicAdapter, ProviderAdapter, ProviderConfig};
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend(width.to_be_bytes());
        bytes.extend(height.to_be_bytes());
        bytes
    }

    fn message(width: u32, height: u32) -> ProviderMessage {
        ProviderMessage::user("").with_content_blocks(vec![ProviderContentBlock::image(
            "image/png",
            encode_standard_base64(&png(width, height)),
        )])
    }

    fn webp(kind: &[u8; 4], chunk: &[u8]) -> Vec<u8> {
        let mut bytes = b"RIFF".to_vec();
        bytes.extend((12 + chunk.len() as u32).to_le_bytes());
        bytes.extend(b"WEBP");
        bytes.extend(kind);
        bytes.extend((chunk.len() as u32).to_le_bytes());
        bytes.extend(chunk);
        bytes
    }

    #[test]
    fn dimensions_support_all_anthropic_formats_and_bounded_truncation() {
        let jpeg = vec![
            0xff, 0xd8, 0xff, 0xe1, 0, 4, 1, 2, 0xff, 0xc2, 0, 7, 8, 0, 2, 0, 3,
        ];
        let fixtures = [
            ("image/png", png(3, 2)),
            ("image/gif", b"GIF89a\x03\0\x02\0".to_vec()),
            ("image/jpeg", jpeg),
            ("image/webp", webp(b"VP8X", &[0, 0, 0, 0, 2, 0, 0, 1, 0, 0])),
            (
                "image/webp",
                webp(b"VP8 ", &[0, 0, 0, 0x9d, 1, 0x2a, 3, 0, 2, 0]),
            ),
            ("image/webp", webp(b"VP8L", &[0x2f, 2, 0x40, 0, 0])),
        ];
        for (mime, bytes) in fixtures {
            assert_eq!(dimensions(mime, &bytes), Some((3, 2)), "{mime}");
            for end in 0..bytes.len() {
                assert_eq!(
                    dimensions(mime, &bytes[..end]),
                    None,
                    "truncated {mime} at {end}"
                );
            }
        }
        assert_eq!(
            dimensions("image/jpeg", &[0xff, 0xd8, 0xff, 0xe1, 0, 1]),
            None
        );
        assert_eq!(dimensions("image/png", b"not a png"), None);
    }

    #[test]
    fn wrappers_around_the_anthropic_wire_do_not_take_tool_result_images() {
        use super::super::{
            CommandCodeAdapter, OpenCodeGoAdapter, COMMANDCODE_BASE_URL, OPENCODE_GO_BASE_URL,
        };
        // A gateway may front a model without vision: only the adapters
        // that are the provider itself opt in.
        assert!(
            !CommandCodeAdapter::new(COMMANDCODE_BASE_URL, "claude-sonnet-4-6", "key", None)
                .unwrap()
                .accepts_tool_result_images()
        );
        assert!(
            !OpenCodeGoAdapter::new(OPENCODE_GO_BASE_URL, "minimax-m3", "key", None)
                .unwrap()
                .accepts_tool_result_images()
        );
        assert!(
            AnthropicAdapter::new(ProviderConfig::anthropic("http://x", "claude", "key"))
                .unwrap()
                .accepts_tool_result_images()
        );
    }

    #[test]
    fn inline_images_need_a_supported_type_readable_dimensions_and_a_small_size() {
        let valid = encode_standard_base64(&png(640, 480));
        let block = inline_image("IMAGE/PNG", &valid, 1024).expect("valid image");
        assert_eq!(
            block,
            ProviderContentBlock::Image {
                media_type: "image/png".into(),
                data: valid.clone()
            }
        );
        assert!(inline_image("image/png", &valid, 8).is_err(), "too large");
        assert!(inline_image("image/svg+xml", &valid, 1024).is_err());
        assert!(inline_image("image/png", "!!!!", 1024).is_err());
        let wide = encode_standard_base64(&png(INLINE_IMAGE_MAX_DIMENSION + 1, 10));
        assert_eq!(
            inline_image("image/png", &wide, 1024),
            Err("image dimensions exceed the inline limit")
        );
        let edge =
            encode_standard_base64(&png(INLINE_IMAGE_MAX_DIMENSION, INLINE_IMAGE_MAX_DIMENSION));
        assert!(inline_image("image/png", &edge, 1024).is_ok());
        let unknown = encode_standard_base64(b"\x89PNG\r\n\x1a\n");
        assert!(inline_image("image/png", &unknown, 1024).is_err());
        let zero = encode_standard_base64(&png(0, 10));
        assert!(inline_image("image/png", &zero, 1024).is_err());
    }

    #[test]
    fn anthropic_wire_wrappers_keep_the_same_image_limits() {
        use super::super::{
            CommandCodeAdapter, OpenCodeGoAdapter, COMMANDCODE_BASE_URL, OPENCODE_GO_BASE_URL,
        };
        let adapters: Vec<Box<dyn ProviderAdapter>> = vec![
            Box::new(
                CommandCodeAdapter::new(
                    COMMANDCODE_BASE_URL,
                    "claude-sonnet-4-6",
                    "fixture-key",
                    None,
                )
                .unwrap(),
            ),
            Box::new(
                OpenCodeGoAdapter::new(OPENCODE_GO_BASE_URL, "minimax-m3", "fixture-key", None)
                    .unwrap(),
            ),
        ];
        for adapter in adapters {
            for (width, allowed) in [(8000, true), (8001, false)] {
                let messages = [message(width, 1)];
                assert_eq!(
                    adapter.build_messages_request_checked(&messages).is_ok(),
                    allowed
                );
                assert_eq!(
                    adapter.prepare_messages_request_checked(&messages).is_ok(),
                    allowed
                );
                assert_eq!(
                    adapter
                        .prepare_compaction_request_checked(&messages)
                        .is_ok(),
                    allowed
                );
            }
        }
    }

    #[test]
    fn pixel_limits_apply_to_history_and_all_checked_anthropic_paths() {
        let adapter = AnthropicAdapter::new(ProviderConfig::anthropic(
            "https://example.invalid/v1/messages",
            "claude-test",
            "fixture-key",
        ))
        .unwrap();
        for (messages, allowed) in [
            (vec![message(8000, 8000)], true),
            (vec![message(8001, 1)], false),
            (vec![message(1, 9001)], false),
            (vec![message(0, 1)], false),
            (vec![message(2001, 1); 20], true),
            (vec![message(2001, 1); 21], false),
            (vec![message(2000, 2000); 21], true),
        ] {
            assert_eq!(
                adapter.build_messages_request_checked(&messages).is_ok(),
                allowed
            );
            assert_eq!(
                adapter
                    .prepare_messages_request_with_tools_checked(&messages, &[])
                    .is_ok(),
                allowed
            );
            assert_eq!(
                adapter
                    .prepare_compaction_request_checked(&messages)
                    .is_ok(),
                allowed
            );
        }
        let invalid = ProviderMessage::user("")
            .with_content_blocks(vec![ProviderContentBlock::image("image/png", "AAEC")]);
        assert!(adapter
            .prepare_messages_request_checked(&[invalid])
            .is_err());
    }
}
