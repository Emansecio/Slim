use super::*;

pub(super) fn validate_tool_arguments(name: &str, arguments: &str) -> Result<(), ProviderError> {
    if name.trim().is_empty() || !matches!(object_arguments(arguments).1, ArgumentShape::Object) {
        Err(ProviderError::MalformedToolCall)
    } else {
        Ok(())
    }
}

/// What the normalized arguments text turned out to be.
pub(super) enum ArgumentShape {
    Object,
    /// Valid JSON, but not an object.
    NotObject,
    Invalid(serde_json::Error),
}

impl ArgumentShape {
    pub(super) fn is_object(&self) -> bool {
        matches!(self, Self::Object)
    }

    /// Why the arguments are not an object, when they are not.
    pub(super) fn issue(&self) -> Option<String> {
        match self {
            Self::Object => None,
            Self::NotObject => Some("arguments must be a JSON object".into()),
            Self::Invalid(error) => Some(error.to_string()),
        }
    }
}

/// The arguments text the executor will parse (fence stripped, one string
/// layer unwrapped, invalid escapes repaired) and its shape. The shape is the
/// outcome of parsing that returned text, obtained without reparsing it.
pub(super) fn object_arguments(raw: &str) -> (Cow<'_, str>, ArgumentShape) {
    let trimmed = strip_json_fence(raw.trim());
    let shape = match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::Object(_)) => return (Cow::Borrowed(trimmed), ArgumentShape::Object),
        Ok(Value::String(inner)) => {
            if serde_json::from_str::<Value>(&inner).is_ok_and(|value| value.is_object()) {
                return (Cow::Owned(inner), ArgumentShape::Object);
            }
            if let Some(repaired) = repaired_json_object(&inner) {
                return (Cow::Owned(repaired), ArgumentShape::Object);
            }
            ArgumentShape::NotObject
        }
        Ok(_) => ArgumentShape::NotObject,
        Err(error) => ArgumentShape::Invalid(error),
    };
    match repaired_json_object(trimmed) {
        Some(repaired) => (Cow::Owned(repaired), ArgumentShape::Object),
        None => (Cow::Borrowed(trimmed), shape),
    }
}

/// A strict-parseable object is left to the caller; otherwise only invalid
/// string escapes are repaired and the result is kept when it now parses as an
/// object. This keeps every other malformed payload failing closed.
pub(super) fn repaired_json_object(candidate: &str) -> Option<String> {
    let repaired = repair_invalid_json_escapes(candidate)?;
    serde_json::from_str::<Value>(&repaired)
        .is_ok_and(|value| value.is_object())
        .then_some(repaired)
}

/// Doubles the backslash of escapes `serde_json` rejects inside strings: `\`
/// followed by a character that cannot start a valid escape, or `\u` without
/// four hex digits. Valid escapes and the surrounding structure are copied
/// verbatim, so valid JSON never changes.
pub(super) fn repair_invalid_json_escapes(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut repaired = String::with_capacity(raw.len() + 8);
    let mut in_string = false;
    let mut changed = false;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'"' {
            in_string = !in_string;
            repaired.push('"');
            index += 1;
            continue;
        }
        if in_string && byte == b'\\' {
            if let Some(&escape) = bytes.get(index + 1) {
                if matches!(
                    escape,
                    b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't'
                ) {
                    repaired.push('\\');
                    repaired.push(escape as char);
                    index += 2;
                    continue;
                }
                if escape == b'u'
                    && index + 6 <= bytes.len()
                    && bytes[index + 2..index + 6]
                        .iter()
                        .all(u8::is_ascii_hexdigit)
                {
                    repaired.push_str(&raw[index..index + 6]);
                    index += 6;
                    continue;
                }
            }
            repaired.push_str("\\\\");
            changed = true;
            index += 1;
            continue;
        }
        let character = raw[index..].chars().next().expect("char boundary");
        repaired.push(character);
        index += character.len_utf8();
    }
    changed.then_some(repaired)
}

pub(super) fn strip_json_fence(raw: &str) -> &str {
    let Some(rest) = raw.strip_prefix("```") else {
        return raw;
    };
    let rest = rest
        .strip_prefix("json")
        .or_else(|| rest.strip_prefix("JSON"))
        .unwrap_or(rest);
    let rest = rest.trim_start();
    rest.strip_suffix("```").map(str::trim_end).unwrap_or(raw)
}

pub(super) fn assign_missing_call_ids(calls: &mut [ProviderToolCall], batch_id: &str) {
    for (index, call) in calls.iter_mut().enumerate() {
        if call.id.is_empty() {
            call.id = format!("{batch_id}-tool-{index}");
        }
    }
}
