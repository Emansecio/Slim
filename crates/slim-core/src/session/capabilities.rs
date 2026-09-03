//! Durable capability dispatch contracts for the v2 harness.
//!
//! This module deliberately stops at a local dispatcher boundary.  It records
//! only capability identity, authorization and terminal status; arguments,
//! prompts, credentials and dispatcher output are never written to the
//! session.  Restoring a service rebuilds state but does not invoke anything.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agents::ChildStatus;
use crate::mcp::{canonical_name, McpCatalog};
use crate::skills::{DiscoveryResult, SkillEntry};
use crate::tools::ToolRegistry;
use crate::OperatingMode;

use super::repository::DurableRepo;
use super::schema_v2::{DurableFact, DurableRecord, ReplayPolicy};

pub const CAPABILITY_SCHEMA_VERSION: u32 = 1;
pub const MAX_CHILD_DEPTH: u8 = 1;
pub const MAX_ACTIVE_CHILDREN: usize = 4;
pub const MAX_CHILD_QUEUE: usize = 32;
pub const MAX_CAPABILITY_QUEUE: usize = 32;
pub const MAX_CAPABILITY_ID_BYTES: usize = 256;
/// Base IDs reserve room for durable suffixes and session-scoped effect IDs.
pub const MAX_BASE_ID_BYTES: usize = 124;
pub const MAX_TASK_COLLECTION: usize = 128;
pub const MAX_TASK_TEXT_BYTES: usize = 4 * 1024;
pub const MAX_FACT_BYTES: usize = 16 * 1024;
pub const MAX_DISCOVERED_SKILLS: usize = 256;
pub const MAX_MCP_CATALOG_ENTRIES: usize = 256;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    NativeTool,
    Skill,
    McpTool,
    McpResource,
    McpPrompt,
    ChildAgent,
    Todo,
    Plan,
    Goal,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizationRequirement {
    None,
    Trusted,
    Explicit,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizationGrant {
    None,
    Trusted,
    Explicit,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySelection {
    None,
    ExplicitMcpSelection,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CapabilityDescriptor {
    pub id: String,
    pub kind: CapabilityKind,
    pub allowed_modes: Vec<OperatingMode>,
    pub authorization: AuthorizationRequirement,
    pub replay_policy: ReplayPolicy,
    pub mutates_workspace: bool,
    pub selection: CapabilitySelection,
}

impl CapabilityDescriptor {
    pub fn allows_mode(&self, mode: OperatingMode) -> bool {
        self.allowed_modes.contains(&mode)
    }

    fn authorize_with_selection(
        &self,
        mode: OperatingMode,
        grant: AuthorizationGrant,
        selection_confirmed: bool,
    ) -> Result<(), CapabilityLedgerError> {
        if !self.allows_mode(mode) {
            return Err(CapabilityLedgerError::ModeDenied {
                capability_id: self.id.clone(),
                mode,
            });
        }
        let granted = match self.authorization {
            AuthorizationRequirement::None => true,
            AuthorizationRequirement::Trusted => {
                matches!(
                    grant,
                    AuthorizationGrant::Trusted | AuthorizationGrant::Explicit
                )
            }
            AuthorizationRequirement::Explicit => matches!(grant, AuthorizationGrant::Explicit),
        };
        if !granted {
            return Err(CapabilityLedgerError::AuthorizationRequired {
                capability_id: self.id.clone(),
            });
        }
        if self.selection == CapabilitySelection::ExplicitMcpSelection && !selection_confirmed {
            return Err(CapabilityLedgerError::SelectionRequired {
                capability_id: self.id.clone(),
            });
        }
        Ok(())
    }
}

/// One catalog for native tools, discoverable skills, MCP contracts and
/// state-management capabilities.  It contains metadata only; adding an
/// entry does not execute or connect to anything.
#[derive(Clone, Debug, Default)]
pub struct CapabilityCatalog {
    descriptors: BTreeMap<String, CapabilityDescriptor>,
    selected: BTreeSet<String>,
}

impl CapabilityCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_native_tools() -> Self {
        let mut catalog = Self::new();
        catalog
            .add_native_tools(&ToolRegistry::default())
            .expect("static native tool catalog");
        catalog
            .add_builtin_state_capabilities()
            .expect("static builtin capability catalog");
        catalog
    }

    pub fn add(&mut self, descriptor: CapabilityDescriptor) -> Result<(), CapabilityLedgerError> {
        validate_descriptor(&descriptor)?;
        if self.descriptors.contains_key(&descriptor.id) {
            return Err(CapabilityLedgerError::DuplicateCapability);
        }
        self.descriptors.insert(descriptor.id.clone(), descriptor);
        Ok(())
    }

    pub fn add_native_tools(
        &mut self,
        registry: &ToolRegistry,
    ) -> Result<(), CapabilityLedgerError> {
        let mut next = self.clone();
        for name in registry.names_for_mode(OperatingMode::Auto) {
            let read_only = !registry.mutates_workspace(name).unwrap_or(true);
            let allowed_modes = if read_only {
                vec![
                    OperatingMode::Auto,
                    OperatingMode::ReadOnly,
                    OperatingMode::Plan,
                ]
            } else {
                vec![OperatingMode::Auto]
            };
            // Native tool names are supplied by the static registry and are
            // therefore already bounded and trusted.
            next.add(CapabilityDescriptor {
                id: format!("tool.{name}"),
                kind: CapabilityKind::NativeTool,
                allowed_modes,
                authorization: AuthorizationRequirement::None,
                replay_policy: ReplayPolicy::Never,
                mutates_workspace: !read_only,
                selection: CapabilitySelection::None,
            })?;
        }
        *self = next;
        Ok(())
    }

    pub fn add_skill(&mut self, name: impl Into<String>) -> Result<(), CapabilityLedgerError> {
        self.add_skill_with_trust(name, false)
    }

    pub fn add_skill_with_trust(
        &mut self,
        name: impl Into<String>,
        trusted: bool,
    ) -> Result<(), CapabilityLedgerError> {
        let name = name.into();
        validate_identifier(&name)?;
        self.add(CapabilityDescriptor {
            id: format!("skill.{name}"),
            kind: CapabilityKind::Skill,
            allowed_modes: vec![OperatingMode::Auto],
            authorization: if trusted {
                AuthorizationRequirement::Trusted
            } else {
                AuthorizationRequirement::Explicit
            },
            replay_policy: ReplayPolicy::Never,
            mutates_workspace: true,
            selection: CapabilitySelection::None,
        })
    }

    pub fn add_skills(&mut self, discovery: &DiscoveryResult) -> Result<(), CapabilityLedgerError> {
        if discovery.active_entries().len() > MAX_DISCOVERED_SKILLS {
            return Err(CapabilityLedgerError::InvalidIdentifier(
                "discovered skills",
            ));
        }
        let mut next = self.clone();
        for entry in discovery.active_entries() {
            next.add_skill(entry.name.clone())?;
        }
        *self = next;
        Ok(())
    }

    pub fn add_skill_entry(&mut self, entry: &SkillEntry) -> Result<(), CapabilityLedgerError> {
        self.add_skill(entry.name.clone())
    }

    pub fn add_mcp_catalog(&mut self, catalog: &McpCatalog) -> Result<(), CapabilityLedgerError> {
        validate_identifier(catalog.server_name())?;
        let entry_count = catalog
            .tool_names()
            .len()
            .saturating_add(catalog.resource_names().len())
            .saturating_add(catalog.prompt_names().len());
        if entry_count > MAX_MCP_CATALOG_ENTRIES {
            return Err(CapabilityLedgerError::InvalidIdentifier(
                "MCP catalog entries",
            ));
        }
        let mut next = self.clone();
        for tool in catalog.tool_names() {
            validate_identifier(tool)?;
            next.add(CapabilityDescriptor {
                id: canonical_name(catalog.server_name(), tool),
                kind: CapabilityKind::McpTool,
                allowed_modes: vec![OperatingMode::Auto],
                authorization: AuthorizationRequirement::Explicit,
                replay_policy: ReplayPolicy::Never,
                mutates_workspace: true,
                selection: CapabilitySelection::None,
            })?;
        }
        // MCP resources and prompts are read/plan surfaces. They remain
        // selection contracts; this catalog never opens a transport.
        for resource in catalog.resource_names() {
            validate_identifier(resource)?;
            let id = format!("mcp.{}.resource.{}", catalog.server_name(), resource);
            next.add(CapabilityDescriptor {
                id: id.clone(),
                kind: CapabilityKind::McpResource,
                allowed_modes: vec![OperatingMode::ReadOnly, OperatingMode::Plan],
                authorization: AuthorizationRequirement::None,
                replay_policy: ReplayPolicy::Never,
                mutates_workspace: false,
                selection: CapabilitySelection::ExplicitMcpSelection,
            })?;
            if catalog
                .selected_resources()
                .iter()
                .any(|selected| selected == resource)
            {
                next.selected.insert(id);
            }
        }
        for prompt in catalog.prompt_names() {
            validate_identifier(prompt)?;
            let id = format!("mcp.{}.prompt.{}", catalog.server_name(), prompt);
            next.add(CapabilityDescriptor {
                id: id.clone(),
                kind: CapabilityKind::McpPrompt,
                allowed_modes: vec![OperatingMode::ReadOnly, OperatingMode::Plan],
                authorization: AuthorizationRequirement::None,
                replay_policy: ReplayPolicy::Never,
                mutates_workspace: false,
                selection: CapabilitySelection::ExplicitMcpSelection,
            })?;
            if catalog
                .selected_prompts()
                .iter()
                .any(|selected| selected == prompt)
            {
                next.selected.insert(id);
            }
        }
        *self = next;
        Ok(())
    }

    pub fn add_builtin_state_capabilities(&mut self) -> Result<(), CapabilityLedgerError> {
        let mut next = self.clone();
        for (id, kind) in [
            ("agent.child", CapabilityKind::ChildAgent),
            ("task.todo", CapabilityKind::Todo),
            ("task.plan", CapabilityKind::Plan),
            ("task.goal", CapabilityKind::Goal),
        ] {
            next.add(CapabilityDescriptor {
                id: id.into(),
                kind,
                allowed_modes: vec![OperatingMode::Auto, OperatingMode::Plan],
                authorization: AuthorizationRequirement::Explicit,
                replay_policy: ReplayPolicy::Never,
                mutates_workspace: false,
                selection: CapabilitySelection::None,
            })?;
        }
        *self = next;
        Ok(())
    }

    pub fn descriptor(&self, id: &str) -> Option<&CapabilityDescriptor> {
        self.descriptors.get(id)
    }

    pub fn list(&self) -> Vec<&CapabilityDescriptor> {
        self.descriptors.values().collect()
    }

    pub fn for_mode(&self, mode: OperatingMode) -> Vec<&CapabilityDescriptor> {
        self.descriptors
            .values()
            .filter(|descriptor| descriptor.allows_mode(mode))
            .collect()
    }

    pub fn authorize(
        &self,
        id: &str,
        mode: OperatingMode,
        grant: AuthorizationGrant,
    ) -> Result<&CapabilityDescriptor, CapabilityLedgerError> {
        let descriptor =
            self.descriptor(id)
                .ok_or_else(|| CapabilityLedgerError::UnknownCapability {
                    capability_id: id.into(),
                })?;
        descriptor.authorize_with_selection(mode, grant, self.selected.contains(id))?;
        Ok(descriptor)
    }

    fn authorize_with_request(
        &self,
        request: &CapabilityRequest,
    ) -> Result<&CapabilityDescriptor, CapabilityLedgerError> {
        let descriptor = self.descriptor(&request.capability_id).ok_or_else(|| {
            CapabilityLedgerError::UnknownCapability {
                capability_id: request.capability_id.clone(),
            }
        })?;
        descriptor.authorize_with_selection(
            request.mode,
            request.authorization,
            self.selected.contains(&request.capability_id),
        )?;
        Ok(descriptor)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CapabilityRequest {
    pub queue_id: String,
    pub execution_id: String,
    pub capability_id: String,
    pub mode: OperatingMode,
    pub authorization: AuthorizationGrant,
}

impl CapabilityRequest {
    pub fn new(
        queue_id: impl Into<String>,
        execution_id: impl Into<String>,
        capability_id: impl Into<String>,
        mode: OperatingMode,
        authorization: AuthorizationGrant,
    ) -> Self {
        Self {
            queue_id: queue_id.into(),
            execution_id: execution_id.into(),
            capability_id: capability_id.into(),
            mode,
            authorization,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityTerminal {
    Success,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapabilityExecutionState {
    Queued,
    Claimed,
    CancellationRequested,
    InFlightRequiresDecision,
    Terminal(CapabilityTerminal),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityDispatch {
    pub queue_id: String,
    pub execution_id: String,
    pub effect_id: String,
    pub status: CapabilityTerminal,
}

pub trait CapabilityDispatcher {
    fn dispatch(
        &mut self,
        effect_id: &str,
        descriptor: &CapabilityDescriptor,
    ) -> CapabilityTerminal;
}

impl<F> CapabilityDispatcher for F
where
    F: FnMut(&str, &CapabilityDescriptor) -> CapabilityTerminal,
{
    fn dispatch(
        &mut self,
        effect_id: &str,
        descriptor: &CapabilityDescriptor,
    ) -> CapabilityTerminal {
        self(effect_id, descriptor)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DurableChildStatus {
    Queued,
    Active,
    CancellationRequested,
    Completed,
    Cancelled,
    Failed,
}

impl DurableChildStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled | Self::Failed)
    }
}

impl From<ChildStatus> for DurableChildStatus {
    fn from(status: ChildStatus) -> Self {
        match status {
            ChildStatus::Queued => Self::Queued,
            ChildStatus::Active => Self::Active,
            ChildStatus::Completed => Self::Completed,
            ChildStatus::Cancelled => Self::Cancelled,
            ChildStatus::Failed => Self::Failed,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChildRequest {
    pub queue_id: String,
    pub execution_id: String,
    pub parent_execution_id: String,
    pub parent_session_id: String,
    pub child_session_id: String,
    /// Derived by the service from the parent execution. Callers cannot use
    /// this field to grant themselves deeper nesting.
    pub depth: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildPromotion {
    pub queue_id: String,
    pub execution_id: String,
    pub child_session_id: String,
}

impl ChildRequest {
    pub fn new(
        queue_id: impl Into<String>,
        execution_id: impl Into<String>,
        parent_execution_id: impl Into<String>,
        parent_session_id: impl Into<String>,
        child_session_id: impl Into<String>,
    ) -> Self {
        Self {
            queue_id: queue_id.into(),
            execution_id: execution_id.into(),
            parent_execution_id: parent_execution_id.into(),
            parent_session_id: parent_session_id.into(),
            child_session_id: child_session_id.into(),
            depth: 0,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskTodoStatus {
    Pending,
    InProgress,
    Completed,
    Blocked,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskGoalAssurance {
    Verified,
    Unverified,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum TaskMutation {
    TodoAdd {
        title: String,
    },
    TodoSetStatus {
        status: TaskTodoStatus,
    },
    PlanAddNode {
        node_id: String,
        dependencies: Vec<String>,
    },
    PlanApprove,
    GoalSetBudget {
        budget: Option<u64>,
    },
    GoalConsume {
        amount: u64,
    },
    GoalComplete {
        assurance: TaskGoalAssurance,
    },
}

impl TaskMutation {
    fn capability_id(&self) -> &'static str {
        match self {
            Self::TodoAdd { .. } | Self::TodoSetStatus { .. } => "task.todo",
            Self::PlanAddNode { .. } | Self::PlanApprove => "task.plan",
            Self::GoalSetBudget { .. } | Self::GoalConsume { .. } | Self::GoalComplete { .. } => {
                "task.goal"
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskMutationRequest {
    pub idempotency_key: String,
    pub entity_id: String,
    pub revision: u64,
    pub mutation: TaskMutation,
}

#[derive(Clone, Debug)]
struct CapabilityState {
    request: CapabilityRequest,
    state: CapabilityExecutionState,
    effect_id: String,
}

#[derive(Clone, Debug)]
struct ChildState {
    request: ChildRequest,
    status: DurableChildStatus,
}

#[derive(Clone, Debug)]
struct TaskState {
    revision: u64,
    mutations: Vec<TaskMutationRequest>,
}

#[derive(Default)]
struct TaskProjection {
    todo_statuses: Vec<TaskTodoStatus>,
    plan_nodes: BTreeMap<String, Vec<String>>,
    plan_approved: bool,
    goal_initialized: bool,
    goal_budget: Option<u64>,
    goal_used: u64,
    goal_paused: bool,
    goal_complete: bool,
}

#[derive(Debug)]
pub struct CapabilityService<R> {
    repo: R,
    catalog: CapabilityCatalog,
    capabilities: BTreeMap<String, CapabilityState>,
    capability_queue: VecDeque<String>,
    children: BTreeMap<String, ChildState>,
    child_queue: VecDeque<String>,
    tasks: BTreeMap<String, TaskState>,
    task_idempotency: BTreeMap<String, TaskMutationRequest>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapabilityLedgerError {
    Persist(String),
    InvalidIdentifier(&'static str),
    QueueFull,
    DuplicateQueueId(String),
    DuplicateExecutionId(String),
    DuplicateCapability,
    InvalidCapabilityPolicy(&'static str),
    UnknownCapability {
        capability_id: String,
    },
    ModeDenied {
        capability_id: String,
        mode: OperatingMode,
    },
    AuthorizationRequired {
        capability_id: String,
    },
    SelectionRequired {
        capability_id: String,
    },
    UnknownQueueId(String),
    AlreadyTerminal(String),
    InvalidCapabilityTransition(String),
    InvalidTaskTransition(&'static str),
    InFlightRequiresDecision(String),
    RetryNotAllowed(String),
    ChildDepthExceeded,
    ParentExecutionUnknown(String),
    ParentNotActive(String),
    ParentOwnershipMismatch(String),
    ChildQueueFull,
    DuplicateChild(String),
    UnknownChild(String),
    InvalidChildTransition(String),
    DuplicateMutation(String),
    RevisionConflict {
        entity_id: String,
        expected: u64,
        actual: u64,
    },
    InvalidRecords(String),
    SequenceOverflow,
}

impl fmt::Display for CapabilityLedgerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Persist(message) => write!(formatter, "capability persistence failed: {message}"),
            Self::InvalidIdentifier(field) => write!(formatter, "invalid capability {field}"),
            Self::QueueFull => formatter.write_str("capability queue full"),
            Self::DuplicateQueueId(id) => write!(formatter, "duplicate capability queue id: {id}"),
            Self::DuplicateExecutionId(id) => write!(formatter, "duplicate execution id: {id}"),
            Self::DuplicateCapability => formatter.write_str("duplicate capability descriptor"),
            Self::InvalidCapabilityPolicy(field) => {
                write!(formatter, "invalid capability policy: {field}")
            }
            Self::UnknownCapability { capability_id } => {
                write!(formatter, "unknown capability: {capability_id}")
            }
            Self::ModeDenied {
                capability_id,
                mode,
            } => write!(
                formatter,
                "capability {capability_id} is unavailable in {mode:?}"
            ),
            Self::AuthorizationRequired { capability_id } => write!(
                formatter,
                "explicit authorization required for {capability_id}"
            ),
            Self::SelectionRequired { capability_id } => {
                write!(
                    formatter,
                    "explicit MCP selection required for {capability_id}"
                )
            }
            Self::UnknownQueueId(id) => write!(formatter, "unknown capability queue id: {id}"),
            Self::AlreadyTerminal(id) => write!(formatter, "capability already terminal: {id}"),
            Self::InvalidCapabilityTransition(id) => {
                write!(formatter, "invalid capability transition: {id}")
            }
            Self::InvalidTaskTransition(field) => {
                write!(formatter, "invalid task transition: {field}")
            }
            Self::InFlightRequiresDecision(id) => {
                write!(
                    formatter,
                    "capability requires explicit retry decision: {id}"
                )
            }
            Self::RetryNotAllowed(id) => write!(formatter, "capability retry not allowed: {id}"),
            Self::ChildDepthExceeded => formatter.write_str("child depth exceeds one level"),
            Self::ParentExecutionUnknown(id) => write!(formatter, "unknown parent execution: {id}"),
            Self::ParentNotActive(id) => write!(formatter, "parent execution is not active: {id}"),
            Self::ParentOwnershipMismatch(id) => {
                write!(formatter, "child parent ownership mismatch: {id}")
            }
            Self::ChildQueueFull => formatter.write_str("child queue full"),
            Self::DuplicateChild(id) => write!(formatter, "duplicate child queue id: {id}"),
            Self::UnknownChild(id) => write!(formatter, "unknown child queue id: {id}"),
            Self::InvalidChildTransition(id) => write!(formatter, "invalid child transition: {id}"),
            Self::DuplicateMutation(id) => {
                write!(formatter, "duplicate mutation idempotency key: {id}")
            }
            Self::RevisionConflict {
                entity_id,
                expected,
                actual,
            } => write!(
                formatter,
                "revision conflict for {entity_id}: expected {expected}, actual {actual}"
            ),
            Self::InvalidRecords(message) => {
                write!(formatter, "invalid capability records: {message}")
            }
            Self::SequenceOverflow => formatter.write_str("capability sequence overflowed"),
        }
    }
}

impl std::error::Error for CapabilityLedgerError {}

impl<R> CapabilityService<R> {
    pub fn new(repo: R, catalog: CapabilityCatalog) -> Result<Self, CapabilityLedgerError>
    where
        R: DurableRepoLike,
    {
        let mut service = Self {
            repo,
            catalog,
            capabilities: BTreeMap::new(),
            capability_queue: VecDeque::new(),
            children: BTreeMap::new(),
            child_queue: VecDeque::new(),
            tasks: BTreeMap::new(),
            task_idempotency: BTreeMap::new(),
        };
        validate_base_identifier(&service.repo.header().id)
            .map_err(|_| CapabilityLedgerError::InvalidIdentifier("session id"))?;
        service.restore_records()?;
        Ok(service)
    }

    pub fn repo(&self) -> &R {
        &self.repo
    }

    pub fn repo_mut(&mut self) -> &mut R {
        &mut self.repo
    }

    pub fn into_repo(self) -> R {
        self.repo
    }

    pub fn catalog(&self) -> &CapabilityCatalog {
        &self.catalog
    }

    pub fn capability_status(&self, queue_id: &str) -> Option<CapabilityTerminal> {
        self.capabilities
            .get(queue_id)
            .and_then(|state| match state.state {
                CapabilityExecutionState::Terminal(status) => Some(status),
                _ => None,
            })
    }

    pub fn capability_state(&self, queue_id: &str) -> Option<CapabilityExecutionState> {
        self.capabilities.get(queue_id).map(|state| state.state)
    }

    pub fn effect_id(&self, queue_id: &str) -> Option<&str> {
        self.capabilities
            .get(queue_id)
            .map(|state| state.effect_id.as_str())
    }

    /// Returns the original durable request for adapter retries.  The
    /// request is policy metadata only; callers cannot mutate ledger state
    /// through this accessor.
    pub fn capability_request(&self, queue_id: &str) -> Option<&CapabilityRequest> {
        self.capabilities.get(queue_id).map(|state| &state.request)
    }

    pub fn capability_states(&self) -> Vec<(CapabilityRequest, CapabilityExecutionState)> {
        self.capabilities
            .values()
            .map(|state| (state.request.clone(), state.state))
            .collect()
    }

    fn effect_id_for(&self, execution_id: &str) -> Result<String, CapabilityLedgerError>
    where
        R: DurableRepoLike,
    {
        let effect_id = format!("effect.{}.{}", self.repo.header().id, execution_id);
        validate_identifier(&effect_id)?;
        Ok(effect_id)
    }

    pub fn pending_queue(&self) -> Vec<&CapabilityRequest> {
        self.capability_queue
            .iter()
            .filter_map(|queue_id| self.capabilities.get(queue_id))
            .map(|state| &state.request)
            .collect()
    }

    pub fn enqueue(&mut self, request: CapabilityRequest) -> Result<(), CapabilityLedgerError>
    where
        R: DurableRepoLike,
    {
        validate_request(&request)?;
        self.catalog.authorize_with_request(&request)?;
        if self.capabilities.contains_key(&request.queue_id) {
            return Err(CapabilityLedgerError::DuplicateQueueId(request.queue_id));
        }
        if self
            .capabilities
            .values()
            .any(|state| state.request.execution_id == request.execution_id)
            || self
                .children
                .values()
                .any(|child| child.request.execution_id == request.execution_id)
        {
            return Err(CapabilityLedgerError::DuplicateExecutionId(
                request.execution_id,
            ));
        }
        if self.capability_queue.len() >= MAX_CAPABILITY_QUEUE {
            return Err(CapabilityLedgerError::QueueFull);
        }
        let effect_id = self.effect_id_for(&request.execution_id)?;
        self.append_fact(
            "capability.v1",
            format!("{}.intent", request.queue_id),
            json!({
                "schema_version": CAPABILITY_SCHEMA_VERSION,
                "kind": "intent",
                "request": request,
            }),
        )?;
        let queue_id = request.queue_id.clone();
        self.capabilities.insert(
            queue_id.clone(),
            CapabilityState {
                request,
                state: CapabilityExecutionState::Queued,
                effect_id,
            },
        );
        self.capability_queue.push_back(queue_id);
        Ok(())
    }

    pub fn dispatch<F>(
        &mut self,
        request: CapabilityRequest,
        dispatcher: F,
    ) -> Result<CapabilityDispatch, CapabilityLedgerError>
    where
        R: DurableRepoLike,
        F: FnOnce(&CapabilityDescriptor, &str) -> CapabilityTerminal,
    {
        let queue_id = request.queue_id.clone();
        self.enqueue(request)?;
        self.dispatch_queued(&queue_id, dispatcher)
    }

    pub fn dispatch_queued<F>(
        &mut self,
        queue_id: &str,
        dispatcher: F,
    ) -> Result<CapabilityDispatch, CapabilityLedgerError>
    where
        R: DurableRepoLike,
        F: FnOnce(&CapabilityDescriptor, &str) -> CapabilityTerminal,
    {
        let (request, effect_id, state_kind) = self
            .capabilities
            .get(queue_id)
            .ok_or_else(|| CapabilityLedgerError::UnknownQueueId(queue_id.into()))
            .map(|state| (state.request.clone(), state.effect_id.clone(), state.state))?;
        match state_kind {
            CapabilityExecutionState::Terminal(_) => {
                return Err(CapabilityLedgerError::AlreadyTerminal(queue_id.into()))
            }
            CapabilityExecutionState::Claimed
            | CapabilityExecutionState::CancellationRequested
            | CapabilityExecutionState::InFlightRequiresDecision => {
                return Err(CapabilityLedgerError::InFlightRequiresDecision(
                    queue_id.into(),
                ))
            }
            CapabilityExecutionState::Queued => {}
        }
        let descriptor = self.catalog.authorize_with_request(&request)?.clone();
        // The claim is the durable exactly-once boundary.  A crash after this
        // append leaves an explicit decision state; restore never invokes the
        // dispatcher from a claim alone.
        self.append_fact(
            "capability.v1",
            format!("{queue_id}.claim"),
            json!({
                "schema_version": CAPABILITY_SCHEMA_VERSION,
                "kind": "claim",
                "queue_id": queue_id,
                "execution_id": request.execution_id,
                "effect_id": effect_id,
            }),
        )?;
        self.capabilities
            .get_mut(queue_id)
            .expect("capability state checked before claim")
            .state = CapabilityExecutionState::Claimed;
        self.capability_queue.retain(|id| id != queue_id);
        let status = dispatcher(&descriptor, &effect_id);
        self.append_fact(
            "capability.v1",
            format!("{queue_id}.terminal"),
            json!({
                "schema_version": CAPABILITY_SCHEMA_VERSION,
                "kind": "terminal",
                "queue_id": queue_id,
                "execution_id": request.execution_id,
                "effect_id": effect_id,
                "status": status,
            }),
        )
        .inspect_err(|_| {
            // The claim is durable, so an append failure must remain an
            // explicit decision state and must never trigger an implicit
            // duplicate dispatch on retry or restore.
            if let Some(state) = self.capabilities.get_mut(queue_id) {
                state.state = CapabilityExecutionState::InFlightRequiresDecision;
            }
        })?;
        self.capabilities
            .get_mut(queue_id)
            .expect("capability state checked before append")
            .state = CapabilityExecutionState::Terminal(status);
        let state = self.capabilities.get(queue_id).expect("state exists");
        Ok(CapabilityDispatch {
            queue_id: queue_id.into(),
            execution_id: state.request.execution_id.clone(),
            effect_id: state.effect_id.clone(),
            status,
        })
    }

    /// Requests cancellation at the durable boundary. Queued work is
    /// terminally cancelled without invoking a dispatcher; claimed work gets
    /// an explicit cancellation marker for a later token-propagation bridge.
    pub fn cancel_capability(
        &mut self,
        queue_id: &str,
        mode: OperatingMode,
        authorization: AuthorizationGrant,
    ) -> Result<(), CapabilityLedgerError>
    where
        R: DurableRepoLike,
    {
        let (request, effect_id, state) = self
            .capabilities
            .get(queue_id)
            .ok_or_else(|| CapabilityLedgerError::UnknownQueueId(queue_id.into()))
            .map(|state| (state.request.clone(), state.effect_id.clone(), state.state))?;
        self.catalog
            .authorize(&request.capability_id, mode, authorization)?;
        match state {
            CapabilityExecutionState::Terminal(_) => {
                Err(CapabilityLedgerError::AlreadyTerminal(queue_id.into()))
            }
            CapabilityExecutionState::Queued => {
                self.append_fact(
                    "capability.v1",
                    format!("{queue_id}.cancelled"),
                    json!({
                        "schema_version": CAPABILITY_SCHEMA_VERSION,
                        "kind": "terminal",
                        "queue_id": queue_id,
                        "execution_id": request.execution_id,
                        "effect_id": effect_id,
                        "status": CapabilityTerminal::Cancelled,
                    }),
                )?;
                self.capabilities
                    .get_mut(queue_id)
                    .expect("capability state checked before cancellation")
                    .state = CapabilityExecutionState::Terminal(CapabilityTerminal::Cancelled);
                self.capability_queue.retain(|id| id != queue_id);
                Ok(())
            }
            CapabilityExecutionState::Claimed
            | CapabilityExecutionState::InFlightRequiresDecision => {
                self.append_fact(
                    "capability.v1",
                    format!("{queue_id}.cancel_requested"),
                    json!({
                        "schema_version": CAPABILITY_SCHEMA_VERSION,
                        "kind": "cancel_requested",
                        "queue_id": queue_id,
                        "execution_id": request.execution_id,
                        "effect_id": effect_id,
                    }),
                )?;
                self.capabilities
                    .get_mut(queue_id)
                    .expect("capability state checked before cancellation")
                    .state = CapabilityExecutionState::CancellationRequested;
                Ok(())
            }
            CapabilityExecutionState::CancellationRequested => Err(
                CapabilityLedgerError::InFlightRequiresDecision(queue_id.into()),
            ),
        }
    }

    /// Completes a previously requested cancellation after the runtime has
    /// observed its token boundary.
    pub fn finish_capability_cancellation(
        &mut self,
        queue_id: &str,
        mode: OperatingMode,
        authorization: AuthorizationGrant,
    ) -> Result<(), CapabilityLedgerError>
    where
        R: DurableRepoLike,
    {
        let (request, effect_id, state) = self
            .capabilities
            .get(queue_id)
            .ok_or_else(|| CapabilityLedgerError::UnknownQueueId(queue_id.into()))
            .map(|state| (state.request.clone(), state.effect_id.clone(), state.state))?;
        let _ = self
            .catalog
            .authorize(&request.capability_id, mode, authorization)?;
        if state != CapabilityExecutionState::CancellationRequested {
            return Err(CapabilityLedgerError::InvalidCapabilityTransition(
                queue_id.into(),
            ));
        }
        self.append_fact(
            "capability.v1",
            format!("{queue_id}.cancelled"),
            json!({
                "schema_version": CAPABILITY_SCHEMA_VERSION,
                "kind": "terminal",
                "queue_id": queue_id,
                "execution_id": request.execution_id,
                "effect_id": effect_id,
                "status": CapabilityTerminal::Cancelled,
            }),
        )?;
        self.capabilities
            .get_mut(queue_id)
            .expect("capability state checked before cancellation")
            .state = CapabilityExecutionState::Terminal(CapabilityTerminal::Cancelled);
        Ok(())
    }

    /// Explicitly authorizes a second attempt after a durable claim whose
    /// terminal marker could not be committed.  The same effect id is passed
    /// to the adapter, allowing a real adapter to deduplicate externally.
    pub fn retry_in_flight<F>(
        &mut self,
        queue_id: &str,
        dispatcher: F,
    ) -> Result<CapabilityDispatch, CapabilityLedgerError>
    where
        R: DurableRepoLike,
        F: FnOnce(&CapabilityDescriptor, &str) -> CapabilityTerminal,
    {
        let (request, effect_id, state_kind) = self
            .capabilities
            .get(queue_id)
            .ok_or_else(|| CapabilityLedgerError::UnknownQueueId(queue_id.into()))
            .map(|state| (state.request.clone(), state.effect_id.clone(), state.state))?;
        if !matches!(
            state_kind,
            CapabilityExecutionState::Claimed | CapabilityExecutionState::InFlightRequiresDecision
        ) {
            return Err(CapabilityLedgerError::RetryNotAllowed(queue_id.into()));
        }
        let descriptor = self.catalog.authorize_with_request(&request)?.clone();
        if descriptor.replay_policy == ReplayPolicy::Never {
            return Err(CapabilityLedgerError::RetryNotAllowed(queue_id.into()));
        }
        self.append_fact(
            "capability.v1",
            format!("{queue_id}.retry"),
            json!({
                "schema_version": CAPABILITY_SCHEMA_VERSION,
                "kind": "retry_decision",
                "queue_id": queue_id,
                "execution_id": request.execution_id,
                "effect_id": effect_id,
            }),
        )?;
        self.capabilities
            .get_mut(queue_id)
            .expect("capability state checked before retry")
            .state = CapabilityExecutionState::Claimed;
        let status = dispatcher(&descriptor, &effect_id);
        self.append_fact(
            "capability.v1",
            format!("{queue_id}.terminal.retry"),
            json!({
                "schema_version": CAPABILITY_SCHEMA_VERSION,
                "kind": "terminal",
                "queue_id": queue_id,
                "execution_id": request.execution_id,
                "effect_id": effect_id,
                "status": status,
            }),
        )
        .inspect_err(|_| {
            if let Some(state) = self.capabilities.get_mut(queue_id) {
                state.state = CapabilityExecutionState::InFlightRequiresDecision;
            }
        })?;
        self.capabilities
            .get_mut(queue_id)
            .expect("capability state checked after retry")
            .state = CapabilityExecutionState::Terminal(status);
        Ok(CapabilityDispatch {
            queue_id: queue_id.into(),
            execution_id: request.execution_id,
            effect_id,
            status,
        })
    }

    pub fn dispatch_with_adapter<D: CapabilityDispatcher>(
        &mut self,
        request: CapabilityRequest,
        adapter: &mut D,
    ) -> Result<CapabilityDispatch, CapabilityLedgerError>
    where
        R: DurableRepoLike,
    {
        self.dispatch(request, |descriptor, effect_id| {
            adapter.dispatch(effect_id, descriptor)
        })
    }

    pub fn retry_in_flight_with_adapter<D: CapabilityDispatcher>(
        &mut self,
        queue_id: &str,
        adapter: &mut D,
    ) -> Result<CapabilityDispatch, CapabilityLedgerError>
    where
        R: DurableRepoLike,
    {
        self.retry_in_flight(queue_id, |descriptor, effect_id| {
            adapter.dispatch(effect_id, descriptor)
        })
    }

    pub fn enqueue_child(
        &mut self,
        mut request: ChildRequest,
        mode: OperatingMode,
        authorization: AuthorizationGrant,
    ) -> Result<(), CapabilityLedgerError>
    where
        R: DurableRepoLike,
    {
        self.catalog.authorize("agent.child", mode, authorization)?;
        validate_child_request(&request)?;
        if request.depth != 0 {
            return Err(CapabilityLedgerError::ChildDepthExceeded);
        }
        let derived_depth = self.derive_child_depth(&request)?;
        if derived_depth > MAX_CHILD_DEPTH {
            return Err(CapabilityLedgerError::ChildDepthExceeded);
        }
        request.depth = derived_depth;
        if self.children.contains_key(&request.queue_id) {
            return Err(CapabilityLedgerError::DuplicateChild(request.queue_id));
        }
        if self
            .children
            .values()
            .any(|child| child.request.execution_id == request.execution_id)
            || self
                .capabilities
                .values()
                .any(|state| state.request.execution_id == request.execution_id)
        {
            return Err(CapabilityLedgerError::DuplicateExecutionId(
                request.execution_id,
            ));
        }
        let active = self.live_child_count();
        let queued = self.child_queue.len();
        if active >= MAX_ACTIVE_CHILDREN && queued >= MAX_CHILD_QUEUE {
            return Err(CapabilityLedgerError::ChildQueueFull);
        }
        let status = if active < MAX_ACTIVE_CHILDREN {
            DurableChildStatus::Active
        } else {
            DurableChildStatus::Queued
        };
        self.append_fact(
            "capability.v1",
            format!("{}.child_intent", request.queue_id),
            json!({
                "schema_version": CAPABILITY_SCHEMA_VERSION,
                "kind": "child_intent",
                "request": request,
                "status": status,
                "capability_id": "agent.child",
                "mode": mode,
                "authorization": authorization,
            }),
        )?;
        let queue_id = request.queue_id.clone();
        self.children
            .insert(queue_id.clone(), ChildState { request, status });
        if status == DurableChildStatus::Queued {
            self.child_queue.push_back(queue_id);
        }
        Ok(())
    }

    pub fn child_status(&self, queue_id: &str) -> Option<DurableChildStatus> {
        self.children.get(queue_id).map(|child| child.status)
    }

    pub fn child_queue(&self) -> Vec<&ChildRequest> {
        self.child_queue
            .iter()
            .filter_map(|id| self.children.get(id).map(|child| &child.request))
            .collect()
    }

    /// Durable child snapshots for runtime scheduler reconstruction.
    pub fn child_states(&self) -> Vec<(ChildRequest, DurableChildStatus)> {
        let mut states = self
            .child_queue
            .iter()
            .filter_map(|queue_id| {
                self.children
                    .get(queue_id)
                    .map(|child| (child.request.clone(), child.status))
            })
            .collect::<Vec<_>>();
        states.extend(
            self.children
                .iter()
                .filter(|(queue_id, _)| !self.child_queue.iter().any(|id| id == *queue_id))
                .map(|(_, child)| (child.request.clone(), child.status)),
        );
        states
    }

    pub fn cancel_child(
        &mut self,
        queue_id: &str,
        mode: OperatingMode,
        authorization: AuthorizationGrant,
    ) -> Result<(), CapabilityLedgerError>
    where
        R: DurableRepoLike,
    {
        self.catalog.authorize("agent.child", mode, authorization)?;
        self.request_child_cancellation(queue_id)
    }

    pub fn finish_child(
        &mut self,
        queue_id: &str,
        status: DurableChildStatus,
        mode: OperatingMode,
        authorization: AuthorizationGrant,
    ) -> Result<(), CapabilityLedgerError>
    where
        R: DurableRepoLike,
    {
        self.catalog.authorize("agent.child", mode, authorization)?;
        if !status.is_terminal() {
            return Err(CapabilityLedgerError::InvalidChildTransition(
                queue_id.into(),
            ));
        }
        self.transition_child(queue_id, status)
    }

    pub fn apply_task_mutation(
        &mut self,
        request: TaskMutationRequest,
        mode: OperatingMode,
        authorization: AuthorizationGrant,
    ) -> Result<bool, CapabilityLedgerError>
    where
        R: DurableRepoLike,
    {
        validate_task_request(&request)?;
        let capability_id = request.mutation.capability_id();
        self.catalog.authorize(capability_id, mode, authorization)?;
        if let Some(previous) = self.task_idempotency.get(&request.idempotency_key) {
            if previous == &request {
                return Ok(false);
            }
            return Err(CapabilityLedgerError::DuplicateMutation(
                request.idempotency_key,
            ));
        }
        let actual = self
            .tasks
            .get(&request.entity_id)
            .map(|task| task.revision)
            .unwrap_or(0);
        if self.task_mutations(&request.entity_id).len() >= MAX_TASK_COLLECTION {
            return Err(CapabilityLedgerError::InvalidIdentifier("task mutations"));
        }
        let expected = actual
            .checked_add(1)
            .ok_or(CapabilityLedgerError::RevisionConflict {
                entity_id: request.entity_id.clone(),
                expected: u64::MAX,
                actual,
            })?;
        if request.revision != expected {
            return Err(CapabilityLedgerError::RevisionConflict {
                entity_id: request.entity_id,
                expected,
                actual,
            });
        }
        self.validate_task_transition(&request)?;
        self.append_fact(
            "task.v1",
            request.idempotency_key.clone(),
            json!({
                "schema_version": CAPABILITY_SCHEMA_VERSION,
                "kind": "mutation",
                "request": request,
                "capability_id": capability_id,
                "mode": mode,
                "authorization": authorization,
            }),
        )?;
        self.task_idempotency
            .insert(request.idempotency_key.clone(), request.clone());
        self.tasks
            .entry(request.entity_id.clone())
            .or_insert_with(|| TaskState {
                revision: 0,
                mutations: Vec::new(),
            })
            .mutations
            .push(request.clone());
        self.tasks
            .get_mut(&request.entity_id)
            .expect("task state inserted")
            .revision = request.revision;
        Ok(true)
    }

    pub fn task_revision(&self, entity_id: &str) -> u64 {
        self.tasks
            .get(entity_id)
            .map(|task| task.revision)
            .unwrap_or(0)
    }

    pub fn task_mutations(&self, entity_id: &str) -> Vec<&TaskMutationRequest> {
        self.tasks
            .get(entity_id)
            .map(|task| task.mutations.iter().collect())
            .unwrap_or_default()
    }

    pub fn task_mutation(&self, idempotency_key: &str) -> Option<&TaskMutationRequest> {
        self.task_idempotency.get(idempotency_key)
    }

    /// Authorize a task mutation without consulting idempotency or changing
    /// durable state. Runtime adapters use this before their local typed
    /// projection so an unauthorized replay cannot observe the idempotent
    /// result.
    pub fn authorize_task_mutation(
        &self,
        request: &TaskMutationRequest,
        mode: OperatingMode,
        authorization: AuthorizationGrant,
    ) -> Result<(), CapabilityLedgerError> {
        self.catalog
            .authorize(request.mutation.capability_id(), mode, authorization)
            .map(|_| ())
    }

    fn validate_task_transition(
        &self,
        request: &TaskMutationRequest,
    ) -> Result<(), CapabilityLedgerError> {
        let projection = self.task_projection(&request.entity_id);
        match &request.mutation {
            TaskMutation::TodoAdd { title } => {
                if title.chars().count() > MAX_TASK_TEXT_BYTES {
                    return Err(CapabilityLedgerError::InvalidTaskTransition(
                        "todo title too long",
                    ));
                }
                if projection
                    .todo_statuses
                    .contains(&TaskTodoStatus::InProgress)
                {
                    return Err(CapabilityLedgerError::InvalidTaskTransition(
                        "todo already in progress",
                    ));
                }
                Ok(())
            }
            TaskMutation::TodoSetStatus { status } => {
                if projection.todo_statuses.is_empty() {
                    return Err(CapabilityLedgerError::InvalidTaskTransition(
                        "todo status requires a todo",
                    ));
                }
                if *status == TaskTodoStatus::InProgress
                    && self.tasks.iter().any(|(entity_id, task)| {
                        entity_id != &request.entity_id
                            && self
                                .task_projection_from_mutations(&task.mutations)
                                .todo_statuses
                                .contains(&TaskTodoStatus::InProgress)
                    })
                {
                    return Err(CapabilityLedgerError::InvalidTaskTransition(
                        "only one todo may be in progress",
                    ));
                }
                Ok(())
            }
            TaskMutation::PlanAddNode {
                node_id,
                dependencies,
            } => {
                if projection.plan_nodes.contains_key(node_id) {
                    return Err(CapabilityLedgerError::InvalidTaskTransition(
                        "duplicate plan node",
                    ));
                }
                if dependencies
                    .iter()
                    .any(|dependency| !projection.plan_nodes.contains_key(dependency))
                {
                    return Err(CapabilityLedgerError::InvalidTaskTransition(
                        "plan dependency not found",
                    ));
                }
                Ok(())
            }
            TaskMutation::PlanApprove => {
                if projection.plan_nodes.is_empty() {
                    return Err(CapabilityLedgerError::InvalidTaskTransition("empty plan"));
                }
                if projection.plan_approved {
                    return Err(CapabilityLedgerError::InvalidTaskTransition(
                        "plan already approved",
                    ));
                }
                Ok(())
            }
            TaskMutation::GoalSetBudget { budget: _ } => {
                if projection.goal_initialized {
                    return Err(CapabilityLedgerError::InvalidTaskTransition(
                        "goal budget already initialized",
                    ));
                }
                Ok(())
            }
            TaskMutation::GoalConsume { amount } => {
                if !projection.goal_initialized || projection.goal_budget.is_none() {
                    return Err(CapabilityLedgerError::InvalidTaskTransition(
                        "goal is not initialized",
                    ));
                }
                if projection.goal_complete {
                    return Err(CapabilityLedgerError::InvalidTaskTransition(
                        "goal already complete",
                    ));
                }
                if projection.goal_paused {
                    return Err(CapabilityLedgerError::InvalidTaskTransition(
                        "goal is paused",
                    ));
                }
                let budget = projection.goal_budget.unwrap_or(0);
                if projection.goal_used.saturating_add(*amount) > budget {
                    return Err(CapabilityLedgerError::InvalidTaskTransition(
                        "goal budget exceeded",
                    ));
                }
                Ok(())
            }
            TaskMutation::GoalComplete { .. } => {
                if !projection.goal_initialized {
                    return Err(CapabilityLedgerError::InvalidTaskTransition(
                        "goal is not initialized",
                    ));
                }
                if projection.goal_complete {
                    return Err(CapabilityLedgerError::InvalidTaskTransition(
                        "goal already complete",
                    ));
                }
                Ok(())
            }
        }
    }

    fn task_projection(&self, entity_id: &str) -> TaskProjection {
        self.tasks
            .get(entity_id)
            .map(|task| self.task_projection_from_mutations(&task.mutations))
            .unwrap_or_default()
    }

    /// Rebuilds the typed task state (todos, plan, goal) from an entity's
    /// mutation history. Pure and strictly ordered: later mutations overwrite
    /// earlier effects, matching the stored TaskProjection semantics.
    fn task_projection_from_mutations(&self, mutations: &[TaskMutationRequest]) -> TaskProjection {
        let mut projection = TaskProjection::default();
        for request in mutations {
            match &request.mutation {
                TaskMutation::TodoAdd { title: _ } => {
                    projection.todo_statuses.push(TaskTodoStatus::Pending);
                }
                TaskMutation::TodoSetStatus { status } => {
                    if let Some(last) = projection.todo_statuses.last_mut() {
                        *last = status.clone();
                    }
                }
                TaskMutation::PlanAddNode {
                    node_id,
                    dependencies,
                } => {
                    projection
                        .plan_nodes
                        .insert(node_id.clone(), dependencies.clone());
                }
                TaskMutation::PlanApprove => {
                    projection.plan_approved = true;
                }
                TaskMutation::GoalSetBudget { budget } => {
                    projection.goal_initialized = true;
                    projection.goal_budget = *budget;
                    projection.goal_paused = budget.is_none();
                }
                TaskMutation::GoalConsume { amount } => {
                    projection.goal_used = projection.goal_used.saturating_add(*amount);
                    if projection
                        .goal_budget
                        .is_some_and(|budget| projection.goal_used >= budget)
                    {
                        projection.goal_paused = true;
                    }
                }
                TaskMutation::GoalComplete { .. } => {
                    projection.goal_complete = true;
                }
            }
        }
        projection
    }
}

fn validate_task_request(request: &TaskMutationRequest) -> Result<(), CapabilityLedgerError> {
    if request.entity_id.is_empty()
        || request.entity_id.len() > MAX_TASK_TEXT_BYTES
        || request.idempotency_key.is_empty()
        || request.idempotency_key.len() > MAX_TASK_TEXT_BYTES
    {
        return Err(CapabilityLedgerError::InvalidIdentifier("task request"));
    }
    let valid_text = |text: &str| !text.is_empty() && text.len() <= MAX_TASK_TEXT_BYTES;
    match &request.mutation {
        TaskMutation::TodoAdd { title } if !valid_text(title) => {
            return Err(CapabilityLedgerError::InvalidIdentifier("task mutation"));
        }
        TaskMutation::PlanAddNode {
            node_id,
            dependencies,
        } if !valid_text(node_id)
            || dependencies.len() > MAX_TASK_COLLECTION
            || dependencies
                .iter()
                .any(|dependency| !valid_text(dependency)) =>
        {
            return Err(CapabilityLedgerError::InvalidIdentifier("task mutation"));
        }
        _ => {}
    }
    Ok(())
}

impl<R> CapabilityService<R>
where
    R: DurableRepoLike,
{
    fn append_fact(
        &mut self,
        namespace: &str,
        key: String,
        value: Value,
    ) -> Result<(), CapabilityLedgerError> {
        let seq = match self.repo.records().last() {
            Some(record) => record
                .seq()
                .checked_add(1)
                .ok_or(CapabilityLedgerError::SequenceOverflow)?,
            None => 0,
        };
        let fact = DurableFact {
            namespace: namespace.to_owned(),
            key,
            value,
        };
        let fact_bytes = serde_json::to_vec(&fact)
            .map_err(|_| CapabilityLedgerError::InvalidIdentifier("fact"))?
            .len();
        if fact_bytes > MAX_FACT_BYTES {
            return Err(CapabilityLedgerError::InvalidIdentifier("fact"));
        }
        self.repo
            .append(DurableRecord::Fact { seq, fact })
            .map_err(|error| CapabilityLedgerError::Persist(error.to_string()))
    }

    fn restore_records(&mut self) -> Result<(), CapabilityLedgerError> {
        let records = self.repo.records().to_vec();
        for record in records {
            let DurableRecord::Fact { fact, .. } = record else {
                continue;
            };
            let fact_bytes = serde_json::to_vec(&fact)
                .map_err(|_| CapabilityLedgerError::InvalidRecords("fact encoding".into()))?
                .len();
            if fact_bytes > MAX_FACT_BYTES {
                return Err(CapabilityLedgerError::InvalidRecords(
                    "fact exceeds bound".into(),
                ));
            }
            if fact.key.len() > MAX_CAPABILITY_ID_BYTES * 4 {
                return Err(CapabilityLedgerError::InvalidRecords(
                    "fact key exceeds bound".into(),
                ));
            }
            match fact.namespace.as_str() {
                "capability.v1" => {
                    let value = fact.value;
                    if value.get("schema_version").and_then(Value::as_u64)
                        != Some(CAPABILITY_SCHEMA_VERSION as u64)
                    {
                        return Err(CapabilityLedgerError::InvalidRecords(
                            "capability schema version".into(),
                        ));
                    }
                    let kind = value.get("kind").and_then(Value::as_str).unwrap_or("");
                    match kind {
                        "intent" => {
                            let request: CapabilityRequest = serde_json::from_value(
                                value.get("request").cloned().unwrap_or_default(),
                            )
                            .map_err(|_| {
                                CapabilityLedgerError::InvalidRecords("capability intent".into())
                            })?;
                            validate_request(&request)?;
                            self.catalog
                                .authorize(
                                    &request.capability_id,
                                    request.mode,
                                    request.authorization,
                                )
                                .map_err(|_| {
                                    CapabilityLedgerError::InvalidRecords(
                                        "capability authorization mismatch".into(),
                                    )
                                })?;
                            let effect_id = self.effect_id_for(&request.execution_id)?;
                            let queue_id = request.queue_id.clone();
                            self.capabilities.insert(
                                queue_id.clone(),
                                CapabilityState {
                                    request,
                                    state: CapabilityExecutionState::Queued,
                                    effect_id,
                                },
                            );
                            self.capability_queue.push_back(queue_id);
                        }
                        "claim" => {
                            let queue_id = required_fact_str(&value, "queue_id")?;
                            if let Some(state) = self.capabilities.get_mut(&queue_id) {
                                state.state = CapabilityExecutionState::Claimed;
                                self.capability_queue.retain(|id| id != &queue_id);
                            }
                        }
                        "terminal" => {
                            let queue_id = required_fact_str(&value, "queue_id")?;
                            let status: CapabilityTerminal = serde_json::from_value(
                                value.get("status").cloned().unwrap_or_default(),
                            )
                            .map_err(|_| {
                                CapabilityLedgerError::InvalidRecords("capability terminal".into())
                            })?;
                            if let Some(state) = self.capabilities.get_mut(&queue_id) {
                                state.state = CapabilityExecutionState::Terminal(status);
                                self.capability_queue.retain(|id| id != &queue_id);
                            }
                        }
                        "cancel_requested" => {
                            let queue_id = required_fact_str(&value, "queue_id")?;
                            if let Some(state) = self.capabilities.get_mut(&queue_id) {
                                state.state = CapabilityExecutionState::CancellationRequested;
                            }
                        }
                        "retry_decision" => {
                            let queue_id = required_fact_str(&value, "queue_id")?;
                            if let Some(state) = self.capabilities.get_mut(&queue_id) {
                                state.state = CapabilityExecutionState::Claimed;
                            }
                        }
                        "child_intent" => {
                            let request: ChildRequest = serde_json::from_value(
                                value.get("request").cloned().unwrap_or_default(),
                            )
                            .map_err(|_| {
                                CapabilityLedgerError::InvalidRecords("child intent".into())
                            })?;
                            validate_child_request(&request)?;
                            let status: DurableChildStatus = serde_json::from_value(
                                value.get("status").cloned().unwrap_or_default(),
                            )
                            .map_err(|_| {
                                CapabilityLedgerError::InvalidRecords("child status".into())
                            })?;
                            let mode: OperatingMode = serde_json::from_value(
                                value.get("mode").cloned().unwrap_or_default(),
                            )
                            .map_err(|_| {
                                CapabilityLedgerError::InvalidRecords("child mode".into())
                            })?;
                            let authorization: AuthorizationGrant = serde_json::from_value(
                                value.get("authorization").cloned().unwrap_or_default(),
                            )
                            .map_err(|_| {
                                CapabilityLedgerError::InvalidRecords("child authorization".into())
                            })?;
                            self.catalog
                                .authorize("agent.child", mode, authorization)
                                .map_err(|_| {
                                    CapabilityLedgerError::InvalidRecords(
                                        "child authorization mismatch".into(),
                                    )
                                })?;
                            let queue_id = request.queue_id.clone();
                            self.children
                                .insert(queue_id.clone(), ChildState { request, status });
                            if matches!(status, DurableChildStatus::Queued) {
                                self.child_queue.push_back(queue_id);
                            }
                        }
                        "child_terminal" => {
                            let queue_id = required_fact_str(&value, "queue_id")?;
                            let status: DurableChildStatus = serde_json::from_value(
                                value.get("status").cloned().unwrap_or_default(),
                            )
                            .map_err(|_| {
                                CapabilityLedgerError::InvalidRecords("child terminal".into())
                            })?;
                            if let Some(child) = self.children.get_mut(&queue_id) {
                                child.status = status;
                                self.child_queue.retain(|id| id != &queue_id);
                            }
                        }
                        "child_started" => {
                            let queue_id = required_fact_str(&value, "queue_id")?;
                            if let Some(child) = self.children.get_mut(&queue_id) {
                                child.status = DurableChildStatus::Active;
                                self.child_queue.retain(|id| id != &queue_id);
                            }
                        }
                        "child_cancel_requested" => {
                            let queue_id = required_fact_str(&value, "queue_id")?;
                            if let Some(child) = self.children.get_mut(&queue_id) {
                                child.status = DurableChildStatus::CancellationRequested;
                            }
                        }
                        _ => {
                            return Err(CapabilityLedgerError::InvalidRecords(
                                "unknown capability fact kind".into(),
                            ));
                        }
                    }
                }
                "task.v1" => {
                    let value = fact.value;
                    if value.get("schema_version").and_then(Value::as_u64)
                        != Some(CAPABILITY_SCHEMA_VERSION as u64)
                    {
                        return Err(CapabilityLedgerError::InvalidRecords(
                            "task schema version".into(),
                        ));
                    }
                    let request: TaskMutationRequest =
                        serde_json::from_value(value.get("request").cloned().unwrap_or_default())
                            .map_err(|_| {
                            CapabilityLedgerError::InvalidRecords("task mutation".into())
                        })?;
                    let capability_id = value
                        .get("capability_id")
                        .and_then(Value::as_str)
                        .unwrap_or("task.todo");
                    let mode: OperatingMode =
                        serde_json::from_value(value.get("mode").cloned().unwrap_or_default())
                            .map_err(|_| {
                                CapabilityLedgerError::InvalidRecords("task mode".into())
                            })?;
                    let authorization: AuthorizationGrant = serde_json::from_value(
                        value.get("authorization").cloned().unwrap_or_default(),
                    )
                    .map_err(|_| {
                        CapabilityLedgerError::InvalidRecords("task authorization".into())
                    })?;
                    self.catalog
                        .authorize(capability_id, mode, authorization)
                        .map_err(|_| {
                            CapabilityLedgerError::InvalidRecords(
                                "task authorization mismatch".into(),
                            )
                        })?;
                    self.task_idempotency
                        .insert(request.idempotency_key.clone(), request.clone());
                    self.tasks
                        .entry(request.entity_id.clone())
                        .or_insert_with(|| TaskState {
                            revision: 0,
                            mutations: Vec::new(),
                        })
                        .mutations
                        .push(request.clone());
                    if let Some(task) = self.tasks.get_mut(&request.entity_id) {
                        task.revision = request.revision;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn derive_child_depth(&self, request: &ChildRequest) -> Result<u8, CapabilityLedgerError> {
        if request.parent_session_id == self.repo.header().id {
            if let Some(parent) = self
                .capabilities
                .values()
                .find(|state| state.request.execution_id == request.parent_execution_id)
            {
                if matches!(
                    parent.state,
                    CapabilityExecutionState::CancellationRequested
                        | CapabilityExecutionState::Terminal(_)
                ) {
                    return Err(CapabilityLedgerError::ParentNotActive(
                        request.parent_execution_id.clone(),
                    ));
                }
                return Ok(1);
            }
            return Err(CapabilityLedgerError::ParentExecutionUnknown(
                request.parent_execution_id.clone(),
            ));
        }
        for child in self.children.values() {
            if child.request.execution_id == request.parent_execution_id
                && child.request.child_session_id == request.parent_session_id
            {
                return Ok(child.request.depth.saturating_add(1));
            }
        }
        Err(CapabilityLedgerError::ParentExecutionUnknown(
            request.parent_execution_id.clone(),
        ))
    }

    fn live_child_count(&self) -> usize {
        self.children
            .values()
            .filter(|child| !child.status.is_terminal())
            .count()
    }

    pub fn pending_child_promotions(&self) -> Vec<ChildRequest> {
        self.child_queue
            .iter()
            .filter_map(|queue_id| {
                self.children
                    .get(queue_id)
                    .map(|child| child.request.clone())
            })
            .collect()
    }

    pub fn promote_next_child(
        &mut self,
        mode: OperatingMode,
        authorization: AuthorizationGrant,
    ) -> Result<Result<(), CapabilityLedgerError>, CapabilityLedgerError> {
        self.catalog.authorize("agent.child", mode, authorization)?;
        let Some(queue_id) = self.child_queue.front().cloned() else {
            return Ok(Err(CapabilityLedgerError::UnknownChild(
                "no promotion available".into(),
            )));
        };
        Ok(self.promote_queued_child(&queue_id))
    }

    fn promote_queued_child(&mut self, queue_id: &str) -> Result<(), CapabilityLedgerError> {
        let Some(request) = self
            .children
            .get(queue_id)
            .map(|child| child.request.clone())
        else {
            return Err(CapabilityLedgerError::UnknownChild(queue_id.into()));
        };
        self.append_fact(
            "capability.v1",
            format!("{queue_id}.child_started"),
            json!({
                "schema_version": CAPABILITY_SCHEMA_VERSION,
                "kind": "child_started",
                "queue_id": queue_id,
                "execution_id": request.execution_id,
                "child_session_id": request.child_session_id,
            }),
        )?;
        if let Some(child) = self.children.get_mut(queue_id) {
            child.status = DurableChildStatus::Active;
        }
        self.child_queue.retain(|id| id != queue_id);
        Ok(())
    }

    fn request_child_cancellation(&mut self, queue_id: &str) -> Result<(), CapabilityLedgerError> {
        let Some(status) = self.children.get(queue_id).map(|child| child.status) else {
            return Err(CapabilityLedgerError::UnknownChild(queue_id.into()));
        };
        match status {
            DurableChildStatus::Completed
            | DurableChildStatus::Cancelled
            | DurableChildStatus::Failed => Err(CapabilityLedgerError::InvalidChildTransition(
                queue_id.into(),
            )),
            DurableChildStatus::CancellationRequested => Ok(()),
            DurableChildStatus::Queued => {
                self.append_fact(
                    "capability.v1",
                    format!("{queue_id}.child_cancelled"),
                    json!({
                        "schema_version": CAPABILITY_SCHEMA_VERSION,
                        "kind": "child_terminal",
                        "queue_id": queue_id,
                        "status": DurableChildStatus::Cancelled,
                    }),
                )?;
                if let Some(child) = self.children.get_mut(queue_id) {
                    child.status = DurableChildStatus::Cancelled;
                }
                self.child_queue.retain(|id| id != queue_id);
                if let Some(next) = self.child_queue.front().cloned() {
                    self.promote_queued_child(&next)?;
                }
                Ok(())
            }
            DurableChildStatus::Active => {
                self.append_fact(
                    "capability.v1",
                    format!("{queue_id}.child_cancel_requested"),
                    json!({
                        "schema_version": CAPABILITY_SCHEMA_VERSION,
                        "kind": "child_cancel_requested",
                        "queue_id": queue_id,
                    }),
                )?;
                if let Some(child) = self.children.get_mut(queue_id) {
                    child.status = DurableChildStatus::CancellationRequested;
                }
                Ok(())
            }
        }
    }

    fn transition_child(
        &mut self,
        queue_id: &str,
        status: DurableChildStatus,
    ) -> Result<(), CapabilityLedgerError> {
        if !status.is_terminal() {
            return Err(CapabilityLedgerError::InvalidChildTransition(
                queue_id.into(),
            ));
        }
        let Some(request) = self
            .children
            .get(queue_id)
            .map(|child| child.request.clone())
        else {
            return Err(CapabilityLedgerError::UnknownChild(queue_id.into()));
        };
        if self
            .children
            .get(queue_id)
            .is_some_and(|child| child.status.is_terminal())
        {
            return Err(CapabilityLedgerError::InvalidChildTransition(
                queue_id.into(),
            ));
        }
        self.append_fact(
            "capability.v1",
            format!("{queue_id}.child_terminal"),
            json!({
                "schema_version": CAPABILITY_SCHEMA_VERSION,
                "kind": "child_terminal",
                "queue_id": queue_id,
                "execution_id": request.execution_id,
                "status": status,
            }),
        )?;
        if let Some(child) = self.children.get_mut(queue_id) {
            child.status = status;
        }
        self.child_queue.retain(|id| id != queue_id);
        if let Some(next) = self.child_queue.front().cloned() {
            self.promote_queued_child(&next)?;
        }
        Ok(())
    }
}

/// Durable repository contract for the capability ledger. Implemented for
/// every DurableRepo (memory and JSONL).
pub trait DurableRepoLike: DurableRepo {}
impl<T: DurableRepo> DurableRepoLike for T {}

fn required_fact_str(value: &Value, field: &str) -> Result<String, CapabilityLedgerError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| CapabilityLedgerError::InvalidRecords(field.to_owned()))
}

fn validate_identifier(id: &str) -> Result<(), CapabilityLedgerError> {
    if id.is_empty() || id.len() > MAX_CAPABILITY_ID_BYTES || id.chars().any(|c| c.is_control()) {
        return Err(CapabilityLedgerError::InvalidIdentifier("identifier"));
    }
    Ok(())
}

fn validate_base_identifier(id: &str) -> Result<(), CapabilityLedgerError> {
    if id.is_empty() || id.len() > MAX_BASE_ID_BYTES || id.chars().any(|c| c.is_control()) {
        return Err(CapabilityLedgerError::InvalidIdentifier("base identifier"));
    }
    Ok(())
}

fn validate_descriptor(descriptor: &CapabilityDescriptor) -> Result<(), CapabilityLedgerError> {
    validate_identifier(&descriptor.id)?;
    let id = descriptor.id.as_str();
    let kind_ok = match descriptor.kind {
        CapabilityKind::NativeTool => id.starts_with("tool."),
        CapabilityKind::Skill => id.starts_with("skill."),
        CapabilityKind::McpTool => id.starts_with("mcp."),
        CapabilityKind::McpResource => id.starts_with("mcp."),
        CapabilityKind::McpPrompt => id.starts_with("mcp."),
        CapabilityKind::ChildAgent => id == "agent.child",
        CapabilityKind::Todo => id == "task.todo",
        CapabilityKind::Plan => id == "task.plan",
        CapabilityKind::Goal => id == "task.goal",
    };
    if !kind_ok {
        return Err(CapabilityLedgerError::InvalidCapabilityPolicy(
            "capability kind does not match its id prefix",
        ));
    }
    Ok(())
}

fn validate_request(request: &CapabilityRequest) -> Result<(), CapabilityLedgerError> {
    validate_base_identifier(&request.queue_id)?;
    validate_base_identifier(&request.execution_id)?;
    validate_identifier(&request.capability_id)?;
    Ok(())
}

fn validate_child_request(request: &ChildRequest) -> Result<(), CapabilityLedgerError> {
    validate_base_identifier(&request.queue_id)?;
    validate_base_identifier(&request.execution_id)?;
    validate_base_identifier(&request.parent_execution_id)?;
    validate_base_identifier(&request.parent_session_id)?;
    validate_base_identifier(&request.child_session_id)?;
    Ok(())
}
