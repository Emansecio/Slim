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

#[allow(dead_code)]
fn _is_skill_directory(path: &Path) -> bool {
    path.is_dir() && path.join("SKILL.md").is_file()
}
