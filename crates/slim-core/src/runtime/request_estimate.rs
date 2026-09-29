use super::*;

pub(super) fn messages_are_text_only(messages: &[ProviderMessage]) -> bool {
    messages.iter().all(|message| {
        message.responses_reasoning.is_empty()
            && message.chat_reasoning.is_none()
            && message
                .content_blocks
                .iter()
                .all(|block| matches!(block, crate::provider::ProviderContentBlock::Text(_)))
    })
}

pub(super) fn estimate_unprepared_request_chars<A: ProviderAdapter>(
    adapter: &A,
    messages: &[ProviderMessage],
    tools: &[Value],
    system_prompt_override: Option<&str>,
) -> Option<u64> {
    const MESSAGE_ENVELOPE_CHARS: u64 = 256;
    const TOOL_ENVELOPE_CHARS: u64 = 128;
    // Without the adapter's envelope bound there is no estimate at all; ask
    // before scanning so adapters without one skip the whole walk.
    let request_envelope_chars = adapter.request_envelope_upper_bound_chars()?;
    let system_chars = system_prompt_override
        .or_else(|| adapter.system_prompt_for_budget())
        .map_or(0, estimate_json_string_chars);
    let message_chars = messages.iter().fold(0_u64, |total, message| {
        let scalar_chars = [
            Some(message.role.as_str()),
            Some(message.content.as_str()),
            message.name.as_deref(),
            message.tool_call_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        .map(estimate_json_string_chars)
        .fold(0_u64, u64::saturating_add);
        let call_chars = message.tool_calls.iter().fold(0_u64, |total, call| {
            total
                .saturating_add(TOOL_ENVELOPE_CHARS)
                .saturating_add(estimate_json_string_chars(&call.id))
                .saturating_add(estimate_json_string_chars(&call.name))
                .saturating_add(estimate_json_string_chars(&call.arguments))
        });
        let block_chars = message.content_blocks.iter().fold(0_u64, |total, block| {
            let payload = match block {
                crate::provider::ProviderContentBlock::Text(text) => {
                    estimate_json_string_chars(text)
                }
                crate::provider::ProviderContentBlock::Image { media_type, data }
                | crate::provider::ProviderContentBlock::Audio { media_type, data }
                | crate::provider::ProviderContentBlock::File { media_type, data } => {
                    estimate_json_string_chars(media_type)
                        .saturating_add(estimate_json_string_chars(data))
                }
                crate::provider::ProviderContentBlock::Unsupported { kind } => {
                    estimate_json_string_chars(kind)
                }
            };
            total
                .saturating_add(MESSAGE_ENVELOPE_CHARS)
                .saturating_add(payload)
        });
        total
            .saturating_add(MESSAGE_ENVELOPE_CHARS)
            .saturating_add(scalar_chars)
            .saturating_add(call_chars)
            .saturating_add(block_chars)
            .saturating_add(message.chat_reasoning.as_ref().map_or(0, |state| {
                state.details.iter().fold(
                    estimate_json_string_chars(&state.content),
                    |total, detail| total.saturating_add(estimate_json_chars(detail)),
                )
            }))
            .saturating_add(
                message
                    .responses_reasoning
                    .iter()
                    .map(|state| estimate_json_chars(&state.item))
                    .sum::<u64>(),
            )
    });
    let tool_chars = tools
        .iter()
        .map(estimate_json_chars)
        .fold(0_u64, |total, chars| {
            total
                .saturating_add(TOOL_ENVELOPE_CHARS)
                .saturating_add(chars)
        });
    Some(
        request_envelope_chars
            .saturating_add(estimate_json_string_chars(adapter.model()))
            .saturating_add(system_chars)
            .saturating_add(message_chars)
            .saturating_add(tool_chars),
    )
}

pub(super) fn estimate_json_string_chars(value: &str) -> u64 {
    // Byte scan: multi-byte UTF-8 sequences contribute 1 via the lead byte;
    // continuation bytes add 0.
    value.bytes().fold(2_u64, |total, byte| {
        total.saturating_add(match byte {
            0x00..=0x1f => 6,
            b'"' | b'\\' => 2,
            0x80..=0xbf => 0,
            _ => 1,
        })
    })
}

pub(super) fn estimate_json_chars(value: &Value) -> u64 {
    match value {
        Value::Null => 4,
        Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Number(number) => number.to_string().len() as u64,
        Value::String(value) => estimate_json_string_chars(value),
        Value::Array(values) => values
            .iter()
            .map(estimate_json_chars)
            .fold(2_u64, |total, chars| {
                total.saturating_add(chars).saturating_add(1)
            }),
        Value::Object(values) => values.iter().fold(2_u64, |total, (key, value)| {
            total
                .saturating_add(estimate_json_string_chars(key))
                .saturating_add(estimate_json_chars(value))
                .saturating_add(2)
        }),
    }
}
