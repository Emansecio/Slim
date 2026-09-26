use slim_core::provider::ProviderError;

use super::{
    skill_user_message_prefix, validated_skill_user_prefix, SkillInstructions,
    MAX_SLASH_SKILL_BODY_BYTES,
};

#[test]
fn invoked_skill_prefix_is_suitable_for_the_first_user_message() {
    let skill = SkillInstructions {
        name: "review-code".into(),
        body: "Inspect the change carefully.".into(),
        source: "D:/Slim/.slim/skills/review-code/SKILL.md".into(),
    };
    let prefix = skill_user_message_prefix(&skill);
    let user_text = format!("{prefix}Fix this PR.");

    assert!(prefix.starts_with("[Skill: review-code]"));
    assert!(prefix.contains("D:/Slim/.slim/skills/review-code/SKILL.md"));
    assert!(prefix.contains(&skill.body));
    assert!(user_text.ends_with("Fix this PR."));
}

#[test]
fn invoked_skill_rejects_a_user_prefix_above_the_context_safe_limit() {
    let skill = SkillInstructions {
        name: "review-code".into(),
        body: "x".repeat(MAX_SLASH_SKILL_BODY_BYTES),
        source: "p".repeat(3_000).into(),
    };

    let error = validated_skill_user_prefix(&skill).expect_err("oversized prefix");

    assert!(matches!(
        error,
        ProviderError::InvalidResponse { message }
            if message.contains("context-safe limit")
    ));
}
