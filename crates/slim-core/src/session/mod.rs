mod attempts;
mod branch_v2;
mod capabilities;
mod effects;
mod event_log;
mod index;
mod inspection;
mod jsonl_repo;
mod manual_drive;
mod manual_journal;
mod memory_repo;
mod queue;
mod recovery;
mod reducer;
mod repository;
mod resume;
mod schema_v2;
mod tool_phases;
mod transcript;

use serde::{Deserialize, Serialize};

pub use crate::context::CompactionReason;
pub use attempts::{
    Attempt, AttemptErrorClass, AttemptLedger, AttemptLedgerError, RetryAttempt, RetryPlanError,
};
pub use branch_v2::{
    branch_durable_v2, branch_v2, create_durable_branch, create_durable_branch_compacted,
    DurableBranch,
};
pub use capabilities::{
    AuthorizationGrant, AuthorizationRequirement, CapabilityCatalog, CapabilityDescriptor,
    CapabilityDispatch, CapabilityDispatcher, CapabilityExecutionState, CapabilityKind,
    CapabilityLedgerError, CapabilityRequest, CapabilitySelection, CapabilityService,
    CapabilityTerminal, ChildPromotion, ChildRequest, DurableChildStatus, DurableRepoLike,
    TaskGoalAssurance, TaskMutation, TaskMutationRequest, TaskTodoStatus,
    CAPABILITY_SCHEMA_VERSION, MAX_ACTIVE_CHILDREN, MAX_BASE_ID_BYTES, MAX_CAPABILITY_ID_BYTES,
    MAX_CAPABILITY_QUEUE, MAX_CHILD_DEPTH, MAX_CHILD_QUEUE, MAX_DISCOVERED_SKILLS, MAX_FACT_BYTES,
    MAX_MCP_CATALOG_ENTRIES,
};
pub use effects::{planned_provider_effects, Effect, EffectId, EffectPlanError};
pub use event_log::SessionWriter;
pub use index::SessionIndex;
pub use inspection::{inspect_session, SessionFormat, SessionInspection};
pub use jsonl_repo::JsonlRepo;
pub use manual_drive::{
    drive_manual, drive_manual_async, restore_manual_run, ConflictKind, ManualDrive,
    ManualDriveError, ManualDriver, ManualExecutor, ManualRunSpec, ProviderResponse,
};
pub use manual_journal::ManualRunJournal;
pub use memory_repo::MemoryRepo;
pub use queue::{DurableQueue, DurableQueueError, QueueItem, QueueStatus};
pub use recovery::{branch, recover, RecoveredSession};
pub use reducer::{reduce, restore_records, DurableState, ReduceError};
pub use repository::DurableRepo;
pub use resume::{
    open_resume_v2, preflight, preflight_session, recover_durable_v2, recover_v2, resume_plan,
    resume_plan_from_path, resume_plan_from_preflight, PreflightStatus, ResumePlan,
    ResumePlanError, SessionPreflight, SessionSummary,
};
pub use schema_v2::{
    CompactionCheckpoint, DurableEntry, DurableEntryRole, DurableErrorClass, DurableFact,
    DurableOperation, DurableOperationKind, DurableOutcome, DurableRecord, DurableSessionHeader,
    DurableUsage, ReplayPolicy, RetryPolicy, DURABLE_SCHEMA_VERSION,
};
pub use schema_v2::{
    MAX_COMPACTION_FILES_BYTES, MAX_COMPACTION_FILES_PER_CLASS, MAX_COMPACTION_PATH_BYTES,
    MAX_COMPACTION_SUMMARY_BYTES, MAX_DURABLE_SESSION_BYTES, MAX_TOOL_BATCH_LIMIT,
    MAX_TOOL_INLINE_BYTES, MAX_TOOL_METADATA_BYTES,
};
pub use tool_phases::{
    ReplayDisposition, ReplayItem, ReplayPlan, ToolBatch, ToolCallState, ToolOutput,
    ToolPhaseError, ToolPhaseLedger,
};
pub use transcript::{
    provider_messages_from_entries, provider_messages_from_records, recovery_tool_results,
};

pub(crate) const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionHeader {
    #[serde(rename = "type")]
    pub record_type: String,
    pub schema_version: u32,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    pub parent_id: Option<String>,
    pub cutoff_seq: Option<u64>,
}

impl SessionHeader {
    pub(crate) fn new(
        id: impl Into<String>,
        cwd: impl Into<String>,
        parent_id: Option<String>,
        cutoff_seq: Option<u64>,
    ) -> Self {
        Self {
            record_type: "session".into(),
            schema_version: CURRENT_SCHEMA_VERSION,
            id: id.into(),
            timestamp: "unknown".into(),
            cwd: cwd.into(),
            parent_id,
            cutoff_seq,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
pub(crate) enum SessionLine {
    #[serde(rename = "session")]
    Session {
        schema_version: u32,
        id: String,
        timestamp: String,
        cwd: String,
        parent_id: Option<String>,
        cutoff_seq: Option<u64>,
    },
    #[serde(rename = "event")]
    Event {
        seq: u64,
        event: crate::events::SessionEvent,
    },
}

impl From<SessionHeader> for SessionLine {
    fn from(header: SessionHeader) -> Self {
        Self::Session {
            schema_version: header.schema_version,
            id: header.id,
            timestamp: header.timestamp,
            cwd: header.cwd,
            parent_id: header.parent_id,
            cutoff_seq: header.cutoff_seq,
        }
    }
}

impl SessionLine {
    pub(crate) fn header(self) -> Option<SessionHeader> {
        match self {
            Self::Session {
                schema_version,
                id,
                timestamp,
                cwd,
                parent_id,
                cutoff_seq,
            } => Some(SessionHeader {
                record_type: "session".into(),
                schema_version,
                id,
                timestamp,
                cwd,
                parent_id,
                cutoff_seq,
            }),
            Self::Event { .. } => None,
        }
    }
}
