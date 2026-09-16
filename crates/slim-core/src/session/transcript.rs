use std::collections::{BTreeMap, BTreeSet};

use crate::provider::{validate_tool_call_arguments, ProviderMessage};

use super::{DurableEntry, DurableEntryRole, DurableRecord};

impl DurableEntry {
    /// Persist visible conversation data only. Opaque provider continuation state
    /// is scoped to its issuing adapter and is never portable across processes.
    pub fn from_provider_message(
        entry_id: String,
        parent_entry_id: Option<String>,
        operation_id: String,
        message: ProviderMessage,
    ) -> Result<Self, &'static str> {
        let role = match message.role.as_str() {
            "user" => DurableEntryRole::User,
            "assistant" => DurableEntryRole::Assistant,
            "tool" => DurableEntryRole::Tool,
            _ => return Err("unsupported durable conversation role"),
        };
        Ok(Self {
            entry_id,
            role,
            content: message.content,
            parent_entry_id,
            operation_id,
            tool_call_id: message.tool_call_id,
            tool_calls: message.tool_calls,
            content_blocks: message.content_blocks,
        })
    }
}

/// Restore a complete transcript without executing any of its tools. Every tool
/// result must close one call in the immediately preceding assistant batch.
pub fn provider_messages_from_entries<'a>(
    entries: impl IntoIterator<Item = &'a DurableEntry>,
) -> Result<Vec<ProviderMessage>, &'static str> {
    let (messages, pending) = decode_entries(entries)?;
    if !pending.is_empty() {
        return Err("unfinished durable tool-call metadata at end of transcript");
    }
    Ok(messages)
}

/// Restore provider messages from a complete durable record stream, applying
/// model-facing tool projections recorded in `tool.presentation.v1` facts.
/// Raw tool entries remain authoritative on disk; a projection only changes
/// the in-memory content sent on resume and is validated against its linked
/// entry, call ID, and tool name.
pub fn provider_messages_from_records<'a>(
    records: impl IntoIterator<Item = &'a DurableRecord>,
) -> Result<Vec<ProviderMessage>, &'static str> {
    let records = records.into_iter().collect::<Vec<_>>();
    let mut projections = BTreeMap::new();
    for record in &records {
        let DurableRecord::Fact { fact, .. } = record else {
            continue;
        };
        if fact.namespace != "tool.presentation.v1" {
            continue;
        }
        if fact.key.trim().is_empty() || projections.contains_key(&fact.key) {
            return Err("invalid or duplicate durable tool presentation fact");
        }
        let value = fact
            .value
            .as_object()
            .ok_or("durable tool presentation fact is not an object")?;
        let name = value
            .get("name")
            .and_then(serde_json::Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .ok_or("durable tool presentation fact has no name")?;
        let call_id = value
            .get("call_id")
            .and_then(serde_json::Value::as_str)
            .filter(|call_id| !call_id.trim().is_empty())
            .ok_or("durable tool presentation fact has no call ID")?;
        let output = value
            .get("output")
            .and_then(serde_json::Value::as_str)
            .ok_or("durable tool presentation fact has no output")?;
        projections.insert(
            fact.key.clone(),
            ToolPresentationFact {
                name: name.to_owned(),
                call_id: call_id.to_owned(),
                output: output.to_owned(),
            },
        );
    }

    let mut entries = Vec::new();
    let mut entry_ids = BTreeSet::new();
    for record in &records {
        let DurableRecord::Entry { entry, .. } = record else {
            continue;
        };
        let mut entry = (*entry).clone();
        entry_ids.insert(entry.entry_id.clone());
        if let Some(projection) = projections.get(&entry.entry_id) {
            if !matches!(entry.role, DurableEntryRole::Tool) {
                return Err("durable tool presentation fact targets a non-tool entry");
            }
            if entry.tool_call_id.as_deref() != Some(projection.call_id.as_str()) {
                return Err("durable tool presentation call ID does not match entry");
            }
            entry.content.clone_from(&projection.output);
        }
        entries.push(entry);
    }
    if projections.keys().any(|key| !entry_ids.contains(key)) {
        return Err("durable tool presentation fact has no matching entry");
    }

    let messages = provider_messages_from_entries(&entries)?;
    for (entry, message) in entries.iter().zip(messages.iter()) {
        let Some(projection) = projections.get(&entry.entry_id) else {
            continue;
        };
        if message.name.as_deref() != Some(projection.name.as_str()) {
            return Err("durable tool presentation name does not match call");
        }
        if message.tool_call_id.as_deref() != Some(projection.call_id.as_str()) {
            return Err("durable tool presentation call ID does not match call");
        }
    }
    Ok(messages)
}

#[derive(Clone, Debug)]
struct ToolPresentationFact {
    name: String,
    call_id: String,
    output: String,
}

type PendingCalls<'a> = BTreeMap<&'a str, (&'a str, &'a str)>;

/// Only for an explicit recovery decision. These entries describe missing
/// evidence, never successful execution or permission to replay a call.
pub fn recovery_tool_results<'a>(
    entries: impl IntoIterator<Item = &'a DurableEntry>,
) -> Result<Vec<DurableEntry>, &'static str> {
    let (_, pending) = decode_entries(entries)?;
    pending.into_iter().map(|(id, (name, operation_id))| {
        DurableEntry::from_provider_message(
            String::new(), None, operation_id.to_owned(),
            ProviderMessage::tool(name, id,
                "[Recovery: unconfirmed result] This call has no durable result. It may have executed before interruption; its effects are unknown. Inspect prior effects and do not replay automatically."),
        )
    }).collect()
}

fn decode_entries<'a>(
    entries: impl IntoIterator<Item = &'a DurableEntry>,
) -> Result<(Vec<ProviderMessage>, PendingCalls<'a>), &'static str> {
    let mut messages = Vec::new();
    let mut pending = BTreeMap::new();
    for entry in entries {
        let mut message = match entry.role {
            DurableEntryRole::User => {
                if !pending.is_empty()
                    || entry.tool_call_id.is_some()
                    || !entry.tool_calls.is_empty()
                {
                    return Err(
                        "invalid or unfinished durable tool-call metadata before user entry",
                    );
                }
                ProviderMessage::user(entry.content.clone())
            }
            DurableEntryRole::Assistant => {
                if !pending.is_empty() || entry.tool_call_id.is_some() {
                    return Err(
                        "invalid or unfinished durable tool-call metadata before assistant entry",
                    );
                }
                for call in &entry.tool_calls {
                    if call.id.trim().is_empty()
                        || call.name.trim().is_empty()
                        || pending
                            .insert(
                                call.id.as_str(),
                                (call.name.as_str(), entry.operation_id.as_str()),
                            )
                            .is_some()
                    {
                        return Err("invalid or duplicate durable tool-call metadata");
                    }
                }
                ProviderMessage::assistant(entry.content.clone(), entry.tool_calls.clone())
            }
            DurableEntryRole::Tool => {
                if !entry.tool_calls.is_empty() || !entry.content_blocks.is_empty() {
                    return Err("invalid durable tool-result metadata");
                }
                let id = entry
                    .tool_call_id
                    .as_deref()
                    .ok_or("durable tool result has no call ID")?;
                let (name, operation_id) = pending
                    .remove(id)
                    .ok_or("durable tool result has no matching call")?;
                if operation_id != entry.operation_id {
                    return Err("durable tool result crosses operation boundaries");
                }
                ProviderMessage::tool(name, id, entry.content.clone())
            }
        };
        for call in &message.tool_calls {
            validate_tool_call_arguments(call, messages.len())
                .map_err(|_| "invalid durable tool-call arguments: expected a valid JSON object")?;
        }
        message.content_blocks.clone_from(&entry.content_blocks);
        messages.push(message);
    }
    Ok((messages, pending))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ProviderContentBlock, ProviderToolCall};

    fn call(id: &str) -> ProviderToolCall {
        ProviderToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: r#"{"path":"file.txt"}"#.into(),
        }
    }

    fn entries(messages: Vec<ProviderMessage>) -> Vec<DurableEntry> {
        messages
            .into_iter()
            .enumerate()
            .map(|(index, message)| {
                DurableEntry::from_provider_message(
                    index.to_string(),
                    None,
                    "operation".into(),
                    message,
                )
                .unwrap()
            })
            .collect()
    }

    #[test]
    fn complete_parallel_batch_and_content_blocks_survive_jsonl_roundtrip() {
        let messages = vec![
            ProviderMessage::user("inspect")
                .with_content_blocks(vec![ProviderContentBlock::image("image/png", "AA==")]),
            ProviderMessage::assistant("checking", vec![call("a"), call("b")]),
            ProviderMessage::tool("read", "b", "error: missing file"),
            ProviderMessage::tool("read", "a", "source contents\r\n"),
            ProviderMessage::assistant("finished", Vec::new()),
        ];
        let serialized = serde_json::to_string(&entries(messages.clone())).unwrap();
        let restored: Vec<DurableEntry> = serde_json::from_str(&serialized).unwrap();
        assert_eq!(provider_messages_from_entries(&restored).unwrap(), messages);
    }

    #[test]
    fn orphan_duplicate_unfinished_and_cross_operation_tools_are_rejected() {
        let cases = [
            vec![ProviderMessage::tool("read", "a", "orphan")],
            vec![ProviderMessage::assistant("", vec![call("a"), call("a")])],
            vec![ProviderMessage::assistant("", vec![call("a")])],
            vec![
                ProviderMessage::assistant("", vec![call("a")]),
                ProviderMessage::user("new turn"),
            ],
            vec![
                ProviderMessage::assistant("", vec![call("a")]),
                ProviderMessage::tool("read", "a", "ok"),
                ProviderMessage::tool("read", "a", "duplicate"),
            ],
        ];
        for messages in cases {
            assert!(provider_messages_from_entries(&entries(messages)).is_err());
        }
        let mut crossed = entries(vec![
            ProviderMessage::assistant("", vec![call("a")]),
            ProviderMessage::tool("read", "a", "ok"),
        ]);
        crossed[1].operation_id = "other".into();
        assert!(provider_messages_from_entries(&crossed).is_err());
    }

    #[test]
    fn tool_arguments_must_be_valid_json_objects_before_reconstruction() {
        for (arguments, expected) in [
            (
                "{",
                "invalid durable tool-call arguments: expected a valid JSON object",
            ),
            (
                "null",
                "invalid durable tool-call arguments: expected a valid JSON object",
            ),
            (
                "[]",
                "invalid durable tool-call arguments: expected a valid JSON object",
            ),
            (
                r#""text""#,
                "invalid durable tool-call arguments: expected a valid JSON object",
            ),
        ] {
            let mut invalid = call("invalid-call");
            invalid.arguments = arguments.into();
            let error = provider_messages_from_entries(&entries(vec![ProviderMessage::assistant(
                "",
                vec![invalid],
            )]))
            .expect_err("invalid tool arguments must block reconstruction");
            assert_eq!(error, expected);
        }

        let expected = ProviderMessage::assistant(
            "",
            vec![ProviderToolCall {
                id: "unicode-call".into(),
                name: "read".into(),
                arguments: r#"{"path":"á😀"}"#.into(),
            }],
        );
        let messages = vec![
            expected,
            ProviderMessage::tool("read", "unicode-call", "ok"),
        ];
        assert_eq!(
            provider_messages_from_entries(&entries(messages.clone())).unwrap(),
            messages
        );
    }

    #[test]
    fn older_text_only_entries_keep_their_wire_format() {
        let old = r#"{"entry_id":"a","role":"assistant","content":"old answer","parent_entry_id":null,"operation_id":"op","tool_call_id":null}"#;
        let entry: DurableEntry = serde_json::from_str(old).unwrap();
        assert_eq!(
            provider_messages_from_entries([&entry]).unwrap(),
            vec![ProviderMessage::assistant("old answer", Vec::new())]
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&serde_json::to_string(&entry).unwrap())
                .unwrap(),
            serde_json::from_str::<serde_json::Value>(old).unwrap()
        );
    }
}
