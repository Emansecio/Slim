//! Jev pruning strategy for compaction. Jev judges which tool calls/results in
//! the summarized prefix are still needed; kept content stays verbatim, dropped
//! content is replaced by bounded notes. Nothing is rewritten by a generative
//! model. The runtime owns fallback to the LLM summary path.
//!
//! The same model is reachable through two endpoints, selected by
//! [`JevBackend`]: TypeSafe's own System One API and the Vercel AI Gateway
//! evaluation route.

use super::compact::{estimate_provider_message_tokens, format_transcript, CompactionSelection};
use crate::provider::ProviderMessage;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::time::Duration;

pub const DEFAULT_JEV_MODEL: &str = "jev-latest";
pub const DEFAULT_VERCEL_JEV_MODEL: &str = "typesafe-ai/jev";
const TYPESAFE_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
const VERCEL_ENDPOINT: &str = "https://ai-gateway.vercel.sh/v4/ai/evaluation-model";
/// Headers the AI Gateway evaluation specification requires on every request.
const VERCEL_PROTOCOL_VERSION: &str = "0.0.1";
const VERCEL_EVALUATION_SPEC_VERSION: &str = "4";
/// State the API sees per request; kept well under the model window so one
/// batch of questions rarely needs a second fitting pass.
const STATE_MAX_CHARS: usize = 30_000;
/// Noul questions per request. Batches run in parallel inside the API.
const QUESTIONS_PER_BATCH: usize = 16;
/// Keep probability below which an item is pruned.
const KEEP_THRESHOLD: f64 = 0.5;
/// Characters of a truncated tool result retained before the marker.
const TRUNCATE_HEAD_CHARS: usize = 300;
/// Pruning a pair must actually free at least this many characters; a note
/// larger than the content it replaces would grow the context.
const MIN_PAIR_SAVINGS_CHARS: usize = 512;
/// Minimum estimated-token reduction for a pruned selection to be accepted.
/// Below this the caller should fall back to the LLM summary path.
const MIN_SAVINGS_TOKENS: u64 = 256;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;

const KEEP_INSTRUCTIONS: &str = "Will the coding agent plausibly need THIS tool call and its result again to finish the task? Yes only if it holds the freshest evidence for work still pending: an exact file path, error, identifier, command or constraint that later turns do not restate. No when it is stale (a listing, search or file version superseded by a later one), a failed attempt already retried, or something the transcript restates later.";

/// Which endpoint evaluates the questions. Both serve the same Jev model; the
/// wire contract differs in the model field, the yes/no primitive name and the
/// answer field, so the request and the parser branch on it.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JevBackend {
    /// TypeSafe System One API, selected by `TYPESAFE_API_KEY`.
    #[default]
    Typesafe,
    /// Vercel AI Gateway evaluation route, selected by `AI_GATEWAY_API_KEY`.
    Vercel,
}

impl JevBackend {
    pub fn name(self) -> &'static str {
        match self {
            Self::Typesafe => "typesafe",
            Self::Vercel => "vercel",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "typesafe" | "systemone" => Ok(Self::Typesafe),
            "vercel" | "gateway" => Ok(Self::Vercel),
            other => Err(format!(
                "unknown Jev backend '{other}'; expected 'typesafe' or 'vercel'"
            )),
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            Self::Typesafe => DEFAULT_JEV_MODEL,
            Self::Vercel => DEFAULT_VERCEL_JEV_MODEL,
        }
    }

    /// Vercel AI Gateway keys carry the `vck_` prefix Vercel issues, so a lone
    /// credential is enough to pick the endpoint when no backend is set.
    pub fn for_api_key(api_key: &str) -> Self {
        if api_key.trim_start().starts_with("vck_") {
            Self::Vercel
        } else {
            Self::Typesafe
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JevPruneConfig {
    pub backend: JevBackend,
    pub api_key: String,
    pub model: String,
}

impl JevPruneConfig {
    pub fn new(backend: JevBackend, api_key: impl Into<String>) -> Self {
        Self {
            backend,
            api_key: api_key.into(),
            model: backend.default_model().to_owned(),
        }
    }

    /// Override the model. An empty value keeps the backend default, matching
    /// how the environment resolution treats a blank `SLIM_JEV_MODEL`.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        let model = model.into();
        if !model.trim().is_empty() {
            self.model = model;
        }
        self
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct JevPruneStats {
    pub pairs_total: usize,
    pub pairs_dropped: usize,
    pub results_truncated: usize,
    pub batches: usize,
    pub estimated_saved_tokens: u64,
}

#[derive(Debug)]
pub enum JevPruneError {
    Transport(String),
    Response(String),
    /// Nothing in the summarized prefix is a tool call/result pair.
    NoCandidates,
    /// Candidates existed but pruning freed too little to be worth it.
    InsufficientReduction,
}

impl std::fmt::Display for JevPruneError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(message) => write!(formatter, "jev request failed: {message}"),
            Self::Response(message) => write!(formatter, "jev response rejected: {message}"),
            Self::NoCandidates => {
                write!(formatter, "no prunable tool calls in the summarized prefix")
            }
            Self::InsufficientReduction => {
                write!(formatter, "jev pruning freed less than the minimum")
            }
        }
    }
}

impl std::error::Error for JevPruneError {}

/// Async boundary so unit tests never touch the network.
#[async_trait::async_trait]
pub trait JevJudge: Send + Sync {
    /// One probability per question id, in the order the ids were sent.
    async fn judge(&self, state: &str, questions: &[(String, String)]) -> Result<Vec<f64>, String>;
}

pub struct HttpJevJudge {
    client: reqwest::Client,
    config: JevPruneConfig,
}

impl HttpJevJudge {
    pub fn new(config: JevPruneConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client, config }
    }
}

const KEEP_CRITERIA_TRUE: &str = "Freshest evidence for pending work; not restated later";
const KEEP_CRITERIA_FALSE: &str = "Stale, superseded, failed-and-retried, or restated later";

fn request_url(backend: JevBackend) -> &'static str {
    match backend {
        JevBackend::Typesafe => TYPESAFE_ENDPOINT,
        JevBackend::Vercel => VERCEL_ENDPOINT,
    }
}

/// The yes/no question type. TypeSafe's `noul` is the AI Gateway evaluation
/// specification's `boolean`.
fn question_type(backend: JevBackend) -> &'static str {
    match backend {
        JevBackend::Typesafe => "noul",
        JevBackend::Vercel => "boolean",
    }
}

/// The field carrying the yes probability: `noul` on TypeSafe, `probability`
/// on the gateway.
fn answer_field(backend: JevBackend) -> &'static str {
    match backend {
        JevBackend::Typesafe => "noul",
        JevBackend::Vercel => "probability",
    }
}

fn request_body(
    backend: JevBackend,
    model: &str,
    state: &str,
    questions: &[(String, String)],
) -> Value {
    let mut map = Map::new();
    for (id, instructions) in questions {
        map.insert(
            id.clone(),
            json!({
                "type": question_type(backend),
                "instructions": instructions,
                "criteria": { "true": KEEP_CRITERIA_TRUE, "false": KEEP_CRITERIA_FALSE }
            }),
        );
    }
    let mut body = json!({ "state": state, "questions": Value::Object(map) });
    // TypeSafe selects the model in the body; the gateway carries it in the
    // `ai-model-id` header instead.
    if backend == JevBackend::Typesafe {
        body["model"] = Value::String(model.to_owned());
    }
    body
}

fn parse_answer_probabilities(
    backend: JevBackend,
    parsed: &Value,
    ids: &[String],
) -> Result<Vec<f64>, String> {
    let answers = parsed
        .get("answers")
        .and_then(Value::as_object)
        .ok_or_else(|| "missing answers object".to_string())?;
    ids.iter()
        .map(|id| {
            answers
                .get(id)
                .and_then(|answer| answer.get(answer_field(backend)))
                .and_then(Value::as_f64)
                .map(|probability| probability.clamp(0.0, 1.0))
                .ok_or_else(|| format!("missing {} answer for '{id}'", answer_field(backend)))
        })
        .collect()
}

#[async_trait::async_trait]
impl JevJudge for HttpJevJudge {
    async fn judge(&self, state: &str, questions: &[(String, String)]) -> Result<Vec<f64>, String> {
        let backend = self.config.backend;
        let body = request_body(backend, &self.config.model, state, questions);
        let mut request = self
            .client
            .post(request_url(backend))
            .bearer_auth(&self.config.api_key)
            .json(&body);
        if backend == JevBackend::Vercel {
            // Without these the gateway rejects the route before Jev runs.
            request = request
                .header("ai-gateway-protocol-version", VERCEL_PROTOCOL_VERSION)
                .header("ai-gateway-auth-method", "api-key")
                .header(
                    "ai-evaluation-model-specification-version",
                    VERCEL_EVALUATION_SPEC_VERSION,
                )
                .header("ai-model-id", &self.config.model);
        }
        let response = request.send().await.map_err(|error| error.to_string())?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(|error| error.to_string())?;
        if bytes.len() as u64 > MAX_RESPONSE_BYTES {
            return Err(format!("response exceeds {MAX_RESPONSE_BYTES} bytes"));
        }
        if !status.is_success() {
            let detail = String::from_utf8_lossy(&bytes)
                .chars()
                .take(200)
                .collect::<String>();
            return Err(format!("http {}: {detail}", status.as_u16()));
        }
        let parsed: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        let ids = questions
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        parse_answer_probabilities(backend, &parsed, &ids)
    }
}

/// One assistant/tool pair inside the summarized prefix that Jev may prune.
#[derive(Clone, Debug)]
struct PrunablePair {
    /// Index of the assistant message (with tool calls) inside `summarized`.
    assistant_index: usize,
    /// Indices of its tool-result messages inside `summarized`.
    tool_indices: Vec<usize>,
    chars: usize,
}

fn prunable_pairs(summarized: &[ProviderMessage]) -> Vec<PrunablePair> {
    let mut pairs = Vec::new();
    let mut index = 0usize;
    while index < summarized.len() {
        let message = &summarized[index];
        if message.role == "assistant" && !message.tool_calls.is_empty() {
            let wanted: std::collections::HashSet<&str> = message
                .tool_calls
                .iter()
                .map(|call| call.id.as_str())
                .collect();
            let mut tool_indices = Vec::new();
            let mut cursor = index + 1;
            while cursor < summarized.len() {
                let candidate = &summarized[cursor];
                if candidate.role != "tool" {
                    break;
                }
                if candidate
                    .tool_call_id
                    .as_deref()
                    .is_some_and(|id| wanted.contains(id))
                {
                    tool_indices.push(cursor);
                }
                cursor += 1;
            }
            if !tool_indices.is_empty() {
                let chars = tool_indices
                    .iter()
                    .chain(std::iter::once(&index))
                    .map(|at| summarized[*at].content.len())
                    .sum();
                pairs.push(PrunablePair {
                    assistant_index: index,
                    tool_indices,
                    chars,
                });
            }
            index = cursor;
        } else {
            index += 1;
        }
    }
    pairs
}

fn dropped_note(pair_index: usize, kept_head: &str) -> String {
    if kept_head.is_empty() {
        format!("[jev-compaction: tool call {pair_index} dropped as no longer needed]")
    } else {
        format!(
            "[jev-compaction: tool call {pair_index} dropped as no longer needed; result head]\n{kept_head}"
        )
    }
}

fn bounded_head(content: &str) -> String {
    content.chars().take(TRUNCATE_HEAD_CHARS).collect()
}

/// Prune the summarized prefix in place. Returns stats or an error that the
/// caller turns into the LLM-summary fallback. Tool calls and tool results are
/// the only candidates; user and assistant text always stays verbatim.
pub async fn prune_summarized(
    judge: &dyn JevJudge,
    selection: &CompactionSelection,
    summarized: &mut [ProviderMessage],
) -> Result<JevPruneStats, JevPruneError> {
    let pairs = prunable_pairs(summarized);
    let mut stats = JevPruneStats {
        pairs_total: pairs.len(),
        ..JevPruneStats::default()
    };
    if pairs.is_empty() {
        return Err(JevPruneError::NoCandidates);
    }
    let tokens_before = estimate_provider_message_tokens(summarized);
    let full_state = format!(
        "[Task]\n{}\n\n[Transcript being compacted]\n{}",
        selection.root_instruction,
        format_transcript(summarized)
    );
    let state = if full_state.chars().count() > STATE_MAX_CHARS {
        full_state.chars().take(STATE_MAX_CHARS).collect::<String>()
    } else {
        full_state
    };
    let questions: Vec<(String, String)> = pairs
        .iter()
        .enumerate()
        .map(|(index, _)| (format!("keep_{index}"), KEEP_INSTRUCTIONS.to_string()))
        .collect();
    let mut probabilities = Vec::with_capacity(questions.len());
    for chunk in questions.chunks(QUESTIONS_PER_BATCH) {
        let batch = judge
            .judge(&state, chunk)
            .await
            .map_err(JevPruneError::Transport)?;
        if batch.len() != chunk.len() {
            return Err(JevPruneError::Response(format!(
                "expected {} answers, got {}",
                chunk.len(),
                batch.len()
            )));
        }
        stats.batches += 1;
        probabilities.extend(batch);
    }
    let mut drop_marks: Vec<Option<String>> = vec![None; summarized.len()];
    for (pair_index, pair) in pairs.iter().enumerate() {
        let keep = probabilities[pair_index];
        if keep >= KEEP_THRESHOLD {
            continue;
        }
        let head = pair
            .tool_indices
            .first()
            .filter(|_| pair.chars > TRUNCATE_HEAD_CHARS * 4)
            .map(|at| bounded_head(&summarized[*at].content))
            .unwrap_or_default();
        let assistant_note = dropped_note(pair_index, "");
        // Only prune when the replacement is genuinely smaller. A stale but
        // tiny result costs nothing, and rewriting it with a longer note
        // would grow the context instead of freeing it.
        let original_chars = summarized[pair.assistant_index].content.chars().count()
            + summarized[pair.assistant_index]
                .tool_calls
                .iter()
                .map(|call| call.arguments.chars().count())
                .sum::<usize>()
            + pair
                .tool_indices
                .iter()
                .map(|at| summarized[*at].content.chars().count())
                .sum::<usize>();
        let replacement_chars = assistant_note.chars().count()
            + pair
                .tool_indices
                .iter()
                .map(|at| {
                    let kept = if Some(at) == pair.tool_indices.first() {
                        head.chars().count()
                    } else {
                        0
                    };
                    dropped_note(pair_index, "").chars().count() + kept
                })
                .sum::<usize>();
        if original_chars.saturating_sub(replacement_chars) < MIN_PAIR_SAVINGS_CHARS {
            continue;
        }
        if !head.is_empty() {
            stats.results_truncated += 1;
        }
        stats.pairs_dropped += 1;
        drop_marks[pair.assistant_index] = Some(assistant_note);
        for tool_index in &pair.tool_indices {
            let note = if Some(tool_index) == pair.tool_indices.first() {
                dropped_note(pair_index, &head)
            } else {
                dropped_note(pair_index, "")
            };
            drop_marks[*tool_index] = Some(note);
        }
    }
    if stats.pairs_dropped == 0 {
        return Err(JevPruneError::InsufficientReduction);
    }
    // Replace in place: tool_call ids stay intact so provider-side pairing
    // never breaks, and call name/args survive as the cheap, useful part.
    for (index, note) in drop_marks.into_iter().enumerate() {
        if let Some(note) = note {
            let message = &mut summarized[index];
            message.content = note;
            message.content_blocks.clear();
            if message.role == "assistant" {
                for call in &mut message.tool_calls {
                    call.arguments = String::new();
                }
            }
        }
    }
    let tokens_after = estimate_provider_message_tokens(summarized);
    stats.estimated_saved_tokens = tokens_before.saturating_sub(tokens_after);
    if stats.estimated_saved_tokens < MIN_SAVINGS_TOKENS {
        return Err(JevPruneError::InsufficientReduction);
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderToolCall;

    struct StubJudge {
        probabilities: Vec<f64>,
    }

    #[async_trait::async_trait]
    impl JevJudge for StubJudge {
        async fn judge(
            &self,
            _state: &str,
            questions: &[(String, String)],
        ) -> Result<Vec<f64>, String> {
            // Deliberately allow fewer answers than questions so the short-list
            // rejection path is reachable in tests.
            let take = questions.len().min(self.probabilities.len());
            Ok(self.probabilities[..take].to_vec())
        }
    }

    fn message(role: &str, content: String) -> ProviderMessage {
        ProviderMessage {
            role: role.into(),
            content,
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
            content_blocks: Vec::new(),
            responses_reasoning: Vec::new(),
            chat_reasoning: None,
        }
    }

    fn pair(id: &str, result_chars: usize) -> Vec<ProviderMessage> {
        let mut assistant = message("assistant", "reading".into());
        assistant.tool_calls = vec![ProviderToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: "{\"path\":\"a.rs\"}".into(),
        }];
        let mut tool = message("tool", "x".repeat(result_chars));
        tool.name = Some("read".into());
        tool.tool_call_id = Some(id.into());
        vec![assistant, tool]
    }

    fn selection() -> CompactionSelection {
        CompactionSelection {
            root_instruction: "fix the bug".into(),
            summarized: Vec::new(),
            pinned: Vec::new(),
            kept: Vec::new(),
            first_kept_index: 0,
            recent_tokens: 0,
        }
    }

    #[tokio::test]
    async fn drops_stale_pair_and_preserves_call_ids() {
        let mut summarized = pair("call-1", 8_000);
        summarized.push(message("user", "thanks, now patch it".into()));
        let judge = StubJudge {
            probabilities: vec![0.05],
        };
        let stats = prune_summarized(&judge, &selection(), &mut summarized)
            .await
            .expect("pruned");
        assert_eq!(stats.pairs_dropped, 1);
        assert_eq!(summarized[0].tool_calls[0].id, "call-1");
        assert_eq!(summarized[1].tool_call_id.as_deref(), Some("call-1"));
        assert!(summarized[1].content.starts_with("[jev-compaction:"));
        assert!(summarized[1].content.contains('x')); // head retained
        assert_eq!(summarized[2].content, "thanks, now patch it");
        assert!(stats.estimated_saved_tokens >= 256);
    }

    #[tokio::test]
    async fn keeps_needed_pair_verbatim() {
        let mut summarized = pair("call-1", 8_000);
        let original = summarized.clone();
        let judge = StubJudge {
            probabilities: vec![0.97],
        };
        let error = prune_summarized(&judge, &selection(), &mut summarized)
            .await
            .expect_err("nothing to drop");
        assert!(matches!(error, JevPruneError::InsufficientReduction));
        assert_eq!(summarized, original);
    }

    #[tokio::test]
    async fn reports_no_candidates_when_only_text_is_summarized() {
        let mut summarized = vec![
            message("user", "start".into()),
            message("assistant", "just text".into()),
        ];
        let judge = StubJudge {
            probabilities: vec![0.01],
        };
        let error = prune_summarized(&judge, &selection(), &mut summarized)
            .await
            .expect_err("no tool pairs");
        assert!(matches!(error, JevPruneError::NoCandidates));
    }

    #[tokio::test]
    async fn rejects_short_answer_lists() {
        let mut summarized = pair("call-1", 400);
        summarized.extend(pair("call-2", 400));
        let judge = StubJudge {
            probabilities: vec![0.1],
        };
        let error = prune_summarized(&judge, &selection(), &mut summarized)
            .await
            .expect_err("short answers rejected");
        assert!(matches!(error, JevPruneError::Response(_)));
    }

    #[test]
    fn parses_backend_names_and_infers_vercel_from_the_key_prefix() {
        assert_eq!(JevBackend::parse(" typesafe "), Ok(JevBackend::Typesafe));
        assert_eq!(JevBackend::parse("VERCEL"), Ok(JevBackend::Vercel));
        assert!(JevBackend::parse("openai").is_err());

        assert_eq!(JevBackend::for_api_key("vck_abc"), JevBackend::Vercel);
        assert_eq!(JevBackend::for_api_key("tsk_abc"), JevBackend::Typesafe);
    }

    #[test]
    fn backend_defaults_supply_a_model_per_endpoint() {
        let typesafe = JevPruneConfig::new(JevBackend::Typesafe, "k");
        assert_eq!(typesafe.model, DEFAULT_JEV_MODEL);
        let gateway = JevPruneConfig::new(JevBackend::Vercel, "k");
        assert_eq!(gateway.model, DEFAULT_VERCEL_JEV_MODEL);
        // A blank override keeps the backend default.
        assert_eq!(
            gateway.clone().with_model("  ").model,
            DEFAULT_VERCEL_JEV_MODEL
        );
        assert_eq!(
            gateway.with_model("typesafe-ai/jev-1.13.0").model,
            "typesafe-ai/jev-1.13.0"
        );
    }

    #[test]
    fn each_backend_builds_its_own_request_shape() {
        let questions = vec![("keep_0".to_owned(), "still needed?".to_owned())];

        let typesafe = request_body(JevBackend::Typesafe, "jev-latest", "state", &questions);
        assert_eq!(typesafe["model"], "jev-latest");
        assert_eq!(typesafe["state"], "state");
        assert_eq!(typesafe["questions"]["keep_0"]["type"], "noul");

        let gateway = request_body(JevBackend::Vercel, "typesafe-ai/jev", "state", &questions);
        assert_eq!(gateway["questions"]["keep_0"]["type"], "boolean");
        // The gateway takes the model from the `ai-model-id` header, not the body.
        assert!(gateway.get("model").is_none());
    }

    #[test]
    fn each_backend_reads_its_own_answer_field() {
        let ids = vec!["keep_0".to_owned(), "keep_1".to_owned()];

        let typesafe = serde_json::json!({
            "answers": {
                "keep_0": { "type": "noul", "noul": 0.25 },
                "keep_1": { "type": "noul", "noul": 1.5 },
            }
        });
        assert_eq!(
            parse_answer_probabilities(JevBackend::Typesafe, &typesafe, &ids),
            Ok(vec![0.25, 1.0])
        );

        let gateway = serde_json::json!({
            "answers": {
                "keep_0": { "type": "boolean", "probability": 0.75 },
                "keep_1": { "type": "boolean", "probability": -1.0 },
            }
        });
        assert_eq!(
            parse_answer_probabilities(JevBackend::Vercel, &gateway, &ids),
            Ok(vec![0.75, 0.0])
        );
    }

    #[test]
    fn rejecting_an_answer_of_the_other_backend_names_the_missing_field() {
        // A TypeSafe-shaped answer must not satisfy the gateway parser.
        let ids = vec!["keep_0".to_owned()];
        let typesafe = serde_json::json!({ "answers": { "keep_0": { "noul": 0.5 } } });
        let error = parse_answer_probabilities(JevBackend::Vercel, &typesafe, &ids)
            .expect_err("gateway needs probability");
        assert!(error.contains("probability"), "{error}");

        let error = parse_answer_probabilities(
            JevBackend::Typesafe,
            &serde_json::json!({ "answers": {} }),
            &ids,
        )
        .expect_err("missing answer");
        assert!(error.contains("noul"), "{error}");
    }
}
