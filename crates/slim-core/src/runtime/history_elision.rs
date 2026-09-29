use super::*;

/// Prefix of the pointer that replaces a result already present in context.
pub(super) const DUPLICATE_POINTER_PREFIX: &str = "[duplicate ";

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
    // Call ids are only unique within one assistant turn (providers recycle
    // short ids), so a tool message resolves against the assistant before it.
    let mut call_paths = vec![None::<(String, String)>; messages.len()];
    let mut turn_calls = std::collections::HashMap::<&str, (String, String)>::new();
    for (index, message) in messages.iter().enumerate() {
        match message.role.as_str() {
            "assistant" => {
                turn_calls.clear();
                for call in &message.tool_calls {
                    if let Some(path) = tool_call_path(&call.arguments) {
                        turn_calls.insert(call.id.as_str(), path);
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
            _ => {}
        }
    }
    if latest_write.is_empty() && latest_mutation.is_empty() {
        return ElisionStats::default();
    }
    let mut stats = ElisionStats::default();
    for (index, message) in messages.iter_mut().enumerate() {
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
            _ => continue,
        };
        if pointer.len() < message.content.len() {
            stats.elided = stats.elided.saturating_add(1);
            stats.original_bytes = stats
                .original_bytes
                .saturating_add(u64::try_from(message.content.len()).unwrap_or(u64::MAX));
            stats.emitted_bytes = stats
                .emitted_bytes
                .saturating_add(u64::try_from(pointer.len()).unwrap_or(u64::MAX));
            message.content = pointer;
        }
    }
    stats
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
        || output.contains("Example context only for the first match at line ")
}
