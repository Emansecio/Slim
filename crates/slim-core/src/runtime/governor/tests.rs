use super::super::temp_root::TempRoot;
use super::compaction::MAX_COMPACTION_SNAPSHOT_BYTES;
use super::{CausalGovernor, GovernorObservation, ValidationResult};
use crate::runtime::CancellationToken;
use crate::tools::{
    PreparedToolInvocation, ToolExecutionOutcome, ToolExecutionReceipt, ToolRegistry, ToolResult,
};
use crate::CausalAnomalyKind::{RedundantValidation, RepeatedFailure, ReusableEvidence};
use crate::CausalProgressKind::{
    DependencyChanged, DistinctFailure, NewEvidence, ValidationGreen, WorkspaceChanged,
};
use crate::CausalShadowAction::{WouldReuse, WouldWarn};
use crate::{CausalAnomalyKind, CausalProgressKind, CausalShadowAction, OperatingMode};
use std::fs;

type Observed = Vec<GovernorObservation>;

/// Workspace temporário com registry e governor novos para um cenário.
struct Rig {
    root: TempRoot,
    registry: ToolRegistry,
    governor: CausalGovernor,
}

impl Rig {
    fn new(label: &str) -> Self {
        Self {
            root: TempRoot::new(label),
            registry: ToolRegistry::default(),
            governor: CausalGovernor::default(),
        }
    }

    /// Grava (ou sobrescreve) um arquivo na raiz do workspace.
    fn put(&self, name: &str, body: impl AsRef<[u8]>) {
        fs::write(self.root.join(name), body).expect("fixture");
    }

    fn prep(&self, tool: &str, arguments: &str) -> PreparedToolInvocation {
        self.registry
            .prepare_invocation(OperatingMode::Auto, &self.root, tool, arguments)
    }

    fn exec(&self, prepared: &PreparedToolInvocation) -> ToolExecutionOutcome {
        self.registry
            .execute_prepared_with_cancellation_and_progress(prepared, None, |_| {})
    }

    /// Observa antes, executa de verdade e observa depois; devolve o
    /// resultado e as observações posteriores.
    fn run(
        &mut self,
        prepared: &PreparedToolInvocation,
        call_id: &str,
    ) -> (ToolExecutionOutcome, Observed) {
        let (pending, _) = self
            .governor
            .observe_before_identified(prepared, "batch", call_id);
        let outcome = self.exec(prepared);
        let observed = self
            .governor
            .observe_after(pending, &outcome.result, &outcome.receipt);
        (outcome, observed)
    }

    /// Observa antes e depois com um resultado e um recibo já prontos.
    fn feed(
        &mut self,
        prepared: &PreparedToolInvocation,
        call_id: &str,
        result: &ToolResult,
        receipt: &ToolExecutionReceipt,
    ) -> Observed {
        observe(&mut self.governor, prepared, call_id, result, receipt)
    }
}

fn observe(
    governor: &mut CausalGovernor,
    prepared: &PreparedToolInvocation,
    call_id: &str,
    result: &ToolResult,
    receipt: &ToolExecutionReceipt,
) -> Observed {
    let (pending, _) = governor.observe_before_identified(prepared, "batch", call_id);
    governor.observe_after(pending, result, receipt)
}

/// Recibo sintético sem dependências nem mutações, na revisão dada.
fn synthetic_receipt(revision: u64) -> ToolExecutionReceipt {
    ToolExecutionReceipt::unobserved(revision, revision, 1)
}

fn shell_ok() -> ToolResult {
    ToolResult::ok("shell", "exit 0\n")
}

fn shell_failed() -> ToolResult {
    ToolResult::fail("shell", "exit 1\nfailed")
}

/// Conteúdo de uma linha maior que o limite de leitura (1 MiB).
fn oversized_line() -> String {
    "x".repeat(1024 * 1024 + 1)
}

fn has_progress(observed: &[GovernorObservation], kind: CausalProgressKind) -> bool {
    observed.iter().any(|observation| {
        matches!(observation,
            GovernorObservation::Progress { kind: seen, .. } if *seen == kind)
    })
}

fn any_progress(observed: &[GovernorObservation]) -> bool {
    observed
        .iter()
        .any(|observation| matches!(observation, GovernorObservation::Progress { .. }))
}

fn leads_with_progress(observed: &[GovernorObservation], kind: CausalProgressKind) -> bool {
    matches!(observed.first(),
        Some(GovernorObservation::Progress { kind: seen, .. }) if *seen == kind)
}

fn has_anomaly(observed: &[GovernorObservation], kind: CausalAnomalyKind) -> bool {
    observed.iter().any(|observation| {
        matches!(observation,
            GovernorObservation::Anomaly { kind: seen, .. } if *seen == kind)
    })
}

fn has_anomaly_with(
    observed: &[GovernorObservation],
    kind: CausalAnomalyKind,
    action: CausalShadowAction,
) -> bool {
    observed.iter().any(|observation| {
        matches!(observation,
            GovernorObservation::Anomaly { kind: seen, action: taken, .. }
                if *seen == kind && *taken == action)
    })
}

fn has_boundary(observed: &[GovernorObservation]) -> bool {
    observed
        .iter()
        .any(|observation| matches!(observation, GovernorObservation::Boundary { .. }))
}

/// Exige (e proíbe) trechos no snapshot, mostrando-o inteiro na falha.
fn assert_snapshot(snapshot: &str, contains: &[&str], absent: &[&str]) {
    for needle in contains {
        assert!(
            snapshot.contains(needle),
            "snapshot lacks {needle:?}:\n{snapshot}"
        );
    }
    for needle in absent {
        assert!(
            !snapshot.contains(needle),
            "snapshot must not contain {needle:?}:\n{snapshot}"
        );
    }
}

#[test]
fn structural_rejections_are_distinct_failures_without_uncertainty() {
    let mut rig = Rig::new("structural-rejection");
    rig.put("sample.txt", "stable\n");
    let invalid = [
        r#"{"path":"sample.txt","offset":0}"#,
        r#"{"path":"./sample.txt","offset":0}"#,
    ];
    let mut invalid_fingerprint = None;
    for (index, arguments) in invalid.into_iter().enumerate() {
        let prepared = rig.prep("read", arguments);
        assert!(prepared.structural_rejection);
        if let Some(expected) = &invalid_fingerprint {
            assert_eq!(&prepared.canonical_fingerprint, expected);
        } else {
            invalid_fingerprint = Some(prepared.canonical_fingerprint.clone());
        }
        let (pending, before) =
            rig.governor
                .observe_before_identified(&prepared, "batch", &format!("invalid-{index}"));
        assert!(before.is_empty(), "structural rejection is not uncertain");
        let outcome = rig.exec(&prepared);
        assert!(!outcome.result.success);
        let observations = rig
            .governor
            .observe_after(pending, &outcome.result, &outcome.receipt);
        if index == 0 {
            assert!(has_progress(&observations, DistinctFailure));
        } else {
            assert!(has_anomaly(&observations, RepeatedFailure));
        }
        assert_eq!(rig.governor.ledger.uncertainty_epoch, 0);
    }

    let valid = rig.prep("read", r#"{"path":"sample.txt","offset":1}"#);
    assert!(!valid.structural_rejection);
    assert!(valid.error.is_none());

    // Path containment and workspace-root failures depend on filesystem
    // state, so they retain the conservative unclassifiable boundary.
    let escape = rig.prep("read", r#"{"path":"../sample.txt","offset":1}"#);
    assert!(!escape.structural_rejection);
    assert_eq!(escape.error.as_deref(), Some("path escapes the workspace"));
    let missing_root = rig.root.join("missing-root");
    let unresolved = rig.registry.prepare_invocation(
        OperatingMode::Auto,
        &missing_root,
        "read",
        r#"{"path":"sample.txt","offset":1}"#,
    );
    assert!(!unresolved.structural_rejection);
    assert!(unresolved
        .error
        .as_deref()
        .is_some_and(|message| message.starts_with("workspace root cannot be resolved:")));
}

#[test]
fn receipts_drive_reuse_and_dependency_changes_without_governor_io() {
    let mut rig = Rig::new("receipts");
    rig.put("sample.txt", "stable\n");
    let arguments = r#"{"path":"sample.txt","max_lines":1}"#;

    let first = rig.prep("read", arguments);
    let (pending, before) = rig
        .governor
        .observe_before_identified(&first, "batch", "one");
    assert!(before.is_empty());
    let outcome = rig.exec(&first);
    let observations = rig
        .governor
        .observe_after(pending, &outcome.result, &outcome.receipt);
    assert!(has_progress(&observations, NewEvidence));

    let second = rig.prep("read", arguments);
    let (outcome, observations) = rig.run(&second, "two");
    assert!(has_anomaly_with(
        &observations,
        ReusableEvidence,
        WouldReuse
    ));

    // A repeated read can request a stop before a later operation in the
    // same batch observes a real dependency change.
    for _ in 0..2 {
        rig.feed(&second, "repeat", &outcome.result, &outcome.receipt);
    }

    rig.put("sample.txt", "changed\n");
    let third = rig.prep("read", arguments);
    let (_, observations) = rig.run(&third, "three");
    assert!(has_progress(&observations, DependencyChanged));
    rig.governor.finish_turn();
    assert!(
        !rig.governor.stop_requested(),
        "the batch made real progress"
    );
}

#[test]
fn admission_notes_do_not_turn_identical_search_evidence_into_progress() {
    let mut rig = Rig::new("admission-evidence");
    rig.put("sample.txt", "needle\n");
    for (index, context) in [10, 11, 12].into_iter().enumerate() {
        let prepared = rig.prep(
            "search",
            &serde_json::json!({"query":"needle", "context_lines": context}).to_string(),
        );
        let (outcome, observations) = rig.run(&prepared, &index.to_string());
        assert!(outcome.result.success);
        assert!(outcome
            .result
            .output
            .contains(&format!("context_lines {context} -> 3")));
        assert_eq!(
            any_progress(&observations),
            index == 0,
            "presentation-only changes cannot reset progress: {observations:?}",
        );
        if index > 0 {
            assert!(observations
                .iter()
                .any(|event| matches!(event, GovernorObservation::Anomaly { .. })));
        }
        rig.governor.finish_turn();
    }
    let content = ToolResult::ok("read", "[admission: literal file content]\nneedle");
    assert_eq!(
        super::evidence::evidence_output(&content, None),
        content.output
    );
}

#[test]
fn search_receipt_carries_workspace_evidence() {
    let mut rig = Rig::new("search-receipt");
    rig.put("sample.txt", "needle\n");
    rig.put("second.txt", "other\n");
    let search = r#"{"path":".","query":"needle"}"#;
    let prepared = rig.prep("search", search);

    let outcome = rig.exec(&prepared);

    assert!(outcome.result.success);
    assert_eq!(outcome.receipt.dependencies.len(), 1);
    assert!(outcome.receipt.bytes_read > 0);

    rig.feed(&prepared, "first", &outcome.result, &outcome.receipt);
    rig.put("sample.txt", "needle changed\n");
    let changed = rig.prep("search", search);
    let (_, observations) = rig.run(&changed, "changed");
    assert!(has_progress(&observations, DependencyChanged));

    let listed = rig.prep("list", r#"{"path":".","max_entries":1}"#);
    let (listed_outcome, observations) = rig.run(&listed, "list");
    assert_eq!(listed_outcome.receipt.dependencies.len(), 1);
    assert!(listed_outcome.receipt.bytes_read > 0);
    assert!(!has_boundary(&observations));

    let marker = "pass \"cursor\": \"";
    let cursor_start = listed_outcome.result.output.find(marker).expect("cursor") + marker.len();
    let cursor_tail = &listed_outcome.result.output[cursor_start..];
    let cursor = &cursor_tail[..cursor_tail.find('"').expect("cursor end")];
    let continued = rig.prep(
        "list",
        &serde_json::json!({"path": ".", "max_entries": 1, "cursor": cursor}).to_string(),
    );
    let continued_outcome = rig.exec(&continued);
    assert!(continued_outcome.result.success);
    assert_eq!(continued_outcome.receipt.dependencies.len(), 1);
    assert_eq!(continued_outcome.receipt.bytes_read, 0);
}

#[test]
fn successful_allowlisted_validation_stays_green_without_fake_dependencies() {
    let mut rig = Rig::new("validation");
    for arguments in [
        r#"{"command":"cargo clippy --fix --allow-dirty"}"#,
        r#"{"command":"cargo","args":["clippy","--fix","--allow-dirty"]}"#,
    ] {
        let fixing = rig.prep("shell", arguments);
        assert_eq!(
            fixing.spec.unwrap().effect_class,
            crate::tools::ToolEffectClass::PotentiallyVolatile,
            "automatic fixes must remain a serial mutation barrier"
        );
    }
    let prepared = rig.prep("shell", r#"{"command":"cargo","args":["check"]}"#);
    let green = shell_ok();
    let receipt = synthetic_receipt(0);

    let observations = rig.feed(&prepared, "validation", &green, &receipt);

    assert!(has_progress(&observations, ValidationGreen));

    let other = rig.prep("shell", r#"{"command":"cargo test"}"#);
    let other_receipt = synthetic_receipt(0);
    assert!(has_progress(
        &rig.feed(&other, "other-validation", &green, &other_receipt),
        ValidationGreen
    ));

    assert!(has_anomaly_with(
        &rig.feed(&prepared, "repeat-validation", &green, &receipt),
        RedundantValidation,
        WouldWarn
    ));

    let (delayed_pending, _) =
        rig.governor
            .observe_before_identified(&prepared, "batch", "delayed-validation");

    rig.put("changed.txt", "before");
    let mutation = rig.prep(
        "write",
        r#"{"path":"changed.txt","content":"after","expected":"before"}"#,
    );
    rig.run(&mutation, "mutation");
    rig.governor
        .observe_after(delayed_pending, &green, &receipt);
    let revision = rig.registry.workspace_revision();
    let post_mutation_receipt = synthetic_receipt(revision);
    assert!(has_progress(
        &rig.feed(&prepared, "post-mutation", &green, &post_mutation_receipt),
        ValidationGreen
    ));

    rig.put("changed.txt", "external");
    let read = rig.prep("read", r#"{"path":"changed.txt","max_lines":1}"#);
    let (_, observations) = rig.run(&read, "external-read");
    assert!(has_progress(&observations, DependencyChanged));

    let external_revision = rig.registry.workspace_revision();
    let after_external_receipt = synthetic_receipt(external_revision);
    assert!(has_progress(
        &rig.feed(&prepared, "after-external", &green, &after_external_receipt),
        ValidationGreen
    ));
}

#[test]
fn validation_finishing_after_a_mutation_does_not_certify_the_new_state() {
    let mut rig = Rig::new("validation-late");
    rig.put("changed.txt", "before");
    let prepared = rig.prep("shell", r#"{"command":"cargo","args":["check"]}"#);
    let green = shell_ok();
    let receipt = synthetic_receipt(0);

    let (started, _) =
        rig.governor
            .observe_before_identified(&prepared, "batch", "late-validation");
    let mutation = rig.prep(
        "write",
        r#"{"path":"changed.txt","content":"after","expected":"before"}"#,
    );
    let (outcome, _) = rig.run(&mutation, "mutation");
    assert!(outcome.result.success);
    assert!(rig.governor.ledger.workspace_revision > 0);

    let observations = rig.governor.observe_after(started, &green, &receipt);
    assert!(has_progress(&observations, ValidationGreen));
    assert!(
        !rig.governor.validations_satisfied(),
        "a validation that started before the mutation tested the old state"
    );
    assert_snapshot(
        &rig.governor.compaction_snapshot(1),
        &[
            "call_id=\"late-validation\"",
            "validation_revision=0",
            "current=false",
        ],
        &[],
    );

    let revision = rig.registry.workspace_revision();
    rig.feed(
        &prepared,
        "fresh-validation",
        &green,
        &synthetic_receipt(revision),
    );
    assert!(rig.governor.validations_satisfied());
}

#[test]
fn code_intel_elapsed_suffix_needs_digits_to_be_stripped() {
    use super::evidence::normalize_output;
    assert_eq!(
        normalize_output("code_intel", "symbol foo | 12ms\n", false),
        "symbol foo"
    );
    assert_eq!(
        normalize_output("code_intel", "symbol foo | ms\n", false),
        "symbol foo | ms"
    );
    assert_eq!(
        normalize_output("code_intel", "symbol foo | 1x2ms", false),
        "symbol foo | 1x2ms"
    );
    assert_eq!(
        normalize_output("read", "symbol foo | 12ms", false),
        "symbol foo | 12ms"
    );
}

/// The line-by-line normalization the allocation-free version replaced.
fn reference_normalize_output(tool_name: &str, output: &str, validation: bool) -> String {
    output
        .replace("\r\n", "\n")
        .lines()
        .map(str::trim_end)
        .map(|line| {
            if tool_name == "code_intel" {
                if let Some((prefix, elapsed)) = line.rsplit_once(" | ") {
                    if elapsed.strip_suffix("ms").is_some_and(|value| {
                        !value.is_empty() && value.chars().all(|c| c.is_ascii_digit())
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

#[test]
fn normalize_output_matches_the_reference_on_line_ending_edge_cases() {
    let inputs = [
        "",
        "\n",
        "\r\n",
        "  \n\t\r\n",
        "one",
        "one\r\ntwo\r\n",
        "one  \r\n\r\n\r\nthree\t\n\n\n",
        "\n\n  lead\nmid\n\ntail  ",
        "a\r\r\nb\rc\r",
        "\r",
        "sym | 12ms\r\nsym | ms\nsym | 3xms\nother | 45ms  \n",
        "   Finished dev in 0.5s\nCompiling x; finished in 2s\nexit 0\r\n",
        "  Finished`test` profile in 1.1s\ntrailing\n\n",
        "ação 日本語  \n\n  fim \u{a0}\n",
    ];
    for tool in ["read", "code_intel", "shell"] {
        for validation in [false, true] {
            for input in inputs {
                assert_eq!(
                    super::evidence::normalize_output(tool, input, validation),
                    reference_normalize_output(tool, input, validation),
                    "{tool} validation={validation} {input:?}"
                );
            }
        }
    }
}

#[test]
fn confidence_orders_from_low_to_high_and_keeps_its_wire_names() {
    use crate::CausalConfidence::{High, Low, Medium};
    assert!(Low < Medium && Medium < High);
    assert_eq!(High.min(Medium), Medium);
    assert_eq!(Low.min(High), Low);
    for (confidence, name) in [(Low, "low"), (Medium, "medium"), (High, "high")] {
        assert_eq!(
            serde_json::to_string(&confidence).unwrap(),
            format!("\"{name}\"")
        );
    }
}

#[test]
fn dependency_keys_are_shared_by_receipts_and_ledger_lookups() {
    use crate::tools::{dependency_key, DependencyKind};
    use std::path::Path;
    let mut rig = Rig::new("dependency-keys");
    rig.put("sample.txt", "stable\n");
    let prepared = rig.prep("read", r#"{"path":"sample.txt","max_lines":1}"#);
    let (outcome, _) = rig.run(&prepared, "read");
    let dependency = &outcome.receipt.dependencies[0];
    assert_eq!(
        dependency.key(),
        dependency_key(DependencyKind::File, &dependency.path)
    );
    assert!(rig.governor.ledger.observed.contains_key(&dependency.key()));
    assert_eq!(
        dependency_key(DependencyKind::Directory, Path::new("a\\b")),
        "directory:a/b"
    );
}

#[test]
fn bounded_maps_replace_known_keys_and_refuse_new_ones_when_full() {
    use super::insert_bounded;
    let mut map = std::collections::BTreeMap::new();
    assert!(insert_bounded(&mut map, "a".to_owned(), 1, 2));
    assert!(insert_bounded(&mut map, "b".to_owned(), 2, 2));
    assert!(!insert_bounded(&mut map, "c".to_owned(), 3, 2));
    assert!(insert_bounded(&mut map, "a".to_owned(), 9, 2));
    assert_eq!(map.get("a"), Some(&9));
    assert!(!map.contains_key("c"));
}

#[test]
fn pending_failures_are_bounded_and_overflow_is_counted() {
    let mut rig = Rig::new("failure-bound");
    let failed = |rig: &mut Rig, index: usize| {
        let prepared = rig.prep(
            "read",
            &serde_json::json!({"path": format!("missing-{index}.txt")}).to_string(),
        );
        let receipt = synthetic_receipt(0);
        rig.feed(
            &prepared,
            &format!("call-{index}"),
            &ToolResult::fail("read", "missing"),
            &receipt,
        );
    };
    for index in 0..300 {
        failed(&mut rig, index);
    }
    assert_eq!(rig.governor.ledger.pending_failures.len(), 256);
    assert_eq!(rig.governor.ledger.pending_failure_omitted, 44);
    failed(&mut rig, 0);
    assert_eq!(
        rig.governor.ledger.pending_failure_omitted, 44,
        "a known failure is replaced, not omitted"
    );
    assert!(rig
        .governor
        .compaction_snapshot(1)
        .contains("omitted_observations failures="));
}

#[test]
fn post_compaction_reacquisition_counts_the_same_stateful_call_once() {
    let mut rig = Rig::new("reacquisition");
    rig.put("state.txt", "stable\ntail\n");
    fn observe_read(rig: &mut Rig, call_id: &str, args: &str) {
        let prepared = rig.prep("read", args);
        let (outcome, _) = rig.run(&prepared, call_id);
        assert!(outcome.result.success);
    }
    let args = r#"{"path":"state.txt","offset":1,"max_lines":10}"#;
    observe_read(&mut rig, "before-compaction", args);
    assert!(rig
        .governor
        .take_post_compaction_reacquisitions()
        .is_empty());

    rig.governor.forget_compacted_evidence();
    observe_read(&mut rig, "after-compaction", args);
    let drained = rig.governor.take_post_compaction_reacquisitions();
    assert_eq!(drained.len(), 1);
    assert!(drained.contains("after-compaction"));
    observe_read(&mut rig, "after-compaction-again", args);
    assert!(rig
        .governor
        .take_post_compaction_reacquisitions()
        .is_empty());
    observe_read(
        &mut rig,
        "different-call",
        r#"{"path":"state.txt","offset":1,"max_lines":2}"#,
    );
    assert!(rig
        .governor
        .take_post_compaction_reacquisitions()
        .is_empty());
}

#[test]
fn validation_outcomes_require_each_failed_command_to_recover() {
    let mut rig = Rig::new("validation-outcomes");
    let record = |rig: &mut Rig, command: &str, success: bool| {
        let prepared = rig.prep(
            "shell",
            &serde_json::json!({"command": command}).to_string(),
        );
        let result = if success { shell_ok() } else { shell_failed() };
        rig.feed(&prepared, command, &result, &synthetic_receipt(0));
    };
    assert!(!rig.governor.validations_satisfied());
    record(&mut rig, "cargo check", true);
    assert!(rig.governor.validations_satisfied());
    record(&mut rig, "cargo test", false);
    assert!(!rig.governor.validations_satisfied());
    record(&mut rig, "cargo check", true);
    assert!(
        !rig.governor.validations_satisfied(),
        "a different check cannot resolve failure"
    );
    rig.governor.forget_compacted_evidence();
    assert!(
        !rig.governor.validations_satisfied(),
        "compaction must retain unresolved validation"
    );
    record(&mut rig, "cargo test", true);
    assert!(rig.governor.validations_satisfied());
    record(&mut rig, "cargo test", false);
    record(&mut rig, "cargo test", true);
    record(&mut rig, "cargo test", false);
    assert!(
        !rig.governor.validations_satisfied(),
        "previously seen failures still invalidate success"
    );
    record(&mut rig, "cargo test", true);
    rig.governor.ledger.workspace_revision += 1;
    assert!(
        !rig.governor.validations_satisfied(),
        "green evidence predates the mutation"
    );
    record(&mut rig, "cargo test", true);
    assert!(rig.governor.validations_satisfied());
    rig.governor.ledger.uncertainty_epoch += 1;
    assert!(
        !rig.governor.validations_satisfied(),
        "volatile effects invalidate old evidence"
    );
}

#[test]
fn compaction_snapshot_marks_validation_stale_after_mutation() {
    let mut rig = Rig::new("compaction-validation-stale");
    rig.put("changed.txt", "before");
    let prepared = rig.prep("shell", r#"{"command":"cargo","args":["check"]}"#);
    let green = shell_ok();
    rig.feed(&prepared, "validation-1", &green, &synthetic_receipt(0));

    let fresh = rig.governor.compaction_snapshot(41);
    assert_snapshot(
        &fresh,
        &[
            "scope=compaction run_start_seq=41",
            "call_id=\"validation-1\"",
            "success=true",
            "validation_revision=0",
            "validation_epoch=0",
            "current=true",
        ],
        &[],
    );

    let mutation = rig.prep(
        "write",
        r#"{"path":"changed.txt","content":"after","expected":"before"}"#,
    );
    let (mutation_outcome, _) = rig.run(&mutation, "mutation-1");
    assert!(mutation_outcome.result.success);

    let stale = rig.governor.compaction_snapshot(42);
    let mutation_revision = format!("revision={}", mutation_outcome.receipt.revision_after);
    assert_snapshot(
        &stale,
        &[
            "call_id=\"validation-1\"",
            "validation_revision=0",
            "current=false",
            "mutation path=",
            "changed.txt",
            &mutation_revision,
        ],
        &[],
    );

    rig.governor.forget_compacted_evidence();
    let after_forget = rig.governor.compaction_snapshot(43);
    assert_snapshot(
        &after_forget,
        &["success=true", "current=false", "changed.txt"],
        &[],
    );

    let revision = rig.registry.workspace_revision();
    rig.feed(
        &prepared,
        "validation-2",
        &green,
        &synthetic_receipt(revision),
    );
    let resolved = rig.governor.compaction_snapshot(44);
    let validation_revision = format!("validation_revision={revision}");
    assert_snapshot(
        &resolved,
        &[
            "call_id=\"validation-2\"",
            &validation_revision,
            "current=true",
        ],
        &[],
    );
}

#[test]
fn compaction_snapshot_keeps_failed_validation_until_exact_success() {
    let mut rig = Rig::new("compaction-validation-failure");
    let failed = rig.prep("shell", r#"{"command":"cargo","args":["check"]}"#);
    let failed_receipt = synthetic_receipt(0);
    rig.feed(&failed, "check-failed", &shell_failed(), &failed_receipt);
    let initial = rig.governor.compaction_snapshot(50);
    assert_snapshot(
        &initial,
        &["call_id=\"check-failed\"", "success=false", "current=true"],
        &[],
    );

    rig.governor.forget_compacted_evidence();
    let retained = rig.governor.compaction_snapshot(51);
    assert_snapshot(
        &retained,
        &["call_id=\"check-failed\"", "success=false"],
        &[],
    );

    let different = rig.prep("shell", r#"{"command":"cargo","args":["test"]}"#);
    let success_result = shell_ok();
    rig.feed(
        &different,
        "other-success",
        &success_result,
        &synthetic_receipt(0),
    );
    let unresolved = rig.governor.compaction_snapshot(52);
    assert_snapshot(
        &unresolved,
        &["call_id=\"check-failed\"", "success=false"],
        &[],
    );

    rig.feed(&failed, "check-fixed", &success_result, &failed_receipt);
    let resolved = rig.governor.compaction_snapshot(53);
    assert_snapshot(
        &resolved,
        &["call_id=\"check-fixed\"", "success=true"],
        &["call_id=\"check-failed\""],
    );
    assert!(rig.governor.validations_satisfied());
}

#[test]
fn compaction_snapshot_keeps_non_validation_failure_until_exact_success() {
    let mut rig = Rig::new("compaction-failure");
    rig.put("large-line.txt", oversized_line());
    let prepared = rig.prep("read", r#"{"path":"large-line.txt","max_lines":1}"#);
    let (failed, _) = rig.run(&prepared, "read-failed");
    assert!(!failed.result.success);
    assert_snapshot(
        &rig.governor.compaction_snapshot(55),
        &["failure tool=\"read\" call_id=\"read-failed\""],
        &[],
    );

    rig.governor.forget_compacted_evidence();
    assert_snapshot(
        &rig.governor.compaction_snapshot(56),
        &["pending=true"],
        &[],
    );

    rig.put("large-line.txt", "small\n");
    let (resolved, _) = rig.run(&prepared, "read-fixed");
    assert!(resolved.result.success);
    assert!(rig.governor.compaction_snapshot(57).is_empty());

    // A rejected validation never reaches the validation ledger, so its
    // failure must remain visible in the generic failure records.
    let rejected = rig.prep("shell", r#"{"command":"cargo test","bogus":true}"#);
    let (pending, _) = rig
        .governor
        .observe_before_identified(&rejected, "batch", "rejected-test");
    assert!(pending.structural_rejection);
    let outcome = rig.exec(&rejected);
    rig.governor
        .observe_after(pending, &outcome.result, &outcome.receipt);
    assert_snapshot(
        &rig.governor.compaction_snapshot(57),
        &["failure tool=\"shell\" call_id=\"rejected-test\""],
        &[],
    );
}

#[test]
fn compaction_snapshot_records_only_changed_mutation_paths() {
    let mut rig = Rig::new("compaction-mutations");
    rig.put("same.txt", "same");
    rig.put("changed.txt", "before");

    let unchanged = rig.prep(
        "write",
        r#"{"path":"same.txt","content":"same","expected":"same"}"#,
    );
    let (unchanged_outcome, _) = rig.run(&unchanged, "same");
    assert!(unchanged_outcome.result.success);
    assert!(unchanged_outcome
        .receipt
        .mutations
        .iter()
        .all(|mutation| !mutation.changed()));

    let changed = rig.prep(
        "write",
        r#"{"path":"changed.txt","content":"after","expected":"before"}"#,
    );
    let (changed_outcome, _) = rig.run(&changed, "changed");
    assert!(changed_outcome.result.success);
    assert!(changed_outcome
        .receipt
        .mutations
        .iter()
        .any(|mutation| mutation.changed()));

    let snapshot = rig.governor.compaction_snapshot(60);
    assert_snapshot(&snapshot, &["changed.txt"], &["same.txt"]);
}

#[test]
fn compaction_snapshot_is_deterministic_bounded_and_epoch_aware() {
    let mut governor = CausalGovernor::default();
    governor.ledger.workspace_revision = 7;
    governor.ledger.validations.insert(
        "validation-key".into(),
        ValidationResult {
            success: true,
            tool_name: "tool\nname".into(),
            call_id: "call\"id".into(),
            workspace_revision: 7,
            uncertainty_epoch: 0,
        },
    );
    let fresh = governor.compaction_snapshot(70);
    assert_snapshot(&fresh, &["tool\\nname", "call\\\"id", "current=true"], &[]);
    assert_eq!(fresh, governor.compaction_snapshot(70));

    governor.ledger.uncertainty_epoch = 1;
    let stale = governor.compaction_snapshot(70);
    assert_snapshot(&stale, &["uncertainty_epoch=1", "current=false"], &[]);

    for index in 0..64 {
        governor.ledger.validations.insert(
            format!("{index:064x}"),
            ValidationResult {
                success: index % 2 == 0,
                tool_name: "shell".into(),
                call_id: format!("call-{index}"),
                workspace_revision: 7,
                uncertainty_epoch: 1,
            },
        );
    }
    let bounded = governor.compaction_snapshot(71);
    assert!(bounded.len() <= MAX_COMPACTION_SNAPSHOT_BYTES);
    assert_snapshot(
        &bounded,
        &["omitted_observations failures=0 validations="],
        &[],
    );
}

#[test]
fn non_validation_commands_cannot_satisfy_task_validation() {
    let mut rig = Rig::new("help-not-validation");
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
        let prepared = rig.prep("shell", &arguments.to_string());
        rig.governor = CausalGovernor::default();
        rig.feed(
            &prepared,
            "help",
            &ToolResult::ok("shell", "exit 0\nUsage: cargo ...\n"),
            &synthetic_receipt(0),
        );
        assert!(
            !rig.governor.validations_satisfied(),
            "command is not validation: {arguments}"
        );
    }
}

#[test]
fn concurrent_source_revision_is_not_counted_twice() {
    let mut rig = Rig::new("concurrent-revision");
    rig.put("sample.txt", "before\n");
    let prepared = rig.prep("read", r#"{"path":"sample.txt","max_lines":1}"#);
    rig.run(&prepared, "initial");

    rig.put("sample.txt", "after\n");
    let (changed_pending, _) = rig
        .governor
        .observe_before_identified(&prepared, "batch", "changed");
    let mut changed = rig.exec(&prepared);
    changed.receipt.revision_after = changed.receipt.revision_before.saturating_add(1);

    rig.governor
        .observe_after(changed_pending, &changed.result, &changed.receipt);

    assert_eq!(
        rig.governor.ledger.workspace_revision,
        changed.receipt.revision_after
    );
}

#[test]
fn unchanged_write_receipt_does_not_claim_workspace_progress() {
    let mut rig = Rig::new("write");
    rig.put("same.txt", "same");
    let prepared = rig.prep(
        "write",
        r#"{"path":"same.txt","content":"same","expected":"same"}"#,
    );
    let (outcome, observations) = rig.run(&prepared, "write");
    assert!(outcome.result.success);
    assert_eq!(
        outcome.receipt.revision_before,
        outcome.receipt.revision_after
    );
    assert!(observations.is_empty());
}

#[test]
fn fused_validation_is_recorded_after_its_mutation_revision() {
    let mut rig = Rig::new("fused-validation");
    fs::create_dir_all(rig.root.join("src")).unwrap();
    rig.put(
        "Cargo.toml",
        "[package]\nname = \"fused_validation_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    rig.put("src/lib.rs", "pub fn answer() -> u8 {\n    42\n}\n");
    rig.put("README.md", "before\n");
    let prepared = rig.prep(
        "write",
        &serde_json::json!({
            "path": "README.md",
            "content": "after\n",
            "expected": "before\n",
            "then_run": {"command": "cargo", "args": ["fmt", "--check"]}
        })
        .to_string(),
    );
    assert!(prepared.error.is_none(), "{:?}", prepared.error);
    let (outcome, observations) = rig.run(&prepared, "edit");
    assert!(outcome.result.success, "{}", outcome.result.output);
    assert!(outcome.receipt.revision_after > outcome.receipt.revision_before);
    assert!(leads_with_progress(&observations, WorkspaceChanged));
    assert!(observations.iter().any(|observation| matches!(observation,
        GovernorObservation::Progress {
            kind: ValidationGreen, call_id, ..
        } if call_id == "edit:then_run"
    )));
    assert!(rig.governor.validations_satisfied());
    assert!(rig
        .governor
        .compaction_snapshot(1)
        .contains("validation_revision=1"));

    let mut cancelled = outcome;
    super::super::mark_cancelled_tool_outcome(&mut cancelled);
    assert!(!cancelled.result.success);
    assert!(!cancelled.receipt.fused_shell.as_ref().unwrap().1.success);
    let mut cancelled_governor = CausalGovernor::default();
    let cancelled_observations = observe(
        &mut cancelled_governor,
        &prepared,
        "cancelled",
        &cancelled.result,
        &cancelled.receipt,
    );
    assert!(has_progress(&cancelled_observations, WorkspaceChanged));
    assert!(!has_progress(&cancelled_observations, ValidationGreen));
    assert!(!cancelled_governor.validations_satisfied());

    rig.put("src/lib.rs", "pub fn answer() -> u8 { 42 }\n");
    let retry = rig.prep(
        "write",
        &serde_json::json!({
            "path": "README.md",
            "content": "after again\n",
            "expected": "after\n",
            "then_run": {"command": "cargo", "args": ["fmt", "--check"]}
        })
        .to_string(),
    );
    let (failed, observations) = rig.run(&retry, "retry");
    assert!(!failed.result.success);
    assert_eq!(
        fs::read_to_string(rig.root.join("README.md")).unwrap(),
        "after again\n"
    );
    assert!(leads_with_progress(&observations, WorkspaceChanged));
    assert!(!rig.governor.validations_satisfied());
    assert!(rig
        .governor
        .compaction_snapshot(2)
        .contains("validation_revision=2"));
}

#[test]
fn out_of_order_receipts_do_not_double_count_or_restore_stale_stamps() {
    let mut rig = Rig::new("write-read");
    rig.put("sample.txt", "before");
    rig.put("other.txt", "stable");
    let read = rig.prep("read", r#"{"path":"sample.txt","max_lines":1}"#);
    let write = rig.prep(
        "write",
        r#"{"path":"sample.txt","content":"after","expected":"before"}"#,
    );
    let write_again = rig.prep(
        "write",
        r#"{"path":"sample.txt","content":"after-again","expected":"after"}"#,
    );
    let other_read = rig.prep("read", r#"{"path":"other.txt","max_lines":1}"#);

    rig.run(&read, "initial-read");

    let (write_pending, _) = rig
        .governor
        .observe_before_identified(&write, "batch", "write-1");
    let write_outcome = rig.exec(&write);
    let first_revision = write_outcome.receipt.revision_after;
    #[cfg(windows)]
    assert!(write_outcome.receipt.mutations[0].after.file_id.is_some());
    let (stale_read_pending, _) =
        rig.governor
            .observe_before_identified(&read, "batch", "read-revision-1");
    let stale_read = rig.exec(&read);
    assert_eq!(stale_read.receipt.revision_before, first_revision);

    rig.run(&other_read, "other-read");

    let (write_again_pending, _) =
        rig.governor
            .observe_before_identified(&write_again, "batch", "write-2");
    let write_again_outcome = rig.exec(&write_again);
    let second_revision = write_again_outcome.receipt.revision_after;
    let (current_read_pending, _) =
        rig.governor
            .observe_before_identified(&read, "batch", "read-revision-2");
    let current_read = rig.exec(&read);
    assert_eq!(current_read.receipt.revision_before, second_revision);
    #[cfg(windows)]
    assert!(current_read.receipt.dependencies[0].stamp.file_id.is_some());
    let observations = rig.governor.observe_after(
        current_read_pending,
        &current_read.result,
        &current_read.receipt,
    );
    assert!(has_progress(&observations, DependencyChanged));
    assert_eq!(rig.governor.ledger.workspace_revision, second_revision);

    let stale_observations =
        rig.governor
            .observe_after(stale_read_pending, &stale_read.result, &stale_read.receipt);
    assert!(!any_progress(&stale_observations));
    assert_eq!(rig.governor.ledger.workspace_revision, second_revision);

    rig.governor
        .observe_after(write_pending, &write_outcome.result, &write_outcome.receipt);
    rig.governor.observe_after(
        write_again_pending,
        &write_again_outcome.result,
        &write_again_outcome.receipt,
    );
    assert_eq!(rig.governor.ledger.workspace_revision, second_revision);

    let (_, observations) = rig.run(&read, "final-read");
    assert!(!has_progress(&observations, DependencyChanged));
}

#[test]
fn failed_tools_keep_observed_dependencies_and_bytes() {
    let rig = Rig::new("failed-read");
    rig.put("large-line.txt", oversized_line());
    rig.put("stale.txt", "actual");
    rig.put("ambiguous.txt", "x x");
    rig.put("invalid.txt", b"valid\xffinvalid");
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    // `deps` e `bytes` ausentes não são verificados; `bytes: None` exige
    // apenas bytes lidos maiores que zero.
    let check = |tool: &str,
                 arguments: &str,
                 cancellation: Option<&CancellationToken>,
                 success: bool,
                 deps: Option<usize>,
                 bytes: Option<u64>|
     -> ToolExecutionOutcome {
        let prepared = rig.prep(tool, arguments);
        let outcome = rig
            .registry
            .execute_prepared_with_cancellation_and_progress(&prepared, cancellation, |_| {});
        assert_eq!(outcome.result.success, success, "{tool} {arguments}");
        if let Some(deps) = deps {
            assert_eq!(
                outcome.receipt.dependencies.len(),
                deps,
                "{tool} {arguments}"
            );
        }
        match bytes {
            Some(bytes) => assert_eq!(outcome.receipt.bytes_read, bytes, "{tool} {arguments}"),
            None => assert!(outcome.receipt.bytes_read > 0, "{tool} {arguments}"),
        }
        outcome
    };

    check(
        "read",
        r#"{"path":"large-line.txt","max_lines":1}"#,
        None,
        false,
        Some(1),
        None,
    );
    check(
        "write",
        r#"{"path":"stale.txt","content":"new","expected":"old"}"#,
        None,
        false,
        Some(1),
        Some(6),
    );
    let ambiguous = check(
        "patch",
        r#"{"path":"ambiguous.txt","expected":"x","replacement":"y"}"#,
        None,
        false,
        Some(1),
        Some(3),
    );
    assert!(ambiguous.receipt.mutations.is_empty());
    check(
        "read",
        r#"{"path":"invalid.txt","max_lines":1}"#,
        None,
        false,
        None,
        Some(13),
    );
    check(
        "list",
        r#"{"path":"."}"#,
        Some(&cancellation),
        false,
        Some(1),
        Some(0),
    );
    // Invalid UTF-8 skips the file instead of failing the search, but the
    // observed dependency and bytes are still reported.
    check(
        "search",
        r#"{"path":"invalid.txt","query":"valid"}"#,
        None,
        true,
        Some(1),
        Some(13),
    );
}

#[test]
fn canonical_defaults_share_a_prepared_fingerprint() {
    let mut rig = Rig::new("canonical");
    rig.put("same.txt", "same");
    let implicit = rig.prep("read", r#"{"path":"same.txt"}"#);
    // Path and offset normalize to the same call, so the fingerprint
    // (and the governor's identity) still matches.
    let normalized = rig.prep("read", r#"{"offset":1,"path":"./same.txt"}"#);
    assert_eq!(
        implicit.canonical_fingerprint,
        normalized.canonical_fingerprint
    );
    // An explicit max_lines is a different call from an omitted one: the
    // read service only widens the omitted default, so identities differ.
    let explicit = rig.prep(
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
    let (left, _) = rig.governor.observe_before_identified(&implicit, "b", "1");
    let (right, _) = rig
        .governor
        .observe_before_identified(&normalized, "b", "2");
    assert_eq!(left.call_fingerprint, right.call_fingerprint);
    let (distinct, _) = rig.governor.observe_before_identified(&explicit, "b", "3");
    assert_ne!(left.call_fingerprint, distinct.call_fingerprint);

    let optional_code_intel_path = rig.prep(
        "code_intel",
        r#"{"action":"symbol","path":"","query":"same"}"#,
    );
    assert!(optional_code_intel_path.error.is_none());
    assert!(optional_code_intel_path.target_paths.is_empty());
}
