//! Tests of the JSON Schema to TypeScript declarations.
use super::typescript::{declarations, signature, ts_type};
use serde_json::{json, Value};

#[test]
fn scalar_types_map_to_typescript_primitives() {
    assert_eq!(ts_type(&json!({"type": "string"})), "string");
    assert_eq!(ts_type(&json!({"type": "number"})), "number");
    assert_eq!(ts_type(&json!({"type": "integer"})), "number");
    assert_eq!(ts_type(&json!({"type": "boolean"})), "boolean");
    assert_eq!(ts_type(&json!({"type": "null"})), "null");
}

#[test]
fn unmodelled_schemas_degrade_to_unknown() {
    for schema in [
        json!({"$ref": "#/definitions/item"}),
        json!({}),
        json!(true),
        json!(null),
        json!("string"),
        json!({"type": "date"}),
        json!({"type": 7}),
        json!({"type": ["string", 7]}),
        json!({"enum": "a"}),
        json!({"enum": ["a", {"b": 1}]}),
        json!({"const": [1]}),
        json!({"anyOf": {"type": "string"}}),
    ] {
        assert_eq!(ts_type(&schema), "unknown", "{schema}");
    }
    assert_eq!(ts_type(&json!(false)), "never");
}

#[test]
fn arrays_render_items_and_group_compound_items() {
    assert_eq!(
        ts_type(&json!({"type": "array", "items": {"type": "string"}})),
        "string[]"
    );
    assert_eq!(ts_type(&json!({"type": "array"})), "unknown[]");
    assert_eq!(
        ts_type(&json!({"type": "array", "items": [{"type": "string"}]})),
        "unknown[]"
    );
    assert_eq!(
        ts_type(&json!({"items": {"type": "array", "items": {"type": "integer"}}})),
        "number[][]"
    );
    assert_eq!(
        ts_type(&json!({"type": "array", "items": {"type": ["string", "null"]}})),
        "(string | null)[]"
    );
}

#[test]
fn objects_mark_optional_fields_and_extra_keys() {
    let schema = json!({
        "type": "object",
        "properties": {"path": {"type": "string"}, "limit": {"type": "integer"}},
        "required": ["path"],
        "additionalProperties": false
    });
    assert_eq!(ts_type(&schema), "{ limit?: number; path: string }");
    assert_eq!(
        ts_type(&json!({"properties": {"a": {"type": "boolean"}}})),
        "{ a?: boolean }"
    );
    assert_eq!(
        ts_type(&json!({
            "type": "object",
            "properties": {"a": {"type": "string"}},
            "additionalProperties": true
        })),
        "{ a?: string; [key: string]: unknown }"
    );
    assert_eq!(
        ts_type(&json!({
            "type": "object",
            "properties": {"a": {"type": "string"}},
            "additionalProperties": {"type": "string"}
        })),
        "{ a?: string; [key: string]: string }"
    );
}

#[test]
fn objects_without_properties_become_records() {
    assert_eq!(
        ts_type(&json!({"type": "object"})),
        "Record<string, unknown>"
    );
    assert_eq!(
        ts_type(&json!({"type": "object", "properties": {}})),
        "Record<string, unknown>"
    );
    assert_eq!(
        ts_type(&json!({"type": "object", "additionalProperties": {"type": "number"}})),
        "Record<string, number>"
    );
    assert_eq!(
        ts_type(&json!({"type": "object", "additionalProperties": false})),
        "{}"
    );
}

#[test]
fn enum_and_const_become_escaped_literals() {
    assert_eq!(
        ts_type(&json!({"type": "string", "enum": ["a", "b\"c", "d\ne"]})),
        r#""a" | "b\"c" | "d\ne""#
    );
    assert_eq!(
        ts_type(&json!({"enum": [1, 2.5, true, null]})),
        "1 | 2.5 | true | null"
    );
    assert_eq!(ts_type(&json!({"enum": ["a", "a"]})), r#""a""#);
    assert_eq!(ts_type(&json!({"enum": []})), "never");
    assert_eq!(ts_type(&json!({"const": "x*/y"})), r#""x*/y""#);
    assert_eq!(ts_type(&json!({"const": false})), "false");
}

#[test]
fn type_arrays_and_combinators_become_unions() {
    assert_eq!(
        ts_type(&json!({"type": ["string", "null"]})),
        "string | null"
    );
    assert_eq!(ts_type(&json!({"type": ["integer", "number"]})), "number");
    assert_eq!(
        ts_type(&json!({"type": ["array", "null"], "items": {"type": "string"}})),
        "string[] | null"
    );
    assert_eq!(
        ts_type(&json!({"anyOf": [{"type": "string"}, {"type": "integer"}]})),
        "string | number"
    );
    assert_eq!(
        ts_type(&json!({"oneOf": [{"const": "a"}, {"type": "null"}]})),
        r#""a" | null"#
    );
    assert_eq!(
        ts_type(&json!({"anyOf": [{"type": "string"}, {"$ref": "#/x"}]})),
        "unknown"
    );
    assert_eq!(ts_type(&json!({"anyOf": []})), "never");
}

#[test]
fn combinators_beside_a_base_shape_are_constraints() {
    let schema = json!({
        "type": "object",
        "properties": {"query": {"type": "string"}, "patterns": {"type": "array", "items": {"type": "string"}}},
        "oneOf": [{"required": ["query"]}, {"required": ["patterns"]}]
    });
    assert_eq!(ts_type(&schema), "{ patterns?: string[]; query?: string }");
}

#[test]
fn all_of_becomes_an_intersection() {
    assert_eq!(
        ts_type(&json!({"allOf": [
            {"properties": {"a": {"type": "string"}}, "required": ["a"]},
            {"properties": {"b": {"type": "number"}}}
        ]})),
        "{ a: string } & { b?: number }"
    );
    assert_eq!(
        ts_type(&json!({"allOf": [{"type": ["string", "null"]}, {"$ref": "#/x"}]})),
        "string | null"
    );
    assert_eq!(
        ts_type(&json!({"allOf": [{"type": ["string", "null"]}, {"type": ["null", "number"]}]})),
        "(string | null) & (null | number)"
    );
    assert_eq!(ts_type(&json!({"allOf": [{}, {"$ref": "#/x"}]})), "unknown");
    assert_eq!(
        ts_type(
            &json!({"type": "array", "items": {"allOf": [{"type": "string"}, {"minLength": 1}]}})
        ),
        "string[]"
    );
}

#[test]
fn invalid_identifiers_are_quoted_with_json_escapes() {
    let schema = json!({
        "type": "object",
        "properties": {
            "ok_$1": {"type": "string"},
            "dash-name": {"type": "string"},
            "1st": {"type": "string"},
            "": {"type": "string"},
            "quote\"\nkey": {"type": "string"},
            "ação": {"type": "string"}
        },
        "required": ["dash-name"]
    });
    assert_eq!(
        ts_type(&schema),
        r#"{ ""?: string; "1st"?: string; "ação"?: string; "dash-name": string; ok_$1?: string; "quote\"\nkey"?: string }"#
    );
    assert_eq!(
        ts_type(
            &json!({"type": "array", "items": {"allOf": [{"type": ["string", "null"]}, {"$ref": "#/x"}]}})
        ),
        "(string | null)[]"
    );
    assert_eq!(
        signature(
            "mcp.server-tool",
            None,
            &json!({"type": "object"}),
            "unknown"
        ),
        r#""mcp.server-tool"(args?: Record<string, unknown>): Promise<unknown>;"#
    );
}

fn nested(depth: usize) -> Value {
    (0..depth).fold(
        json!({"type": "string"}),
        |inner, _| json!({"type": "object", "properties": {"a": inner}, "required": ["a"]}),
    )
}

#[test]
fn depth_limit_degrades_the_deepest_level_to_unknown() {
    assert_eq!(
        ts_type(&nested(8)),
        "{ a: { a: { a: { a: { a: { a: { a: { a: string } } } } } } } }"
    );
    assert_eq!(
        ts_type(&nested(9)),
        "{ a: { a: { a: { a: { a: { a: { a: { a: { a: unknown } } } } } } } } }"
    );
    // Deep schemas stay bounded and never overflow the stack.
    assert!(ts_type(&nested(500)).len() < 200);
}

#[test]
fn size_limits_degrade_oversized_types_and_signatures() {
    let wide = |count: usize| {
        let properties: serde_json::Map<String, Value> = (0..count)
            .map(|index| (format!("field_{index:04}"), json!({"type": "string"})))
            .collect();
        json!({"type": "object", "properties": properties})
    };
    let small = ts_type(&wide(10));
    assert!(small.starts_with("{ field_0000?: string;"), "{small}");
    assert_eq!(ts_type(&wide(1000)), "unknown");
    let nested_wide =
        json!({"type": "object", "properties": {"inner": wide(1000), "id": {"type": "string"}}});
    assert_eq!(ts_type(&nested_wide), "{ id?: string; inner?: unknown }");

    // Each field fits, but the signature as a whole would not.
    let near_limit = wide(170);
    assert!(ts_type(&near_limit).len() <= 4096);
    let long_name = "n".repeat(1200);
    let degraded = signature(&long_name, None, &near_limit, "string");
    assert_eq!(
        degraded,
        format!("{long_name}(args?: unknown): Promise<string>;")
    );
}

#[test]
fn signature_summarises_the_first_sentence_safely() {
    let schema =
        json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]});
    assert_eq!(
        signature(
            "read",
            Some("Read a file.\nSecond sentence. Third."),
            &schema,
            "string"
        ),
        "/** Read a file. */\nread(args: { path: string }): Promise<string>;"
    );
    assert_eq!(
        signature("read", None, &schema, "string"),
        "read(args: { path: string }): Promise<string>;"
    );
    assert_eq!(
        signature("read", Some("  \n\t "), &schema, "string"),
        "read(args: { path: string }): Promise<string>;"
    );
    let hostile = signature(
        "x",
        Some("Ends here */ declare const evil: 1; /*\r\n next line"),
        &json!({}),
        "string",
    );
    assert_eq!(
        hostile,
        "/** Ends here * / declare const evil: 1; /* next line */\nx(args: unknown): Promise<string>;"
    );
    assert_eq!(hostile.matches("*/").count(), 1);
    assert_eq!(hostile.lines().count(), 2);

    let long = signature("x", Some(&"word ".repeat(100)), &json!({}), "string");
    let summary = long.lines().next().unwrap();
    let body = summary
        .strip_prefix("/** ")
        .and_then(|rest| rest.strip_suffix(" */"))
        .unwrap();
    assert_eq!(body.chars().count(), 160, "{body}");
    assert!(body.ends_with("word…"), "{body}");
}

#[test]
fn declarations_skip_unnamed_entries_and_are_deterministic() {
    let definitions = vec![
        json!({"name": "first", "description": "One. Two.", "input_schema": {"type": "object", "properties": {"b": {"type": "string"}, "a": {"type": "integer"}}, "required": ["b"]}}),
        json!({"description": "no name", "input_schema": {"type": "object"}}),
        json!({"name": 3}),
        json!({"name": "second-tool"}),
    ];
    let expected = "declare const ns: {\n  /** One. */\n  first(args: { a?: number; b: string }): Promise<string>;\n  \"second-tool\"(args: unknown): Promise<string>;\n};";
    assert_eq!(declarations("ns", &definitions), expected);
    assert_eq!(
        declarations("ns", &definitions),
        declarations("ns", &definitions)
    );
    assert_eq!(declarations("ns", &[]), "declare const ns: {\n};");
}

#[test]
fn native_tool_declarations_snapshot() {
    // The block Code mode advertises: compacted definitions, code_intel on.
    let definitions = super::native_definitions(true);
    let rendered = declarations("slim", definitions);
    assert_eq!(rendered, declarations("slim", definitions));
    let expected = concat!(
        "declare const slim: {\n",
        "  /** Read exact UTF-8 text, at most 1000 lines per call; follow the returned offset for more. */\n",
        "  read(args: { max_lines?: number; offset?: number; outline?: boolean; path: string; ranges?: number[][]; raw?: boolean; structured?: boolean; symbols?: string[] }): Promise<string | { path: string; content: string; lines: string[]; first_line: number; total_lines: number; next_offset: number | null; complete: boolean }>;\n",
        "  /** List directory pages. */\n",
        "  list(args?: { cursor?: string; max_entries?: number; offset?: number; path?: string; structured?: boolean }): Promise<string | { entries: string[]; total: number; next_cursor: string | null }>;\n",
        "  /** Search literal UTF-8 text using query OR patterns. */\n",
        "  search(args: { context_lines?: number; cursor?: string; glob?: string; max_hits?: number; offset?: number; path?: string; patterns?: string[]; query?: string; structured?: boolean }): Promise<string | { hits: { path: string; line: number; text: string; context: { line: number; text: string }[] }[]; total: number; next_cursor: string | null; capped: boolean }>;\n",
        "  /** Write a complete UTF-8 file, creating parents. */\n",
        "  write(args: { content: string; expected?: string | null; path: string; then_run?: { args?: string[] | null; command: string; timeout_ms?: number } }): Promise<string>;\n",
        "  /** Apply ordered edits to one file: edits. */\n",
        "  patch(args: { edits: { expected: string; replace_all?: boolean; replacement: string }[]; files?: { edits: { expected: string; replace_all?: boolean; replacement: string }[]; path: string }[]; path: string; rollback_on_check_fail?: boolean; then_run?: { args?: string[] | null; command: string; timeout_ms?: number } }): Promise<string>;\n",
        "  /** Run in workspace; rejects on non-zero exit unless allow_non_zero: true. */\n",
        "  shell(args: { allow_non_zero?: boolean; args?: string[] | null; command: string; no_cache?: boolean; readonly?: boolean; timeout_ms?: number }): Promise<string>;\n",
        "  /** Semantic Rust, JavaScript and TypeScript navigation and diagnostics; definition includes a bounded preview. */\n",
        "  code_intel(args: { action: \"symbol\" | \"definition\" | \"references\" | \"hover\" | \"diagnostics\" | \"status\"; column?: number; include_info?: boolean; line?: number; max_results?: number; offset?: number; path?: string; query?: string; revision?: number; server?: string; symbol?: string }): Promise<string>;\n",
        "  /** Find workspace files matching a glob pattern. */\n",
        "  glob(args?: { max_entries?: number; path?: string; pattern?: string }): Promise<{ files: string[]; truncated: boolean; timed_out: boolean }>;\n",
        "  /** Read-only structured git status, diff, log, branch, or show. */\n",
        "  git(args: { limit?: number; op: \"status\" | \"diff\" | \"log\" | \"branch\" | \"show\"; path?: string; rev?: string; staged?: boolean }): Promise<Record<string, unknown>>;\n",
        "};",
    );
    assert_eq!(rendered, expected);
    assert!(
        rendered.len() <= 2820,
        "declarations grew too large: {} bytes",
        rendered.len()
    );
}
