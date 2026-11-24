//! TypeScript declarations generated from tool JSON Schemas for Code mode.
//!
//! The output is a compact, deterministic hint for the model, not a validator:
//! the tool still validates its own arguments. Anything the renderer does not
//! model (`$ref`, tuples, unusual keywords, oversized or overly deep schemas)
//! degrades to `unknown` instead of failing or growing without bound.
use serde_json::{Map, Value};

/// Nesting depth past which a schema renders as `unknown`.
const MAX_DEPTH: usize = 8;
/// Upper bound for any rendered type, including nested ones.
const MAX_TYPE_BYTES: usize = 4 * 1024;
/// Upper bound for one signature; past it the arguments render as `unknown`.
const MAX_SIGNATURE_BYTES: usize = 4 * 1024;
/// Upper bound for the JSDoc summary, in characters.
const MAX_SUMMARY_CHARS: usize = 160;

const UNKNOWN: &str = "unknown";

/// Compact, deterministic declaration of a namespace of async functions.
/// `definitions` use the Slim tool format (`{name, description, input_schema}`);
/// entries without a string `name` are skipped. Every function resolves to the
/// tool's textual output.
pub(super) fn declarations(namespace: &str, definitions: &[Value]) -> String {
    let mut out = format!("declare const {namespace}: {{\n");
    for definition in definitions {
        let Some(name) = definition.get("name").and_then(Value::as_str) else {
            continue;
        };
        let description = definition.get("description").and_then(Value::as_str);
        let schema = definition.get("input_schema").unwrap_or(&Value::Null);
        let returns = definition
            .get("return_type")
            .and_then(Value::as_str)
            .unwrap_or("string");
        for line in signature(name, description, schema, returns).lines() {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str("};");
    out
}

/// Signature of one function, `name(args: T): Promise<R>;`, preceded by a
/// single-line JSDoc with the first sentence of `description` when there is one.
pub(super) fn signature(
    name: &str,
    description: Option<&str>,
    input_schema: &Value,
    returns: &str,
) -> String {
    let mut prefix = String::new();
    if let Some(summary) = description.map(summary).filter(|text| !text.is_empty()) {
        prefix.push_str("/** ");
        prefix.push_str(&summary);
        prefix.push_str(" */\n");
    }
    prefix.push_str(&property_key(name));
    let param = if allows_omitted_argument(input_schema) {
        "args?"
    } else {
        "args"
    };
    let build = |args: &str| format!("{prefix}({param}: {args}): Promise<{returns}>;");
    let full = build(&ts_type(input_schema));
    if full.len() <= MAX_SIGNATURE_BYTES {
        full
    } else {
        build(UNKNOWN)
    }
}

fn allows_omitted_argument(schema: &Value) -> bool {
    let Some(object) = schema.as_object() else {
        return false;
    };
    let no_required = object
        .get("required")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty);
    let no_top_level_combinator = !object.contains_key("oneOf")
        && !object.contains_key("anyOf")
        && !object.contains_key("allOf");
    let is_object = object.get("type").and_then(Value::as_str) == Some("object")
        || object.contains_key("properties")
        || object.contains_key("additionalProperties");
    is_object && no_required && no_top_level_combinator
}

/// TypeScript type of a JSON Schema; depth and size limits degrade to `unknown`.
pub(super) fn ts_type(schema: &Value) -> String {
    render(schema, 0).text
}

/// A rendered type and whether it is a bare union or intersection, which needs
/// parentheses before `[]` or inside another operator.
struct Ty {
    text: String,
    compound: bool,
}

impl Ty {
    fn simple(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            compound: false,
        }
    }

    fn unknown() -> Self {
        Self::simple(UNKNOWN)
    }

    fn is_unknown(&self) -> bool {
        self.text == UNKNOWN
    }

    fn grouped(self) -> String {
        if self.compound {
            format!("({})", self.text)
        } else {
            self.text
        }
    }
}

fn render(schema: &Value, depth: usize) -> Ty {
    if depth > MAX_DEPTH {
        return Ty::unknown();
    }
    let ty = match schema {
        Value::Bool(false) => Ty::simple("never"),
        Value::Object(schema) => render_object_schema(schema, depth),
        _ => Ty::unknown(),
    };
    if ty.text.len() > MAX_TYPE_BYTES {
        Ty::unknown()
    } else {
        ty
    }
}

fn render_object_schema(schema: &Map<String, Value>, depth: usize) -> Ty {
    if let Some(value) = schema.get("const") {
        return literal(value).map_or_else(Ty::unknown, Ty::simple);
    }
    if let Some(values) = schema.get("enum") {
        let Some(values) = values.as_array() else {
            return Ty::unknown();
        };
        let mut members = Vec::with_capacity(values.len());
        for value in values {
            let Some(text) = literal(value) else {
                return Ty::unknown();
            };
            members.push(Ty::simple(text));
        }
        return union(members);
    }
    match schema.get("type") {
        Some(Value::String(kind)) => return render_kind(kind, schema, depth),
        Some(Value::Array(kinds)) => {
            let mut members = Vec::with_capacity(kinds.len());
            for kind in kinds {
                let Some(kind) = kind.as_str() else {
                    return Ty::unknown();
                };
                members.push(render_kind(kind, schema, depth));
            }
            return union(members);
        }
        Some(_) => return Ty::unknown(),
        None => {}
    }
    if schema.contains_key("properties") || schema.contains_key("additionalProperties") {
        return render_kind("object", schema, depth);
    }
    if schema.contains_key("items") {
        return render_kind("array", schema, depth);
    }
    // Combinators define the type only when nothing above did; beside a `type`
    // or `properties` they are validation constraints (for example alternative
    // `required` sets) and the base shape already describes the arguments.
    for keyword in ["anyOf", "oneOf"] {
        if let Some(branches) = schema.get(keyword) {
            let Some(branches) = branches.as_array() else {
                return Ty::unknown();
            };
            let members = branches
                .iter()
                .map(|branch| render(branch, depth + 1))
                .collect();
            return union(members);
        }
    }
    if let Some(branches) = schema.get("allOf") {
        let Some(branches) = branches.as_array() else {
            return Ty::unknown();
        };
        let members = branches
            .iter()
            .map(|branch| render(branch, depth + 1))
            .collect();
        return intersection(members);
    }
    Ty::unknown()
}

fn render_kind(kind: &str, schema: &Map<String, Value>, depth: usize) -> Ty {
    match kind {
        "string" => Ty::simple("string"),
        "number" | "integer" => Ty::simple("number"),
        "boolean" => Ty::simple("boolean"),
        "null" => Ty::simple("null"),
        "array" => match schema.get("items") {
            Some(items @ (Value::Object(_) | Value::Bool(_))) => {
                Ty::simple(format!("{}[]", render(items, depth + 1).grouped()))
            }
            _ => Ty::simple("unknown[]"),
        },
        "object" => render_object(schema, depth),
        _ => Ty::unknown(),
    }
}

fn render_object(schema: &Map<String, Value>, depth: usize) -> Ty {
    let properties = schema.get("properties").and_then(Value::as_object);
    let required = schema.get("required").and_then(Value::as_array);
    let is_required = |name: &str| {
        required.is_some_and(|names| names.iter().any(|item| item.as_str() == Some(name)))
    };
    let index = match schema.get("additionalProperties") {
        Some(Value::Bool(false)) => None,
        Some(extra @ Value::Object(_)) => Some(render(extra, depth + 1).text),
        _ => Some(UNKNOWN.to_owned()),
    };
    let Some(properties) = properties.filter(|properties| !properties.is_empty()) else {
        return Ty::simple(match index {
            None => "{}".to_owned(),
            Some(value) => format!("Record<string, {value}>"),
        });
    };
    let mut fields = Vec::with_capacity(properties.len() + 1);
    let mut size = 0;
    for (name, property) in properties {
        let optional = if is_required(name) { "" } else { "?" };
        let field = format!(
            "{}{optional}: {}",
            property_key(name),
            render(property, depth + 1).text
        );
        size += field.len() + 2;
        if size > MAX_TYPE_BYTES {
            return Ty::unknown();
        }
        fields.push(field);
    }
    // An open object keeps its declared fields and admits extra keys; when no
    // extra keys are described, the index signature stays implicit and short.
    if let Some(value) = index.filter(|_| schema.contains_key("additionalProperties")) {
        fields.push(format!("[key: string]: {value}"));
    }
    Ty::simple(format!("{{ {} }}", fields.join("; ")))
}

fn union(members: Vec<Ty>) -> Ty {
    combine(members, " | ", true)
}

fn intersection(members: Vec<Ty>) -> Ty {
    combine(members, " & ", false)
}

/// Joins distinct members in their schema order. `unknown` absorbs a union and
/// is the identity of an intersection; an empty union is `never`.
fn combine(members: Vec<Ty>, separator: &str, is_union: bool) -> Ty {
    let mut distinct: Vec<Ty> = Vec::with_capacity(members.len());
    for member in members {
        if member.is_unknown() {
            if is_union {
                return Ty::unknown();
            }
            continue;
        }
        if !distinct.iter().any(|seen| seen.text == member.text) {
            distinct.push(member);
        }
    }
    match distinct.len() {
        0 if is_union => Ty::simple("never"),
        0 => Ty::unknown(),
        1 => distinct.pop().unwrap_or_else(Ty::unknown),
        _ => Ty {
            text: distinct
                .into_iter()
                // `&` binds tighter than `|`: only intersections group members.
                .map(|member| {
                    if is_union {
                        member.text
                    } else {
                        member.grouped()
                    }
                })
                .collect::<Vec<_>>()
                .join(separator),
            compound: true,
        },
    }
}

/// A JSON scalar as a TypeScript literal type; strings use JSON escaping.
fn literal(value: &Value) -> Option<String> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            Some(value.to_string())
        }
        Value::Array(_) | Value::Object(_) => None,
    }
}

/// Property or method key: a bare ASCII identifier, otherwise a JSON string.
fn property_key(name: &str) -> String {
    if is_identifier(name) {
        name.to_owned()
    } else {
        Value::String(name.to_owned()).to_string()
    }
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_' || first == '$')
        && chars.all(|next| next.is_ascii_alphanumeric() || next == '_' || next == '$')
}

/// First sentence of a description on one line, bounded, and unable to close
/// the surrounding comment.
fn summary(description: &str) -> String {
    let flat = description.split_whitespace().collect::<Vec<_>>().join(" ");
    let sentence = flat
        .match_indices(". ")
        .next()
        .map_or(flat.as_str(), |(end, _)| &flat[..=end]);
    let safe = sentence.replace("*/", "* /");
    if safe.chars().count() <= MAX_SUMMARY_CHARS {
        return safe;
    }
    let mut cut: String = safe.chars().take(MAX_SUMMARY_CHARS - 1).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}
