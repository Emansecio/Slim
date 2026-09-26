use super::{resolve_slash_skill_command, SlashSkillCommand, MAX_SLASH_SKILL_BODY_BYTES};

#[test]
fn slash_skill_metadata_is_lazy_and_body_loads_only_for_a_task() {
    let root = std::env::temp_dir().join(format!(
        "slim-slash-skill-{}-{}",
        std::process::id(),
        super::system_time_nanos(std::time::SystemTime::now())
    ));
    let skill_dir = root.join(".slim/skills/review-code");
    let skill_file = skill_dir.join("SKILL.md");
    std::fs::create_dir_all(&skill_dir).expect("skill directory");
    let native_collision = root.join(".slim/skills/models");
    std::fs::create_dir_all(&native_collision).expect("collision directory");
    std::fs::write(
        native_collision.join("SKILL.md"),
        "---\nname: models\ndescription: Must not shadow the native command\n---\nIgnored.\n",
    )
    .expect("collision fixture");

    let mut metadata_with_invalid_body =
        b"---\nname: review-code\ndescription: Review code\n---\n".to_vec();
    metadata_with_invalid_body.push(0xff);
    std::fs::write(&skill_file, metadata_with_invalid_body).expect("lazy skill fixture");

    assert!(matches!(
        resolve_slash_skill_command(&root, "/models"),
        Ok(None)
    ));
    assert!(matches!(
        resolve_slash_skill_command(&root, "/review-code"),
        Ok(Some(SlashSkillCommand::Selected { name })) if name == "review-code"
    ));

    std::fs::write(
        &skill_file,
        "---\nname: review-code\ndescription: Review code\n---\nInspect the change carefully.\n",
    )
    .expect("invokable skill fixture");
    assert!(matches!(
        resolve_slash_skill_command(&root, "/review-code check this"),
        Ok(Some(SlashSkillCommand::Invoke { name, body, source }))
            if name == "review-code"
                && body == "Inspect the change carefully.\n"
                && source == skill_file
    ));
    assert!(matches!(
        resolve_slash_skill_command(&root, "please /review-code check this"),
        Ok(Some(SlashSkillCommand::Invoke { name, .. })) if name == "review-code"
    ));

    std::fs::write(
        &skill_file,
        format!(
            "---\nname: review-code\ndescription: Review code\n---\n{}",
            "x".repeat(MAX_SLASH_SKILL_BODY_BYTES)
        ),
    )
    .expect("maximum-size skill fixture");
    assert!(matches!(
        resolve_slash_skill_command(&root, "/review-code check this"),
        Ok(Some(SlashSkillCommand::Invoke { body, .. }))
            if body.len() == MAX_SLASH_SKILL_BODY_BYTES
    ));

    std::fs::write(
        &skill_file,
        format!(
            "---\nname: review-code\ndescription: Review code\n---\n{}",
            "x".repeat(MAX_SLASH_SKILL_BODY_BYTES + 1)
        ),
    )
    .expect("oversized skill fixture");
    let error = match resolve_slash_skill_command(&root, "/review-code check this") {
        Err(error) => error,
        Ok(_) => panic!("oversized skill must be rejected"),
    };
    assert!(error.contains("slash limit"), "{error}");
    assert!(matches!(
        resolve_slash_skill_command(&root, "/missing check this"),
        Ok(None)
    ));

    let _ = std::fs::remove_dir_all(&root);
}
