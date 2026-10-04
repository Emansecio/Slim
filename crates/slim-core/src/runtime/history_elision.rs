use super::*;

pub(super) use crate::context::DUPLICATE_POINTER_PREFIX;

pub(super) fn duplicate_pointer(tool_name: &str) -> String {
    format!(
        "{DUPLICATE_POINTER_PREFIX}{tool_name} result omitted; identical output already in context]"
    )
}

pub(super) fn tool_output_already_in_context(
    messages: &[ProviderMessage],
    tool_name: &str,
    output: &str,
) -> bool {
    // Consult the actual retained history, including resumed turns. A summary
    // or another omission marker is not a replacement for the original result.
    !output.starts_with(DUPLICATE_POINTER_PREFIX)
        && messages.iter().rev().any(|message| {
            message.role == "tool"
                && message.name.as_deref() == Some(tool_name)
                && message.content_blocks.is_empty()
                && message.content == output
        })
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct ElisionStats {
    pub(super) elided: u32,
    pub(super) original_bytes: u64,
    pub(super) emitted_bytes: u64,
}

pub(super) fn tool_call_path(arguments: &str) -> Option<(String, String)> {
    let raw = serde_json::from_str::<PathOnly>(arguments).ok()?.0?;
    let key = crate::tools::path_identity(Path::new(&raw));
    (!key.is_empty()).then_some((key, raw))
}

/// The string `path` of a JSON object argument (last duplicate wins, any other
/// type is `None`), read without building a `Value` of the whole argument: a
/// historical `write` carries its full content.
struct PathOnly(Option<String>);

impl<'de> serde::Deserialize<'de> for PathOnly {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};

        struct IsPath(bool);
        impl<'de> Visitor<'de> for IsPath {
            type Value = Self;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("an object key")
            }
            fn visit_str<E: serde::de::Error>(self, key: &str) -> Result<Self, E> {
                Ok(Self(key == "path"))
            }
        }
        impl<'de> serde::Deserialize<'de> for IsPath {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                deserializer.deserialize_str(Self(false))
            }
        }

        struct StringOrOther(Option<String>);
        impl<'de> Visitor<'de> for StringOrOther {
            type Value = Self;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("any JSON value")
            }
            fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Self, E> {
                Ok(Self(Some(text.to_owned())))
            }
            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self, E> {
                Ok(Self(None))
            }
            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self, E> {
                Ok(Self(None))
            }
            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self, E> {
                Ok(Self(None))
            }
            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self, E> {
                Ok(Self(None))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self, E> {
                Ok(Self(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Self(None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self, A::Error> {
                while map.next_key::<IgnoredAny>()?.is_some() {
                    map.next_value::<IgnoredAny>()?;
                }
                Ok(Self(None))
            }
        }
        impl<'de> serde::Deserialize<'de> for StringOrOther {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                deserializer.deserialize_any(Self(None))
            }
        }

        struct Object;
        impl<'de> Visitor<'de> for Object {
            type Value = PathOnly;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<PathOnly, A::Error> {
                let mut path = None;
                while let Some(IsPath(is_path)) = map.next_key()? {
                    if is_path {
                        path = map.next_value::<StringOrOther>()?.0;
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(PathOnly(path))
            }
        }
        deserializer.deserialize_map(Object)
    }
}

/// Replace retained tool outputs whose evidence a later mutation of the same
/// path provably superseded. A `read` observed before a successful whole-file
/// `write` describes bytes that no longer exist; a failed write/patch recovery
/// body embeds the old file and is dead weight once any later mutation of that
/// path succeeded. Only live wire content is elided — durable entries keep the
/// full output — and the pointer never outlives its usefulness (reads after
/// the last write and the mutation's own success result are preserved).
pub(super) fn elide_superseded_tool_outputs(messages: &mut [ProviderMessage]) -> ElisionStats {
    let plan = plan_elisions(messages);
    apply_elisions(messages, plan)
}

/// [`elide_superseded_tool_outputs`] for a history the provider already caches.
/// Rewriting a message changes the request from there on, so the rewrite is
/// made only when the bytes it saves are at least those of the messages after
/// the first one it rewrites (what the cache would have to take in again).
/// Otherwise nothing changes now: the next run's seed pass elides it.
pub(super) fn elide_superseded_tool_outputs_if_it_pays(
    messages: &mut [ProviderMessage],
) -> ElisionStats {
    let plan = plan_elisions(messages);
    let Some(first) = plan.first().map(|planned| planned.index) else {
        return ElisionStats::default();
    };
    let saved: usize = plan
        .iter()
        .map(|planned| messages[planned.index].content.len() - planned.pointer.len())
        .sum();
    // What is sent after that message, as the provider view sends it: a
    // completed large `write` goes out as a short receipt, not its content.
    let invalidated: usize = messages
        .iter()
        .enumerate()
        .skip(first + 1)
        .map(|(index, message)| {
            message.content.len()
                + message
                    .content_blocks
                    .iter()
                    .map(content_block_bytes)
                    .sum::<usize>()
                + message
                    .tool_calls
                    .iter()
                    .map(|call| super::mode::wire_argument_bytes(messages, index, call))
                    .sum::<usize>()
        })
        .sum();
    if saved < invalidated {
        return ElisionStats::default();
    }
    apply_elisions(messages, plan)
}

fn content_block_bytes(block: &crate::provider::ProviderContentBlock) -> usize {
    use crate::provider::ProviderContentBlock as Block;
    match block {
        Block::Text(text) => text.len(),
        Block::Image { media_type, data }
        | Block::Audio { media_type, data }
        | Block::File { media_type, data } => media_type.len() + data.len(),
        Block::Unsupported { kind } => kind.len(),
    }
}

/// A tool message and the pointer that replaces its content.
struct PlannedElision {
    index: usize,
    pointer: String,
}

/// The rewrites [`elide_superseded_tool_outputs`] makes, in history order.
fn plan_elisions(messages: &[ProviderMessage]) -> Vec<PlannedElision> {
    // Call ids are only unique within one assistant turn (providers recycle
    // short ids), so a tool message resolves against the assistant before it.
    let mut call_paths = vec![None::<(String, String)>; messages.len()];
    let mut turn_calls = std::collections::HashMap::<&str, (String, String)>::new();
    for (index, message) in messages.iter().enumerate() {
        match message.role.as_str() {
            "assistant" => {
                turn_calls.clear();
                for call in &message.tool_calls {
                    // A shell call is keyed by its exact arguments; the NUL
                    // keeps that key apart from every path identity.
                    let key = if call.name == "shell" {
                        Some((format!("\0{}", call.arguments), String::new()))
                    } else {
                        tool_call_path(&call.arguments)
                    };
                    if let Some(key) = key {
                        turn_calls.insert(call.id.as_str(), key);
                    }
                }
            }
            "tool" => {
                call_paths[index] = message
                    .tool_call_id
                    .as_deref()
                    .and_then(|call_id| turn_calls.get(call_id).cloned());
            }
            _ => {}
        }
    }
    let mut latest_write = std::collections::HashMap::<String, usize>::new();
    let mut latest_mutation = std::collections::HashMap::<String, usize>::new();
    let mut latest_shell = std::collections::HashMap::<String, usize>::new();
    // The latest result of a command that is a pointer to an earlier run.
    let mut shell_pointer = std::collections::HashMap::<String, usize>::new();
    for (index, message) in messages.iter().enumerate() {
        if message.role != "tool" || !message.content_blocks.is_empty() {
            continue;
        }
        let Some((key, _)) = &call_paths[index] else {
            continue;
        };
        match message.name.as_deref() {
            Some("write") if message.content.starts_with("written ") => {
                latest_write.insert(key.clone(), index);
                latest_mutation.insert(key.clone(), index);
            }
            Some("patch") if message.content.starts_with("patched ") => {
                latest_mutation.insert(key.clone(), index);
            }
            // A duplicate pointer stands for one of the earlier runs: they stay.
            Some("shell") if message.content.starts_with(DUPLICATE_POINTER_PREFIX) => {
                shell_pointer.insert(key.clone(), index);
            }
            // Only a run that finished (not timed out or cancelled) supersedes.
            Some("shell") if shell_completed_status(&message.content).is_some() => {
                latest_shell.insert(key.clone(), index);
            }
            _ => {}
        }
    }
    if latest_write.is_empty() && latest_mutation.is_empty() && latest_shell.is_empty() {
        return Vec::new();
    }
    let mut plan = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        if message.role != "tool" || !message.content_blocks.is_empty() {
            continue;
        }
        let Some((key, raw)) = &call_paths[index] else {
            continue;
        };
        let name = message.name.as_deref().unwrap_or("");
        let pointer = match name {
            "read" if latest_write.get(key).is_some_and(|&later| later > index) => format!(
                "[superseded read output elided; {raw} was overwritten by a later write]",
                raw = raw.as_str()
            ),
            "write" | "patch"
                if write_output_is_recovery(&message.content)
                    && latest_mutation.get(key).is_some_and(|&later| later > index) =>
            {
                format!(
                    "[superseded {name} failure output elided; {raw} was updated by a later mutation]",
                    raw = raw.as_str()
                )
            }
            // The same command ran again later: only the latest run describes
            // the workspace. The status stays in the pointer as a record of the
            // attempt, behind the prefix every consumer of pointers knows.
            "shell"
                if latest_shell.get(key).is_some_and(|&later| later > index)
                    && !shell_pointer.get(key).is_some_and(|&later| later > index) =>
            {
                match shell_status(&message.content) {
                    Some(status) => format!(
                        "[superseded shell output elided; {status}; the same command ran again later]"
                    ),
                    None => continue,
                }
            }
            _ => continue,
        };
        if pointer.len() < message.content.len() {
            plan.push(PlannedElision { index, pointer });
        }
    }
    plan
}

fn apply_elisions(messages: &mut [ProviderMessage], plan: Vec<PlannedElision>) -> ElisionStats {
    let mut stats = ElisionStats::default();
    for PlannedElision { index, pointer } in plan {
        let message = &mut messages[index];
        stats.elided = stats.elided.saturating_add(1);
        stats.original_bytes = stats
            .original_bytes
            .saturating_add(u64::try_from(message.content.len()).unwrap_or(u64::MAX));
        stats.emitted_bytes = stats
            .emitted_bytes
            .saturating_add(u64::try_from(pointer.len()).unwrap_or(u64::MAX));
        message
            .recorded_content
            .get_or_insert_with(|| Arc::from(message.content.as_str()));
        message.content = pointer;
    }
    stats
}

/// The status of a shell run that finished with an exit code: not a timeout, a
/// cancellation or a run without one (`exit n/a`).
fn shell_completed_status(output: &str) -> Option<&str> {
    shell_status(output).filter(|status| {
        !status.contains('·')
            && status
                .strip_prefix("exit ")
                .is_some_and(|code| code.trim().parse::<i64>().is_ok())
    })
}

/// The status header of a completed shell result (`exit 0`, `exit 1 · timed out`).
fn shell_status(output: &str) -> Option<&str> {
    output
        .lines()
        .next()
        .filter(|line| line.starts_with("exit "))
}

pub(super) fn truncate_result(output: &str, max_bytes: usize) -> String {
    if output.len() <= max_bytes {
        return output.to_owned();
    }
    const MARKER: &str = "\n[truncated]";
    // The marker counts against the limit; below its size it is dropped.
    let (mut end, marker) = match max_bytes.checked_sub(MARKER.len()) {
        Some(budget) => (budget, MARKER),
        None => (max_bytes, ""),
    };
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{marker}", &output[..end])
}

pub(super) fn write_output_is_recovery(output: &str) -> bool {
    output.contains("Current file is below")
        || output.contains("Current file edges are below")
        || output.contains("Suggested unique expected:")
        || output.contains("Closest text is at line")
        || output.contains("Nearest text is around line")
        || output.contains("Example context only for the first match at line ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange(id: &str, name: &str, path: &str, output: String) -> [ProviderMessage; 2] {
        [
            ProviderMessage::assistant(
                "",
                vec![ProviderToolCall {
                    id: id.into(),
                    name: name.into(),
                    arguments: serde_json::json!({ "path": path }).to_string(),
                }],
            ),
            ProviderMessage::tool(name, id, output),
        ]
    }

    /// A read of `a.txt` that a later write supersedes, then `tail_bytes` of
    /// unrelated output.
    fn history(tail_bytes: usize) -> Vec<ProviderMessage> {
        let mut messages = vec![ProviderMessage::user("fix a")];
        messages.extend(exchange("r1", "read", "a.txt", "old ".repeat(1000)));
        messages.extend(exchange(
            "w1",
            "write",
            "a.txt",
            "written a.txt; bytes=5; sha256=abc; exists=true; do not re-read".into(),
        ));
        messages.extend(exchange("r2", "read", "b.txt", "b".repeat(tail_bytes)));
        messages
    }

    #[test]
    fn every_patch_recovery_lead_in_marks_the_output_as_recovery() {
        for lead_in in [
            "Current file is below:",
            "Current file edges are below:",
            "Suggested unique expected:",
            "Closest text is at line 3",
            "Nearest text is around line 3",
        ] {
            assert!(write_output_is_recovery(&format!("unchanged.\n{lead_in}")));
        }
        assert!(!write_output_is_recovery("patched a.txt; 1 edits applied"));
    }

    #[test]
    fn a_whole_file_rewrite_costs_its_receipt_not_its_content_when_elision_is_weighed() {
        use sha2::{Digest, Sha256};
        let old = "old line\n".repeat(2200);
        let new = "new line\n".repeat(2200);
        let hash = format!("{:x}", Sha256::digest(new.as_bytes()));
        let arguments = serde_json::json!({ "path": "a.txt", "content": new }).to_string();
        let mut messages = vec![ProviderMessage::user("rewrite a")];
        messages.extend(exchange("r1", "read", "a.txt", old.clone()));
        messages.push(ProviderMessage::assistant(
            "",
            vec![ProviderToolCall {
                id: "w1".into(),
                name: "write".into(),
                arguments: arguments.clone(),
            }],
        ));
        messages.push(ProviderMessage::tool(
            "write",
            "w1",
            format!(
                "written a.txt; bytes={}; sha256={}; exists=true; do not re-read",
                new.len(),
                &hash[..12]
            ),
        ));
        // Counted by the raw arguments the rewrite would look as costly as the
        // read it replaces; the provider is sent a receipt instead.
        assert!(arguments.len() > old.len());
        assert_eq!(
            elide_superseded_tool_outputs_if_it_pays(&mut messages).elided,
            1
        );
        assert!(messages[2].content.starts_with("[superseded read output"));
    }

    #[test]
    fn a_mid_run_elision_that_saves_more_than_it_invalidates_is_applied() {
        let mut messages = history(500);
        let stats = elide_superseded_tool_outputs_if_it_pays(&mut messages);
        assert_eq!(stats.elided, 1);
        assert!(messages[2].content.starts_with("[superseded read output"));
    }

    #[test]
    fn a_mid_run_elision_that_would_rewrite_more_cache_than_it_saves_is_deferred() {
        let mut messages = history(20_000);
        let original = messages.clone();
        assert_eq!(
            elide_superseded_tool_outputs_if_it_pays(&mut messages),
            ElisionStats::default()
        );
        assert_eq!(messages, original);
        // The unconditional pass (the seed of the next run) still elides it.
        assert_eq!(elide_superseded_tool_outputs(&mut messages).elided, 1);
    }
}
