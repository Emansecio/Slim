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
/// made only when the bytes it saves outweigh the invalidated tail multiplied
/// by `write_penalty` (1 for uncached adapters; higher when cache writes cost
/// multiples of cache reads).
#[cfg(test)]
pub(super) fn elide_superseded_tool_outputs_if_it_pays(
    messages: &mut [ProviderMessage],
) -> ElisionStats {
    elide_superseded_tool_outputs_with_penalty(messages, 1)
}

pub(super) fn elide_superseded_tool_outputs_with_penalty(
    messages: &mut [ProviderMessage],
    write_penalty: usize,
) -> ElisionStats {
    let plan = plan_elisions(messages);
    if plan.is_empty() {
        return ElisionStats::default();
    }
    debug_assert!(plan.windows(2).all(|w| w[0].index < w[1].index));
    // Precompute wire bytes from the tail so every suffix `plan[k..]` can be
    // evaluated in O(1), eliding the largest profitable suffix without
    // invalidating a huge cached tail because of one older entry.
    let mut suffix_wire_bytes = vec![0_usize; messages.len() + 1];
    for index in (0..messages.len()).rev() {
        let message = &messages[index];
        let msg_bytes = message.content.len()
            + message
                .content_blocks
                .iter()
                .map(content_block_bytes)
                .sum::<usize>()
            + message
                .tool_calls
                .iter()
                .map(|call| super::mode::wire_argument_bytes(messages, index, call))
                .sum::<usize>();
        suffix_wire_bytes[index] = suffix_wire_bytes[index + 1].saturating_add(msg_bytes);
    }
    let mut remaining_saved: usize = plan
        .iter()
        .map(|planned| messages[planned.index].content.len() - planned.pointer.len())
        .sum();
    let penalty = write_penalty.max(1);
    let chosen_start = plan.iter().enumerate().find_map(|(k, planned)| {
        let invalidated = suffix_wire_bytes[planned.index + 1];
        if remaining_saved >= invalidated.saturating_mul(penalty) {
            Some(k)
        } else {
            remaining_saved -= messages[planned.index].content.len() - planned.pointer.len();
            None
        }
    });
    let Some(start) = chosen_start else {
        return ElisionStats::default();
    };
    apply_elisions(messages, plan.into_iter().skip(start).collect())
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

/// Whether an output looks like a failure/recovery notice rather than fresh
/// superseding evidence.
fn is_error_like_tool_output(output: &str) -> bool {
    let trimmed = output.trim_start();
    let Some(first_line) = trimmed.lines().next() else {
        return true;
    };
    let lower = first_line.to_ascii_lowercase();
    lower.starts_with("error:")
        || lower.starts_with("failed")
        || lower.starts_with("invalid ")
        || lower.starts_with("[no output]")
        || lower.contains("does not exist")
        || lower.contains("is a directory")
        || lower.contains("not allowed")
        || lower.contains("timed out")
        || lower.contains("cancelled")
        || lower.contains("not available")
        || lower.contains("permission denied")
}

fn is_exact_superseding_tool(name: &str) -> bool {
    matches!(name, "read" | "list" | "search" | "code_intel")
}

/// The rewrites [`elide_superseded_tool_outputs`] makes, in history order.
fn plan_elisions(messages: &[ProviderMessage]) -> Vec<PlannedElision> {
    // Call ids are only unique within one assistant turn (providers recycle
    // short ids), so a tool message resolves against the assistant before it.
    let mut call_paths = vec![None::<(String, String)>; messages.len()];
    let mut exact_calls = vec![None::<(&str, &str)>; messages.len()];
    // Per id, the calls of the turn in order: a provider that repeats an id
    // inside one turn answers them in the same order.
    type TurnCallEntry<'a> = (Option<(String, String)>, Option<(&'a str, &'a str)>);
    let mut turn_calls = std::collections::HashMap::<
        &str,
        std::collections::VecDeque<TurnCallEntry<'_>>,
    >::new();
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
                    let exact = is_exact_superseding_tool(&call.name)
                        .then_some((call.name.as_str(), call.arguments.as_str()));
                    turn_calls
                        .entry(call.id.as_str())
                        .or_default()
                        .push_back((key, exact));
                }
            }
            "tool" => {
                if let Some((path_key, exact_key)) = message
                    .tool_call_id
                    .as_deref()
                    .and_then(|call_id| turn_calls.get_mut(call_id))
                    .and_then(|calls| {
                        if calls.len() > 1 {
                            calls.pop_front()
                        } else {
                            calls.front().cloned()
                        }
                    })
                {
                    call_paths[index] = path_key;
                    exact_calls[index] = exact_key;
                }
            }
            _ => {}
        }
    }
    let mut latest_write = std::collections::HashMap::<String, usize>::new();
    let mut latest_mutation = std::collections::HashMap::<String, usize>::new();
    let mut latest_shell = std::collections::HashMap::<String, usize>::new();
    // The latest result of a command that is a pointer to an earlier run.
    let mut shell_pointer = std::collections::HashMap::<String, usize>::new();
    let mut latest_list = std::collections::HashMap::<String, usize>::new();
    let mut list_pointer = std::collections::HashMap::<String, usize>::new();
    let mut latest_exact = std::collections::HashMap::<(&str, &str), usize>::new();
    let mut exact_pointer = std::collections::HashMap::<(&str, &str), usize>::new();
    for (index, message) in messages.iter().enumerate() {
        if message.role != "tool" || !message.content_blocks.is_empty() {
            continue;
        }
        if let Some((name, args)) = exact_calls[index] {
            if message.name.as_deref() == Some(name) {
                if message.content.starts_with(DUPLICATE_POINTER_PREFIX) {
                    exact_pointer.insert((name, args), index);
                } else if !message.content.is_empty()
                    && !message.content.starts_with(SUPERSEDED_OUTPUT_PREFIX)
                    && !message.content.starts_with(PRUNED_OLD_OUTPUT_PREFIX)
                    && !write_output_is_recovery(&message.content)
                    && !is_error_like_tool_output(&message.content)
                {
                    latest_exact.insert((name, args), index);
                }
            }
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
            Some("list") => {
                if message.content.starts_with(DUPLICATE_POINTER_PREFIX) {
                    list_pointer.insert(key.clone(), index);
                } else if !message.content.is_empty()
                    && !message.content.starts_with(SUPERSEDED_OUTPUT_PREFIX)
                    && !message.content.starts_with(PRUNED_OLD_OUTPUT_PREFIX)
                    && !is_error_like_tool_output(&message.content)
                {
                    latest_list.insert(key.clone(), index);
                }
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
    if latest_write.is_empty()
        && latest_mutation.is_empty()
        && latest_shell.is_empty()
        && latest_list.is_empty()
        && latest_exact.is_empty()
    {
        return Vec::new();
    }
    let mut plan = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        if message.role != "tool"
            || !message.content_blocks.is_empty()
            || message.content.starts_with(DUPLICATE_POINTER_PREFIX)
            || message.content.starts_with(SUPERSEDED_OUTPUT_PREFIX)
            || message.content.starts_with(PRUNED_OLD_OUTPUT_PREFIX)
        {
            continue;
        }
        let name = message.name.as_deref().unwrap_or("");
        let pointer = match (name, &call_paths[index]) {
            ("read", Some((key, raw)))
                if latest_write.get(key).is_some_and(|&later| later > index) =>
            {
                format!(
                    "[superseded read output elided; {raw} was overwritten by a later write]",
                    raw = raw.as_str()
                )
            }
            ("patch", Some((key, raw)))
                if message.content.starts_with("patched ")
                    && message.content.len() > 256
                    && latest_write.get(key).is_some_and(|&later| later > index) =>
            {
                format!(
                    "[superseded patch output elided; {raw} was overwritten by a later write]",
                    raw = raw.as_str()
                )
            }
            ("write" | "patch", Some((key, raw)))
                if write_output_is_recovery(&message.content)
                    && latest_mutation.get(key).is_some_and(|&later| later > index) =>
            {
                format!(
                    "[superseded {name} failure output elided; {raw} was updated by a later mutation]",
                    raw = raw.as_str()
                )
            }
            ("list", Some((key, raw)))
                if latest_list.get(key).is_some_and(|&later| later > index)
                    && !list_pointer.get(key).is_some_and(|&later| later > index) =>
            {
                format!(
                    "[superseded list output elided; {raw} was listed again later]",
                    raw = raw.as_str()
                )
            }
            // The same command ran again later: only the latest run describes
            // the workspace. The status stays in the pointer as a record of the
            // attempt, behind the prefix every consumer of pointers knows.
            ("shell", Some((key, _)))
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
            _ => match exact_calls[index] {
                Some((exact_name, exact_args))
                    if exact_name == name
                        && latest_exact
                            .get(&(exact_name, exact_args))
                            .is_some_and(|&later| later > index)
                        && !exact_pointer
                            .get(&(exact_name, exact_args))
                            .is_some_and(|&later| later > index) =>
                {
                    format!("[superseded {name} output elided; the same call ran again later]")
                }
                _ => continue,
            },
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

/// The tool-result sizes a history that still exceeds the window after
/// compaction is cut to, one step at a time.
const FIT_TOOL_RESULT_BYTES: [usize; 4] = [16 * 1024, 4 * 1024, 1024, 256];

/// One step towards a history that fits the context window: first the
/// reasoning of every assistant message but the last (whose opaque state a
/// live continuation needs), then tool results cut to ever smaller sizes.
/// `step` advances past steps that change nothing; `None` once none is left.
/// The journal keeps each original (`recorded_content`).
pub(super) fn shrink_history_to_fit(
    messages: &mut [ProviderMessage],
    step: &mut usize,
) -> Option<ElisionStats> {
    while *step <= FIT_TOOL_RESULT_BYTES.len() {
        let current = *step;
        *step += 1;
        let stats = if current == 0 {
            drop_earlier_reasoning(messages)
        } else {
            cap_tool_results(messages, FIT_TOOL_RESULT_BYTES[current - 1])
        };
        if stats.elided > 0 {
            return Some(stats);
        }
    }
    None
}

pub(super) fn drop_earlier_reasoning(messages: &mut [ProviderMessage]) -> ElisionStats {
    let last_assistant = messages
        .iter()
        .rposition(|message| message.role == "assistant");
    let mut stats = ElisionStats::default();
    for (index, message) in messages.iter_mut().enumerate() {
        if message.role != "assistant" || Some(index) == last_assistant {
            continue;
        }
        let bytes = message
            .responses_reasoning
            .iter()
            .map(|reasoning| reasoning.item.to_string().len())
            .sum::<usize>()
            + message.chat_reasoning.as_ref().map_or(0, |reasoning| {
                reasoning.content.len()
                    + reasoning
                        .details
                        .iter()
                        .map(|detail| detail.to_string().len())
                        .sum::<usize>()
            });
        if bytes == 0 {
            continue;
        }
        message.responses_reasoning.clear();
        message.chat_reasoning = None;
        stats.elided += 1;
        stats.original_bytes += bytes as u64;
    }
    stats
}

fn cap_tool_results(messages: &mut [ProviderMessage], max_bytes: usize) -> ElisionStats {
    let mut stats = ElisionStats::default();
    for message in messages
        .iter_mut()
        .filter(|message| message.role == "tool" && message.content.len() > max_bytes)
    {
        // Preserve the receipt line of a successful `write` (`written ...;
        // bytes=...; sha256=...; exists=true;`) so `mode::project_call` never
        // re-expands a >4 KiB `write` argument on the wire during overflow.
        let cut = if message.name.as_deref() == Some("write")
            && message.content.starts_with("written ")
        {
            let first_line = message.content.lines().next().unwrap_or("");
            if first_line.contains("; sha256=") && first_line.contains("; exists=true;") {
                let kept_line = first_line.to_owned();
                if kept_line.len() < message.content.len() {
                    kept_line
                } else {
                    continue;
                }
            } else {
                truncate_result(&message.content, max_bytes)
            }
        } else {
            truncate_result(&message.content, max_bytes)
        };
        stats.elided += 1;
        stats.original_bytes += message.content.len() as u64;
        stats.emitted_bytes += cut.len() as u64;
        message
            .recorded_content
            .get_or_insert_with(|| std::sync::Arc::from(message.content.as_str()));
        message.content = cut;
    }
    stats
}

pub(super) fn write_output_is_recovery(output: &str) -> bool {
    output.contains("Current file is below")
        || output.contains("Current file edges are below")
        || output.contains("Suggested unique expected:")
        || output.contains("Closest text is at line")
        || output.contains("Nearest text is around line")
        || output.contains("Example context only for the first match at line ")
}

const MICRO_PRUNE_MIN_BYTES: usize = 4_096;
const MICRO_PRUNE_COMPACT_MIN_BYTES: usize = 2_048;
const PRUNED_OLD_OUTPUT_PREFIX: &str = "[pruned old ";
const SUPERSEDED_OUTPUT_PREFIX: &str = "[superseded ";

fn is_micro_prunable_read_tool(message: &ProviderMessage) -> bool {
    if message.role != "tool"
        || message.content.starts_with(DUPLICATE_POINTER_PREFIX)
        || message.content.starts_with(SUPERSEDED_OUTPUT_PREFIX)
        || message.content.starts_with(PRUNED_OLD_OUTPUT_PREFIX)
    {
        return false;
    }
    let name = message.name.as_deref();
    if matches!(name, Some("search" | "list" | "ls" | "subagent")) {
        return message.content.len() > MICRO_PRUNE_COMPACT_MIN_BYTES;
    }
    message.content.len() > MICRO_PRUNE_MIN_BYTES
        && (matches!(
            name,
            Some("read" | "grep" | "glob" | "find" | "code_intel" | "artifact_read" | "codemode" | "batch_code")
        ) || (name == Some("shell") && shell_completed_status(&message.content).is_some()))
}

fn pruned_old_output_note(tool_name: &str, content: &str) -> String {
    let original_bytes = content.len();
    if tool_name == "shell" {
        if let Some(status) = shell_completed_status(content) {
            return format!(
                "{PRUNED_OLD_OUTPUT_PREFIX}shell output ({original_bytes} bytes; {status}); call history_recall if needed]"
            );
        }
    }
    if tool_name == "subagent" {
        return format!(
            "{PRUNED_OLD_OUTPUT_PREFIX}subagent output ({original_bytes} bytes; synthesized in assistant response); call history_recall if needed]"
        );
    }
    format!(
        "{PRUNED_OLD_OUTPUT_PREFIX}{tool_name} output ({original_bytes} bytes); call history_recall or re-read if needed]"
    )
}

fn message_reasoning_bytes(message: &ProviderMessage) -> usize {
    message
        .responses_reasoning
        .iter()
        .map(|reasoning| reasoning.item.to_string().len())
        .sum::<usize>()
        + message.chat_reasoning.as_ref().map_or(0, |reasoning| {
            reasoning.content.len()
                + reasoning
                    .details
                    .iter()
                    .map(|detail| detail.to_string().len())
                    .sum::<usize>()
        })
}

/// Estimates how many characters of old read/search/shell tool results and
/// earlier assistant reasoning strictly before `cut_index` can be deterministically
/// collapsed before running an LLM compaction.
pub(super) fn estimate_micro_prune_savings_chars(
    messages: &[ProviderMessage],
    cut_index: usize,
) -> u64 {
    let limit = cut_index.min(messages.len());
    let last_assistant = messages.iter().rposition(|message| message.role == "assistant");
    let tool_savings: u64 = messages
        .iter()
        .take(limit)
        .filter(|message| is_micro_prunable_read_tool(message))
        .map(|message| {
            let name = message.name.as_deref().unwrap_or("tool");
            let note_len = pruned_old_output_note(name, &message.content).len();
            message.content.len().saturating_sub(note_len) as u64
        })
        .sum();
    let reasoning_savings: u64 = messages
        .iter()
        .enumerate()
        .take(limit)
        .filter(|(idx, msg)| msg.role == "assistant" && Some(*idx) != last_assistant)
        .map(|(_, msg)| message_reasoning_bytes(msg) as u64)
        .sum();
    tool_savings.saturating_add(reasoning_savings)
}

/// Deterministically collapses large (>4 KiB) read/search/completed-shell tool
/// outputs and stale earlier assistant reasoning strictly before `cut_index`
/// while preserving `recorded_content` so the durable journal fingerprint stays valid.
pub(super) fn micro_prune_before_cut(
    messages: &mut [ProviderMessage],
    cut_index: usize,
) -> ElisionStats {
    let mut stats = ElisionStats::default();
    let limit = cut_index.min(messages.len());
    let last_assistant = messages.iter().rposition(|message| message.role == "assistant");
    for (idx, message) in messages.iter_mut().take(limit).enumerate() {
        if message.role == "assistant" && Some(idx) != last_assistant {
            let bytes = message_reasoning_bytes(message);
            if bytes > 0 {
                message.responses_reasoning.clear();
                message.chat_reasoning = None;
                stats.elided += 1;
                stats.original_bytes += bytes as u64;
            }
            continue;
        }
        if !is_micro_prunable_read_tool(message) {
            continue;
        }
        let name = message.name.as_deref().unwrap_or("tool").to_owned();
        let original_len = message.content.len();
        let note = pruned_old_output_note(&name, &message.content);
        if note.len() >= original_len {
            continue;
        }
        stats.elided += 1;
        stats.original_bytes += original_len as u64;
        stats.emitted_bytes += note.len() as u64;
        message
            .recorded_content
            .get_or_insert_with(|| std::sync::Arc::from(message.content.as_str()));
        message.content = note;
    }
    stats
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

    #[test]
    fn a_history_is_shrunk_reasoning_first_and_keeps_the_last_answers_state() {
        let reasoning = || {
            Some(crate::provider::ChatReasoning {
                scope_id: 1,
                model: "m".into(),
                provenance: 0,
                content: "thinking".into(),
                details: Vec::new(),
            })
        };
        let mut earlier = ProviderMessage::assistant("earlier", Vec::new());
        earlier.chat_reasoning = reasoning();
        let mut last = ProviderMessage::assistant("last", Vec::new());
        last.chat_reasoning = reasoning();
        let big = "x".repeat(20 * 1024);
        let mut messages = vec![
            ProviderMessage::user("go"),
            earlier,
            ProviderMessage::tool("read", "call-1", big.clone()),
            last,
        ];
        let mut step = 0;

        let first = shrink_history_to_fit(&mut messages, &mut step).expect("reasoning");
        assert_eq!((first.elided, first.emitted_bytes), (1, 0));
        assert!(messages[1].chat_reasoning.is_none());
        assert!(messages[3].chat_reasoning.is_some());
        assert_eq!(messages[2].content, big);

        let second = shrink_history_to_fit(&mut messages, &mut step).expect("tool cut");
        assert_eq!(second.elided, 1);
        assert_eq!(messages[2].content.len(), 16 * 1024);
        assert_eq!(messages[2].recorded_content.as_deref(), Some(big.as_str()));

        while shrink_history_to_fit(&mut messages, &mut step).is_some() {}
        assert_eq!(messages[2].content.len(), 256);
        assert_eq!(messages[2].recorded_content.as_deref(), Some(big.as_str()));
        assert!(shrink_history_to_fit(&mut messages, &mut step).is_none());
    }

    #[test]
    fn micro_prune_before_cut_collapses_old_large_reads_and_keeps_recorded_content() {
        let large_read = "R".repeat(16 * 1024);
        let large_bash = "B".repeat(16 * 1024);
        let mut messages = vec![
            ProviderMessage::user("start"),
            ProviderMessage::tool("read", "t1", large_read.clone()),
            ProviderMessage::tool("shell", "t2", large_bash.clone()),
            ProviderMessage::user("kept turn"),
            ProviderMessage::tool("read", "t3", large_read.clone()),
        ];

        let estimated = estimate_micro_prune_savings_chars(&messages, 3);
        assert!(estimated > 15_000, "old large read must register substantial savings: {estimated}");

        let stats = micro_prune_before_cut(&mut messages, 3);
        assert_eq!(stats.elided, 1);
        assert!(messages[1].content.starts_with(PRUNED_OLD_OUTPUT_PREFIX));
        assert_eq!(messages[1].recorded_content.as_deref(), Some(large_read.as_str()));
        // Non-read tool before cut and read tool at/after cut are untouched.
        assert_eq!(messages[2].content, large_bash);
        assert_eq!(messages[4].content, large_read);
        // Second call is idempotent on already-pruned entries.
        assert_eq!(micro_prune_before_cut(&mut messages, 3).elided, 0);
    }

    #[test]
    fn suffix_optimal_elision_keeps_later_savings_when_early_tail_is_too_large() {
        let mut messages = vec![ProviderMessage::user("fix both")];
        // Early small superseded read followed by a huge live tail.
        messages.extend(exchange("r1", "read", "a.txt", "a".repeat(2_000)));
        messages.extend(exchange(
            "w1",
            "write",
            "a.txt",
            "written a.txt; bytes=1; sha256=abc; exists=true;".into(),
        ));
        messages.extend(exchange("r2", "read", "huge.txt", "h".repeat(30_000)));
        // Later large superseded read near the end (tiny tail after it).
        messages.extend(exchange("r3", "read", "b.txt", "b".repeat(12_000)));
        messages.extend(exchange(
            "w2",
            "write",
            "b.txt",
            "written b.txt; bytes=1; sha256=def; exists=true;".into(),
        ));

        let stats = elide_superseded_tool_outputs_with_penalty(&mut messages, 4);
        assert_eq!(stats.elided, 1);
        assert!(messages[2].content.starts_with("aaaa"));
        assert!(messages[8].content.starts_with("[superseded read output"));
    }

    #[test]
    fn repeated_exact_read_and_completed_shell_micro_prune_behave_cleanly() {
        let mut messages = vec![
            ProviderMessage::user("check"),
            ProviderMessage::assistant(
                "",
                vec![ProviderToolCall {
                    id: "s1".into(),
                    name: "search".into(),
                    arguments: r#"{"query":"needle"}"#.into(),
                }],
            ),
            ProviderMessage::tool("search", "s1", "old match\n".repeat(40)),
            ProviderMessage::assistant(
                "",
                vec![ProviderToolCall {
                    id: "s2".into(),
                    name: "search".into(),
                    arguments: r#"{"query":"needle"}"#.into(),
                }],
            ),
            ProviderMessage::tool("search", "s2", "new match\n".repeat(40)),
        ];
        let stats = elide_superseded_tool_outputs(&mut messages);
        assert_eq!(stats.elided, 1);
        assert!(messages[2]
            .content
            .starts_with("[superseded search output elided;"));

        let large_shell = format!("exit 0\n{}", "ok\n".repeat(3_000));
        let mut prune_msgs = vec![
            ProviderMessage::user("run"),
            ProviderMessage::tool("shell", "sh1", large_shell.clone()),
            ProviderMessage::user("recent"),
        ];
        let pruned = micro_prune_before_cut(&mut prune_msgs, 2);
        assert_eq!(pruned.elided, 1);
        assert!(prune_msgs[1].content.contains("exit 0"));
        assert_eq!(
            prune_msgs[1].recorded_content.as_deref(),
            Some(large_shell.as_str())
        );

        let mut reasoning_msgs = vec![
            ProviderMessage::user("first"),
            {
                let mut m = ProviderMessage::assistant("step 1", Vec::new());
                m.chat_reasoning = Some(crate::provider::ChatReasoning {
                    scope_id: 1,
                    model: "m".into(),
                    provenance: 0,
                    content: "old chain of thought".repeat(100),
                    details: Vec::new(),
                });
                m
            },
            ProviderMessage::user("second"),
            {
                let mut m = ProviderMessage::assistant("step 2", Vec::new());
                m.chat_reasoning = Some(crate::provider::ChatReasoning {
                    scope_id: 1,
                    model: "m".into(),
                    provenance: 0,
                    content: "latest reasoning".into(),
                    details: Vec::new(),
                });
                m
            },
        ];
        assert!(estimate_micro_prune_savings_chars(&reasoning_msgs, 4) > 0);
        let r_stats = micro_prune_before_cut(&mut reasoning_msgs, 4);
        assert_eq!(r_stats.elided, 1);
        assert!(reasoning_msgs[1].chat_reasoning.is_none());
        assert!(reasoning_msgs[3].chat_reasoning.is_some());
    }

    #[test]
    fn expanded_micro_prune_handles_patch_before_write_list_and_subagent() {
        let mut msgs = vec![
            ProviderMessage::user("start"),
            ProviderMessage::assistant(
                "",
                vec![ProviderToolCall {
                    id: "p1".into(),
                    name: "patch".into(),
                    arguments: r#"{"path":"a.rs","edits":[]}"#.into(),
                }],
            ),
            ProviderMessage::tool(
                "patch",
                "p1",
                format!("patched a.rs:1; {}", "detail ".repeat(60)),
            ),
            ProviderMessage::assistant(
                "",
                vec![ProviderToolCall {
                    id: "l1".into(),
                    name: "list".into(),
                    arguments: r#"{"path":"src","max_entries":50}"#.into(),
                }],
            ),
            ProviderMessage::tool("list", "l1", "old.rs\n".repeat(40)),
            ProviderMessage::assistant(
                "",
                vec![ProviderToolCall {
                    id: "w1".into(),
                    name: "write".into(),
                    arguments: r#"{"path":"a.rs","content":"new"}"#.into(),
                }],
            ),
            ProviderMessage::tool("write", "w1", "written a.rs; bytes=3; sha256=abc; exists=true;"),
            ProviderMessage::assistant(
                "",
                vec![ProviderToolCall {
                    id: "l2".into(),
                    name: "list".into(),
                    arguments: r#"{"path":"src","max_entries":100}"#.into(),
                }],
            ),
            ProviderMessage::tool("list", "l2", "new.rs\n".repeat(20)),
        ];
        let stats = elide_superseded_tool_outputs(&mut msgs);
        assert_eq!(stats.elided, 2);
        assert!(msgs[2].content.starts_with("[superseded patch output elided;"));
        assert!(msgs[4].content.starts_with("[superseded list output elided;"));

        let mut prune = vec![
            ProviderMessage::user("ask"),
            ProviderMessage::tool("search", "s1", "hit\n".repeat(700)),
            ProviderMessage::tool("subagent", "sa1", "report ".repeat(400)),
            ProviderMessage::assistant("synthesized summary", Vec::new()),
        ];
        let pruned = micro_prune_before_cut(&mut prune, 3);
        assert_eq!(pruned.elided, 2);
        assert!(prune[1].content.starts_with("[pruned old search output"));
        assert!(prune[2]
            .content
            .contains("synthesized in assistant response"));
    }
}
