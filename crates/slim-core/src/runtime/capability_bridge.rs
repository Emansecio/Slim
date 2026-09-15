//! Runtime-facing composition for durable capabilities.
//!
//! The session capability ledger owns authorization and persistence.  This
//! module owns the in-process adapters around that ledger: native tools,
//! discovered skills, selected MCP contracts, the child scheduler and typed
//! task models.  It deliberately does not create a provider, MCP transport or
//! child process.  Callers that have such an external executor must implement
//! [`RuntimeCapabilityAdapter`] and receive the stable persisted effect id.

use std::collections::BTreeMap;
use std::path::Path;

use crate::agents::ChildStatus;
use crate::mcp::McpCatalog;
use crate::session::{
    AuthorizationGrant, CapabilityCatalog, CapabilityDescriptor, CapabilityDispatch,
    CapabilityKind, CapabilityLedgerError, CapabilityRequest, CapabilityService,
    CapabilityTerminal, ChildRequest, DurableChildStatus, DurableRecord, DurableRepoLike,
    TaskGoalAssurance, TaskMutation, TaskMutationRequest, TaskTodoStatus,
};
use crate::skills::{
    invoke_script_with_limits, DiscoveryResult, SkillEntry, SkillInvocationError,
    DEFAULT_SKILL_OUTPUT_BYTES, DEFAULT_SKILL_TIMEOUT,
};
use crate::task::{Assurance, Goal, Plan, TodoStatus, TodoTracker};
use crate::tools::{ToolRegistry, ToolResult};
use crate::OperatingMode;

use super::CancellationToken;

/// The concrete in-process target represented by a catalog descriptor.
#[derive(Clone, Debug)]
pub enum RuntimeCapabilityTarget {
    NativeTool { name: String },
    Skill { entry: SkillEntry },
    McpTool { catalog: McpCatalog, name: String },
    McpResource { catalog: McpCatalog, name: String },
    McpPrompt { catalog: McpCatalog, name: String },
}

/// Adapter boundary used by the runtime bridge.
///
/// The descriptor is policy-checked by [`CapabilityService`] before this
/// method is called.  The effect id is durable and must be used by adapters
/// that need external idempotency.  No adapter result is persisted until this
/// method returns and the service writes its terminal record.
pub trait RuntimeCapabilityAdapter {
    fn dispatch(
        &mut self,
        effect_id: &str,
        descriptor: &CapabilityDescriptor,
        target: &RuntimeCapabilityTarget,
        request: &CapabilityRequest,
    ) -> CapabilityTerminal;
}

impl<F> RuntimeCapabilityAdapter for F
where
    F: FnMut(&str, &CapabilityDescriptor, &RuntimeCapabilityTarget) -> CapabilityTerminal,
{
    fn dispatch(
        &mut self,
        effect_id: &str,
        descriptor: &CapabilityDescriptor,
        target: &RuntimeCapabilityTarget,
        _request: &CapabilityRequest,
    ) -> CapabilityTerminal {
        self(effect_id, descriptor, target)
    }
}

/// A small adapter for offline/native execution.
///
/// Native tools and discovered skills run in-process.  Skill scripts use the
/// bounded invoker; MCP remains a local catalog contract, not a transport.
pub struct InProcessCapabilityAdapter<'a> {
    tools: &'a ToolRegistry,
    cwd: &'a Path,
    arguments: &'a str,
    cancellation: Option<&'a CancellationToken>,
    last_output: String,
}

impl<'a> InProcessCapabilityAdapter<'a> {
    pub fn new(tools: &'a ToolRegistry, cwd: &'a Path, arguments: &'a str) -> Self {
        Self {
            tools,
            cwd,
            arguments,
            cancellation: None,
            last_output: String::new(),
        }
    }

    pub fn with_cancellation(mut self, cancellation: &'a CancellationToken) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    pub fn take_output(&mut self) -> String {
        std::mem::take(&mut self.last_output)
    }
}

impl RuntimeCapabilityAdapter for InProcessCapabilityAdapter<'_> {
    fn dispatch(
        &mut self,
        _effect_id: &str,
        descriptor: &CapabilityDescriptor,
        target: &RuntimeCapabilityTarget,
        request: &CapabilityRequest,
    ) -> CapabilityTerminal {
        match (descriptor.kind, target) {
            (CapabilityKind::NativeTool, RuntimeCapabilityTarget::NativeTool { name }) => {
                let result = self.tools.execute_with_cancellation(
                    request.mode,
                    self.cwd,
                    name,
                    self.arguments,
                    self.cancellation,
                );
                if result.success {
                    CapabilityTerminal::Success
                } else {
                    CapabilityTerminal::Failed
                }
            }
            (CapabilityKind::McpTool, RuntimeCapabilityTarget::McpTool { catalog, name }) => {
                if catalog
                    .call_tool(request.mode, name, self.arguments)
                    .is_ok()
                {
                    CapabilityTerminal::Success
                } else {
                    CapabilityTerminal::Failed
                }
            }
            (
                CapabilityKind::McpResource,
                RuntimeCapabilityTarget::McpResource { catalog, name },
            ) if catalog.selected_resources().iter().any(|item| item == name) => {
                CapabilityTerminal::Success
            }
            (CapabilityKind::McpPrompt, RuntimeCapabilityTarget::McpPrompt { catalog, name })
                if catalog.selected_prompts().iter().any(|item| item == name) =>
            {
                CapabilityTerminal::Success
            }
            (CapabilityKind::Skill, RuntimeCapabilityTarget::Skill { entry }) => {
                if let Some(body) = crate::skills::fallback_skill_body(&entry.path, "run.ps1") {
                    self.last_output = body;
                    return CapabilityTerminal::Success;
                }
                match invoke_script_with_limits(
                    &entry.path,
                    "run.ps1",
                    request.mode,
                    true,
                    DEFAULT_SKILL_TIMEOUT,
                    DEFAULT_SKILL_OUTPUT_BYTES,
                    self.cancellation,
                ) {
                    Ok(output) => {
                        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                        self.last_output = if stderr.trim().is_empty() {
                            stdout
                        } else {
                            format!("{stdout}\nstderr:\n{stderr}")
                        };
                        if output.status.success() {
                            CapabilityTerminal::Success
                        } else {
                            CapabilityTerminal::Failed
                        }
                    }
                    Err(error) => {
                        let cancelled = matches!(error, SkillInvocationError::Cancelled);
                        self.last_output = error.to_string();
                        if cancelled {
                            CapabilityTerminal::Cancelled
                        } else {
                            CapabilityTerminal::Failed
                        }
                    }
                }
            }
            _ => CapabilityTerminal::Failed,
        }
    }
}

/// Runtime-facing durable capability composition.
pub struct RuntimeCapabilityBridge<R> {
    service: CapabilityService<R>,
    tools: ToolRegistry,
    targets: BTreeMap<String, RuntimeCapabilityTarget>,
    cancellation: CancellationToken,
    child_tokens: BTreeMap<String, CancellationToken>,
    capability_tokens: BTreeMap<String, CancellationToken>,
    todos: BTreeMap<String, TodoTracker>,
    plans: BTreeMap<String, Plan>,
    goals: BTreeMap<String, Goal>,
}

impl<R> RuntimeCapabilityBridge<R>
where
    R: DurableRepoLike,
{
    pub fn new(
        repo: R,
        catalog: CapabilityCatalog,
        discovery: &DiscoveryResult,
        mcp_catalogs: &[McpCatalog],
        tools: ToolRegistry,
        cancellation: CancellationToken,
    ) -> Result<Self, CapabilityLedgerError> {
        let service = CapabilityService::new(repo, catalog)?;
        let mut targets = BTreeMap::new();
        for name in tools.names_for_mode(OperatingMode::Auto) {
            targets.insert(
                format!("tool.{name}"),
                RuntimeCapabilityTarget::NativeTool { name: name.into() },
            );
        }
        for entry in discovery.active_entries() {
            targets.insert(
                format!("skill.{}", entry.name),
                RuntimeCapabilityTarget::Skill {
                    entry: entry.clone(),
                },
            );
        }
        for mcp in mcp_catalogs {
            for name in mcp.tool_names() {
                targets.insert(
                    crate::mcp::canonical_name(mcp.server_name(), name),
                    RuntimeCapabilityTarget::McpTool {
                        catalog: mcp.clone(),
                        name: name.clone(),
                    },
                );
            }
            for name in mcp.resource_names() {
                targets.insert(
                    format!("mcp.{}.resource.{name}", mcp.server_name()),
                    RuntimeCapabilityTarget::McpResource {
                        catalog: mcp.clone(),
                        name: name.clone(),
                    },
                );
            }
            for name in mcp.prompt_names() {
                targets.insert(
                    format!("mcp.{}.prompt.{name}", mcp.server_name()),
                    RuntimeCapabilityTarget::McpPrompt {
                        catalog: mcp.clone(),
                        name: name.clone(),
                    },
                );
            }
        }
        let mut bridge = Self {
            service,
            tools,
            targets,
            cancellation,
            child_tokens: BTreeMap::new(),
            capability_tokens: BTreeMap::new(),
            todos: BTreeMap::new(),
            plans: BTreeMap::new(),
            goals: BTreeMap::new(),
        };
        bridge.restore_typed_models()?;
        bridge.restore_scheduler()?;
        bridge.restore_capability_tokens();
        Ok(bridge)
    }

    pub fn service(&self) -> &CapabilityService<R> {
        &self.service
    }

    pub fn service_mut(&mut self) -> &mut CapabilityService<R> {
        &mut self.service
    }

    pub fn into_service(self) -> CapabilityService<R> {
        self.service
    }

    pub fn target(&self, capability_id: &str) -> Option<&RuntimeCapabilityTarget> {
        self.targets.get(capability_id)
    }

    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancellation
    }

    pub fn child_token(&self, queue_id: &str) -> Option<&CancellationToken> {
        self.child_tokens.get(queue_id)
    }

    pub fn capability_token(&self, queue_id: &str) -> Option<&CancellationToken> {
        self.capability_tokens.get(queue_id)
    }

    pub fn scheduler_status(&self, queue_id: &str) -> Option<ChildStatus> {
        self.service
            .child_status(queue_id)
            .map(child_status_from_durable)
    }

    pub fn scheduler_depth(&self, queue_id: &str) -> Option<u8> {
        self.service
            .child_states()
            .into_iter()
            .find(|(request, _)| request.queue_id == queue_id)
            .map(|(request, _)| request.depth)
    }

    pub fn scheduler_queue_ids(&self) -> Vec<String> {
        self.service
            .child_queue()
            .into_iter()
            .map(|request| request.queue_id.clone())
            .collect()
    }

    pub fn dispatch<A: RuntimeCapabilityAdapter>(
        &mut self,
        request: CapabilityRequest,
        adapter: &mut A,
    ) -> Result<CapabilityDispatch, CapabilityLedgerError> {
        self.require_mcp_selection(&request)?;
        let queue_id = request.queue_id.clone();
        self.capability_tokens.entry(queue_id.clone()).or_default();
        let targets = &self.targets;
        let adapter_request = request.clone();
        let result = self.service.dispatch(request, |descriptor, effect_id| {
            targets
                .get(&descriptor.id)
                .map_or(CapabilityTerminal::Failed, |target| {
                    adapter.dispatch(effect_id, descriptor, target, &adapter_request)
                })
        });
        if result.is_err() && self.service.capability_state(&queue_id).is_none() {
            self.capability_tokens.remove(&queue_id);
        }
        result
    }

    pub fn enqueue_capability(
        &mut self,
        request: CapabilityRequest,
    ) -> Result<(), CapabilityLedgerError> {
        let queue_id = request.queue_id.clone();
        self.service.enqueue(request)?;
        self.capability_tokens
            .insert(queue_id, CancellationToken::new());
        Ok(())
    }

    pub fn dispatch_queued<A: RuntimeCapabilityAdapter>(
        &mut self,
        queue_id: &str,
        adapter: &mut A,
    ) -> Result<CapabilityDispatch, CapabilityLedgerError> {
        let request = self
            .service
            .capability_request(queue_id)
            .cloned()
            .ok_or_else(|| CapabilityLedgerError::UnknownQueueId(queue_id.into()))?;
        self.require_mcp_selection(&request)?;
        self.capability_tokens.entry(queue_id.into()).or_default();
        let targets = &self.targets;
        self.service
            .dispatch_queued(queue_id, |descriptor, effect_id| {
                targets
                    .get(&descriptor.id)
                    .map_or(CapabilityTerminal::Failed, |target| {
                        adapter.dispatch(effect_id, descriptor, target, &request)
                    })
            })
    }

    pub fn retry_in_flight<A: RuntimeCapabilityAdapter>(
        &mut self,
        queue_id: &str,
        adapter: &mut A,
    ) -> Result<CapabilityDispatch, CapabilityLedgerError> {
        let request = self
            .service
            .capability_request(queue_id)
            .cloned()
            .ok_or_else(|| CapabilityLedgerError::UnknownQueueId(queue_id.into()))?;
        self.require_mcp_selection(&request)?;
        self.capability_tokens.entry(queue_id.into()).or_default();
        let targets = &self.targets;
        self.service
            .retry_in_flight(queue_id, |descriptor, effect_id| {
                targets
                    .get(&descriptor.id)
                    .map_or(CapabilityTerminal::Failed, |target| {
                        adapter.dispatch(effect_id, descriptor, target, &request)
                    })
            })
    }

    pub fn dispatch_in_process(
        &mut self,
        request: CapabilityRequest,
        cwd: &Path,
        arguments: &str,
    ) -> Result<CapabilityDispatch, CapabilityLedgerError> {
        let token = self
            .capability_tokens
            .entry(request.queue_id.clone())
            .or_default()
            .clone();
        let tools = self.tools.clone();
        let mut adapter =
            InProcessCapabilityAdapter::new(&tools, cwd, arguments).with_cancellation(&token);
        self.dispatch(request, &mut adapter)
    }

    pub fn dispatch_queued_in_process(
        &mut self,
        queue_id: &str,
        cwd: &Path,
        arguments: &str,
    ) -> Result<CapabilityDispatch, CapabilityLedgerError> {
        let token = self
            .capability_tokens
            .entry(queue_id.into())
            .or_default()
            .clone();
        let tools = self.tools.clone();
        let mut adapter =
            InProcessCapabilityAdapter::new(&tools, cwd, arguments).with_cancellation(&token);
        self.dispatch_queued(queue_id, &mut adapter)
    }

    fn require_mcp_selection(
        &self,
        request: &CapabilityRequest,
    ) -> Result<(), CapabilityLedgerError> {
        let Some(target) = self.targets.get(&request.capability_id) else {
            return Ok(());
        };
        let selected = match target {
            RuntimeCapabilityTarget::McpResource { catalog, name } => {
                catalog.selected_resources().iter().any(|item| item == name)
            }
            RuntimeCapabilityTarget::McpPrompt { catalog, name } => {
                catalog.selected_prompts().iter().any(|item| item == name)
            }
            _ => true,
        };
        if !selected {
            return Err(CapabilityLedgerError::SelectionRequired {
                capability_id: request.capability_id.clone(),
            });
        }
        Ok(())
    }

    pub fn enqueue_child(
        &mut self,
        request: ChildRequest,
        mode: OperatingMode,
        authorization: AuthorizationGrant,
    ) -> Result<(), CapabilityLedgerError> {
        self.service.enqueue_child(request, mode, authorization)?;
        self.restore_scheduler()?;
        Ok(())
    }

    pub fn cancel_child(
        &mut self,
        queue_id: &str,
        mode: OperatingMode,
        authorization: AuthorizationGrant,
    ) -> Result<(), CapabilityLedgerError> {
        self.service.cancel_child(queue_id, mode, authorization)?;
        if let Some(token) = self.child_tokens.get(queue_id) {
            token.cancel();
        }
        self.restore_scheduler()?;
        Ok(())
    }

    pub fn finish_child(
        &mut self,
        queue_id: &str,
        status: DurableChildStatus,
        mode: OperatingMode,
        authorization: AuthorizationGrant,
    ) -> Result<(), CapabilityLedgerError> {
        self.service
            .finish_child(queue_id, status, mode, authorization)?;
        self.restore_scheduler()?;
        Ok(())
    }

    pub fn child_status(&self, queue_id: &str) -> Option<DurableChildStatus> {
        self.service.child_status(queue_id)
    }

    pub fn apply_task_mutation(
        &mut self,
        request: TaskMutationRequest,
        mode: OperatingMode,
        authorization: AuthorizationGrant,
    ) -> Result<bool, CapabilityLedgerError> {
        self.service
            .authorize_task_mutation(&request, mode, authorization)?;
        if let Some(previous) = self.service.task_mutation(&request.idempotency_key) {
            if previous == &request {
                return Ok(false);
            }
            return self
                .service
                .apply_task_mutation(request, mode, authorization);
        }
        let entity_id = request.entity_id.clone();
        let mutation = request.mutation.clone();
        let mut todos = self.todos.clone();
        let mut plans = self.plans.clone();
        let mut goals = self.goals.clone();
        apply_typed_mutation(&mut todos, &mut plans, &mut goals, &entity_id, &mutation)?;
        let changed = self
            .service
            .apply_task_mutation(request, mode, authorization)?;
        if changed {
            self.todos = todos;
            self.plans = plans;
            self.goals = goals;
        }
        Ok(changed)
    }

    pub fn todo(&self, entity_id: &str) -> Option<&TodoTracker> {
        self.todos.get(entity_id)
    }

    pub fn task_revision(&self, entity_id: &str) -> u64 {
        self.service.task_revision(entity_id)
    }

    pub fn plan(&self, entity_id: &str) -> Option<&Plan> {
        self.plans.get(entity_id)
    }

    pub fn goal(&self, entity_id: &str) -> Option<&Goal> {
        self.goals.get(entity_id)
    }

    pub fn cancel_capability(
        &mut self,
        queue_id: &str,
        mode: OperatingMode,
        authorization: AuthorizationGrant,
    ) -> Result<(), CapabilityLedgerError> {
        self.service
            .cancel_capability(queue_id, mode, authorization)?;
        if let Some(token) = self.capability_tokens.get(queue_id) {
            token.cancel();
        }
        Ok(())
    }

    pub fn finish_capability_cancellation(
        &mut self,
        queue_id: &str,
        mode: OperatingMode,
        authorization: AuthorizationGrant,
    ) -> Result<(), CapabilityLedgerError> {
        self.service
            .finish_capability_cancellation(queue_id, mode, authorization)
    }

    fn restore_typed_models(&mut self) -> Result<(), CapabilityLedgerError> {
        let mutations = self
            .service
            .repo()
            .records()
            .iter()
            .filter_map(|record| match record {
                DurableRecord::Fact { fact, .. }
                    if fact.namespace == "task.v1" && fact.value["kind"] == "mutation" =>
                {
                    Some(fact.value["request"].clone())
                }
                _ => None,
            })
            .map(serde_json::from_value::<TaskMutationRequest>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| CapabilityLedgerError::InvalidRecords(error.to_string()))?;
        for request in mutations {
            let entity_id = request.entity_id.clone();
            let mutation = request.mutation.clone();
            apply_typed_mutation(
                &mut self.todos,
                &mut self.plans,
                &mut self.goals,
                &entity_id,
                &mutation,
            )?;
        }
        Ok(())
    }

    fn restore_scheduler(&mut self) -> Result<(), CapabilityLedgerError> {
        let snapshots = self.service.child_states();
        let mut existing_tokens = std::mem::take(&mut self.child_tokens);
        let mut reconciled_tokens = BTreeMap::new();
        for (request, status) in snapshots {
            let token = existing_tokens
                .remove(&request.queue_id)
                .unwrap_or_default();
            if matches!(
                status,
                DurableChildStatus::CancellationRequested
                    | DurableChildStatus::Completed
                    | DurableChildStatus::Cancelled
                    | DurableChildStatus::Failed
            ) {
                token.cancel();
            }
            reconciled_tokens.insert(request.queue_id, token);
        }
        self.child_tokens = reconciled_tokens;
        Ok(())
    }

    fn restore_capability_tokens(&mut self) {
        self.capability_tokens.clear();
        for (request, state) in self.service.capability_states() {
            let token = CancellationToken::new();
            if matches!(
                state,
                crate::session::CapabilityExecutionState::CancellationRequested
                    | crate::session::CapabilityExecutionState::Terminal(_)
            ) {
                token.cancel();
            }
            self.capability_tokens.insert(request.queue_id, token);
        }
    }
}

fn apply_typed_mutation(
    todos: &mut BTreeMap<String, TodoTracker>,
    plans: &mut BTreeMap<String, Plan>,
    goals: &mut BTreeMap<String, Goal>,
    entity_id: &str,
    mutation: &TaskMutation,
) -> Result<(), CapabilityLedgerError> {
    let error = || CapabilityLedgerError::InvalidIdentifier("task invariant");
    match mutation {
        TaskMutation::TodoAdd { title, status } => {
            let tracker = todos.entry(entity_id.into()).or_default();
            let id = tracker.add(title.clone());
            if let Some(status) = status {
                tracker
                    .set_status(id, todo_status(status.clone()))
                    .map_err(CapabilityLedgerError::InvalidTaskTransition)?;
            }
        }
        TaskMutation::TodoSetStatus { id, status } => {
            let tracker = todos.entry(entity_id.into()).or_default();
            let id = id
                .or_else(|| {
                    tracker
                        .items()
                        .iter()
                        .find(|item| item.status == TodoStatus::Pending)
                        .or_else(|| tracker.items().last())
                        .map(|item| item.id)
                })
                .ok_or_else(error)?;
            tracker
                .set_status(id, todo_status(status.clone()))
                .map_err(CapabilityLedgerError::InvalidTaskTransition)?;
        }
        TaskMutation::PlanAddNode {
            node_id,
            dependencies,
        } => {
            let plan = plans.entry(entity_id.into()).or_default();
            let dependencies = dependencies.iter().map(String::as_str).collect::<Vec<_>>();
            plan.add_node(node_id, &dependencies).map_err(|_| error())?;
        }
        TaskMutation::PlanApprove => {
            plans
                .entry(entity_id.into())
                .or_default()
                .approve()
                .map_err(|_| error())?;
        }
        TaskMutation::GoalSetBudget { budget } => {
            goals
                .entry(entity_id.into())
                .or_insert_with(|| Goal::new(*budget))
                .set_budget(*budget)
                .map_err(|_| error())?;
        }
        TaskMutation::GoalConsume { amount } => {
            goals
                .entry(entity_id.into())
                .or_insert_with(|| Goal::new(None))
                .consume(*amount)
                .map_err(|_| error())?;
        }
        TaskMutation::GoalComplete { assurance } => {
            goals
                .entry(entity_id.into())
                .or_insert_with(|| Goal::new(None))
                .complete(match assurance {
                    TaskGoalAssurance::Verified => Assurance::Verified,
                    TaskGoalAssurance::Unverified => Assurance::Unverified,
                })
                .map_err(|_| error())?;
        }
    }
    Ok(())
}

fn todo_status(status: TaskTodoStatus) -> TodoStatus {
    match status {
        TaskTodoStatus::Pending => TodoStatus::Pending,
        TaskTodoStatus::InProgress => TodoStatus::InProgress,
        TaskTodoStatus::Completed => TodoStatus::Completed,
        TaskTodoStatus::Blocked => TodoStatus::Blocked,
        TaskTodoStatus::Cancelled => TodoStatus::Cancelled,
    }
}

fn child_status_from_durable(status: DurableChildStatus) -> ChildStatus {
    match status {
        DurableChildStatus::Queued => ChildStatus::Queued,
        DurableChildStatus::Active | DurableChildStatus::CancellationRequested => {
            ChildStatus::Active
        }
        DurableChildStatus::Completed => ChildStatus::Completed,
        DurableChildStatus::Cancelled => ChildStatus::Cancelled,
        DurableChildStatus::Failed => ChildStatus::Failed,
    }
}

/// Exposes the actual tool result for callers that need output while keeping
/// capability persistence in [`RuntimeCapabilityBridge::dispatch`].
pub fn execute_native_tool(
    tools: &ToolRegistry,
    mode: OperatingMode,
    cwd: &Path,
    name: &str,
    arguments: &str,
    cancellation: Option<&CancellationToken>,
) -> ToolResult {
    tools.execute_with_cancellation(mode, cwd, name, arguments, cancellation)
}
