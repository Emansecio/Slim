use serde_json::{json, Value};

// Provider presentation only. Native admission keeps accepting legacy forms.
pub(crate) fn compact_provider_definitions(tools: &mut [Value]) {
    hide_legacy_fields(tools);
    deduplicate_descriptions(tools);
}

fn hide_legacy_fields(tools: &mut [Value]) {
    for tool in tools {
        match tool["name"].as_str() {
            Some("read") => {
                tool["input_schema"]["properties"]
                    .as_object_mut()
                    .unwrap()
                    .remove("lines");
            }
            Some("patch") => {
                let properties = tool["input_schema"]["properties"].as_object_mut().unwrap();
                properties.remove("expected");
                properties.remove("replacement");
                tool["input_schema"]
                    .as_object_mut()
                    .unwrap()
                    .remove("oneOf");
                tool["input_schema"]["required"] = json!(["path", "edits"]);
                tool["description"] = Value::String(
                    tool["description"]
                        .as_str()
                        .unwrap()
                        .replace("edits OR top-level expected/replacement", "edits"),
                );
            }
            _ => {}
        }
    }
}

fn deduplicate_descriptions(tools: &mut [Value]) {
    for tool in tools {
        match tool["name"].as_str() {
            Some("patch") => {
                // Keep raw-source/prefix guidance once, beside the other patch rules.
                tool["description"] = Value::String(tool["description"].as_str().unwrap().replace(
                    "Match unique raw text;",
                    "Match unique raw text from read/search (strip `N: `/`N- ` prefixes);",
                ));
                tool["input_schema"]["properties"]["edits"]["items"]["properties"]["expected"]
                    .as_object_mut()
                    .unwrap()
                    .remove("description");
            }
            Some("write") => {
                // The expected property's description already specifies the requirements.
                tool["description"] = Value::String(tool["description"].as_str().unwrap().replace(
                    "See expected for overwrite requirements; stale content is rejected.",
                    "Stale content is rejected.",
                ));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_candidates_reduce_bytes_without_changing_native_contracts() {
        let registry = crate::tools::ToolRegistry::default();
        let baseline = registry.definitions_for_mode(crate::OperatingMode::Auto);
        let mut aliases = baseline.clone();
        hide_legacy_fields(&mut aliases);
        let mut prose = aliases.clone();
        deduplicate_descriptions(&mut prose);
        let sizes =
            [&baseline, &aliases, &prose].map(|tools| serde_json::to_vec(tools).unwrap().len());
        eprintln!(
            "native_schema_bytes baseline={} aliases={} aliases_and_prose={}",
            sizes[0], sizes[1], sizes[2]
        );
        assert!(sizes[2] < sizes[1] && sizes[1] < sizes[0]);
        assert_eq!(
            registry.definitions_for_mode(crate::OperatingMode::Auto),
            baseline
        );
        let patch = prose.iter().find(|tool| tool["name"] == "patch").unwrap();
        assert_eq!(patch["input_schema"]["required"], json!(["path", "edits"]));
        assert_eq!(
            patch["input_schema"]["properties"]["edits"]["items"]["required"],
            json!(["expected", "replacement"])
        );
        let description = patch["description"].as_str().unwrap();
        for rule in [
            "atomic ordered",
            "unique raw text",
            "read/search",
            "`N: `/`N- `",
            "never line numbers",
            "[truncated]",
            "no complete read",
            "CRLF",
            "unchanged",
            "recovery",
            "no confirmation read",
            "Independent paths",
        ] {
            assert!(description.contains(rule), "missing {rule}");
        }
        let mut description_free_baseline = aliases;
        let mut description_free_candidate = prose;
        fn strip_descriptions(value: &mut Value) {
            match value {
                Value::Object(map) => {
                    map.remove("description");
                    for child in map.values_mut() {
                        strip_descriptions(child);
                    }
                }
                Value::Array(items) => {
                    for child in items {
                        strip_descriptions(child);
                    }
                }
                _ => {}
            }
        }
        for tool in description_free_baseline
            .iter_mut()
            .chain(&mut description_free_candidate)
        {
            strip_descriptions(tool);
        }
        assert_eq!(description_free_baseline, description_free_candidate);
    }
}
