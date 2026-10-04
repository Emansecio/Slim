use super::*;

pub(super) fn push_runtime_event(
    app: &mut AppHandle,
    next_seq: &mut u64,
    kind: crate::EventKind,
) -> Result<(), ProviderError> {
    let persistence = if let Some(journal) = &app.run_journal {
        journal
            .lock()
            .map_err(|_| journal_error("durable run lock poisoned"))
            .and_then(|mut journal| journal.record_event(&kind).map_err(journal_error))
    } else {
        Ok(())
    };
    app.push_event(crate::SessionEvent::new(*next_seq, kind))
        .map_err(|message| ProviderError::InvalidResponse {
            message: message.into(),
        })?;
    *next_seq = checked_next_seq(*next_seq)?;
    persistence?;
    Ok(())
}

pub(super) fn push_tool_started_notice(
    app: &mut AppHandle,
    next_seq: &mut u64,
    batch_id: &str,
    calls: &[ProviderToolCall],
    notice: ToolStartedNotice,
) -> Result<(), ProviderError> {
    let Some(call) = calls.get(notice.index) else {
        return Err(ProviderError::InvalidResponse {
            message: format!("tool start notice index {} is out of range", notice.index),
        });
    };
    push_runtime_event(
        app,
        next_seq,
        crate::EventKind::ToolStarted {
            batch_id: batch_id.to_owned(),
            call_id: call.id.clone(),
            name: call.name.clone(),
            arguments: notice.arguments,
        },
    )
}

pub(super) fn drain_tool_started_notices(
    app: &mut AppHandle,
    next_seq: &mut u64,
    batch_id: &str,
    calls: &[ProviderToolCall],
    started_rx: &mut tokio::sync::mpsc::Receiver<ToolStartedNotice>,
) -> Result<(), ProviderError> {
    while let Ok(notice) = started_rx.try_recv() {
        push_tool_started_notice(app, next_seq, batch_id, calls, notice)?;
    }
    Ok(())
}

/// Structured facts that follow a call's `ToolOutput`: the backing process
/// and, for a successful patch, its display diff (already redacted).
pub(super) fn push_tool_result_facts(
    app: &mut AppHandle,
    next_seq: &mut u64,
    batch_id: &str,
    call_id: &str,
    name: &str,
    process: Option<&crate::process::ProcessExecutionFacts>,
    edit_diff: Option<crate::ToolEditDiff>,
) -> Result<(), ProviderError> {
    if let Some(process) = process {
        push_runtime_event(
            app,
            next_seq,
            crate::EventKind::ToolProcessFinished {
                batch_id: batch_id.to_owned(),
                call_id: call_id.to_owned(),
                name: name.to_owned(),
                process: process.clone(),
            },
        )?;
    }
    if let Some(diff) = edit_diff {
        push_runtime_event(
            app,
            next_seq,
            crate::EventKind::ToolEditApplied {
                batch_id: batch_id.to_owned(),
                call_id: call_id.to_owned(),
                name: name.to_owned(),
                diff,
            },
        )?;
    }
    Ok(())
}

/// Replace only the exact admission prefix generated for a known prepared
/// invocation. This lets an in-batch evidence alias carry the current call's
/// notes without treating arbitrary tool output as a marker.
pub(super) fn replace_admission_prefix(output: &mut String, from: &[String], to: &[String]) {
    if let Some(prefix) = crate::tools::admission_output_prefix(from) {
        if output.starts_with(&prefix) {
            output.drain(..prefix.len());
        }
    }
    if let Some(prefix) = crate::tools::admission_output_prefix(to) {
        output.insert_str(0, &prefix);
    }
}

pub(super) fn journal_error(error: impl std::fmt::Display) -> ProviderError {
    ProviderError::InvalidResponse {
        message: error.to_string(),
    }
}

pub(crate) fn persist_provider_call(
    journal: &Option<Arc<Mutex<crate::session::ManualRunJournal>>>,
    telemetry: ProviderCallTelemetry,
) -> Result<Option<String>, ProviderError> {
    let Some(journal) = journal else {
        return Ok(None);
    };
    journal
        .lock()
        .map_err(|_| journal_error("durable run lock poisoned"))?
        .record_provider_call(&telemetry)
        .map(Some)
        .map_err(journal_error)
}

pub(crate) fn persist_tool_batch(
    journal: &Option<Arc<Mutex<crate::session::ManualRunJournal>>>,
    batch_id: &str,
    calls: &[crate::provider::ProviderToolCall],
    wall_ms: u64,
) -> Result<(), ProviderError> {
    let Some(journal) = journal else {
        return Ok(());
    };
    let call_ids: Vec<&str> = calls.iter().map(|call| call.id.as_str()).collect();
    journal
        .lock()
        .map_err(|_| journal_error("durable run lock poisoned"))?
        .record_tool_batch(batch_id, &call_ids, wall_ms)
        .map_err(journal_error)
}

pub(crate) fn persist_provider_validation_failure(
    journal: &Option<Arc<Mutex<crate::session::ManualRunJournal>>>,
    provider_call_id: Option<&str>,
    error: &ProviderError,
) -> Result<(), ProviderError> {
    let (Some(journal), Some(provider_call_id)) = (journal, provider_call_id) else {
        return Ok(());
    };
    journal
        .lock()
        .map_err(|_| journal_error("durable run lock poisoned"))?
        .record_provider_validation_failure(provider_call_id, error)
        .map_err(journal_error)
}

pub(super) fn push_runtime_transient_event(
    app: &mut AppHandle,
    next_seq: &mut u64,
    kind: crate::EventKind,
) -> Result<(), ProviderError> {
    app.push_transient_event(crate::SessionEvent::new(*next_seq, kind))
        .map_err(|message| ProviderError::InvalidResponse {
            message: message.into(),
        })?;
    *next_seq = checked_next_seq(*next_seq)?;
    Ok(())
}

pub(super) fn checked_next_seq(seq: u64) -> Result<u64, ProviderError> {
    seq.checked_add(1)
        .ok_or_else(|| ProviderError::InvalidResponse {
            message: "event sequence overflow".into(),
        })
}

pub(super) fn usage_since(app: &AppHandle, start: usize) -> UsageTotals {
    UsageTotals::from_events(app.events().get(start..).unwrap_or_default(), false)
}

pub(super) fn duration_millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub(super) fn elapsed_millis(started: Instant) -> u64 {
    duration_millis(started.elapsed())
}

pub(super) fn elapsed_micros(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

pub(super) fn assistant_text_since(app: &AppHandle, event_start: usize) -> String {
    app.events()
        .get(event_start..)
        .unwrap_or_default()
        .iter()
        .filter_map(|event| match &event.kind {
            crate::EventKind::AssistantTextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

pub(super) fn tool_calls_since(app: &AppHandle, event_start: usize) -> Vec<ProviderToolCall> {
    app.events()
        .get(event_start..)
        .unwrap_or_default()
        .iter()
        .filter_map(|event| match &event.kind {
            crate::EventKind::ProviderToolCall {
                id,
                name,
                arguments,
            } => Some(ProviderToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            }),
            crate::EventKind::ToolCall { name, arguments } => Some(ProviderToolCall {
                id: String::new(),
                name: name.clone(),
                arguments: arguments.clone(),
            }),
            _ => None,
        })
        .collect()
}
