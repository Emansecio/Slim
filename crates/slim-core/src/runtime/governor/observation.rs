use super::evidence::bounded_identifier;
use crate::tools::{PreparedToolInvocation, ToolEffectClass, ToolOperationalSpec};
use crate::{
    CausalAnomalyKind, CausalBoundaryKind, CausalConfidence, CausalProgressKind, CausalShadowAction,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::runtime) enum GovernorObservation {
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

impl From<GovernorObservation> for crate::EventKind {
    fn from(observation: GovernorObservation) -> Self {
        match observation {
            GovernorObservation::Progress {
                batch_id,
                call_id,
                kind,
                tool_name,
                call_fingerprint,
                evidence_id,
                workspace_revision,
            } => Self::CausalProgressObserved {
                batch_id: batch_id.into_boxed_str(),
                call_id: call_id.into_boxed_str(),
                kind,
                tool_name: tool_name.into_boxed_str(),
                call_fingerprint: call_fingerprint.into_boxed_str(),
                evidence_id: evidence_id.into_boxed_str(),
                workspace_revision,
            },
            GovernorObservation::Boundary {
                batch_id,
                call_id,
                kind,
                tool_name,
                call_fingerprint,
                uncertainty_epoch,
            } => Self::CausalBoundaryObserved {
                batch_id: batch_id.into_boxed_str(),
                call_id: call_id.into_boxed_str(),
                kind,
                tool_name: tool_name.into_boxed_str(),
                call_fingerprint: call_fingerprint.into_boxed_str(),
                uncertainty_epoch,
            },
            GovernorObservation::Anomaly {
                batch_id,
                call_id,
                kind,
                tool_name,
                call_fingerprint,
                evidence_id,
                workspace_revision,
                occurrence,
                confidence,
                action,
            } => Self::CausalAnomalyDetected {
                batch_id: batch_id.into_boxed_str(),
                call_id: call_id.into_boxed_str(),
                kind,
                tool_name: tool_name.into_boxed_str(),
                call_fingerprint: call_fingerprint.into_boxed_str(),
                evidence_id: evidence_id.into_boxed_str(),
                workspace_revision,
                occurrence,
                confidence,
                action,
            },
        }
    }
}

/// Bounded event identifiers of one call.
pub(super) struct CallIds {
    pub(super) batch_id: String,
    pub(super) call_id: String,
    pub(super) tool_name: String,
}

impl CallIds {
    pub(super) fn new(prepared: &PreparedToolInvocation, batch_id: &str, call_id: &str) -> Self {
        Self {
            batch_id: bounded_identifier(batch_id),
            call_id: bounded_identifier(call_id),
            tool_name: bounded_identifier(&prepared.name),
        }
    }
}

#[derive(Clone, Debug)]
pub(in crate::runtime) struct PendingCall {
    pub(super) batch_id: String,
    pub(super) call_id: String,
    pub(super) tool_name: String,
    pub(super) canonical_fingerprint: String,
    pub(super) call_fingerprint: String,
    pub(super) evidence_scope: String,
    pub(super) workspace_revision_at_start: u64,
    pub(super) spec: Option<ToolOperationalSpec>,
    pub(super) confidence: CausalConfidence,
    pub(super) diagnostics: bool,
    pub(super) admission_prefix: Option<String>,
    pub(super) structural_rejection: bool,
}

impl PendingCall {
    /// An unclassifiable call: no spec, lowest confidence, no evidence scope.
    /// Callers that know more override the fields they know.
    pub(super) fn new(
        ids: CallIds,
        canonical_fingerprint: String,
        call_fingerprint: String,
        workspace_revision_at_start: u64,
    ) -> Self {
        Self {
            batch_id: ids.batch_id,
            call_id: ids.call_id,
            tool_name: ids.tool_name,
            canonical_fingerprint,
            call_fingerprint,
            evidence_scope: String::new(),
            workspace_revision_at_start,
            spec: None,
            confidence: CausalConfidence::Low,
            diagnostics: false,
            admission_prefix: None,
            structural_rejection: false,
        }
    }

    pub(super) fn is_validation(&self) -> bool {
        self.spec
            .is_some_and(|spec| spec.effect_class == ToolEffectClass::Validation)
    }

    pub(super) fn progress(
        self,
        kind: CausalProgressKind,
        evidence_id: String,
        workspace_revision: u64,
    ) -> GovernorObservation {
        GovernorObservation::Progress {
            batch_id: self.batch_id,
            call_id: self.call_id,
            kind,
            tool_name: self.tool_name,
            call_fingerprint: self.call_fingerprint,
            evidence_id,
            workspace_revision,
        }
    }

    pub(super) fn boundary(
        self,
        kind: CausalBoundaryKind,
        uncertainty_epoch: u64,
    ) -> GovernorObservation {
        GovernorObservation::Boundary {
            batch_id: self.batch_id,
            call_id: self.call_id,
            kind,
            tool_name: self.tool_name,
            call_fingerprint: self.call_fingerprint,
            uncertainty_epoch,
        }
    }

    pub(super) fn anomaly(
        self,
        kind: CausalAnomalyKind,
        evidence_id: String,
        workspace_revision: u64,
        occurrence: u32,
        action: CausalShadowAction,
    ) -> GovernorObservation {
        GovernorObservation::Anomaly {
            batch_id: self.batch_id,
            call_id: self.call_id,
            kind,
            tool_name: self.tool_name,
            call_fingerprint: self.call_fingerprint,
            evidence_id,
            workspace_revision,
            occurrence,
            confidence: self.confidence,
            action,
        }
    }
}
