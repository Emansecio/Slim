//! Tool discovery ranking: Okapi BM25 over tool metadata.
//!
//! Follows Pi's `tool-search` ranker: k1 1.2, b 0.75, stop words, a naive
//! singular stem, camelCase / snake_case splitting. A document is built from
//! the tool name, its description, the property names and descriptions of its
//! input schema, and the name, description and instructions of its server.
//! Terms are Unicode-aware (accented words stay whole); only ASCII endings are
//! stemmed.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::mcp::McpToolSummary;

const K1: f64 = 1.2;
const B: f64 = 0.75;
/// Schema nesting followed when collecting search text.
const MAX_SCHEMA_DEPTH: usize = 6;
/// Search text of one tool is bounded so a hostile catalog cannot make the
/// ranker's cost unbounded.
const MAX_DOCUMENT_BYTES: usize = 32 * 1024;

const STOP_WORDS: [&str; 21] = [
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it", "of", "on",
    "or", "that", "the", "this", "to", "with",
];

/// Naive singular form: `issues` matches `issue`, `searches` matches `search`.
fn stem(term: &str) -> String {
    let length = term.chars().count();
    if length > 4 && term.ends_with("ies") {
        return format!("{}y", &term[..term.len() - 3]);
    }
    if length > 4
        && ["ches", "shes", "sses", "xes", "zes"]
            .iter()
            .any(|suffix| term.ends_with(suffix))
    {
        return term[..term.len() - 2].to_owned();
    }
    if length > 3 && term.ends_with('s') && !term.ends_with("ss") {
        return term[..term.len() - 1].to_owned();
    }
    term.to_owned()
}

/// Lowercase terms, split at camelCase boundaries and non-alphanumerics,
/// without stop words, singularized.
pub fn tokenize(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut terms = Vec::new();
    let mut current = String::new();
    let mut flush = |current: &mut String| {
        if current.is_empty() {
            return;
        }
        let term = std::mem::take(current);
        if !STOP_WORDS.contains(&term.as_str()) {
            terms.push(stem(&term));
        }
    };
    for (index, &character) in chars.iter().enumerate() {
        if !character.is_alphanumeric() {
            flush(&mut current);
            continue;
        }
        if character.is_uppercase() {
            let previous = index.checked_sub(1).map(|at| chars[at]);
            let next = chars.get(index + 1).copied();
            // `fooBar` -> foo bar; `HTTPServer` -> http server.
            let after_lower =
                previous.is_some_and(|prev| prev.is_lowercase() || prev.is_ascii_digit());
            let before_lower_after_upper =
                previous.is_some_and(char::is_uppercase) && next.is_some_and(char::is_lowercase);
            if after_lower || before_lower_after_upper {
                flush(&mut current);
            }
        }
        current.extend(character.to_lowercase());
    }
    flush(&mut current);
    terms
}

/// Schema descriptions and property names, recursively and bounded.
fn schema_text(schema: &Value, parts: &mut Vec<String>, depth: usize) {
    let Some(object) = schema.as_object() else {
        return;
    };
    if let Some(description) = object.get("description").and_then(Value::as_str) {
        parts.push(description.to_owned());
    }
    if depth >= MAX_SCHEMA_DEPTH {
        return;
    }
    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        for (name, property) in properties {
            parts.push(name.clone());
            schema_text(property, parts, depth + 1);
        }
    }
    if let Some(items) = object.get("items") {
        schema_text(items, parts, depth + 1);
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(variants) = object.get(key).and_then(Value::as_array) {
            for variant in variants {
                schema_text(variant, parts, depth + 1);
            }
        }
    }
}

/// What the ranker needs to know of a tool's server.
#[derive(Clone, Copy, Debug, Default)]
pub struct SearchServer<'a> {
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub instructions: Option<&'a str>,
}

/// Search text of a tool: its name (also with `_` as spaces), description,
/// schema property names and descriptions, and its server's name, description
/// and instructions.
pub fn tool_search_document(server: SearchServer<'_>, tool: &McpToolSummary) -> String {
    let mut parts = vec![tool.name.clone(), tool.name.replace('_', " ")];
    if let Some(description) = &tool.description {
        parts.push(description.clone());
    }
    schema_text(&tool.schema, &mut parts, 0);
    parts.push(server.name.to_owned());
    parts.extend(server.description.map(str::to_owned));
    parts.extend(server.instructions.map(str::to_owned));
    let mut text = parts
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if text.len() > MAX_DOCUMENT_BYTES {
        let mut end = MAX_DOCUMENT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// One ranked document: its index in the input and its score.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bm25Match {
    pub index: usize,
    pub score: f64,
}

/// Okapi BM25 with the usual parameters. Ties keep document order.
#[derive(Clone, Copy, Debug)]
pub struct Bm25Ranker {
    k1: f64,
    b: f64,
}

impl Default for Bm25Ranker {
    fn default() -> Self {
        Self { k1: K1, b: B }
    }
}

impl Bm25Ranker {
    /// Ranks `documents` (search texts) for `query`; only documents that
    /// contain at least one query term are returned, best first, at most
    /// `limit`.
    pub fn rank<S: AsRef<str>>(
        &self,
        query: &str,
        documents: &[S],
        limit: usize,
    ) -> Vec<Bm25Match> {
        let mut seen = HashSet::new();
        let query_terms: Vec<String> = tokenize(query)
            .into_iter()
            .filter(|term| seen.insert(term.clone()))
            .collect();
        if query_terms.is_empty() || documents.is_empty() || limit == 0 {
            return Vec::new();
        }
        let counts: Vec<HashMap<String, usize>> = documents
            .iter()
            .map(|document| {
                let mut counts = HashMap::new();
                for term in tokenize(document.as_ref()) {
                    *counts.entry(term).or_insert(0) += 1;
                }
                counts
            })
            .collect();
        let lengths: Vec<f64> = counts
            .iter()
            .map(|counts| counts.values().sum::<usize>() as f64)
            .collect();
        let total = documents.len() as f64;
        let average = {
            let average = lengths.iter().sum::<f64>() / total;
            if average > 0.0 {
                average
            } else {
                1.0
            }
        };
        let idf: Vec<f64> = query_terms
            .iter()
            .map(|term| {
                let frequency = counts.iter().filter(|c| c.contains_key(term)).count() as f64;
                (1.0 + (total - frequency + 0.5) / (frequency + 0.5)).ln()
            })
            .collect();
        let mut matches = Vec::new();
        for (index, document_counts) in counts.iter().enumerate() {
            let mut score = 0.0;
            for (term, idf) in query_terms.iter().zip(&idf) {
                let Some(&count) = document_counts.get(term) else {
                    continue;
                };
                let count = count as f64;
                let norm = self.k1 * (1.0 - self.b + self.b * lengths[index] / average);
                score += idf * (count * (self.k1 + 1.0)) / (count + norm);
            }
            if score > 0.0 {
                matches.push(Bm25Match { index, score });
            }
        }
        matches.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.index.cmp(&b.index))
        });
        matches.truncate(limit);
        matches
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str, description: &str, schema: Value) -> McpToolSummary {
        McpToolSummary {
            name: name.into(),
            description: Some(description.into()),
            schema,
            output_schema: None,
        }
    }

    #[test]
    fn tokenizer_splits_case_snake_and_punctuation_and_drops_stop_words() {
        assert_eq!(
            tokenize("createIssueComment"),
            ["create", "issue", "comment"]
        );
        assert_eq!(tokenize("list_pull_requests"), ["list", "pull", "request"]);
        assert_eq!(
            tokenize("HTTPServer v2Api"),
            ["http", "server", "v2", "api"]
        );
        assert_eq!(tokenize("The list of the items"), ["list", "item"]);
        assert!(tokenize("   --- ").is_empty());
    }

    #[test]
    fn tokenizer_keeps_accented_words_whole_and_stems_plurals() {
        assert_eq!(tokenize("Ação rápida"), ["ação", "rápida"]);
        assert_eq!(tokenize("queries"), ["query"]);
        assert_eq!(tokenize("searches"), ["search"]);
        assert_eq!(tokenize("class"), ["class"]);
        assert_eq!(tokenize("issues"), ["issue"]);
        // Short words are not stemmed into nothing.
        assert_eq!(tokenize("gas"), ["gas"]);
        assert_eq!(tokenize("is"), Vec::<String>::new());
    }

    #[test]
    fn document_contains_name_description_schema_and_server_text() {
        let schema = json!({
            "type": "object",
            "properties": {
                "branchName": {"type": "string", "description": "Branch to protect"},
                "options": {"type": "object", "properties": {"force": {"description": "Overwrite"}}},
            },
        });
        let document = tool_search_document(
            SearchServer {
                name: "git",
                description: Some("Repository helper"),
                instructions: Some("Prefer shallow clones"),
            },
            &tool("create_branch", "Creates a branch", schema),
        );
        for needle in [
            "create_branch",
            "create branch",
            "Creates a branch",
            "branchName",
            "Branch to protect",
            "force",
            "Overwrite",
            "git",
            "Repository helper",
            "Prefer shallow clones",
        ] {
            assert!(
                document.contains(needle),
                "{needle} missing from {document}"
            );
        }
    }

    #[test]
    fn document_is_bounded_and_schema_depth_is_limited() {
        let mut schema = json!({"description": "deepest"});
        for _ in 0..20 {
            schema = json!({"properties": {"p": schema}});
        }
        let document = tool_search_document(
            SearchServer::default(),
            &tool("t", &"x".repeat(100_000), schema),
        );
        assert!(document.len() <= MAX_DOCUMENT_BYTES);
        let shallow = tool_search_document(
            SearchServer::default(),
            &tool(
                "t",
                "d",
                json!({"properties": {"p": {"properties": {"q": {}}}}}),
            ),
        );
        assert!(shallow.contains('q'));
        let deep = {
            let mut schema = json!({"description": "deepest"});
            for _ in 0..20 {
                schema = json!({"properties": {"p": schema}});
            }
            tool_search_document(SearchServer::default(), &tool("t", "d", schema))
        };
        assert!(!deep.contains("deepest"));
    }

    #[test]
    fn ranking_prefers_name_and_rare_terms_and_ignores_non_matches() {
        let documents = [
            "list_issues List issues of a repository",
            "create_issue Create a new issue in a repository",
            "get_weather Current weather for a city",
        ];
        let ranker = Bm25Ranker::default();
        let hits = ranker.rank("create issue", &documents, 8);
        assert_eq!(hits.iter().map(|hit| hit.index).collect::<Vec<_>>(), [1, 0]);
        assert!(hits[0].score > hits[1].score);
        let hits = ranker.rank("weather", &documents, 8);
        assert_eq!(hits.iter().map(|hit| hit.index).collect::<Vec<_>>(), [2]);
        assert!(ranker.rank("kubernetes", &documents, 8).is_empty());
    }

    #[test]
    fn ranking_is_stem_insensitive_limited_and_stable_on_ties() {
        let documents = ["issue tracker", "issue tracker", "unrelated words"];
        let ranker = Bm25Ranker::default();
        let hits = ranker.rank("issues", &documents, 8);
        assert_eq!(hits.iter().map(|hit| hit.index).collect::<Vec<_>>(), [0, 1]);
        assert_eq!(hits[0].score, hits[1].score);
        assert_eq!(ranker.rank("issues", &documents, 1).len(), 1);
        assert!(ranker.rank("issues", &documents, 0).is_empty());
        assert!(ranker.rank("the of and", &documents, 8).is_empty());
        assert!(ranker.rank("issues", &[] as &[&str], 8).is_empty());
    }

    #[test]
    fn repeated_query_terms_do_not_double_count() {
        let documents = ["alpha beta", "gamma"];
        let ranker = Bm25Ranker::default();
        let once = ranker.rank("alpha", &documents, 8);
        let twice = ranker.rank("alpha alpha alpha", &documents, 8);
        assert_eq!(once, twice);
    }

    #[test]
    fn server_description_and_schema_fields_make_a_tool_discoverable() {
        let github = SearchServer {
            name: "github",
            description: Some("Pull requests and repositories"),
            instructions: None,
        };
        let docs = [
            tool_search_document(
                github,
                &tool(
                    "run",
                    "Executes",
                    json!({"properties": {"sha": {"description": "Commit"}}}),
                ),
            ),
            tool_search_document(
                SearchServer::default(),
                &tool("noop", "Does nothing", json!({})),
            ),
        ];
        let ranker = Bm25Ranker::default();
        assert_eq!(ranker.rank("pull request", &docs, 8)[0].index, 0);
        assert_eq!(ranker.rank("commit", &docs, 8)[0].index, 0);
    }
}
