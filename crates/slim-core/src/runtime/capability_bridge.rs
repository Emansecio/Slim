//! Durable task state (Todo/Plan/Goal) for the runtime.
//!
//! The session capability ledger owns authorization and persistence of task
//! mutations; this module keeps the typed models rebuilt from those records.
//! It executes nothing: restoring a session never replays a tool or effect.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::session::{
    AuthorizationGrant, CapabilityCatalog, CapabilityLedgerError, CapabilityService, DurableRecord,
    DurableRepoLike, TaskGoalAssurance, TaskMutation, TaskMutationRequest, TaskTodoStatus,
};
use crate::task::{Assurance, Goal, Plan, TodoStatus, TodoTracker};
use crate::OperatingMode;

/// The typed task models, keyed by entity id, that mutations rebuild.
#[derive(Clone, Default)]
struct TaskModels {
    todos: BTreeMap<String, TodoTracker>,
    plans: BTreeMap<String, Plan>,
    goals: BTreeMap<String, Goal>,
}

/// Task models over the durable capability ledger.
pub struct RuntimeCapabilityBridge<R> {
    service: CapabilityService<R>,
    models: TaskModels,
}

impl<R> RuntimeCapabilityBridge<R>
where
    R: DurableRepoLike,
{
    pub fn new(repo: R, catalog: CapabilityCatalog) -> Result<Self, CapabilityLedgerError> {
        let mut bridge = Self {
            service: CapabilityService::new(repo, catalog)?,
            models: TaskModels::default(),
        };
        bridge.restore_typed_models()?;
        Ok(bridge)
    }

    pub fn service(&self) -> &CapabilityService<R> {
        &self.service
    }

    pub fn into_service(self) -> CapabilityService<R> {
        self.service
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
        // Stage on a copy: neither a failed typed mutation nor a ledger
        // rejection may leave the models half-applied.
        let mut staged = self.models.clone();
        staged.apply(&request.entity_id, &request.mutation)?;
        let changed = self
            .service
            .apply_task_mutation(request, mode, authorization)?;
        if changed {
            self.models = staged;
        }
        Ok(changed)
    }

    pub fn todo(&self, entity_id: &str) -> Option<&TodoTracker> {
        self.models.todos.get(entity_id)
    }

    pub fn task_revision(&self, entity_id: &str) -> u64 {
        self.service.task_revision(entity_id)
    }

    pub fn plan(&self, entity_id: &str) -> Option<&Plan> {
        self.models.plans.get(entity_id)
    }

    pub fn goal(&self, entity_id: &str) -> Option<&Goal> {
        self.models.goals.get(entity_id)
    }

    fn restore_typed_models(&mut self) -> Result<(), CapabilityLedgerError> {
        // Parse every record before applying any: a malformed ledger reports
        // its syntax error, not the first invariant it would also break.
        let mutations = self
            .service
            .repo()
            .records()
            .iter()
            .filter_map(|record| match record {
                DurableRecord::Fact { fact, .. }
                    if fact.namespace == "task.v1" && fact.value["kind"] == "mutation" =>
                {
                    Some(TaskMutationRequest::deserialize(&fact.value["request"]))
                }
                _ => None,
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| CapabilityLedgerError::InvalidRecords(error.to_string()))?;
        for request in &mutations {
            self.models.apply(&request.entity_id, &request.mutation)?;
        }
        Ok(())
    }
}

impl TaskModels {
    fn apply(
        &mut self,
        entity_id: &str,
        mutation: &TaskMutation,
    ) -> Result<(), CapabilityLedgerError> {
        let error = || CapabilityLedgerError::InvalidIdentifier("task invariant");
        match mutation {
            TaskMutation::TodoAdd { title, status } => {
                let tracker = self.todos.entry(entity_id.into()).or_default();
                let id = tracker.add(title.clone());
                if let Some(status) = status {
                    tracker
                        .set_status(id, todo_status(status.clone()))
                        .map_err(CapabilityLedgerError::InvalidTaskTransition)?;
                }
            }
            TaskMutation::TodoSetStatus { id, status, reason } => {
                let tracker = self.todos.entry(entity_id.into()).or_default();
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
                    .set_status_with_reason(id, todo_status(status.clone()), reason.clone())
                    .map_err(CapabilityLedgerError::InvalidTaskTransition)?;
            }
            TaskMutation::PlanAddNode {
                node_id,
                dependencies,
            } => {
                let plan = self.plans.entry(entity_id.into()).or_default();
                let dependencies = dependencies.iter().map(String::as_str).collect::<Vec<_>>();
                plan.add_node(node_id, &dependencies).map_err(|_| error())?;
            }
            TaskMutation::PlanApprove => {
                self.plans
                    .entry(entity_id.into())
                    .or_default()
                    .approve()
                    .map_err(|_| error())?;
            }
            TaskMutation::GoalSetBudget { budget } => {
                self.goals
                    .entry(entity_id.into())
                    .or_insert_with(|| Goal::new(*budget))
                    .set_budget(*budget)
                    .map_err(|_| error())?;
            }
            TaskMutation::GoalConsume { amount } => {
                self.goals
                    .entry(entity_id.into())
                    .or_insert_with(|| Goal::new(None))
                    .consume(*amount)
                    .map_err(|_| error())?;
            }
            TaskMutation::GoalComplete { assurance } => {
                self.goals
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
