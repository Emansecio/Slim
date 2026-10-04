//! Causal progress ledger: classifies each completed tool call as progress,
//! an uncertainty boundary or an anomaly.
//!
//! - `observation`: the events the ledger emits and the in-flight call;
//! - `evidence`: identities and digests of results and states;
//! - `compaction`: what the ledger forgets when a compaction drops evidence.

mod compaction;
mod evidence;
mod observation;
#[cfg(test)]
mod tests;

use crate::tools::{
    dependency_key, DependencyKind, DependencyObservation, PreparedToolInvocation,
    ToolDependencyScope, ToolEffectClass, ToolExecutionReceipt, ToolOperationalSpec,
    ToolReplayPolicy, ToolResult, ToolVolatility,
};
use crate::{
    CausalAnomalyKind, CausalBoundaryKind, CausalConfidence, CausalProgressKind, CausalShadowAction,
};
use evidence::{
    evidence_applicable, evidence_id, evidence_scope, hash_tagged, mutations_digest,
    observations_digest, seen_key, stateful_call_fingerprint, validation_green,
};
use observation::CallIds;
pub(super) use observation::{GovernorObservation, PendingCall};
use std::collections::{BTreeMap, HashMap, HashSet};

const MAX_OBSERVED_DEPENDENCIES: usize = 256;

#[derive(Default)]
pub(super) struct CausalGovernor {
    ledger: ProgressLedger,
    stop_requested: bool,
    pending_post_compaction_reacquisitions: HashSet<String>,
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
    compacted_fingerprints: HashSet<String>,
}

impl ProgressLedger {
    /// Whether a fact recorded at `revision`/`epoch` still describes the
    /// current workspace.
    fn is_current(&self, revision: u64, epoch: u64) -> bool {
        revision == self.workspace_revision && epoch == self.uncertainty_epoch
    }
}

struct ObservedDependency {
    digest: String,
    source_revision: u64,
}

struct TurnState {
    has_calls: bool,
    made_progress: bool,
    /// An uncertain effect or unclassifiable call happened this turn.
    boundary: bool,
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
            confidence: CausalConfidence::High,
            last_batch_id: String::new(),
            last_call_id: String::new(),
            last_tool_name: String::new(),
            last_call_fingerprint: String::new(),
        }
    }
}

impl TurnState {
    fn lower_confidence(&mut self, confidence: CausalConfidence) {
        self.confidence = self.confidence.min(confidence);
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

/// What recording an evidence result did to the call's record.
#[derive(Clone, Copy, Eq, PartialEq)]
enum EvidenceRecording {
    Inserted,
    /// The same evidence and outcome again; carries the repetition count.
    Repeated(u32),
    /// The call's record now holds different evidence or outcome.
    Changed,
}

struct ValidationResult {
    success: bool,
    workspace_revision: u64,
    uncertainty_epoch: u64,
}

/// How a receipt's dependencies compare with what the ledger already knows.
#[derive(Default)]
struct DependencyDelta {
    changed: bool,
    /// A change at the same source revision: nothing the runtime did explains it.
    unexplained: bool,
    /// The receipt is older than what the ledger already observed.
    stale: bool,
}

impl CausalGovernor {
    /// Stop only when the completed batch has no progress or uncertain boundary.
    pub(super) fn stop_requested(&self) -> bool {
        self.stop_requested
    }

    pub(super) fn validations_satisfied(&self) -> bool {
        let validations = &self.ledger.validations;
        validations.values().all(|result| result.success)
            && validations.values().any(|result| {
                self.ledger
                    .is_current(result.workspace_revision, result.uncertainty_epoch)
            })
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
        let ids = CallIds::new(prepared, batch_id, call_id);
        if prepared.structural_rejection {
            if let Some(spec) = prepared.spec {
                return (self.structural_pending(ids, prepared, spec), Vec::new());
            }
        }
        let Some(spec) = prepared.spec.filter(|_| prepared.error.is_none()) else {
            return self.unclassifiable_call(ids, prepared.canonical_fingerprint.clone());
        };
        (self.stateful_pending(ids, prepared, spec), Vec::new())
    }

    /// A call rejected by deterministic argument admission.
    fn structural_pending(
        &mut self,
        ids: CallIds,
        prepared: &PreparedToolInvocation,
        spec: ToolOperationalSpec,
    ) -> PendingCall {
        let confidence = confidence_for(spec);
        let call_fingerprint = prepared.canonical_fingerprint.clone();
        self.begin_call(&ids, &call_fingerprint, confidence);
        // The prepared fingerprint is already canonical. Reuse it directly so
        // a rejected alias has one evidence key without another state hash or
        // uncertainty epoch.
        let evidence_scope = call_fingerprint.clone();
        PendingCall {
            evidence_scope,
            spec: Some(spec),
            confidence,
            admission_prefix: crate::tools::admission_output_prefix(&prepared.admission_notes),
            structural_rejection: true,
            ..PendingCall::new(
                ids,
                prepared.canonical_fingerprint.clone(),
                call_fingerprint,
                self.ledger.workspace_revision,
            )
        }
    }

    /// A classifiable call, stamped with the state it will run against.
    fn stateful_pending(
        &mut self,
        ids: CallIds,
        prepared: &PreparedToolInvocation,
        spec: ToolOperationalSpec,
    ) -> PendingCall {
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
        self.begin_call(&ids, &call_fingerprint, confidence);
        PendingCall {
            evidence_scope,
            spec: Some(spec),
            confidence,
            diagnostics: prepared.name == "code_intel" && prepared.arguments.is_diagnostics(),
            admission_prefix: crate::tools::admission_output_prefix(&prepared.admission_notes),
            ..PendingCall::new(
                ids,
                prepared.canonical_fingerprint.clone(),
                call_fingerprint,
                self.ledger.workspace_revision,
            )
        }
    }

    pub(super) fn observe_after(
        &mut self,
        mut pending: PendingCall,
        result: &ToolResult,
        receipt: &ToolExecutionReceipt,
    ) -> Vec<GovernorObservation> {
        if pending.structural_rejection {
            return self.observe_evidence(pending, result, EvidenceOutcome::Failure);
        }
        let source_revision = self.sync_revision(receipt);
        let Some(spec) = pending.spec else {
            return Vec::new();
        };
        // Hashed once: it identifies the receipt's state and, for a snapshot
        // read, the evidence of a changed dependency.
        let dependencies_digest = (receipt.mutations.is_empty()
            && !receipt.dependencies.is_empty())
        .then(|| observations_digest(&receipt.dependencies));
        let receipt_digest = self.receipt_state_digest(
            receipt,
            spec.dependency_scope,
            dependencies_digest.as_deref(),
        );
        let evidence_revision = if spec.effect_class == ToolEffectClass::Validation {
            pending.workspace_revision_at_start.max(source_revision)
        } else {
            self.ledger.workspace_revision
        };
        self.restamp(&mut pending, spec, &receipt_digest, evidence_revision);

        let fused = receipt
            .fused_shell
            .as_deref()
            .map(|fused| (pending.batch_id.clone(), pending.call_id.clone(), fused));
        let mut observations = match spec.effect_class {
            ToolEffectClass::PotentiallyVolatile => self.observe_volatile(pending),
            ToolEffectClass::WorkspaceMutation
                if result.success
                    || receipt.mutations.iter().any(|mutation| mutation.changed()) =>
            {
                self.observe_mutation(pending, receipt)
            }
            ToolEffectClass::Interaction if result.success => {
                self.observe_interaction(pending, result)
            }
            ToolEffectClass::InternalState if result.success => {
                self.ledger.internal_epoch = self.ledger.internal_epoch.saturating_add(1);
                Vec::new()
            }
            ToolEffectClass::WorkspaceMutation
            | ToolEffectClass::Interaction
            | ToolEffectClass::InternalState => {
                self.observe_evidence(pending, result, EvidenceOutcome::Failure)
            }
            ToolEffectClass::Validation => {
                self.observe_validation(pending, result, evidence_revision)
            }
            ToolEffectClass::SnapshotRead => {
                self.observe_snapshot(pending, result, receipt, spec, dependencies_digest)
            }
        };
        if let Some((batch_id, call_id, (shell_call, shell_result))) = fused {
            observations.extend(self.observe_fused_shell(
                &batch_id,
                &call_id,
                shell_call,
                shell_result,
                receipt.revision_after,
            ));
        }
        observations
    }

    /// Adopts the receipt's source revision, counting only its increase.
    /// Returns the revision the receipt was produced against.
    fn sync_revision(&mut self, receipt: &ToolExecutionReceipt) -> u64 {
        let source_revision = source_revision(receipt);
        if source_revision > self.ledger.source_workspace_revision {
            self.ledger.workspace_revision = self.ledger.workspace_revision.saturating_add(
                source_revision.saturating_sub(self.ledger.source_workspace_revision),
            );
            self.ledger.source_workspace_revision = source_revision;
        }
        source_revision
    }

    /// Re-keys the call by the state its receipt observed, and tracks a call
    /// that re-acquires evidence dropped by compaction.
    fn restamp(
        &mut self,
        pending: &mut PendingCall,
        spec: ToolOperationalSpec,
        receipt_digest: &str,
        evidence_revision: u64,
    ) {
        pending.call_fingerprint = stateful_call_fingerprint(
            &pending.canonical_fingerprint,
            receipt_digest,
            self.ledger.uncertainty_epoch,
            (spec.effect_class == ToolEffectClass::Validation).then_some(evidence_revision),
        );
        pending.evidence_scope = evidence_scope(
            spec,
            receipt_digest,
            evidence_revision,
            self.ledger.uncertainty_epoch,
        );
        if self
            .ledger
            .compacted_fingerprints
            .remove(&pending.call_fingerprint)
        {
            self.pending_post_compaction_reacquisitions
                .insert(pending.call_id.clone());
        }
        self.ledger
            .turn
            .last_call_fingerprint
            .clone_from(&pending.call_fingerprint);
    }

    fn observe_interaction(
        &mut self,
        pending: PendingCall,
        result: &ToolResult,
    ) -> Vec<GovernorObservation> {
        self.ledger.interaction_epoch = self.ledger.interaction_epoch.saturating_add(1);
        self.mark_progress();
        let evidence_id = evidence_id(
            &pending.tool_name,
            result,
            false,
            pending.admission_prefix.as_deref(),
        );
        vec![pending.progress(
            CausalProgressKind::ExternalInput,
            evidence_id,
            self.ledger.workspace_revision,
        )]
    }

    /// Observes the shell command fused into a mutation (`then_run`) as its
    /// own call, at the mutation's final revision.
    fn observe_fused_shell(
        &mut self,
        batch_id: &str,
        call_id: &str,
        shell_call: &PreparedToolInvocation,
        shell_result: &ToolResult,
        revision_after: u64,
    ) -> Vec<GovernorObservation> {
        let fused_call_id = format!("{call_id}:then_run");
        let (shell_pending, mut observations) =
            self.observe_before_identified(shell_call, batch_id, &fused_call_id);
        let shell_receipt = ToolExecutionReceipt::unobserved(revision_after, revision_after, 0);
        observations.extend(self.observe_after(shell_pending, shell_result, &shell_receipt));
        observations
    }

    pub(super) fn finish_turn(&mut self) -> Vec<GovernorObservation> {
        let turn = std::mem::take(&mut self.ledger.turn);
        if !turn.has_calls {
            return Vec::new();
        }
        if turn.made_progress || turn.boundary {
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
        spec: ToolOperationalSpec,
        dependencies_digest: Option<String>,
    ) -> Vec<GovernorObservation> {
        if !result.success {
            return self.observe_evidence(pending, result, EvidenceOutcome::Failure);
        }
        if matches!(
            spec.dependency_scope,
            ToolDependencyScope::TargetFile
                | ToolDependencyScope::ImmediateDirectory
                | ToolDependencyScope::ObservedWorkspace
        ) && receipt.dependencies.is_empty()
        {
            return self.unclassifiable_after(pending);
        }
        let Some(delta) = self.track_dependencies(&receipt.dependencies, source_revision(receipt))
        else {
            return self.unclassifiable_after(pending);
        };
        if delta.stale {
            return self.unclassifiable_after(pending);
        }
        if delta.changed {
            if delta.unexplained {
                self.ledger.workspace_revision = self.ledger.workspace_revision.saturating_add(1);
            }
            self.mark_progress();
            let evidence_id =
                dependencies_digest.unwrap_or_else(|| observations_digest(&receipt.dependencies));
            return vec![pending.progress(
                CausalProgressKind::DependencyChanged,
                evidence_id,
                self.ledger.workspace_revision,
            )];
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
                    dependency_key(DependencyKind::File, &mutation.path),
                    digest,
                    receipt.revision_after,
                );
            }
        }
        if !receipt.mutations.iter().any(|mutation| mutation.changed()) {
            return Vec::new();
        }
        self.mark_progress();
        vec![pending.progress(
            CausalProgressKind::WorkspaceChanged,
            mutations_digest(receipt),
            self.ledger.workspace_revision,
        )]
    }

    fn observe_validation(
        &mut self,
        pending: PendingCall,
        result: &ToolResult,
        evidence_revision: u64,
    ) -> Vec<GovernorObservation> {
        let success = validation_green(result, pending.admission_prefix.as_deref());
        // Track every outcome, including repetitions suppressed by the progress
        // ledger. A different green command cannot resolve this command's failure.
        self.ledger.validations.insert(
            pending.canonical_fingerprint.clone(),
            ValidationResult {
                success,
                // The revision the call actually tested, not the one at
                // completion: a mutation in between makes it stale.
                workspace_revision: evidence_revision,
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
        self.mark_uncertain();
        vec![pending.boundary(
            CausalBoundaryKind::PotentiallyVolatile,
            self.ledger.uncertainty_epoch,
        )]
    }

    fn observe_evidence(
        &mut self,
        pending: PendingCall,
        result: &ToolResult,
        outcome: EvidenceOutcome,
    ) -> Vec<GovernorObservation> {
        if !evidence_applicable(&pending, outcome) {
            return Vec::new();
        }
        let is_validation = pending.is_validation();
        let evidence_id = evidence_id(
            &pending.tool_name,
            result,
            is_validation,
            pending.admission_prefix.as_deref(),
        );
        let is_new_evidence =
            self.ledger
                .seen_evidence
                .insert(seen_key(&evidence_id, &pending, is_validation));
        let recording = self.record_evidence(&pending.call_fingerprint, &evidence_id, outcome);
        let changed_evidence = match recording {
            EvidenceRecording::Repeated(occurrence) => {
                return self.repeat_anomaly(pending, evidence_id, outcome, occurrence);
            }
            EvidenceRecording::Inserted => false,
            EvidenceRecording::Changed => outcome != EvidenceOutcome::Failure,
        };
        if !is_new_evidence && !changed_evidence {
            return Vec::new();
        }
        self.mark_progress();
        let kind = match outcome {
            EvidenceOutcome::ValidationGreen => CausalProgressKind::ValidationGreen,
            EvidenceOutcome::Failure => CausalProgressKind::DistinctFailure,
            _ if pending.diagnostics && recording == EvidenceRecording::Changed => {
                CausalProgressKind::DiagnosticsChanged
            }
            _ => CausalProgressKind::NewEvidence,
        };
        vec![pending.progress(kind, evidence_id, self.ledger.workspace_revision)]
    }

    /// Records an evidence result against the call's fingerprint.
    fn record_evidence(
        &mut self,
        fingerprint: &str,
        evidence_id: &str,
        outcome: EvidenceOutcome,
    ) -> EvidenceRecording {
        // `entry` would clone the fingerprint on every call; clone it only to
        // insert.
        if let Some(record) = self.ledger.evidence.get_mut(fingerprint) {
            if record.evidence_id == evidence_id && record.outcome == outcome {
                record.repetitions = record.repetitions.saturating_add(1);
                return EvidenceRecording::Repeated(record.repetitions);
            }
            record.evidence_id.clear();
            record.evidence_id.push_str(evidence_id);
            record.outcome = outcome;
            record.repetitions = 0;
            return EvidenceRecording::Changed;
        }
        self.ledger.evidence.insert(
            fingerprint.to_owned(),
            EvidenceRecord {
                evidence_id: evidence_id.to_owned(),
                outcome,
                repetitions: 0,
            },
        );
        EvidenceRecording::Inserted
    }

    /// The anomaly for evidence the call already produced.
    fn repeat_anomaly(
        &mut self,
        pending: PendingCall,
        evidence_id: String,
        outcome: EvidenceOutcome,
        occurrence: u32,
    ) -> Vec<GovernorObservation> {
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
        vec![pending.anomaly(
            kind,
            evidence_id,
            self.ledger.workspace_revision,
            occurrence,
            action,
        )]
    }

    /// A call the governor cannot classify: an uncertainty boundary.
    fn unclassifiable_call(
        &mut self,
        ids: CallIds,
        call_fingerprint: String,
    ) -> (PendingCall, Vec<GovernorObservation>) {
        self.mark_uncertain();
        self.begin_call(&ids, &call_fingerprint, CausalConfidence::Low);
        let pending = PendingCall::new(
            ids,
            call_fingerprint.clone(),
            call_fingerprint,
            self.ledger.workspace_revision,
        );
        let boundary = pending.clone().boundary(
            CausalBoundaryKind::Unclassifiable,
            self.ledger.uncertainty_epoch,
        );
        (pending, vec![boundary])
    }

    fn unclassifiable_after(&mut self, pending: PendingCall) -> Vec<GovernorObservation> {
        self.mark_uncertain();
        self.ledger.turn.lower_confidence(CausalConfidence::Low);
        vec![pending.boundary(
            CausalBoundaryKind::Unclassifiable,
            self.ledger.uncertainty_epoch,
        )]
    }

    /// Opens a new uncertainty epoch: earlier evidence no longer applies, and
    /// the turn is not judged for stagnation.
    fn mark_uncertain(&mut self) {
        self.ledger.uncertainty_epoch = self.ledger.uncertainty_epoch.saturating_add(1);
        self.ledger.turn.boundary = true;
    }

    fn begin_call(&mut self, ids: &CallIds, call_fingerprint: &str, confidence: CausalConfidence) {
        let turn = &mut self.ledger.turn;
        turn.has_calls = true;
        turn.lower_confidence(confidence);
        ids.batch_id.clone_into(&mut turn.last_batch_id);
        ids.call_id.clone_into(&mut turn.last_call_id);
        ids.tool_name.clone_into(&mut turn.last_tool_name);
        call_fingerprint.clone_into(&mut turn.last_call_fingerprint);
    }

    fn mark_progress(&mut self) {
        self.ledger.turn.made_progress = true;
    }

    /// Compares each dependency with the ledger and stores the new stamps.
    /// `None` when a dependency has no comparable stamp, before anything is
    /// stored.
    fn track_dependencies(
        &mut self,
        dependencies: &[DependencyObservation],
        source_revision: u64,
    ) -> Option<DependencyDelta> {
        let digests = dependencies
            .iter()
            .map(|dependency| dependency.stamp.comparison_digest())
            .collect::<Option<Vec<_>>>()?;
        let mut delta = DependencyDelta::default();
        for (dependency, digest) in dependencies.iter().zip(digests) {
            let key = dependency.key();
            if let Some(known) = self.ledger.observed.get(&key) {
                if source_revision < known.source_revision {
                    delta.stale = true;
                    continue;
                }
                if known.digest != digest {
                    delta.changed = true;
                    delta.unexplained |= source_revision == known.source_revision;
                }
            }
            self.store_dependency(key, digest, source_revision);
        }
        Some(delta)
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
        // A full ledger drops new keys silently: untracked dependencies only
        // make later reads look new, which is the conservative direction.
        insert_bounded(
            &mut self.ledger.observed,
            key,
            ObservedDependency {
                digest,
                source_revision,
            },
            MAX_OBSERVED_DEPENDENCIES,
        );
    }

    fn prepared_state_digest(
        &self,
        prepared: &PreparedToolInvocation,
        scope: ToolDependencyScope,
    ) -> String {
        match scope {
            ToolDependencyScope::TargetFile | ToolDependencyScope::ImmediateDirectory => {
                let kind = if scope == ToolDependencyScope::TargetFile {
                    DependencyKind::File
                } else {
                    DependencyKind::Directory
                };
                prepared
                    .target_paths
                    .first()
                    .and_then(|path| self.ledger.observed.get(&dependency_key(kind, path)))
                    .map(|dependency| dependency.digest.clone())
                    .unwrap_or_default()
            }
            _ => self.scoped_state_digest(scope),
        }
    }

    fn receipt_state_digest(
        &self,
        receipt: &ToolExecutionReceipt,
        scope: ToolDependencyScope,
        dependencies_digest: Option<&str>,
    ) -> String {
        if !receipt.mutations.is_empty() {
            return mutations_digest(receipt);
        }
        if let Some(digest) = dependencies_digest {
            return digest.to_owned();
        }
        self.scoped_state_digest(scope)
    }

    fn scoped_state_digest(&self, scope: ToolDependencyScope) -> String {
        match scope {
            ToolDependencyScope::ObservedWorkspace => hash_tagged(
                b"slim-observed-workspace-v2",
                self.ledger
                    .observed
                    .iter()
                    .map(|(key, dependency)| format!("{key}\0{}", dependency.digest)),
            ),
            ToolDependencyScope::Interaction => self.ledger.interaction_epoch.to_string(),
            ToolDependencyScope::Internal => self.ledger.internal_epoch.to_string(),
            ToolDependencyScope::Unknown => self.ledger.uncertainty_epoch.to_string(),
            ToolDependencyScope::TargetFile | ToolDependencyScope::ImmediateDirectory => {
                String::new()
            }
        }
    }
}

/// Inserts into a size-bounded map. An existing key is always replaced; a new
/// key is refused (`false`) once the map holds `max` entries.
fn insert_bounded<V>(map: &mut BTreeMap<String, V>, key: String, value: V, max: usize) -> bool {
    if map.len() >= max && !map.contains_key(&key) {
        return false;
    }
    map.insert(key, value);
    true
}

fn source_revision(receipt: &ToolExecutionReceipt) -> u64 {
    receipt.revision_before.max(receipt.revision_after)
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
