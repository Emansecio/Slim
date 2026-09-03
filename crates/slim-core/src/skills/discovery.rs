use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::metadata::{read_metadata, SkillMetadata};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillRoot {
    pub path: PathBuf,
    pub priority: u8,
}

impl SkillRoot {
    pub fn new(path: impl Into<PathBuf>, priority: u8) -> Self {
        Self {
            path: path.into(),
            priority,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillEntry {
    pub name: String,
    pub path: PathBuf,
    pub metadata: SkillMetadata,
}

#[derive(Clone, Debug, Default)]
pub struct DiscoveryResult {
    active_entries: Vec<SkillEntry>,
    shadowed_entries: Vec<SkillEntry>,
    pub warnings: Vec<String>,
}

impl DiscoveryResult {
    pub fn active(&self, name: &str) -> Option<&SkillEntry> {
        self.active_entries.iter().find(|entry| entry.name == name)
    }

    pub fn shadowed(&self, name: &str) -> Vec<&SkillEntry> {
        self.shadowed_entries
            .iter()
            .filter(|entry| entry.name == name)
            .collect()
    }

    pub fn active_entries(&self) -> &[SkillEntry] {
        &self.active_entries
    }
}

pub fn discover(roots: &[SkillRoot]) -> io::Result<DiscoveryResult> {
    let mut roots = roots.to_vec();
    roots.sort_by_key(|root| root.priority);
    let mut result = DiscoveryResult::default();
    for root in roots {
        if !root.path.exists() {
            continue;
        }
        for entry in fs::read_dir(&root.path)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path();
            let skill_file = path.join("SKILL.md");
            if !skill_file.is_file() {
                continue;
            }
            let metadata = match read_metadata(&skill_file) {
                Ok(metadata) => metadata,
                Err(error) => {
                    result
                        .warnings
                        .push(format!("{}: {error}", skill_file.display()));
                    continue;
                }
            };
            let skill = SkillEntry {
                name: metadata.name.clone(),
                path,
                metadata,
            };
            if result.active(&skill.name).is_some() {
                result.shadowed_entries.push(skill);
            } else {
                result.active_entries.push(skill);
            }
        }
    }
    result
        .active_entries
        .sort_by(|left, right| left.name.cmp(&right.name));
    result
        .shadowed_entries
        .sort_by(|left, right| left.name.cmp(&right.name));
    Ok(result)
}

/// Discovers the native skill roots visible to a Slim run in `cwd`.
/// Only frontmatter metadata is read; `SKILL.md` bodies remain lazy.
pub fn discover_workspace(cwd: impl AsRef<Path>) -> io::Result<DiscoveryResult> {
    let profile = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    discover_workspace_with_profile(cwd.as_ref(), profile.as_deref())
}

fn discover_workspace_with_profile(
    cwd: &Path,
    profile: Option<&Path>,
) -> io::Result<DiscoveryResult> {
    let mut roots = vec![
        SkillRoot::new(cwd.join(".slim").join("skills"), 0),
        SkillRoot::new(cwd.join(".claude").join("skills"), 1),
    ];
    if let Some(profile) = profile {
        let global = profile.join(".slim").join("skills");
        if !roots.iter().any(|root| root.path == global) {
            roots.push(SkillRoot::new(global, 10));
        }
    }
    roots.retain(|root| root.path.is_dir());
    discover(&roots)
}

#[allow(dead_code)]
fn _is_skill_directory(path: &Path) -> bool {
    path.is_dir() && path.join("SKILL.md").is_file()
}

#[cfg(test)]
mod tests {
    use super::discover_workspace_with_profile;

    fn write_skill(root: &Path, name: &str, description: &str) {
        let skill = root.join(name);
        std::fs::create_dir_all(&skill).expect("skill directory");
        std::fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\nbody\n"),
        )
        .expect("skill fixture");
    }

    use std::path::Path;

    #[test]
    fn workspace_skills_override_global_skills_and_global_only_is_visible() {
        let root = std::env::temp_dir().join(format!(
            "slim-workspace-skills-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let workspace = root.join("workspace");
        let profile = root.join("profile");
        write_skill(&workspace.join(".slim/skills"), "same", "workspace");
        write_skill(&profile.join(".slim/skills"), "same", "global");
        write_skill(&profile.join(".slim/skills"), "global-only", "global");

        let discovery = discover_workspace_with_profile(&workspace, Some(&profile))
            .expect("workspace discovery");

        assert_eq!(
            discovery
                .active("same")
                .expect("workspace skill")
                .metadata
                .description,
            "workspace"
        );
        assert!(discovery.active("global-only").is_some());
        let _ = std::fs::remove_dir_all(root);
    }
}
