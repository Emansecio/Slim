use super::observation::PendingCall;
use super::EvidenceOutcome;
use crate::tools::{
    hash_fields, path_identity, DependencyObservation, ToolCacheability, ToolExecutionReceipt,
    ToolOperationalSpec, ToolReplayPolicy, ToolResult,
};

const MAX_EVENT_IDENTIFIER_BYTES: usize = 256;

/// Digest of a domain tag followed by an ordered list of items.
pub(super) fn hash_tagged<I>(tag: &[u8], items: I) -> String
where
    I: IntoIterator,
    I::Item: AsRef<[u8]>,
{
    let owned = items.into_iter().collect::<Vec<_>>();
    let mut fields: Vec<&[u8]> = Vec::with_capacity(owned.len() + 1);
    fields.push(tag);
    fields.extend(owned.iter().map(AsRef::as_ref));
    hash_fields(&fields)
}

pub(super) fn observations_digest(observations: &[DependencyObservation]) -> String {
    hash_tagged(
        b"slim-receipt-dependencies-v1",
        observations
            .iter()
            .map(|observation| format!("{}\0{}", observation.key(), observation.stamp.digest())),
    )
}

pub(super) fn mutations_digest(receipt: &ToolExecutionReceipt) -> String {
    hash_tagged(
        b"slim-receipt-mutations-v1",
        receipt.mutations.iter().map(|mutation| {
            format!(
                "{}\0{}\0{}",
                path_identity(&mutation.path),
                mutation.before_content_digest.as_deref().unwrap_or(""),
                mutation.after.digest()
            )
        }),
    )
}

/// Whether an outcome can count as evidence for this call: failures and green
/// validations always; other results only for replayable evidence tools.
pub(super) fn evidence_applicable(pending: &PendingCall, outcome: EvidenceOutcome) -> bool {
    matches!(
        outcome,
        EvidenceOutcome::Failure | EvidenceOutcome::ValidationGreen
    ) || pending.spec.is_some_and(|spec| {
        spec.cacheability == ToolCacheability::Evidence
            && spec.replay_policy == ToolReplayPolicy::EquivalentEvidence
    })
}

/// Key under which an evidence result was already seen. A validation is also
/// keyed by its call, so different commands never share one.
pub(super) fn seen_key(evidence_id: &str, pending: &PendingCall, validation: bool) -> String {
    let tag: &[u8] = if validation {
        b"slim-causal-seen-validation-v1"
    } else {
        b"slim-causal-seen-evidence-v1"
    };
    let fields = [
        tag,
        evidence_id.as_bytes(),
        pending.evidence_scope.as_bytes(),
        pending.call_fingerprint.as_bytes(),
    ];
    hash_fields(if validation {
        &fields[..]
    } else {
        &fields[..3]
    })
}

pub(super) fn evidence_scope(
    spec: ToolOperationalSpec,
    dependency_digest: &str,
    workspace_revision: u64,
    uncertainty_epoch: u64,
) -> String {
    hash_fields(&[
        b"slim-causal-evidence-scope-v2",
        format!("{:?}", spec.dependency_scope).as_bytes(),
        dependency_digest.as_bytes(),
        workspace_revision.to_string().as_bytes(),
        uncertainty_epoch.to_string().as_bytes(),
    ])
}

pub(super) fn stateful_call_fingerprint(
    canonical_fingerprint: &str,
    dependency_digest: &str,
    uncertainty_epoch: u64,
    validation_revision: Option<u64>,
) -> String {
    hash_fields(&[
        b"slim-causal-call-v2",
        canonical_fingerprint.as_bytes(),
        dependency_digest.as_bytes(),
        uncertainty_epoch.to_string().as_bytes(),
        validation_revision
            .map(|revision| revision.to_string())
            .unwrap_or_default()
            .as_bytes(),
    ])
}

pub(super) fn evidence_id(
    tool_name: &str,
    result: &ToolResult,
    validation: bool,
    admission_prefix: Option<&str>,
) -> String {
    let success = if result.success { "success" } else { "failure" };
    let class = if validation { "validation" } else { "tool" };
    let normalized = normalize_output(
        tool_name,
        evidence_output(result, admission_prefix),
        validation,
    );
    hash_fields(&[
        b"slim-causal-evidence-v2",
        tool_name.as_bytes(),
        class.as_bytes(),
        success.as_bytes(),
        normalized.as_bytes(),
    ])
}

pub(super) fn evidence_output<'a>(
    result: &'a ToolResult,
    admission_prefix: Option<&str>,
) -> &'a str {
    // Strip only this invocation's generated metadata, never a marker guessed
    // from file content. Presentation choices must not count as new evidence.
    admission_prefix
        .and_then(|prefix| result.output.strip_prefix(prefix))
        .unwrap_or(&result.output)
}

pub(super) fn validation_green(result: &ToolResult, admission_prefix: Option<&str>) -> bool {
    result.success
        && evidence_output(result, admission_prefix)
            .lines()
            .next()
            .is_some_and(|line| line == "exit 0")
}

/// Output with volatile parts (trailing whitespace, elapsed times, outer blank
/// lines) removed, so equivalent results hash alike.
pub(super) fn normalize_output(tool_name: &str, output: &str, validation: bool) -> String {
    // `lines` already treats `\r\n` as a line ending; a stray `\r` is
    // whitespace removed by `trim_end`.
    let mut normalized = String::with_capacity(output.len());
    for line in output.lines() {
        normalized.push_str(strip_volatile_suffix(
            tool_name,
            line.trim_end(),
            validation,
        ));
        normalized.push('\n');
    }
    normalized.truncate(normalized.trim_end().len());
    let leading = normalized.len() - normalized.trim_start().len();
    normalized.drain(..leading);
    normalized
}

pub(super) fn strip_volatile_suffix<'a>(
    tool_name: &str,
    line: &'a str,
    validation: bool,
) -> &'a str {
    if tool_name == "code_intel" {
        if let Some((prefix, elapsed)) = line.rsplit_once(" | ") {
            if elapsed.strip_suffix("ms").is_some_and(|value| {
                !value.is_empty() && value.chars().all(|character| character.is_ascii_digit())
            }) {
                return prefix;
            }
        }
    }
    if validation {
        if let Some((prefix, _)) = line.rsplit_once("; finished in ") {
            return prefix;
        }
        if line.trim_start().starts_with("Finished ") {
            if let Some((prefix, _)) = line.rsplit_once(" in ") {
                return prefix;
            }
        }
    }
    line
}

pub(super) fn bounded_identifier(value: &str) -> String {
    if value.len() <= MAX_EVENT_IDENTIFIER_BYTES {
        value.to_owned()
    } else {
        format!(
            "sha256:{}",
            hash_fields(&[b"slim-causal-identifier-v1", value.as_bytes()])
        )
    }
}
