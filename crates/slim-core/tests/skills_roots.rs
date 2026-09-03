use std::fs;
use std::path::{Path, PathBuf};

use slim_core::skills::{discover, read_body, read_metadata, SkillRoot};
use slim_core::OperatingMode;

fn temp_dir() -> PathBuf {
    let unique = format!(
        "slim-skills-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    );
    std::env::temp_dir().join(unique)
}

fn write_skill(root: &Path, name: &str, description: &str, body: &str) -> PathBuf {
    let path = root.join(name);
    fs::create_dir_all(&path).expect("mkdir");
    fs::write(
        path.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}"),
    )
    .expect("skill");
    path
}

#[test]
fn more_specific_root_wins_and_shadowing_is_visible() {
    let root = temp_dir();
    let global = root.join("global");
    let project = root.join("project");
    let cli = root.join("cli");
    let global_skill = write_skill(&global, "same", "global", "global body");
    let _project_skill = write_skill(&project, "same", "project", "project body");
    let cli_skill = write_skill(&cli, "same", "cli", "cli body");
    let unique_skill = write_skill(&project, "unique", "unique", "unique body");

    let result = discover(&[
        SkillRoot::new(cli, 0),
        SkillRoot::new(project, 1),
        SkillRoot::new(global, 3),
    ])
    .expect("discover");
    assert_eq!(result.active("same").expect("active").path, cli_skill);
    assert_eq!(result.active("unique").expect("unique").path, unique_skill);
    assert_eq!(result.shadowed("same").len(), 2);
    assert!(result
        .shadowed("same")
        .iter()
        .any(|entry| entry.path == global_skill));

    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn metadata_is_loaded_without_loading_body_and_invalid_skill_is_diagnostic() {
    let root = temp_dir();
    let valid = write_skill(&root, "valid", "desc", "body");
    fs::create_dir_all(root.join("invalid")).expect("mkdir");
    fs::write(root.join("invalid").join("SKILL.md"), "not frontmatter").expect("skill");

    let result = discover(&[SkillRoot::new(root.clone(), 0)]).expect("discover");
    let entry = result.active("valid").expect("valid");
    assert_eq!(entry.metadata.description, "desc");
    assert_eq!(
        read_metadata(valid.join("SKILL.md"))
            .expect("metadata")
            .name,
        "valid"
    );
    assert_eq!(read_body(valid.join("SKILL.md")).expect("body"), "body");
    assert_eq!(result.warnings.len(), 1);

    let _ = OperatingMode::Auto;
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn metadata_does_not_decode_the_skill_body() {
    let root = temp_dir();
    let skill = root.join("binary-body");
    fs::create_dir_all(&skill).expect("mkdir");
    fs::write(
        skill.join("SKILL.md"),
        b"---\nname: binary-body\ndescription: valid metadata\n---\n\xff",
    )
    .expect("skill");

    let metadata = read_metadata(skill.join("SKILL.md")).expect("metadata");

    assert_eq!(metadata.name, "binary-body");
    assert_eq!(metadata.description, "valid metadata");
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn skill_body_has_a_bounded_read_budget() {
    let root = temp_dir();
    let skill = write_skill(&root, "large-body", "desc", &"x".repeat(1024 * 1024 + 1));

    let error = read_body(skill.join("SKILL.md")).expect_err("oversized body");

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("skill file exceeds"));
    fs::remove_dir_all(root).expect("cleanup");
}
