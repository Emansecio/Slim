use super::CausalGovernor;
use std::collections::HashSet;

/// Cap on evidence fingerprints remembered across compactions.
const MAX_COMPACTED_FINGERPRINTS: usize = 4_096;

impl CausalGovernor {
    /// A compaction dropped the evidence the ledger was holding: remember its
    /// fingerprints, so reading it again counts as a reacquisition, and
    /// restart the stagnation count.
    pub(in crate::runtime) fn forget_compacted_evidence(&mut self) {
        let mut compacted = self.ledger.evidence.keys().cloned().collect::<Vec<_>>();
        compacted.sort();
        for fingerprint in compacted {
            if self.ledger.compacted_fingerprints.len() >= MAX_COMPACTED_FINGERPRINTS {
                break;
            }
            self.ledger.compacted_fingerprints.insert(fingerprint);
        }
        self.ledger.evidence.clear();
        self.ledger.seen_evidence.clear();
        self.ledger.stagnant_turns = 0;
        self.stop_requested = false;
    }

    pub(in crate::runtime) fn take_post_compaction_reacquisitions(&mut self) -> HashSet<String> {
        std::mem::take(&mut self.pending_post_compaction_reacquisitions)
    }
}
