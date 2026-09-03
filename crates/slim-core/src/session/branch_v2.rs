use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::jsonl_repo::JsonlRepo;
use super::repository::DurableRepo;
use super::resume::{preflight_session, PreflightStatus};
use super::schema_v2::DurableRecord;
use super::SessionFormat;
use crate::context::{
    build_summary_prompt_with_checkpoint, compaction_prefix_fingerprint,
    estimate_provider_message_tokens, select_compaction_history, CompactionPolicy,
    CompactionReason,
};
use crate::provider::ProviderMessage;

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
    let child_header = super::schema_v2::DurableSessionHeader::new(
        child_id,
        header.timestamp,
        header.cwd,
        Some(parent_id.clone()),
        Some(cutoff_seq),
    );
    let mut child = JsonlRepo::create(&child_path, child_header)?;
    for record in report
        .records
        .into_iter()
        .filter(|record| record.seq() <= cutoff_seq)
    {
        if let Err(error) = child.append(record) {
            drop(child);
            let _ = fs::remove_file(&child_path);
            return Err(error);
        }
    }
    drop(child);
    Ok(DurableBranch {
        path: child_path,
        parent_id,
        cutoff_seq,
        next_seq,
    })
}

/// Create the confirmed child prefix first, then summarize its compactable
/// history through an injected offline-testable callback and append a v2
/// checkpoint. The pure branch helper remains unchanged and `/tree` is not
/// involved.
pub async fn create_durable_branch_compacted<F, Fut>(
    path: impl AsRef<Path>,
    child_id: &str,
    cutoff_seq: u64,
    summarize: F,
) -> io::Result<DurableBranch>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = io::Result<String>>,
{
    let branch = create_durable_branch(path, child_id, cutoff_seq)?;
    let report = preflight_session(&branch.path)?;
    let mut messages = Vec::new();
    let mut entry_ids = Vec::new();
    for record in &report.records {
        let DurableRecord::Entry { entry, .. } = record else {
            continue;
        };
        if entry.tool_call_id.is_some() {
            continue;
        }
        let message = match entry.role {
            super::schema_v2::DurableEntryRole::User => {
                ProviderMessage::user(entry.content.clone())
            }
            super::schema_v2::DurableEntryRole::Assistant => {
                ProviderMessage::assistant(entry.content.clone(), Vec::new())
            }
            super::schema_v2::DurableEntryRole::Tool => continue,
        };
        messages.push(message);
        entry_ids.push(entry.entry_id.clone());
    }
    let mut policy = CompactionPolicy::default();
    policy.keep_recent_tokens = policy.keep_recent_for_window(32_000);
    let Ok(selection) = select_compaction_history(&messages, &policy) else {
        return Ok(branch);
    };
    let previous = report.records.iter().rev().find_map(|record| {
        if let DurableRecord::Compaction { checkpoint, .. } = record {
            Some(checkpoint)
        } else {
            None
        }
    });
    let prompt = build_summary_prompt_with_checkpoint(
        &selection.summarized,
        previous.map(|checkpoint| checkpoint.summary.as_str()),
    );
    let started = std::time::Instant::now();
    let summary = summarize(prompt).await?;
    if summary.trim().is_empty() || summary.len() > policy.summary_max_bytes {
        return Err(invalid_input("branch compaction summary is invalid"));
    }
    let mut child = JsonlRepo::open(&branch.path)?;
    let seq = child.next_seq()?;
    let previous_checkpoint_id = previous.map(|checkpoint| checkpoint.checkpoint_id.clone());
    child.append(DurableRecord::Compaction {
        seq,
        checkpoint: super::schema_v2::CompactionCheckpoint {
            checkpoint_id: format!("compact-{child_id}-{seq}"),
            summary,
            first_kept_entry_id: entry_ids[selection.first_kept_index].clone(),
            prefix_fingerprint: compaction_prefix_fingerprint(&selection.summarized),
            previous_checkpoint_id,
            tokens_before: estimate_provider_message_tokens(&messages),
            tokens_after: selection.recent_tokens,
            input_tokens: None,
            output_tokens: None,
            duration_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            reason: CompactionReason::Branch,
            read_files: Vec::new(),
            modified_files: Vec::new(),
        },
    })?;
    Ok(branch)
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
