use super::{CausalGovernor, ProgressLedger, ValidationResult};
use std::collections::HashSet;

/// Cap on remembered failed calls kept for the compaction snapshot.
pub(super) const MAX_COMPACTION_FAILURES: usize = 256;
/// Cap on evidence fingerprints remembered across compactions.
const MAX_COMPACTED_FINGERPRINTS: usize = 4_096;
/// Cap on remembered mutated paths kept for the compaction snapshot.
pub(super) const MAX_COMPACTION_MUTATIONS: usize = 256;
pub(super) const MAX_COMPACTION_SNAPSHOT_BYTES: usize = 4096;
const COMPACTION_OMISSION_RESERVE_BYTES: usize = 128;

impl CausalGovernor {
    pub(in crate::runtime) fn compaction_snapshot(&self, run_start_seq: u64) -> String {
        let ledger = &self.ledger;
        if ledger.workspace_revision == 0
            && ledger.uncertainty_epoch == 0
            && ledger.validations.is_empty()
            && ledger.pending_failures.is_empty()
            && ledger.pending_failure_omitted == 0
            && ledger.mutations.is_empty()
            && ledger.mutation_omitted == 0
        {
            return String::new();
        }

        let mut writer = SnapshotWriter::new(format!(
            "execution_facts scope=compaction run_start_seq={run_start_seq} workspace_revision={} uncertainty_epoch={}\n",
            ledger.workspace_revision, ledger.uncertainty_epoch,
        ));
        let mut omissions = Omissions {
            failures: ledger.pending_failure_omitted,
            validations: 0,
            mutations: ledger.mutation_omitted,
        };
        ledger.push_failures(&mut writer, &mut omissions);
        ledger.push_validations(&mut writer, &mut omissions);
        ledger.push_mutations(&mut writer, &mut omissions);
        omissions.append_to(&mut writer.text);
        debug_assert!(writer.text.len() <= MAX_COMPACTION_SNAPSHOT_BYTES);
        writer.text
    }

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

/// Accumulates snapshot lines within the byte budget.
struct SnapshotWriter {
    text: String,
    fact_bytes: usize,
    available: usize,
}

impl SnapshotWriter {
    /// Starts from the header line; the omission line has a reserved share of
    /// the budget.
    fn new(header: String) -> Self {
        let available = MAX_COMPACTION_SNAPSHOT_BYTES
            .saturating_sub(COMPACTION_OMISSION_RESERVE_BYTES)
            .saturating_sub(header.len());
        Self {
            text: header,
            fact_bytes: 0,
            available,
        }
    }

    /// Appends `line` if it fits; `false` means it was omitted.
    fn push(&mut self, line: &str) -> bool {
        if self.fact_bytes.saturating_add(line.len()) <= self.available {
            self.fact_bytes = self.fact_bytes.saturating_add(line.len());
            self.text.push_str(line);
            true
        } else {
            false
        }
    }
}

/// Facts that did not fit the snapshot, by kind.
struct Omissions {
    failures: usize,
    validations: usize,
    mutations: usize,
}

impl Omissions {
    fn append_to(&self, snapshot: &mut String) {
        if self.failures > 0 || self.validations > 0 || self.mutations > 0 {
            snapshot.push_str(&format!(
                "omitted_observations failures={} validations={} mutations={}\n",
                self.failures, self.validations, self.mutations
            ));
        }
    }
}

impl ProgressLedger {
    fn push_failures(&self, writer: &mut SnapshotWriter, omissions: &mut Omissions) {
        for failure in self.pending_failures.values() {
            if failure.validation {
                continue;
            }
            let current = self.is_current(failure.workspace_revision, failure.uncertainty_epoch);
            let line = format!(
                "failure tool={} call_id={} revision={} epoch={} current={} pending=true\n",
                json_string(&failure.tool_name),
                json_string(&failure.call_id),
                failure.workspace_revision,
                failure.uncertainty_epoch,
                current,
            );
            if !writer.push(&line) {
                omissions.failures = omissions.failures.saturating_add(1);
            }
        }
    }

    fn push_validations(&self, writer: &mut SnapshotWriter, omissions: &mut Omissions) {
        // Failures first, then current successes, then stale ones.
        let mut validations = self.validations.iter().collect::<Vec<_>>();
        validations.sort_by_cached_key(|&(key, validation)| {
            (self.validation_priority(validation), key.as_str())
        });
        for (_, validation) in validations {
            let current =
                self.is_current(validation.workspace_revision, validation.uncertainty_epoch);
            let line = format!(
                "validation tool={} call_id={} success={} validation_revision={} validation_epoch={} current={}\n",
                json_string(&validation.tool_name),
                json_string(&validation.call_id),
                validation.success,
                validation.workspace_revision,
                validation.uncertainty_epoch,
                current,
            );
            if !writer.push(&line) {
                omissions.validations = omissions.validations.saturating_add(1);
            }
        }
    }

    fn push_mutations(&self, writer: &mut SnapshotWriter, omissions: &mut Omissions) {
        for (path, revision) in &self.mutations {
            let line = format!("mutation path={} revision={revision}\n", json_string(path));
            if !writer.push(&line) {
                omissions.mutations = omissions.mutations.saturating_add(1);
            }
        }
    }

    fn validation_priority(&self, validation: &ValidationResult) -> (u8, u64, u64) {
        let status = if !validation.success {
            0
        } else if self.is_current(validation.workspace_revision, validation.uncertainty_epoch) {
            1
        } else {
            2
        };
        (
            status,
            validation.workspace_revision,
            validation.uncertainty_epoch,
        )
    }
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}
