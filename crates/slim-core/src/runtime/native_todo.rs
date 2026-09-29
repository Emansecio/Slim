use super::*;

pub(super) const TODO_PROGRESS_REVIEW: &str = "[Todo progress review] Reconcile evidenced transitions using returned IDs alongside independent work, before the final answer. Keep unfinished work pending/blocked; do not repeat work.";

pub(super) const TODO_FINAL_REVIEW: &str = "[Todo final review] Before answering, reconcile task statuses using returned IDs and actual results. Leave unfinished work pending/blocked and explain it; interruption is not cancellation. Do not expand scope.";

#[derive(Default)]
pub(super) struct TodoCadence {
    pub(super) snapshot: Vec<crate::TodoChangedItem>,
    pub(super) unchanged_batches: usize,
    pub(super) reminders: usize,
    pub(super) final_reviewed: bool,
}

impl TodoCadence {
    pub(super) fn unfinished(items: &[crate::TodoChangedItem]) -> bool {
        items
            .iter()
            .any(|item| matches!(item.status.as_str(), "pending" | "in_progress"))
    }

    pub(super) fn after_batch(&mut self, items: &[crate::TodoChangedItem]) -> bool {
        if self.snapshot != items {
            self.snapshot = items.to_vec();
            self.unchanged_batches = 0;
            if Self::unfinished(items) && self.reminders < 2 {
                self.reminders += 1;
                return true;
            }
            return false;
        }
        if !Self::unfinished(items) {
            return false;
        }
        self.unchanged_batches = self.unchanged_batches.saturating_add(1);
        if self.unchanged_batches < 4 || self.reminders >= 2 {
            return false;
        }
        self.unchanged_batches = 0;
        self.reminders += 1;
        true
    }

    pub(super) fn before_final(&mut self, items: &[crate::TodoChangedItem]) -> bool {
        if self.final_reviewed || !Self::unfinished(items) {
            return false;
        }
        self.final_reviewed = true;
        true
    }
}

pub(super) fn todo_tool_definition() -> Value {
    json!({
        "name": "todo",
        "description": "Track multi-step work when needed/requested. Update at transitions and before answering, based on results, not command success alone. Ordered entries: add {title,status?}; update {id,status,reason?} using returned IDs. One in_progress: complete the previous step before starting the next. Leave unfinished work pending/blocked; give blockers a short reason (requires id). Titles do not rename. Failure retains earlier applied entries.",
        "input_schema": {
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": {"type": "string"},
                            "id": {"type": ["string", "integer"], "minimum": 0},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "blocked", "cancelled"]},
                            "reason": {"type": "string", "minLength": 1, "maxLength": 1024}
                        },
                        "additionalProperties": false
                    }
                }
            },
            "required": ["todos"],
            "additionalProperties": false
        }
    })
}

pub(super) fn todo_changed_items(
    tracker: &crate::task::TodoTracker,
) -> Vec<crate::TodoChangedItem> {
    tracker
        .items()
        .iter()
        .map(|item| crate::TodoChangedItem {
            reason: item.reason.clone(),
            id: Some(item.id),
            title: item.title.clone(),
            status: todo_status_name(item.status).to_owned(),
        })
        .collect()
}

pub(super) fn todo_line(item: &crate::task::TodoItem) -> String {
    format!(
        "todo {} [{}]: {}",
        item.id,
        todo_status_name(item.status),
        item.title
    )
}

pub(super) fn todo_status_name(status: crate::task::TodoStatus) -> &'static str {
    match status {
        crate::task::TodoStatus::Pending => "pending",
        crate::task::TodoStatus::InProgress => "in_progress",
        crate::task::TodoStatus::Completed => "completed",
        crate::task::TodoStatus::Blocked => "blocked",
        crate::task::TodoStatus::Cancelled => "cancelled",
    }
}

pub(super) fn todo_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => {
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        }
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

pub(super) fn parse_todo_entry(entry: &Value) -> Result<(String, TaskMutation), String> {
    let reason = entry
        .get("reason")
        .map(|value| {
            let text = value
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty() && text.len() <= 1024)
                .ok_or_else(|| "reason must be nonempty text of at most 1024 bytes".to_owned())?;
            if entry.get("id").is_none() {
                return Err("reason requires an id/status update".to_owned());
            }
            Ok(text.to_owned())
        })
        .transpose()?;
    let status = entry
        .get("status")
        .map(|value| {
            value
                .as_str()
                .and_then(todo_status_from_name)
                .ok_or_else(|| {
                    "status must be pending, in_progress, completed, blocked, or cancelled"
                        .to_owned()
                })
        })
        .transpose()?;
    if let Some(value) = entry.get("id") {
        let id = todo_text(value)
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| "id must be an unsigned integer returned by todo".to_owned())?;
        let status = status.ok_or_else(|| "an id update requires status".to_owned())?;
        return Ok((
            format!("todo {id}"),
            TaskMutation::TodoSetStatus {
                reason,
                id: Some(id),
                status,
            },
        ));
    }
    let title = match entry {
        Value::String(_) => todo_text(entry),
        _ => entry
            .get("content")
            .or_else(|| entry.get("title"))
            .and_then(todo_text),
    };
    if let Some(title) = title {
        return Ok((title.clone(), TaskMutation::TodoAdd { title, status }));
    }
    Err("entry requires a nonempty title/content or an id/status update".into())
}

pub(super) fn parse_todo_mutations(args: &Value) -> Result<Vec<(String, TaskMutation)>, String> {
    match args.get("todos").unwrap_or(args) {
        Value::Array(entries) if entries.is_empty() => {
            Err("todos must contain at least one entry".into())
        }
        Value::Array(entries) => entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                parse_todo_entry(entry).map_err(|error| format!("entry {}: {error}", index + 1))
            })
            .collect(),
        entry => parse_todo_entry(entry).map(|entry| vec![entry]),
    }
}

pub(super) fn todo_status_from_name(name: &str) -> Option<TaskTodoStatus> {
    match name.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "pending" | "todo" => Some(TaskTodoStatus::Pending),
        "in_progress" | "inprogress" => Some(TaskTodoStatus::InProgress),
        "completed" | "complete" | "done" => Some(TaskTodoStatus::Completed),
        "blocked" => Some(TaskTodoStatus::Blocked),
        "cancelled" | "canceled" => Some(TaskTodoStatus::Cancelled),
        _ => None,
    }
}

pub(super) fn capability_error(error: CapabilityLedgerError) -> ProviderError {
    ProviderError::InvalidResponse {
        message: format!("capability: {error}"),
    }
}

impl Runtime {
    pub(super) fn execute_todo(
        &mut self,
        mode: crate::OperatingMode,
        invocation: ToolInvocation<'_>,
        next_seq: u64,
    ) -> Result<(ToolResult, u64), ProviderError> {
        self.ensure_not_cancelled()?;
        let mut seq = next_seq;
        let started_at = self.begin_tool(invocation, &mut seq)?;
        let before = self.todo_snapshot();
        let mut result = self.apply_todo_tool(mode, invocation);
        if let Some(items) = self.todo_snapshot() {
            if result.success || before.as_ref() != Some(&items) {
                push_runtime_event(
                    &mut self.app,
                    &mut seq,
                    crate::EventKind::TodoChanged { items },
                )?;
            }
        }
        self.finish_tool(invocation, &mut result, started_at, &mut seq, None)?;
        Ok((result, seq))
    }

    /// Applies todo mutations through the capability bridge (entity "session").
    pub(super) fn apply_todo_tool(
        &mut self,
        mode: crate::OperatingMode,
        invocation: ToolInvocation<'_>,
    ) -> ToolResult {
        if !mode.allows_mutation() {
            return ToolResult::fail(invocation.name, "todo is only available in Auto mode");
        }
        let args: Value = match serde_json::from_str(invocation.arguments) {
            Ok(args) => args,
            Err(error) => {
                return ToolResult::fail(
                    invocation.name,
                    format!("invalid todo arguments: {error}"),
                )
            }
        };
        let mutations = match parse_todo_mutations(&args) {
            Ok(mutations) => mutations,
            Err(error) => {
                return ToolResult::fail(
                    invocation.name,
                    format!("invalid todo arguments: {error}; no items changed"),
                )
            }
        };
        let Some(bridge) = self.capability_bridge.as_mut() else {
            return ToolResult::fail(
                invocation.name,
                "todo unavailable: no capability bridge configured",
            );
        };
        let mut lines = Vec::new();
        for (index, (label, mutation)) in mutations.into_iter().enumerate() {
            // Models trained on full-list todo writes resend every entry
            // without ids. A status entry whose title matches one existing
            // item exactly is that item's update, not a new duplicate.
            let mutation = match mutation {
                TaskMutation::TodoAdd {
                    title,
                    status: Some(status),
                } => {
                    let matched = bridge.todo("session").and_then(|tracker| {
                        let mut hits = tracker
                            .items()
                            .iter()
                            .filter(|item| item.title.trim() == title.trim());
                        match (hits.next(), hits.next()) {
                            (Some(item), None) => Some(item.id),
                            _ => None,
                        }
                    });
                    match matched {
                        Some(id) => TaskMutation::TodoSetStatus {
                            reason: None,
                            id: Some(id),
                            status,
                        },
                        None => TaskMutation::TodoAdd {
                            title,
                            status: Some(status),
                        },
                    }
                }
                other => other,
            };
            let target = match &mutation {
                TaskMutation::TodoSetStatus { id, .. } => *id,
                _ => None,
            };
            let revision = bridge.task_revision("session").saturating_add(1);
            let request = TaskMutationRequest {
                idempotency_key: format!("{}-{index}", invocation.call_id),
                entity_id: "session".into(),
                revision,
                mutation,
            };
            match bridge.apply_task_mutation(request, mode, AuthorizationGrant::Explicit) {
                Ok(_changed) => {
                    if let Some(item) = bridge.todo("session").and_then(|tracker| {
                        target.map_or_else(
                            || tracker.items().last(),
                            |id| tracker.items().iter().find(|item| item.id == id),
                        )
                    }) {
                        lines.push(todo_line(item));
                    }
                }
                Err(error) => {
                    lines.push(format!("todo rejected ({label}): {error}"));
                    if let Some(tracker) = bridge.todo("session") {
                        lines.push(
                            "Current items (earlier successful entries remain applied):".into(),
                        );
                        lines.extend(tracker.items().iter().map(todo_line));
                    }
                    return ToolResult::fail(invocation.name, lines.join("\n"));
                }
            }
        }
        ToolResult::ok(invocation.name, lines.join("\n"))
    }

    /// Restore structured task records only; no tool or external operation is replayed.
    pub fn restore_task_facts(
        &mut self,
        facts: &[DurableFact],
        cwd: &Path,
    ) -> Result<(), ProviderError> {
        let catalog = CapabilityCatalog::with_native_tools();
        let header = DurableSessionHeader::new(
            "loop",
            "now",
            cwd.to_string_lossy().into_owned(),
            None,
            None,
        );
        let mut repo = MemoryRepo::new(header);
        for (index, fact) in facts.iter().enumerate() {
            if fact.namespace != "task.v1" {
                return Err(ProviderError::InvalidResponse {
                    message: "invalid task state namespace".into(),
                });
            }
            repo.append(DurableRecord::Fact {
                seq: index as u64,
                fact: fact.clone(),
            })
            .map_err(|error| ProviderError::InvalidResponse {
                message: format!("task state: {error}"),
            })?;
        }
        let bridge = RuntimeCapabilityBridge::new(repo, catalog).map_err(capability_error)?;
        self.capability_bridge = Some(bridge);
        Ok(())
    }

    pub fn task_facts(&self) -> Vec<DurableFact> {
        self.capability_bridge
            .as_ref()
            .into_iter()
            .flat_map(|bridge| bridge.service().repo().records())
            .filter_map(|record| match record {
                DurableRecord::Fact { fact, .. } if fact.namespace == "task.v1" => {
                    let mut fact = fact.clone();
                    fact.key = self.redact_sensitive(&fact.key);
                    redact_task_value(&mut fact.value, &self.sensitive_values.0);
                    Some(fact)
                }
                _ => None,
            })
            .collect()
    }

    /// The session todo list, or `None` without a bridge or tracker.
    pub(super) fn todo_snapshot(&self) -> Option<Vec<crate::TodoChangedItem>> {
        self.capability_bridge
            .as_ref()
            .and_then(|bridge| bridge.todo("session"))
            .map(todo_changed_items)
    }

    pub fn todo_items(&self) -> Vec<crate::TodoChangedItem> {
        self.todo_snapshot().unwrap_or_default()
    }
}
