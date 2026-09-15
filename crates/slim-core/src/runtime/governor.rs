use crate::tools::{
    DependencyObservation, PreparedToolInvocation, ToolCacheability, ToolDependencyScope,
    ToolEffectClass, ToolExecutionReceipt, ToolOperationalSpec, ToolReplayPolicy, ToolResult,
    ToolVolatility,
};
use crate::{
    CausalAnomalyKind, CausalBoundaryKind, CausalConfidence, CausalProgressKind, CausalShadowAction,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;

const MAX_OBSERVED_DEPENDENCIES: usize = 256;
const MAX_EVENT_IDENTIFIER_BYTES: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum GovernorObservation {
    Progress {
        batch_id: String,
        call_id: String,
        kind: CausalProgressKind,
        tool_name: String,
        call_fingerprint: String,
        evidence_id: String,
        workspace_revision: u64,
    },
    Boundary {
        batch_id: String,
        call_id: String,
        kind: CausalBoundaryKind,
        tool_name: String,
        call_fingerprint: String,
        uncertainty_epoch: u64,
    },
    Anomaly {
        batch_id: String,
        call_id: String,
        kind: CausalAnomalyKind,
        tool_name: String,
        call_fingerprint: String,
        evidence_id: String,
        workspace_revision: u64,
        occurrence: u32,
        confidence: CausalConfidence,
        action: CausalShadowAction,
    },
}

#[derive(Clone, Debug)]
pub(super) struct PendingCall {
    batch_id: String,
    call_id: String,
    tool_name: String,
    canonical_fingerprint: String,
    call_fingerprint: String,
    evidence_scope: String,
    workspace_revision_at_start: u64,
    spec: Option<ToolOperationalSpec>,
    confidence: CausalConfidence,
    diagnostics: bool,
    admission_prefix: Option<String>,
}

impl PendingCall {
    #[cfg(test)]
    fn call_fingerprint(&self) -> &str {
        &self.call_fingerprint
    }
}

#[derive(Default)]
pub(super) struct CausalGovernor {
    ledger: ProgressLedger,
    stop_requested: bool,
}

#[derive(Default)]
struct ProgressLedger {
    workspace_revision: u64,
    source_workspace_revision: u64,
    uncertainty_epoch: u64,
    interaction_epoch: u64,
    internal_epoch: u64,
    observed: BTreeMap<String, ObservedDependency>,
    evidence: HashMap<String, EvidenceRecord>,
    seen_evidence: HashSet<String>,
    validations: HashMap<String, ValidationResult>,
    stagnant_turns: u32,
    turn: TurnState,
}

struct ObservedDependency {
    digest: String,
    source_revision: u64,
}

struct TurnState {
    has_calls: bool,
    made_progress: bool,
    boundary: bool,
    classifiable: bool,
    confidence: CausalConfidence,
    last_batch_id: String,
    last_call_id: String,
    last_tool_name: String,
    last_call_fingerprint: String,
}

impl Default for TurnState {
    fn default() -> Self {
        Self {
            has_calls: false,
            made_progress: false,
            boundary: false,
            classifiable: true,
            confidence: CausalConfidence::High,
            last_batch_id: String::new(),
            last_call_id: String::new(),
            last_tool_name: String::new(),
            last_call_fingerprint: String::new(),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum EvidenceOutcome {
    Success,
    Failure,
    ValidationGreen,
}

struct EvidenceRecord {
    evidence_id: String,
    outcome: EvidenceOutcome,
    repetitions: u32,
}

struct ValidationResult {
    success: bool,
    workspace_revision: u64,
    uncertainty_epoch: u64,
}

impl CausalGovernor {
    /// Stop only when the completed batch has no progress or uncertain boundary.
    pub(super) fn stop_requested(&self) -> bool {
        self.stop_requested
    }

    pub(super) fn validations_satisfied(&self) -> bool {
        self.ledger
            .validations
            .values()
            .all(|result| result.success)
            && self.ledger.validations.values().any(|result| {
                result.success
                    && result.workspace_revision == self.ledger.workspace_revision
                    && result.uncertainty_epoch == self.ledger.uncertainty_epoch
            })
    }

    pub(super) fn forget_compacted_evidence(&mut self) {
        self.ledger.evidence.clear();
        self.ledger.seen_evidence.clear();
        self.ledger.stagnant_turns = 0;
        self.stop_requested = false;
    }

    fn note_stop(&mut self, action: CausalShadowAction) {
        if action == CausalShadowAction::WouldStop {
            self.stop_requested = true;
        }
    }

    pub(super) fn observe_before_identified(
        &mut self,
        prepared: &PreparedToolInvocation,
        batch_id: &str,
        call_id: &str,
    ) -> (PendingCall, Vec<GovernorObservation>) {
        let batch_id = bounded_identifier(batch_id);
        let call_id = bounded_identifier(call_id);
        let tool_name = bounded_identifier(&prepared.name);
        let Some(spec) = prepared.spec.filter(|_| prepared.error.is_none()) else {
            return self.unclassifiable_call(
                batch_id,
                call_id,
                tool_name,
                prepared.canonical_fingerprint.clone(),
            );
        };
        let confidence = confidence_for(spec);
        let dependency_digest = self.prepared_state_digest(prepared, spec.dependency_scope);
        let call_fingerprint = stateful_call_fingerprint(
            &prepared.canonical_fingerprint,
            &dependency_digest,
            self.ledger.uncertainty_epoch,
            (spec.effect_class == ToolEffectClass::Validation)
                .then_some(self.ledger.workspace_revision),
        );
        let evidence_scope = evidence_scope(
            spec,
            &dependency_digest,
            self.ledger.workspace_revision,
            self.ledger.uncertainty_epoch,
        );
        self.begin_call(
            &batch_id,
            &call_id,
            &tool_name,
            &call_fingerprint,
            confidence,
        );
        (
            PendingCall {
                batch_id,
                call_id,
                tool_name,
                canonical_fingerprint: prepared.canonical_fingerprint.clone(),
                call_fingerprint,
                evidence_scope,
                workspace_revision_at_start: self.ledger.workspace_revision,
                spec: Some(spec),
                confidence,
                diagnostics: prepared.name == "code_intel" && prepared.arguments.is_diagnostics(),
                admission_prefix: crate::tools::admission_output_prefix(&prepared.admission_notes),
            },
            Vec::new(),
        )
    }

    pub(super) fn observe_before_batch(
        &mut self,
        calls: &[PreparedToolInvocation],
        batch_id: &str,
        call_ids: &[String],
    ) -> Vec<(PendingCall, Vec<GovernorObservation>)> {
        calls
            .iter()
            .zip(call_ids)
            .map(|(prepared, call_id)| self.observe_before_identified(prepared, batch_id, call_id))
            .collect()
    }

    pub(super) fn observe_after(
        &mut self,
        mut pending: PendingCall,
        result: &ToolResult,
        receipt: &ToolExecutionReceipt,
    ) -> Vec<GovernorObservation> {
        let _receipt_metrics = (
            receipt.modified_paths.len(),
            receipt.bytes_read,
            receipt.preparation_us,
            receipt.execution_us,
            receipt.finalization_us,
        );
        let source_revision = receipt.revision_before.max(receipt.revision_after);
        if source_revision > self.ledger.source_workspace_revision {
            self.ledger.workspace_revision = self.ledger.workspace_revision.saturating_add(
                source_revision.saturating_sub(self.ledger.source_workspace_revision),
            );
            self.ledger.source_workspace_revision = source_revision;
        }
        let Some(spec) = pending.spec else {
            return Vec::new();
        };
        let receipt_digest = self.receipt_state_digest(receipt, spec.dependency_scope);
        let evidence_revision = if spec.effect_class == ToolEffectClass::Validation {
            pending.workspace_revision_at_start.max(source_revision)
        } else {
            self.ledger.workspace_revision
        };
        pending.call_fingerprint = stateful_call_fingerprint(
            &pending.canonical_fingerprint,
            &receipt_digest,
            self.ledger.uncertainty_epoch,
            (spec.effect_class == ToolEffectClass::Validation).then_some(evidence_revision),
        );
        pending.evidence_scope = evidence_scope(
            spec,
            &receipt_digest,
            evidence_revision,
            self.ledger.uncertainty_epoch,
        );
        self.ledger
            .turn
            .last_call_fingerprint
            .clone_from(&pending.call_fingerprint);

        match spec.effect_class {
            ToolEffectClass::PotentiallyVolatile => self.observe_volatile(pending),
            ToolEffectClass::WorkspaceMutation if result.success => {
                self.observe_mutation(pending, receipt)
            }
            ToolEffectClass::WorkspaceMutation => {
                self.observe_evidence(pending, result, EvidenceOutcome::Failure)
            }
            ToolEffectClass::Interaction if result.success => {
                self.ledger.interaction_epoch = self.ledger.interaction_epoch.saturating_add(1);
                self.mark_progress();
                vec![GovernorObservation::Progress {
                    batch_id: pending.batch_id,
                    call_id: pending.call_id,
                    kind: CausalProgressKind::ExternalInput,
                    tool_name: pending.tool_name.clone(),
                    call_fingerprint: pending.call_fingerprint,
                    evidence_id: evidence_id(
                        &pending.tool_name,
                        result,
                        false,
                        pending.admission_prefix.as_deref(),
                    ),
                    workspace_revision: self.ledger.workspace_revision,
                }]
            }
            ToolEffectClass::Interaction => {
                self.observe_evidence(pending, result, EvidenceOutcome::Failure)
            }
            ToolEffectClass::InternalState if result.success => {
                self.ledger.internal_epoch = self.ledger.internal_epoch.saturating_add(1);
                Vec::new()
            }
            ToolEffectClass::InternalState => {
                self.observe_evidence(pending, result, EvidenceOutcome::Failure)
            }
            ToolEffectClass::Validation => self.observe_validation(pending, result, receipt),
            ToolEffectClass::SnapshotRead => self.observe_snapshot(pending, result, receipt),
        }
    }

    pub(super) fn observe_snapshot_batch_after(
        &mut self,
        calls: Vec<(PendingCall, ToolResult, ToolExecutionReceipt)>,
    ) -> Vec<Vec<GovernorObservation>> {
        calls
            .into_iter()
            .map(|(pending, result, receipt)| self.observe_after(pending, &result, &receipt))
            .collect()
    }

    pub(super) fn finish_turn(&mut self) -> Vec<GovernorObservation> {
        let turn = std::mem::take(&mut self.ledger.turn);
        if !turn.has_calls {
            return Vec::new();
        }
        if turn.made_progress {
            self.stop_requested = false;
            self.ledger.stagnant_turns = 0;
            return Vec::new();
        }
        if turn.boundary || !turn.classifiable {
            self.stop_requested = false;
            self.ledger.stagnant_turns = 0;
            return Vec::new();
        }
        self.ledger.stagnant_turns = self.ledger.stagnant_turns.saturating_add(1);
        let occurrence = self.ledger.stagnant_turns;
        let kind = if occurrence == 1 {
            CausalAnomalyKind::StagnantTurn
        } else {
            CausalAnomalyKind::NoProgressCandidate
        };
        let action = match occurrence {
            1 => CausalShadowAction::Observe,
            2 => CausalShadowAction::WouldWarn,
            _ if turn.confidence == CausalConfidence::High => CausalShadowAction::WouldStop,
            _ => CausalShadowAction::WouldWarn,
        };
        self.note_stop(action);
        vec![GovernorObservation::Anomaly {
            batch_id: turn.last_batch_id,
            call_id: turn.last_call_id,
            kind,
            tool_name: turn.last_tool_name,
            call_fingerprint: turn.last_call_fingerprint,
            evidence_id: String::new(),
            workspace_revision: self.ledger.workspace_revision,
            occurrence,
            confidence: turn.confidence,
            action,
        }]
    }

    fn observe_snapshot(
        &mut self,
        pending: PendingCall,
        result: &ToolResult,
        receipt: &ToolExecutionReceipt,
    ) -> Vec<GovernorObservation> {
        if !result.success {
            return self.observe_evidence(pending, result, EvidenceOutcome::Failure);
        }
        let scope = pending
            .spec
            .expect("classifiable pending call has spec")
            .dependency_scope;
        if matches!(
            scope,
            ToolDependencyScope::TargetFile
                | ToolDependencyScope::ImmediateDirectory
                | ToolDependencyScope::ObservedWorkspace
        ) && receipt.dependencies.is_empty()
        {
            return self.unclassifiable_after(pending);
        }
        if receipt
            .dependencies
            .iter()
            .any(|dependency| dependency.stamp.comparison_digest().is_none())
        {
            return self.unclassifiable_after(pending);
        }
        let source_revision = receipt.revision_before.max(receipt.revision_after);
        let (changed, unexplained_change, stale) =
            self.track_dependencies(&receipt.dependencies, source_revision);
        if stale {
            return self.unclassifiable_after(pending);
        }
        if changed {
            if unexplained_change {
                self.ledger.workspace_revision = self.ledger.workspace_revision.saturating_add(1);
            }
            self.mark_progress();
            return vec![GovernorObservation::Progress {
                batch_id: pending.batch_id,
                call_id: pending.call_id,
                kind: CausalProgressKind::DependencyChanged,
                tool_name: pending.tool_name,
                call_fingerprint: pending.call_fingerprint,
                evidence_id: observations_digest(&receipt.dependencies),
                workspace_revision: self.ledger.workspace_revision,
            }];
        }
        self.observe_evidence(pending, result, EvidenceOutcome::Success)
    }

    fn observe_mutation(
        &mut self,
        pending: PendingCall,
        receipt: &ToolExecutionReceipt,
    ) -> Vec<GovernorObservation> {
        for mutation in &receipt.mutations {
            if let Some(digest) = mutation.after.comparison_digest() {
                self.store_dependency(
                    format!("file:{}", path_identity(&mutation.path)),
                    digest,
                    receipt.revision_after,
                );
            }
        }
        if !receipt.mutations.iter().any(|mutation| mutation.changed()) {
            return Vec::new();
        }
        self.mark_progress();
        vec![GovernorObservation::Progress {
            batch_id: pending.batch_id,
            call_id: pending.call_id,
            kind: CausalProgressKind::WorkspaceChanged,
            tool_name: pending.tool_name,
            call_fingerprint: pending.call_fingerprint,
            evidence_id: mutations_digest(receipt),
            workspace_revision: self.ledger.workspace_revision,
        }]
    }

    fn observe_validation(
        &mut self,
        pending: PendingCall,
        result: &ToolResult,
        _receipt: &ToolExecutionReceipt,
    ) -> Vec<GovernorObservation> {
        let success = validation_green(result, pending.admission_prefix.as_deref());
        // Track every outcome, including repetitions suppressed by the progress
        // ledger. A different green command cannot resolve this command's failure.
        self.ledger.validations.insert(
            pending.canonical_fingerprint.clone(),
            ValidationResult {
                success,
                workspace_revision: self.ledger.workspace_revision,
                uncertainty_epoch: self.ledger.uncertainty_epoch,
            },
        );
        let outcome = if success {
            EvidenceOutcome::ValidationGreen
        } else {
            EvidenceOutcome::Failure
        };
        self.observe_evidence(pending, result, outcome)
    }

    fn observe_volatile(&mut self, pending: PendingCall) -> Vec<GovernorObservation> {
        self.ledger.uncertainty_epoch = self.ledger.uncertainty_epoch.saturating_add(1);
        self.ledger.turn.boundary = true;
        vec![GovernorObservation::Boundary {
            batch_id: pending.batch_id,
            call_id: pending.call_id,
            kind: CausalBoundaryKind::PotentiallyVolatile,
            tool_name: pending.tool_name,
            call_fingerprint: pending.call_fingerprint,
            uncertainty_epoch: self.ledger.uncertainty_epoch,
        }]
    }

    fn observe_evidence(
        &mut self,
        pending: PendingCall,
        result: &ToolResult,
        outcome: EvidenceOutcome,
    ) -> Vec<GovernorObservation> {
        if !matches!(
            outcome,
            EvidenceOutcome::Failure | EvidenceOutcome::ValidationGreen
        ) && pending.spec.is_none_or(|spec| {
            spec.cacheability != ToolCacheability::Evidence
                || spec.replay_policy != ToolReplayPolicy::EquivalentEvidence
        }) {
            return Vec::new();
        }
        let is_validation = pending
            .spec
            .is_some_and(|spec| spec.effect_class == ToolEffectClass::Validation);
        let evidence_id = evidence_id(
            &pending.tool_name,
            result,
            is_validation,
            pending.admission_prefix.as_deref(),
        );
        let seen_key = if is_validation {
            hash_fields(&[
                b"slim-causal-seen-validation-v1",
                evidence_id.as_bytes(),
                pending.evidence_scope.as_bytes(),
                pending.call_fingerprint.as_bytes(),
            ])
        } else {
            hash_fields(&[
                b"slim-causal-seen-evidence-v1",
                evidence_id.as_bytes(),
                pending.evidence_scope.as_bytes(),
            ])
        };
        let is_new_evidence = self.ledger.seen_evidence.insert(seen_key);
        let had_record = self.ledger.evidence.contains_key(&pending.call_fingerprint);
        let mut changed_for_call = false;
        if let Some(record) = self.ledger.evidence.get_mut(&pending.call_fingerprint) {
            if record.evidence_id == evidence_id && record.outcome == outcome {
                record.repetitions = record.repetitions.saturating_add(1);
                let occurrence = record.repetitions;
                let kind = match outcome {
                    EvidenceOutcome::Success => CausalAnomalyKind::ReusableEvidence,
                    EvidenceOutcome::Failure => CausalAnomalyKind::RepeatedFailure,
                    EvidenceOutcome::ValidationGreen => CausalAnomalyKind::RedundantValidation,
                };
                let action = if pending
                    .spec
                    .is_some_and(|spec| spec.replay_policy == ToolReplayPolicy::Never)
                {
                    CausalShadowAction::WouldWarn
                } else {
                    repetition_action(kind, occurrence, pending.confidence)
                };
                self.note_stop(action);
                return vec![GovernorObservation::Anomaly {
                    batch_id: pending.batch_id,
                    call_id: pending.call_id,
                    kind,
                    tool_name: pending.tool_name,
                    call_fingerprint: pending.call_fingerprint,
                    evidence_id,
                    workspace_revision: self.ledger.workspace_revision,
                    occurrence,
                    confidence: pending.confidence,
                    action,
                }];
            }
            changed_for_call = true;
            record.evidence_id.clone_from(&evidence_id);
            record.outcome = outcome;
            record.repetitions = 0;
        } else {
            self.ledger.evidence.insert(
                pending.call_fingerprint.clone(),
                EvidenceRecord {
                    evidence_id: evidence_id.clone(),
                    outcome,
                    repetitions: 0,
                },
            );
        }
        let changed_evidence = changed_for_call && outcome != EvidenceOutcome::Failure;
        if !is_new_evidence && !changed_evidence {
            return Vec::new();
        }
        self.mark_progress();
        let kind = match outcome {
            EvidenceOutcome::ValidationGreen => CausalProgressKind::ValidationGreen,
            EvidenceOutcome::Failure => CausalProgressKind::DistinctFailure,
            _ if pending.diagnostics && had_record => CausalProgressKind::DiagnosticsChanged,
            _ => CausalProgressKind::NewEvidence,
        };
        vec![GovernorObservation::Progress {
            batch_id: pending.batch_id,
            call_id: pending.call_id,
            kind,
            tool_name: pending.tool_name,
            call_fingerprint: pending.call_fingerprint,
            evidence_id,
            workspace_revision: self.ledger.workspace_revision,
        }]
    }

    fn unclassifiable_call(
        &mut self,
        batch_id: String,
        call_id: String,
        tool_name: String,
        call_fingerprint: String,
    ) -> (PendingCall, Vec<GovernorObservation>) {
        self.ledger.uncertainty_epoch = self.ledger.uncertainty_epoch.saturating_add(1);
        self.ledger.turn.boundary = true;
        self.ledger.turn.classifiable = false;
        self.begin_call(
            &batch_id,
            &call_id,
            &tool_name,
            &call_fingerprint,
            CausalConfidence::Low,
        );
        (
            PendingCall {
                batch_id: batch_id.clone(),
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                canonical_fingerprint: call_fingerprint.clone(),
                call_fingerprint: call_fingerprint.clone(),
                evidence_scope: String::new(),
                workspace_revision_at_start: self.ledger.workspace_revision,
                spec: None,
                confidence: CausalConfidence::Low,
                diagnostics: false,
                admission_prefix: None,
            },
            vec![GovernorObservation::Boundary {
                batch_id,
                call_id,
                kind: CausalBoundaryKind::Unclassifiable,
                tool_name,
                call_fingerprint,
                uncertainty_epoch: self.ledger.uncertainty_epoch,
            }],
        )
    }

    fn unclassifiable_after(&mut self, pending: PendingCall) -> Vec<GovernorObservation> {
        self.ledger.uncertainty_epoch = self.ledger.uncertainty_epoch.saturating_add(1);
        self.ledger.turn.boundary = true;
        self.ledger.turn.classifiable = false;
        self.ledger.turn.confidence = CausalConfidence::Low;
        vec![GovernorObservation::Boundary {
            batch_id: pending.batch_id,
            call_id: pending.call_id,
            kind: CausalBoundaryKind::Unclassifiable,
            tool_name: pending.tool_name,
            call_fingerprint: pending.call_fingerprint,
            uncertainty_epoch: self.ledger.uncertainty_epoch,
        }]
    }

    fn begin_call(
        &mut self,
        batch_id: &str,
        call_id: &str,
        tool_name: &str,
        call_fingerprint: &str,
        confidence: CausalConfidence,
    ) {
        self.ledger.turn.has_calls = true;
        self.ledger.turn.confidence = lower_confidence(self.ledger.turn.confidence, confidence);
        batch_id.clone_into(&mut self.ledger.turn.last_batch_id);
        call_id.clone_into(&mut self.ledger.turn.last_call_id);
        tool_name.clone_into(&mut self.ledger.turn.last_tool_name);
        call_fingerprint.clone_into(&mut self.ledger.turn.last_call_fingerprint);
    }

    fn mark_progress(&mut self) {
        self.ledger.turn.made_progress = true;
    }

    fn track_dependencies(
        &mut self,
        dependencies: &[DependencyObservation],
        source_revision: u64,
    ) -> (bool, bool, bool) {
        let mut changed = false;
        let mut unexplained_change = false;
        let mut stale = false;
        for dependency in dependencies {
            let key = dependency.key();
            let Some(digest) = dependency.stamp.comparison_digest() else {
                continue;
            };
            if let Some(known) = self.ledger.observed.get(&key) {
                if source_revision < known.source_revision {
                    stale = true;
                    continue;
                }
                if known.digest != digest {
                    changed = true;
                    unexplained_change |= source_revision == known.source_revision;
                }
            }
            self.store_dependency(key, digest, source_revision);
        }
        (changed, unexplained_change, stale)
    }

    fn store_dependency(&mut self, key: String, digest: String, source_revision: u64) {
        if self
            .ledger
            .observed
            .get(&key)
            .is_some_and(|known| source_revision < known.source_revision)
        {
            return;
        }
        if !self.ledger.observed.contains_key(&key)
            && self.ledger.observed.len() >= MAX_OBSERVED_DEPENDENCIES
        {
            return;
        }
        self.ledger.observed.insert(
            key,
            ObservedDependency {
                digest,
                source_revision,
            },
        );
    }

    fn prepared_state_digest(
        &self,
        prepared: &PreparedToolInvocation,
        scope: ToolDependencyScope,
    ) -> String {
        match scope {
            ToolDependencyScope::TargetFile | ToolDependencyScope::ImmediateDirectory => prepared
                .target_paths
                .first()
                .and_then(|path| {
                    let prefix = if scope == ToolDependencyScope::TargetFile {
                        "file"
                    } else {
                        "directory"
                    };
                    self.ledger
                        .observed
                        .get(&format!("{prefix}:{}", path_identity(path)))
                })
                .map(|dependency| dependency.digest.clone())
                .unwrap_or_default(),
            _ => self.scoped_state_digest(scope),
        }
    }

    fn receipt_state_digest(
        &self,
        receipt: &ToolExecutionReceipt,
        scope: ToolDependencyScope,
    ) -> String {
        if !receipt.mutations.is_empty() {
            return mutations_digest(receipt);
        }
        if !receipt.dependencies.is_empty() {
            return observations_digest(&receipt.dependencies);
        }
        self.scoped_state_digest(scope)
    }

    fn scoped_state_digest(&self, scope: ToolDependencyScope) -> String {
        match scope {
            ToolDependencyScope::ObservedWorkspace => {
                let owned = self
                    .ledger
                    .observed
                    .iter()
                    .map(|(key, dependency)| format!("{key}\0{}", dependency.digest))
                    .collect::<Vec<_>>();
                let mut fields = vec![b"slim-observed-workspace-v2".as_slice()];
                fields.extend(owned.iter().map(|field| field.as_bytes()));
                hash_fields(&fields)
            }
            ToolDependencyScope::Interaction => self.ledger.interaction_epoch.to_string(),
            ToolDependencyScope::Internal => self.ledger.internal_epoch.to_string(),
            ToolDependencyScope::Unknown => self.ledger.uncertainty_epoch.to_string(),
            ToolDependencyScope::TargetFile | ToolDependencyScope::ImmediateDirectory => {
                String::new()
            }
        }
    }
}

fn observations_digest(observations: &[DependencyObservation]) -> String {
    let owned = observations
        .iter()
        .map(|observation| format!("{}\0{}", observation.key(), observation.stamp.digest()))
        .collect::<Vec<_>>();
    let mut fields = vec![b"slim-receipt-dependencies-v1".as_slice()];
    fields.extend(owned.iter().map(|field| field.as_bytes()));
    hash_fields(&fields)
}

fn mutations_digest(receipt: &ToolExecutionReceipt) -> String {
    let owned = receipt
        .mutations
        .iter()
        .map(|mutation| {
            format!(
                "{}\0{}\0{}",
                path_identity(&mutation.path),
                mutation.before_content_digest.as_deref().unwrap_or(""),
                mutation.after.digest()
            )
        })
        .collect::<Vec<_>>();
    let mut fields = vec![b"slim-receipt-mutations-v1".as_slice()];
    fields.extend(owned.iter().map(|field| field.as_bytes()));
    hash_fields(&fields)
}

fn confidence_for(spec: ToolOperationalSpec) -> CausalConfidence {
    match (spec.dependency_scope, spec.volatility) {
        (
            ToolDependencyScope::TargetFile | ToolDependencyScope::ImmediateDirectory,
            ToolVolatility::Stable,
        ) => CausalConfidence::High,
        (ToolDependencyScope::ObservedWorkspace, _) => CausalConfidence::Medium,
        _ => CausalConfidence::Low,
    }
}

fn lower_confidence(left: CausalConfidence, right: CausalConfidence) -> CausalConfidence {
    use CausalConfidence::{High, Low, Medium};
    match (left, right) {
        (Low, _) | (_, Low) => Low,
        (Medium, _) | (_, Medium) => Medium,
        (High, High) => High,
    }
}

fn repetition_action(
    kind: CausalAnomalyKind,
    occurrence: u32,
    confidence: CausalConfidence,
) -> CausalShadowAction {
    match occurrence {
        1 if kind == CausalAnomalyKind::RepeatedFailure => CausalShadowAction::WouldReject,
        1 => CausalShadowAction::WouldReuse,
        2 => CausalShadowAction::WouldWarn,
        _ if confidence == CausalConfidence::High => CausalShadowAction::WouldStop,
        _ => CausalShadowAction::WouldWarn,
    }
}

fn evidence_scope(
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

fn stateful_call_fingerprint(
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

fn evidence_id(
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

fn evidence_output<'a>(result: &'a ToolResult, admission_prefix: Option<&str>) -> &'a str {
    // Strip only this invocation's generated metadata, never a marker guessed
    // from file content. Presentation choices must not count as new evidence.
    admission_prefix
        .and_then(|prefix| result.output.strip_prefix(prefix))
        .unwrap_or(&result.output)
}

fn validation_green(result: &ToolResult, admission_prefix: Option<&str>) -> bool {
    result.success
        && evidence_output(result, admission_prefix)
            .lines()
            .next()
            .is_some_and(|line| line == "exit 0")
}

fn normalize_output(tool_name: &str, output: &str, validation: bool) -> String {
    output
        .replace("\r\n", "\n")
        .lines()
        .map(str::trim_end)
        .map(|line| {
            if tool_name == "code_intel" {
                if let Some((prefix, elapsed)) = line.rsplit_once(" | ") {
                    if elapsed.strip_suffix("ms").is_some_and(|value| {
                        value.chars().all(|character| character.is_ascii_digit())
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
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

fn bounded_identifier(value: &str) -> String {
    if value.len() <= MAX_EVENT_IDENTIFIER_BYTES {
        value.to_owned()
    } else {
        format!(
            "sha256:{}",
            hash_fields(&[b"slim-causal-identifier-v1", value.as_bytes()])
        )
    }
}

fn path_identity(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn hash_fields(fields: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for field in fields {
        hasher.update(u64::try_from(field.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(field);
    }
    let bytes = hasher.finalize();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{CausalGovernor, GovernorObservation};
    use crate::runtime::CancellationToken;
    use crate::tools::{ToolExecutionReceipt, ToolRegistry, ToolResult};
    use crate::{CausalAnomalyKind, CausalProgressKind, CausalShadowAction, OperatingMode};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_ROOT: AtomicU64 = AtomicU64::new(1);

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(label: &str) -> Self {
            let sequence = NEXT_TEST_ROOT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "slim-governor-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test root");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn receipts_drive_reuse_and_dependency_changes_without_governor_io() {
        let temp = TestRoot::new("receipts");
        let path = temp.path().join("sample.txt");
        fs::write(&path, "stable\n").expect("write fixture");
        let registry = ToolRegistry::default();
        let mut governor = CausalGovernor::default();
        let arguments = r#"{"path":"sample.txt","max_lines":1}"#;

        let first =
            registry.prepare_invocation(OperatingMode::Auto, temp.path(), "read", arguments);
        let (pending, before) = governor.observe_before_identified(&first, "batch", "one");
        assert!(before.is_empty());
        let outcome =
            registry.execute_prepared_with_cancellation_and_progress(&first, None, |_| {});
        let observations = governor.observe_after(pending, &outcome.result, &outcome.receipt);
        assert!(observations.iter().any(|observation| matches!(
            observation,
            GovernorObservation::Progress {
                kind: CausalProgressKind::NewEvidence,
                ..
            }
        )));

        let second =
            registry.prepare_invocation(OperatingMode::Auto, temp.path(), "read", arguments);
        let (pending, _) = governor.observe_before_identified(&second, "batch", "two");
        let outcome =
            registry.execute_prepared_with_cancellation_and_progress(&second, None, |_| {});
        let observations = governor.observe_after(pending, &outcome.result, &outcome.receipt);
        assert!(observations.iter().any(|observation| matches!(
            observation,
            GovernorObservation::Anomaly {
                kind: CausalAnomalyKind::ReusableEvidence,
                action: CausalShadowAction::WouldReuse,
                ..
            }
        )));

        // A repeated read can request a stop before a later operation in the
        // same batch observes a real dependency change.
        for _ in 0..2 {
            let (pending, _) = governor.observe_before_identified(&second, "batch", "repeat");
            governor.observe_after(pending, &outcome.result, &outcome.receipt);
        }

        fs::write(&path, "changed\n").expect("change fixture");
        let third =
            registry.prepare_invocation(OperatingMode::Auto, temp.path(), "read", arguments);
        let (pending, _) = governor.observe_before_identified(&third, "batch", "three");
        let outcome =
            registry.execute_prepared_with_cancellation_and_progress(&third, None, |_| {});
        let observations = governor.observe_after(pending, &outcome.result, &outcome.receipt);
        assert!(observations.iter().any(|observation| matches!(
            observation,
            GovernorObservation::Progress {
                kind: CausalProgressKind::DependencyChanged,
                ..
            }
        )));
        governor.finish_turn();
        assert!(!governor.stop_requested(), "the batch made real progress");
    }

    #[test]
    fn admission_notes_do_not_turn_identical_search_evidence_into_progress() {
        let temp = TestRoot::new("admission-evidence");
        fs::write(temp.path().join("sample.txt"), "needle\n").unwrap();
        let registry = ToolRegistry::default();
        let mut governor = CausalGovernor::default();
        for (index, context) in [10, 11, 12].into_iter().enumerate() {
            let prepared = registry.prepare_invocation(
                OperatingMode::Auto,
                temp.path(),
                "search",
                &serde_json::json!({"query":"needle", "context_lines": context}).to_string(),
            );
            let (pending, _) =
                governor.observe_before_identified(&prepared, "batch", &index.to_string());
            let outcome =
                registry.execute_prepared_with_cancellation_and_progress(&prepared, None, |_| {});
            assert!(outcome.result.success);
            assert!(outcome
                .result
                .output
                .contains(&format!("context_lines {context} -> 3")));
            let observations = governor.observe_after(pending, &outcome.result, &outcome.receipt);
            assert_eq!(
                observations
                    .iter()
                    .any(|event| matches!(event, GovernorObservation::Progress { .. })),
                index == 0,
                "presentation-only changes cannot reset progress: {observations:?}",
            );
            if index > 0 {
                assert!(observations
                    .iter()
                    .any(|event| matches!(event, GovernorObservation::Anomaly { .. })));
            }
            governor.finish_turn();
        }
        let content = ToolResult {
            name: "read".into(),
            success: true,
            output: "[admission: literal file content]\nneedle".into(),
            artifact: None,
        };
        assert_eq!(super::evidence_output(&content, None), content.output);
    }

    #[test]
    fn search_receipt_carries_workspace_evidence() {
        let temp = TestRoot::new("search-receipt");
        fs::write(temp.path().join("sample.txt"), "needle\n").expect("write fixture");
        fs::write(temp.path().join("second.txt"), "other\n").expect("write second fixture");
        let registry = ToolRegistry::default();
        let prepared = registry.prepare_invocation(
            OperatingMode::Auto,
            temp.path(),
            "search",
            r#"{"path":".","query":"needle"}"#,
        );

        let outcome =
            registry.execute_prepared_with_cancellation_and_progress(&prepared, None, |_| {});

        assert!(outcome.result.success);
        assert_eq!(outcome.receipt.dependencies.len(), 1);
        assert!(outcome.receipt.bytes_read > 0);

        let mut governor = CausalGovernor::default();
        let (first, _) = governor.observe_before_identified(&prepared, "batch", "first");
        governor.observe_after(first, &outcome.result, &outcome.receipt);
        fs::write(temp.path().join("sample.txt"), "needle changed\n").expect("change fixture");
        let changed = registry.prepare_invocation(
            OperatingMode::Auto,
            temp.path(),
            "search",
            r#"{"path":".","query":"needle"}"#,
        );
        let (pending, _) = governor.observe_before_identified(&changed, "batch", "changed");
        let changed_outcome =
            registry.execute_prepared_with_cancellation_and_progress(&changed, None, |_| {});
        assert!(governor
            .observe_after(pending, &changed_outcome.result, &changed_outcome.receipt)
            .iter()
            .any(|observation| matches!(
                observation,
                GovernorObservation::Progress {
                    kind: CausalProgressKind::DependencyChanged,
                    ..
                }
            )));

        let listed = registry.prepare_invocation(
            OperatingMode::Auto,
            temp.path(),
            "list",
            r#"{"path":".","max_entries":1}"#,
        );
        let (pending, _) = governor.observe_before_identified(&listed, "batch", "list");
        let listed_outcome =
            registry.execute_prepared_with_cancellation_and_progress(&listed, None, |_| {});
        assert_eq!(listed_outcome.receipt.dependencies.len(), 1);
        assert!(listed_outcome.receipt.bytes_read > 0);
        assert!(!governor
            .observe_after(pending, &listed_outcome.result, &listed_outcome.receipt)
            .iter()
            .any(|observation| matches!(observation, GovernorObservation::Boundary { .. })));

        let marker = "pass \"cursor\": \"";
        let cursor_start =
            listed_outcome.result.output.find(marker).expect("cursor") + marker.len();
        let cursor_tail = &listed_outcome.result.output[cursor_start..];
        let cursor = &cursor_tail[..cursor_tail.find('"').expect("cursor end")];
        let continued = registry.prepare_invocation(
            OperatingMode::Auto,
            temp.path(),
            "list",
            &serde_json::json!({"path": ".", "max_entries": 1, "cursor": cursor}).to_string(),
        );
        let continued_outcome =
            registry.execute_prepared_with_cancellation_and_progress(&continued, None, |_| {});
        assert!(continued_outcome.result.success);
        assert_eq!(continued_outcome.receipt.dependencies.len(), 1);
        assert_eq!(continued_outcome.receipt.bytes_read, 0);
    }

    #[test]
    fn successful_allowlisted_validation_stays_green_without_fake_dependencies() {
        let temp = TestRoot::new("validation");
        let registry = ToolRegistry::default();
        for arguments in [
            r#"{"command":"cargo clippy --fix --allow-dirty"}"#,
            r#"{"command":"cargo","args":["clippy","--fix","--allow-dirty"]}"#,
        ] {
            let fixing =
                registry.prepare_invocation(OperatingMode::Auto, temp.path(), "shell", arguments);
            assert_eq!(
                fixing.spec.unwrap().effect_class,
                crate::tools::ToolEffectClass::PotentiallyVolatile,
                "automatic fixes must remain a serial mutation barrier"
            );
        }
        let prepared = registry.prepare_invocation(
            OperatingMode::Auto,
            temp.path(),
            "shell",
            r#"{"command":"cargo","args":["check"]}"#,
        );
        let mut governor = CausalGovernor::default();
        let (pending, _) = governor.observe_before_identified(&prepared, "batch", "validation");
        let result = ToolResult {
            name: "shell".into(),
            output: "exit 0\n".into(),
            success: true,
            artifact: None,
        };
        let receipt = ToolExecutionReceipt::unobserved(&prepared, 0, 0, 1);

        let observations = governor.observe_after(pending, &result, &receipt);

        assert!(observations.iter().any(|observation| matches!(
            observation,
            GovernorObservation::Progress {
                kind: CausalProgressKind::ValidationGreen,
                ..
            }
        )));

        let other = registry.prepare_invocation(
            OperatingMode::Auto,
            temp.path(),
            "shell",
            r#"{"command":"cargo test"}"#,
        );
        let (other_pending, _) =
            governor.observe_before_identified(&other, "batch", "other-validation");
        let other_receipt = ToolExecutionReceipt::unobserved(&other, 0, 0, 1);
        assert!(governor
            .observe_after(other_pending, &result, &other_receipt)
            .iter()
            .any(|observation| matches!(
                observation,
                GovernorObservation::Progress {
                    kind: CausalProgressKind::ValidationGreen,
                    ..
                }
            )));

        let (repeat_pending, _) =
            governor.observe_before_identified(&prepared, "batch", "repeat-validation");
        assert!(governor
            .observe_after(repeat_pending, &result, &receipt)
            .iter()
            .any(|observation| matches!(
                observation,
                GovernorObservation::Anomaly {
                    kind: CausalAnomalyKind::RedundantValidation,
                    action: CausalShadowAction::WouldWarn,
                    ..
                }
            )));

        let (delayed_pending, _) =
            governor.observe_before_identified(&prepared, "batch", "delayed-validation");

        fs::write(temp.path().join("changed.txt"), "before").expect("write fixture");
        let mutation = registry.prepare_invocation(
            OperatingMode::Auto,
            temp.path(),
            "write",
            r#"{"path":"changed.txt","content":"after","expected":"before"}"#,
        );
        let (mutation_pending, _) =
            governor.observe_before_identified(&mutation, "batch", "mutation");
        let mutation_outcome =
            registry.execute_prepared_with_cancellation_and_progress(&mutation, None, |_| {});
        governor.observe_after(
            mutation_pending,
            &mutation_outcome.result,
            &mutation_outcome.receipt,
        );
        governor.observe_after(delayed_pending, &result, &receipt);
        let (post_mutation, _) =
            governor.observe_before_identified(&prepared, "batch", "post-mutation");
        let revision = registry.workspace_revision();
        let post_mutation_receipt =
            ToolExecutionReceipt::unobserved(&prepared, revision, revision, 1);
        assert!(governor
            .observe_after(post_mutation, &result, &post_mutation_receipt)
            .iter()
            .any(|observation| matches!(
                observation,
                GovernorObservation::Progress {
                    kind: CausalProgressKind::ValidationGreen,
                    ..
                }
            )));

        fs::write(temp.path().join("changed.txt"), "external").expect("external change");
        let read = registry.prepare_invocation(
            OperatingMode::Auto,
            temp.path(),
            "read",
            r#"{"path":"changed.txt","max_lines":1}"#,
        );
        let (read_pending, _) = governor.observe_before_identified(&read, "batch", "external-read");
        let read_outcome =
            registry.execute_prepared_with_cancellation_and_progress(&read, None, |_| {});
        assert!(governor
            .observe_after(read_pending, &read_outcome.result, &read_outcome.receipt)
            .iter()
            .any(|observation| matches!(
                observation,
                GovernorObservation::Progress {
                    kind: CausalProgressKind::DependencyChanged,
                    ..
                }
            )));

        let (after_external, _) =
            governor.observe_before_identified(&prepared, "batch", "after-external");
        let external_revision = registry.workspace_revision();
        let after_external_receipt =
            ToolExecutionReceipt::unobserved(&prepared, external_revision, external_revision, 1);
        assert!(governor
            .observe_after(after_external, &result, &after_external_receipt)
            .iter()
            .any(|observation| matches!(
                observation,
                GovernorObservation::Progress {
                    kind: CausalProgressKind::ValidationGreen,
                    ..
                }
            )));
    }

    #[test]
    fn validation_outcomes_require_each_failed_command_to_recover() {
        let temp = TestRoot::new("validation-outcomes");
        let registry = ToolRegistry::default();
        let mut governor = CausalGovernor::default();
        let observe = |governor: &mut CausalGovernor, command: &str, success: bool| {
            let prepared = registry.prepare_invocation(
                OperatingMode::Auto,
                temp.path(),
                "shell",
                &serde_json::json!({"command": command}).to_string(),
            );
            let (pending, _) = governor.observe_before_identified(&prepared, "batch", command);
            let result = ToolResult {
                name: "shell".into(),
                output: if success {
                    "exit 0\n"
                } else {
                    "exit 1\nfailed"
                }
                .into(),
                success,
                artifact: None,
            };
            let receipt = ToolExecutionReceipt::unobserved(&prepared, 0, 0, 1);
            governor.observe_after(pending, &result, &receipt);
        };
        assert!(!governor.validations_satisfied());
        observe(&mut governor, "cargo check", true);
        assert!(governor.validations_satisfied());
        observe(&mut governor, "cargo test", false);
        assert!(!governor.validations_satisfied());
        observe(&mut governor, "cargo check", true);
        assert!(
            !governor.validations_satisfied(),
            "a different check cannot resolve failure"
        );
        governor.forget_compacted_evidence();
        assert!(
            !governor.validations_satisfied(),
            "compaction must retain unresolved validation"
        );
        observe(&mut governor, "cargo test", true);
        assert!(governor.validations_satisfied());
        observe(&mut governor, "cargo test", false);
        observe(&mut governor, "cargo test", true);
        observe(&mut governor, "cargo test", false);
        assert!(
            !governor.validations_satisfied(),
            "previously seen failures still invalidate success"
        );
        observe(&mut governor, "cargo test", true);
        governor.ledger.workspace_revision += 1;
        assert!(
            !governor.validations_satisfied(),
            "green evidence predates the mutation"
        );
        observe(&mut governor, "cargo test", true);
        assert!(governor.validations_satisfied());
        governor.ledger.uncertainty_epoch += 1;
        assert!(
            !governor.validations_satisfied(),
            "volatile effects invalidate old evidence"
        );
    }

    #[test]
    fn non_validation_commands_cannot_satisfy_task_validation() {
        let temp = TestRoot::new("help-not-validation");
        let registry = ToolRegistry::default();
        for arguments in [
            serde_json::json!({"command":"cargo test --help"}),
            serde_json::json!({"command":"cargo test -h"}),
            serde_json::json!({"command":"cargo fmt --check --help"}),
            serde_json::json!({"command":"cargo check '-h'"}),
            serde_json::json!({"command":"cargo", "args":["clippy", "--version"]}),
            serde_json::json!({"command":"cargo --version"}),
            serde_json::json!({"command":"rustc --version"}),
            serde_json::json!({"command":"rustfmt --version"}),
            serde_json::json!({"command":"git status"}),
            serde_json::json!({"command":"git diff --check"}),
        ] {
            let prepared = registry.prepare_invocation(
                OperatingMode::Auto,
                temp.path(),
                "shell",
                &arguments.to_string(),
            );
            let mut governor = CausalGovernor::default();
            let (pending, _) = governor.observe_before_identified(&prepared, "batch", "help");
            governor.observe_after(
                pending,
                &ToolResult {
                    name: "shell".into(),
                    output: "exit 0\nUsage: cargo ...\n".into(),
                    success: true,
                    artifact: None,
                },
                &ToolExecutionReceipt::unobserved(&prepared, 0, 0, 1),
            );
            assert!(
                !governor.validations_satisfied(),
                "command is not validation: {arguments}"
            );
        }
    }

    #[test]
    fn concurrent_source_revision_is_not_counted_twice() {
        let temp = TestRoot::new("concurrent-revision");
        let path = temp.path().join("sample.txt");
        fs::write(&path, "before\n").expect("write fixture");
        let registry = ToolRegistry::default();
        let prepared = registry.prepare_invocation(
            OperatingMode::Auto,
            temp.path(),
            "read",
            r#"{"path":"sample.txt","max_lines":1}"#,
        );
        let mut governor = CausalGovernor::default();
        let (initial_pending, _) =
            governor.observe_before_identified(&prepared, "batch", "initial");
        let initial =
            registry.execute_prepared_with_cancellation_and_progress(&prepared, None, |_| {});
        governor.observe_after(initial_pending, &initial.result, &initial.receipt);

        fs::write(path, "after\n").expect("change fixture");
        let (changed_pending, _) =
            governor.observe_before_identified(&prepared, "batch", "changed");
        let mut changed =
            registry.execute_prepared_with_cancellation_and_progress(&prepared, None, |_| {});
        changed.receipt.revision_after = changed.receipt.revision_before.saturating_add(1);

        governor.observe_after(changed_pending, &changed.result, &changed.receipt);

        assert_eq!(
            governor.ledger.workspace_revision,
            changed.receipt.revision_after
        );
    }

    #[test]
    fn unchanged_write_receipt_does_not_claim_workspace_progress() {
        let temp = TestRoot::new("write");
        fs::write(temp.path().join("same.txt"), "same").expect("write fixture");
        let registry = ToolRegistry::default();
        let prepared = registry.prepare_invocation(
            OperatingMode::Auto,
            temp.path(),
            "write",
            r#"{"path":"same.txt","content":"same","expected":"same"}"#,
        );
        let mut governor = CausalGovernor::default();
        let (pending, _) = governor.observe_before_identified(&prepared, "batch", "write");
        let outcome =
            registry.execute_prepared_with_cancellation_and_progress(&prepared, None, |_| {});
        assert!(outcome.result.success);
        assert_eq!(
            outcome.receipt.revision_before,
            outcome.receipt.revision_after
        );
        assert!(governor
            .observe_after(pending, &outcome.result, &outcome.receipt)
            .is_empty());
    }

    #[test]
    fn out_of_order_receipts_do_not_double_count_or_restore_stale_stamps() {
        let temp = TestRoot::new("write-read");
        fs::write(temp.path().join("sample.txt"), "before").expect("write fixture");
        fs::write(temp.path().join("other.txt"), "stable").expect("write other fixture");
        let registry = ToolRegistry::default();
        let read = registry.prepare_invocation(
            OperatingMode::Auto,
            temp.path(),
            "read",
            r#"{"path":"sample.txt","max_lines":1}"#,
        );
        let write = registry.prepare_invocation(
            OperatingMode::Auto,
            temp.path(),
            "write",
            r#"{"path":"sample.txt","content":"after","expected":"before"}"#,
        );
        let write_again = registry.prepare_invocation(
            OperatingMode::Auto,
            temp.path(),
            "write",
            r#"{"path":"sample.txt","content":"after-again","expected":"after"}"#,
        );
        let other_read = registry.prepare_invocation(
            OperatingMode::Auto,
            temp.path(),
            "read",
            r#"{"path":"other.txt","max_lines":1}"#,
        );
        let mut governor = CausalGovernor::default();

        let (pending, _) = governor.observe_before_identified(&read, "batch", "initial-read");
        let initial_read =
            registry.execute_prepared_with_cancellation_and_progress(&read, None, |_| {});
        governor.observe_after(pending, &initial_read.result, &initial_read.receipt);

        let (write_pending, _) = governor.observe_before_identified(&write, "batch", "write-1");
        let write_outcome =
            registry.execute_prepared_with_cancellation_and_progress(&write, None, |_| {});
        let first_revision = write_outcome.receipt.revision_after;
        #[cfg(windows)]
        assert!(write_outcome.receipt.mutations[0].after.file_id.is_some());
        let (stale_read_pending, _) =
            governor.observe_before_identified(&read, "batch", "read-revision-1");
        let stale_read =
            registry.execute_prepared_with_cancellation_and_progress(&read, None, |_| {});
        assert_eq!(stale_read.receipt.revision_before, first_revision);

        let (other_pending, _) =
            governor.observe_before_identified(&other_read, "batch", "other-read");
        let other_outcome =
            registry.execute_prepared_with_cancellation_and_progress(&other_read, None, |_| {});
        governor.observe_after(other_pending, &other_outcome.result, &other_outcome.receipt);

        let (write_again_pending, _) =
            governor.observe_before_identified(&write_again, "batch", "write-2");
        let write_again_outcome =
            registry.execute_prepared_with_cancellation_and_progress(&write_again, None, |_| {});
        let second_revision = write_again_outcome.receipt.revision_after;
        let (current_read_pending, _) =
            governor.observe_before_identified(&read, "batch", "read-revision-2");
        let current_read =
            registry.execute_prepared_with_cancellation_and_progress(&read, None, |_| {});
        assert_eq!(current_read.receipt.revision_before, second_revision);
        #[cfg(windows)]
        assert!(current_read.receipt.dependencies[0].stamp.file_id.is_some());
        let observations = governor.observe_after(
            current_read_pending,
            &current_read.result,
            &current_read.receipt,
        );
        assert!(observations.iter().any(|observation| matches!(
            observation,
            GovernorObservation::Progress {
                kind: CausalProgressKind::DependencyChanged,
                ..
            }
        )));
        assert_eq!(governor.ledger.workspace_revision, second_revision);

        let stale_observations =
            governor.observe_after(stale_read_pending, &stale_read.result, &stale_read.receipt);
        assert!(!stale_observations
            .iter()
            .any(|observation| matches!(observation, GovernorObservation::Progress { .. })));
        assert_eq!(governor.ledger.workspace_revision, second_revision);

        governor.observe_after(write_pending, &write_outcome.result, &write_outcome.receipt);
        governor.observe_after(
            write_again_pending,
            &write_again_outcome.result,
            &write_again_outcome.receipt,
        );
        assert_eq!(governor.ledger.workspace_revision, second_revision);

        let (final_pending, _) = governor.observe_before_identified(&read, "batch", "final-read");
        let final_read =
            registry.execute_prepared_with_cancellation_and_progress(&read, None, |_| {});
        let observations =
            governor.observe_after(final_pending, &final_read.result, &final_read.receipt);
        assert!(!observations.iter().any(|observation| matches!(
            observation,
            GovernorObservation::Progress {
                kind: CausalProgressKind::DependencyChanged,
                ..
            }
        )));
    }

    #[test]
    fn failed_tools_keep_observed_dependencies_and_bytes() {
        let temp = TestRoot::new("failed-read");
        fs::write(
            temp.path().join("large-line.txt"),
            "x".repeat(1024 * 1024 + 1),
        )
        .expect("write fixture");
        let registry = ToolRegistry::default();
        let prepared = registry.prepare_invocation(
            OperatingMode::Auto,
            temp.path(),
            "read",
            r#"{"path":"large-line.txt","max_lines":1}"#,
        );

        let outcome =
            registry.execute_prepared_with_cancellation_and_progress(&prepared, None, |_| {});

        assert!(!outcome.result.success);
        assert_eq!(outcome.receipt.dependencies.len(), 1);
        assert!(outcome.receipt.bytes_read > 0);

        fs::write(temp.path().join("stale.txt"), "actual").expect("write stale fixture");
        let stale_write = registry.prepare_invocation(
            OperatingMode::Auto,
            temp.path(),
            "write",
            r#"{"path":"stale.txt","content":"new","expected":"old"}"#,
        );
        let outcome =
            registry.execute_prepared_with_cancellation_and_progress(&stale_write, None, |_| {});
        assert!(!outcome.result.success);
        assert_eq!(outcome.receipt.dependencies.len(), 1);
        assert_eq!(outcome.receipt.bytes_read, 6);

        fs::write(temp.path().join("ambiguous.txt"), "x x").expect("write patch fixture");
        let ambiguous_patch = registry.prepare_invocation(
            OperatingMode::Auto,
            temp.path(),
            "patch",
            r#"{"path":"ambiguous.txt","expected":"x","replacement":"y"}"#,
        );
        let outcome = registry.execute_prepared_with_cancellation_and_progress(
            &ambiguous_patch,
            None,
            |_| {},
        );
        assert!(!outcome.result.success);
        assert_eq!(outcome.receipt.dependencies.len(), 1);
        assert_eq!(outcome.receipt.bytes_read, 3);
        assert!(outcome.receipt.mutations.is_empty());

        fs::write(temp.path().join("invalid.txt"), b"valid\xffinvalid")
            .expect("write invalid utf-8 fixture");
        let invalid = registry.prepare_invocation(
            OperatingMode::Auto,
            temp.path(),
            "read",
            r#"{"path":"invalid.txt","max_lines":1}"#,
        );
        let outcome =
            registry.execute_prepared_with_cancellation_and_progress(&invalid, None, |_| {});
        assert!(!outcome.result.success);
        assert_eq!(outcome.receipt.bytes_read, 13);

        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let cancelled_list = registry.prepare_invocation(
            OperatingMode::Auto,
            temp.path(),
            "list",
            r#"{"path":"."}"#,
        );
        let outcome = registry.execute_prepared_with_cancellation_and_progress(
            &cancelled_list,
            Some(&cancellation),
            |_| {},
        );
        assert!(!outcome.result.success);
        assert_eq!(outcome.receipt.dependencies.len(), 1);
        assert_eq!(outcome.receipt.bytes_read, 0);

        let invalid_search = registry.prepare_invocation(
            OperatingMode::Auto,
            temp.path(),
            "search",
            r#"{"path":"invalid.txt","query":"valid"}"#,
        );
        let outcome =
            registry.execute_prepared_with_cancellation_and_progress(&invalid_search, None, |_| {});
        // Invalid UTF-8 skips the file instead of failing the search, but the
        // observed dependency and bytes are still reported.
        assert!(outcome.result.success);
        assert_eq!(outcome.receipt.dependencies.len(), 1);
        assert_eq!(outcome.receipt.bytes_read, 13);
    }

    #[test]
    fn canonical_defaults_share_a_prepared_fingerprint() {
        let temp = TestRoot::new("canonical");
        fs::write(temp.path().join("same.txt"), "same").expect("write fixture");
        let registry = ToolRegistry::default();
        let implicit = registry.prepare_invocation(
            OperatingMode::Auto,
            temp.path(),
            "read",
            r#"{"path":"same.txt"}"#,
        );
        // Path and offset normalize to the same call, so the fingerprint
        // (and the governor's identity) still matches.
        let normalized = registry.prepare_invocation(
            OperatingMode::Auto,
            temp.path(),
            "read",
            r#"{"offset":1,"path":"./same.txt"}"#,
        );
        assert_eq!(
            implicit.canonical_fingerprint,
            normalized.canonical_fingerprint
        );
        // An explicit max_lines is a different call from an omitted one: the
        // read service only widens the omitted default, so identities differ.
        let explicit = registry.prepare_invocation(
            OperatingMode::Auto,
            temp.path(),
            "read",
            &serde_json::json!({
                "max_lines": crate::tools::DEFAULT_MAX_READ_LINES,
                "offset": 1,
                "path": "./same.txt"
            })
            .to_string(),
        );
        assert_ne!(
            implicit.canonical_fingerprint,
            explicit.canonical_fingerprint
        );
        let mut governor = CausalGovernor::default();
        let (left, _) = governor.observe_before_identified(&implicit, "b", "1");
        let (right, _) = governor.observe_before_identified(&normalized, "b", "2");
        assert_eq!(left.call_fingerprint(), right.call_fingerprint());
        let (distinct, _) = governor.observe_before_identified(&explicit, "b", "3");
        assert_ne!(left.call_fingerprint(), distinct.call_fingerprint());

        let optional_code_intel_path = registry.prepare_invocation(
            OperatingMode::Auto,
            temp.path(),
            "code_intel",
            r#"{"action":"symbol","path":"","query":"same"}"#,
        );
        assert!(optional_code_intel_path.error.is_none());
        assert!(optional_code_intel_path.target_paths.is_empty());
    }
}
