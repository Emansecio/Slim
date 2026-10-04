//! Context-overflow detection by error text.
//!
//! Port of `OVERFLOW_PATTERNS` and `NON_OVERFLOW_PATTERNS` from
//! `packages/ai/src/utils/overflow.ts` of Pi, Copyright (c) 2025 Mario
//! Zechner, MIT License (https://github.com/earendil-works/pi). Pi matches
//! regular expressions; this crate has no regex engine, so each pattern is a
//! small template over the lowercased message: literals, runs of digits (with
//! or without thousands commas), optional whitespace and gaps (`.*`).

/// One element of a pattern.
#[derive(Clone, Copy)]
enum Part {
    /// A literal.
    Lit(&'static str),
    /// One of several literals; an empty alternative makes the part optional.
    Any(&'static [&'static str]),
    /// `\d+`.
    Digits,
    /// `[\d,]+`.
    DigitsComma,
    /// `\s*`.
    Space,
    /// `.*`.
    Gap,
}

use Part::{Any, Digits, DigitsComma, Gap, Lit, Space};

/// Context-window overflow messages of the providers Pi knows, matched
/// anywhere in the message, case-insensitively.
const OVERFLOW_PATTERNS: &[&[Part]] = &[
    // Anthropic and z.ai token overflow.
    &[Lit("prompt "), Any(&["is ", ""]), Lit("too long")],
    // z.ai CN endpoint.
    &[Lit("prompt exceeds max length")],
    // Anthropic request byte-size overflow (HTTP 413).
    &[Lit("request_too_large")],
    // Amazon Bedrock.
    &[Lit("input is too long for requested model")],
    // OpenAI (Completions and Responses).
    &[Lit("exceeds the context window")],
    // OpenAI-compatible proxies (LiteLLM), with the limit in words.
    &[
        Lit("exceeds "),
        Any(&["the ", ""]),
        Any(&["model's ", "models ", ""]),
        Lit("maximum context length of "),
        DigitsComma,
        Lit(" token"),
    ],
    // The same with the limit in parentheses.
    &[
        Lit("exceeds "),
        Any(&["the ", ""]),
        Any(&["model's ", "models ", ""]),
        Lit("maximum context length"),
        Space,
        Lit("("),
        DigitsComma,
        Lit(")"),
    ],
    // Google (Gemini).
    &[Lit("input token count"), Gap, Lit("exceeds the maximum")],
    // xAI (Grok).
    &[Lit("maximum prompt length is "), Digits],
    // Groq.
    &[Lit("reduce the length of the messages")],
    // OpenRouter (most backends).
    &[Lit("maximum context length is "), Digits, Lit(" tokens")],
    // OpenRouter and Poolside.
    &[
        Lit("exceeds "),
        Any(&["the ", ""]),
        Lit("maximum allowed input length of "),
        DigitsComma,
        Lit(" token"),
    ],
    // Together AI.
    &[
        Lit("input ("),
        Digits,
        Lit(" tokens) is longer than the model"),
        Any(&["'s", "s", ""]),
        Lit(" context length ("),
        Digits,
        Lit(" tokens)"),
    ],
    // GitHub Copilot.
    &[Lit("exceeds the limit of "), Digits],
    // llama.cpp server.
    &[Lit("exceeds the available context size")],
    // LM Studio.
    &[Lit("greater than the context length")],
    // MiniMax.
    &[Lit("context window exceeds limit")],
    // Kimi For Coding.
    &[Lit("exceeded model token limit")],
    // Mistral.
    &[
        Lit("too large for model with "),
        Digits,
        Lit(" maximum context length"),
    ],
    // DS4 server.
    &[
        Lit("prompt has "),
        DigitsComma,
        Lit(" token"),
        Any(&["s", ""]),
        Lit(", but the configured context size is "),
        DigitsComma,
        Lit(" token"),
    ],
    // z.ai finish reason surfaced as error text.
    &[Lit("model_context_window_exceeded")],
    // Ollama.
    &[
        Lit("prompt too long; exceeded "),
        Any(&["max ", ""]),
        Lit("context length"),
    ],
    // DashScope and Qwen Token Plan.
    &[Lit("range of input length should be")],
    // Generic fallbacks.
    &[
        Lit("context"),
        Any(&["_", " "]),
        Lit("length"),
        Any(&["_", " "]),
        Lit("exceeded"),
    ],
    &[Lit("too many tokens")],
    &[Lit("token limit exceeded")],
];

/// Messages that are never overflow even when a pattern above matches: rate
/// limiting and throttling ("Too many tokens, please wait" from Bedrock).
const NON_OVERFLOW_PATTERNS: &[&[Part]] = &[&[Lit("rate limit")], &[Lit("too many requests")]];

/// Prefixes of AWS Bedrock's human-readable non-overflow errors, anchored at
/// the start of the message.
const NON_OVERFLOW_PREFIXES: &[&str] = &["throttling error:", "service unavailable:"];

fn match_here(text: &str, parts: &[Part]) -> bool {
    let Some((first, rest)) = parts.split_first() else {
        return true;
    };
    match first {
        Lit(literal) => text
            .strip_prefix(literal)
            .is_some_and(|tail| match_here(tail, rest)),
        Any(alternatives) => alternatives.iter().any(|literal| {
            text.strip_prefix(literal)
                .is_some_and(|tail| match_here(tail, rest))
        }),
        Digits | DigitsComma => {
            let accepts = |character: char| {
                character.is_ascii_digit() || (matches!(first, DigitsComma) && character == ',')
            };
            let run = text
                .find(|character| !accepts(character))
                .unwrap_or(text.len());
            // Backtrack from the longest run; the run needs at least one
            // character, as in `\d+`.
            (1..=run)
                .rev()
                .any(|length| match_here(&text[length..], rest))
        }
        Space => {
            let run = text
                .find(|character: char| !character.is_whitespace())
                .unwrap_or(text.len());
            (0..=run)
                .rev()
                .filter(|&length| text.is_char_boundary(length))
                .any(|length| match_here(&text[length..], rest))
        }
        Gap => text
            .char_indices()
            .map(|(index, _)| index)
            .chain(std::iter::once(text.len()))
            .any(|index| match_here(&text[index..], rest)),
    }
}

/// Whether `pattern` matches somewhere in `text`.
fn contains_match(text: &str, pattern: &[Part]) -> bool {
    text.char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(text.len()))
        .any(|index| match_here(&text[index..], pattern))
}

/// Pi's `isContextOverflow` over an error message: an overflow pattern matches
/// and no non-overflow pattern does.
pub(super) fn is_context_overflow_message(message: &str) -> bool {
    let lower = message.to_lowercase();
    if NON_OVERFLOW_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
        || NON_OVERFLOW_PATTERNS
            .iter()
            .any(|pattern| contains_match(&lower, pattern))
    {
        return false;
    }
    OVERFLOW_PATTERNS
        .iter()
        .any(|pattern| contains_match(&lower, pattern))
}

#[cfg(test)]
mod tests {
    use super::is_context_overflow_message;

    // The example messages in Pi's overflow.ts, one per provider.
    #[test]
    fn matches_the_documented_provider_messages() {
        for message in [
            "prompt is too long: 213462 tokens > 200000 maximum",
            "413 {\"error\":{\"type\":\"request_too_large\",\"message\":\"Request exceeds the maximum size\"}}",
            "Your input exceeds the context window of this model",
            "Requested token count exceeds the model's maximum context length of 131072 tokens",
            "Input length (265330) exceeds model's maximum context length (262144).",
            "The input token count (1196265) exceeds the maximum number of tokens allowed (1048575)",
            "This model's maximum prompt length is 131072 but the request contains 537812 tokens",
            "Please reduce the length of the messages or completion",
            "This endpoint's maximum context length is 8192 tokens. However, you requested about 9000 tokens",
            "Input length 9000 exceeds the maximum allowed input length of 8,192 tokens.",
            "The input (9000 tokens) is longer than the model's context length (8192 tokens).",
            "the request exceeds the available context size, try increasing it",
            "tokens to keep from the initial prompt is greater than the context length",
            "prompt token count of 9000 exceeds the limit of 8192",
            "invalid params, context window exceeds limit",
            "Your request exceeded model token limit: 262144 (requested: 300000)",
            "Prompt has 9000 tokens, but the configured context size is 8192 tokens",
            "Prompt contains 9000 tokens ... too large for model with 8192 maximum context length",
            "{\"code\":\"1261\",\"message\":\"Prompt too long\"}",
            "Prompt exceeds max length",
            "Range of input length should be [1, 129024]",
            "prompt too long; exceeded max context length by 1024 tokens",
            "input is too long for requested model",
            "model_context_window_exceeded",
            "context_length_exceeded",
            "Context length exceeded",
            "too many tokens in the request",
            "token limit exceeded",
        ] {
            assert!(is_context_overflow_message(message), "{message}");
        }
    }

    #[test]
    fn throttling_and_rate_limits_are_not_overflow() {
        for message in [
            "Throttling error: Too many tokens, please wait before trying again.",
            "Service unavailable: too many tokens",
            "Rate limit reached: too many tokens per minute",
            "429 Too Many Requests: token limit exceeded",
            "Internal server error",
            "invalid api key",
            "max_tokens must be at least 1",
        ] {
            assert!(!is_context_overflow_message(message), "{message}");
        }
    }

    #[test]
    fn numbers_are_required_where_pi_requires_them() {
        assert!(!is_context_overflow_message(
            "maximum prompt length is unknown"
        ));
        assert!(!is_context_overflow_message("exceeds the limit of many"));
        assert!(!is_context_overflow_message(
            "maximum context length is 8192 words"
        ));
    }
}
