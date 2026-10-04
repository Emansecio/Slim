use std::io;
use std::path::{Path, PathBuf};

use super::jsonl_repo::JsonlRepo;
use super::resume::{preflight_session, PreflightStatus};
use super::schema_v2::{DurableRecord, DurableSessionHeader};
use super::SessionFormat;
use crate::context::{
    apply_compaction, compaction_prefix_fingerprint, estimate_context_tokens,
    fit_summary_for_persistence, prepare_compaction, CompactionPolicy, CompactionReason,
    ContextUsage, SummaryRequest,
};

/// Metadata returned by the richer branch API. The path-returning
/// `branch_v2` is kept symmetrical with the existing v1 helper; callers that
/// need the sequence contract can use this API and receive `next_seq` too.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableBranch {
    pub path: PathBuf,
    pub parent_id: String,
    pub cutoff_seq: u64,
    pub next_seq: u64,
}

/// Copy a confirmed v2 prefix into a new durable child without repairing or
/// changing the parent. The child header records the parent and explicit
/// cutoff, and the first valid append sequence is exactly cutoff+1.
pub fn branch_v2(path: impl AsRef<Path>, child_id: &str, cutoff_seq: u64) -> io::Result<PathBuf> {
    Ok(create_durable_branch(path, child_id, cutoff_seq)?.path)
}

pub fn branch_durable_v2(
    path: impl AsRef<Path>,
    child_id: &str,
    cutoff_seq: u64,
) -> io::Result<DurableBranch> {
    create_durable_branch(path, child_id, cutoff_seq)
}

pub fn create_durable_branch(
    path: impl AsRef<Path>,
    child_id: &str,
    cutoff_seq: u64,
) -> io::Result<DurableBranch> {
    let prepared = prepare_durable_branch(path, child_id, cutoff_seq)?;
    drop(JsonlRepo::create_with_records(
        &prepared.branch.path,
        prepared.header,
        prepared.records,
    )?);
    Ok(prepared.branch)
}

struct PreparedBranch {
    branch: DurableBranch,
    header: DurableSessionHeader,
    records: Vec<DurableRecord>,
}

fn prepare_durable_branch(
    path: impl AsRef<Path>,
    child_id: &str,
    cutoff_seq: u64,
) -> io::Result<PreparedBranch> {
    validate_child_id(child_id)?;
    let report = preflight_session(path)?;
    if report.format != Some(SessionFormat::DurableV2) {
        return Err(invalid_input(
            "v2 branch requires a durable schema-v2 parent",
        ));
    }
    match report.status {
        PreflightStatus::Healthy => {}
        PreflightStatus::TornTail { .. } => {
            return Err(invalid_input(
                "branch requires explicit recovery before copying a torn prefix",
            ));
        }
        PreflightStatus::Invalid { .. } | PreflightStatus::UnsupportedSchema { .. } => {
            return Err(invalid_input("branch source is invalid"));
        }
    }
    if report.needs_separator {
        return Err(invalid_input(
            "branch requires explicit recovery before appending a separator",
        ));
    }
    let parent_id = report
        .session_id
        .clone()
        .ok_or_else(|| invalid_input("durable parent id is missing"))?;
    let first_seq = report.records.first().map(DurableRecord::seq);
    let last_seq = report.last_seq.ok_or_else(|| {
        invalid_input("branch cutoff is out of range for an empty durable session")
    })?;
    let next_seq = cutoff_seq
        .checked_add(1)
        .ok_or_else(|| invalid_input("branch cutoff successor overflowed"))?;
    if cutoff_seq < first_seq.unwrap_or(cutoff_seq) || cutoff_seq > last_seq {
        return Err(invalid_input(
            "branch cutoff is outside the confirmed source range",
        ));
    }
    if !report
        .records
        .iter()
        .any(|record| record.seq() == cutoff_seq)
    {
        return Err(invalid_input(
            "branch cutoff must identify a confirmed source record",
        ));
    }
    let parent_path = report.path;
    let parent_directory = parent_path
        .parent()
        .ok_or_else(|| invalid_input("durable parent has no directory"))?;
    let stem = parent_path
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| invalid_input("durable parent filename is invalid"))?;
    let child_path = parent_directory.join(format!("{stem}-{child_id}.jsonl"));
    if child_path.parent() != Some(parent_directory) {
        return Err(invalid_input(
            "durable child path escaped the parent directory",
        ));
    }

    let header = report
        .header
        .ok_or_else(|| invalid_input("durable parent header is missing"))?;
    let child_header = DurableSessionHeader::new(
        child_id,
        header.timestamp,
        header.cwd,
        Some(parent_id.clone()),
        Some(cutoff_seq),
    );
    let records = report
        .records
        .into_iter()
        .filter(|record| record.seq() <= cutoff_seq)
        .collect();
    Ok(PreparedBranch {
        branch: DurableBranch {
            path: child_path,
            parent_id,
            cutoff_seq,
            next_seq,
        },
        header: child_header,
        records,
    })
}

/// Prepare and summarize the confirmed prefix, then publish child and checkpoint
/// together. A failed summary leaves the requested child id free for retry.
///
/// The summary is Pi's: `summarize` answers each [`SummaryRequest`] (the
/// history summary and, when the cut splits a turn, the summary of that turn's
/// prefix) with the model's text. An earlier checkpoint of the session
/// is the starting point and its summary the previous summary.
pub async fn create_durable_branch_compacted<F, Fut>(
    path: impl AsRef<Path>,
    child_id: &str,
    cutoff_seq: u64,
    mut summarize: F,
) -> io::Result<DurableBranch>
where
    F: FnMut(SummaryRequest) -> Fut,
    Fut: std::future::Future<Output = io::Result<String>>,
{
    let mut prepared = prepare_durable_branch(path, child_id, cutoff_seq)?;
    // The history the model would see: the latest applied checkpoint's summary
    // followed by what that checkpoint kept (the same rebuild resume uses, so
    // the new checkpoint chains onto one that really applies).
    let rebuilt = super::rebuild_provider_history(&prepared.records)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let (history, history_ids) = (rebuilt.messages, rebuilt.entry_ids);
    let policy = CompactionPolicy::default();
    let preparation = prepare_compaction(&history, &policy.settings(), ContextUsage::default())
        .ok_or_else(|| invalid_input("nothing to compact"))?;
    let first_kept_entry_id = history_ids
        .get(preparation.first_kept_index)
        .cloned()
        .flatten()
        .ok_or_else(|| invalid_input("compaction kept no durable entry"))?;
    let requests = preparation.summary_requests(None);
    let started = std::time::Instant::now();
    let history_text = match requests.history {
        Some(request) => Some(summarize(request).await?),
        None => None,
    };
    let turn_prefix_text = match requests.turn_prefix {
        Some(request) => Some(summarize(request).await?),
        None => None,
    };
    let text = preparation
        .assemble_summary(history_text.as_deref(), turn_prefix_text.as_deref())
        .map_err(invalid_input)?;
    if text.trim().is_empty() {
        return Err(invalid_input("branch compaction summary is invalid"));
    }
    let fitted =
        fit_summary_for_persistence(&text, &preparation.file_ops, policy.summary_max_bytes)
            .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
    let seq = prepared.branch.next_seq;
    let following_seq = seq
        .checked_add(1)
        .ok_or_else(|| invalid_input("branch checkpoint successor overflowed"))?;
    let previous_checkpoint_id = rebuilt.applied_checkpoint_id;
    let tokens_after = estimate_context_tokens(
        &apply_compaction(&history, preparation.first_kept_index, &fitted.summary),
        ContextUsage::default(),
    )
    .tokens;
    prepared.records.push(DurableRecord::Compaction {
        seq,
        checkpoint: super::schema_v2::CompactionCheckpoint {
            checkpoint_id: format!("compact-{child_id}-{seq}"),
            summary: fitted.summary,
            first_kept_entry_id,
            prefix_fingerprint: compaction_prefix_fingerprint(
                &history[..preparation.first_kept_index],
            ),
            previous_checkpoint_id,
            tokens_before: preparation.tokens_before,
            tokens_after,
            input_tokens: None,
            output_tokens: None,
            duration_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            reason: CompactionReason::Branch,
            read_files: fitted.files.read_files,
            modified_files: fitted.files.modified_files,
        },
    });
    drop(JsonlRepo::create_with_records(
        &prepared.branch.path,
        prepared.header,
        prepared.records,
    )?);
    prepared.branch.next_seq = following_seq;
    Ok(prepared.branch)
}

pub(crate) fn validate_child_id(child_id: &str) -> io::Result<()> {
    if child_id.is_empty() || child_id == "." || child_id == ".." {
        return Err(invalid_input(
            "child id must be non-empty and non-traversal",
        ));
    }
    if !child_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(invalid_input(
            "child id contains path or unsupported characters",
        ));
    }
    Ok(())
}

fn invalid_input(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
