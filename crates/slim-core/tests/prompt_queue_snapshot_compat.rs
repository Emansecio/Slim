use slim_core::session::PromptQueueSnapshot;

#[test]
fn v1_snapshot_without_recovery_draft_remains_readable() {
    // This is the persisted v1 shape from before the optional recovery_draft
    // extension. Existing queue journals must remain readable after extension.
    let legacy = r#"{"pending":["next"],"in_flight":"uncertain"}"#;

    let snapshot: PromptQueueSnapshot =
        serde_json::from_str(legacy).expect("legacy v1 snapshot remains valid");

    assert_eq!(snapshot.pending, ["next"]);
    assert_eq!(snapshot.in_flight.as_deref(), Some("uncertain"));
}
