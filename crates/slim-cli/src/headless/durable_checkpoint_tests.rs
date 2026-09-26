use slim_core::ProviderMessage;

use super::durable_checkpoint_anchor;

#[test]
fn checkpoint_anchor_requires_an_exact_durable_prefix() {
    let messages = vec![
        ProviderMessage::user("root"),
        ProviderMessage::assistant("answer", Vec::new()),
        ProviderMessage::user("next"),
    ];
    let entry_ids = vec![Some("u1".into()), Some("a1".into()), Some("u2".into())];
    let fingerprint = slim_core::context::compaction_prefix_fingerprint(&messages[..2]);

    assert_eq!(
        durable_checkpoint_anchor(&messages, &entry_ids, 2, &fingerprint).as_deref(),
        Some("u2")
    );
    let compacted_ids = [Some("u1".into()), None, Some("u2".into())];
    assert_eq!(
        durable_checkpoint_anchor(&messages, &compacted_ids, 2, &fingerprint).as_deref(),
        Some("u2")
    );
    assert!(durable_checkpoint_anchor(
        &messages,
        &compacted_ids,
        1,
        &slim_core::context::compaction_prefix_fingerprint(&messages[..1]),
    )
    .is_none());
    assert!(
        durable_checkpoint_anchor(&messages, &entry_ids, messages.len(), &fingerprint,).is_none()
    );
}
