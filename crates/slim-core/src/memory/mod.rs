//! Zero-LLM deterministic durable project memory stored under
//! `~/.slim/memory/<project_hash>.json`.
//!
//! Entries capture high-signal verified commands and explicit developer notes
//! (`User` entries are preserved ahead of `Auto` entries during LRU eviction)
//! and are rendered deterministically for `initial_workspace_context` so
//! provider prompt-cache prefixes stay byte-identical across turns.

mod extract;
mod render;
mod store;
#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_MEMORY_ENTRIES: usize = 24;
pub const MAX_ENTRY_CHARS: usize = 240;
pub const MAX_RENDERED_MEMORY_BYTES: usize = 1_800;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    #[default]
    Command,
    Convention,
    Architecture,
    Gotcha,
}

impl MemoryKind {
    pub const ALL: [Self; 4] = [
        Self::Command,
        Self::Convention,
        Self::Architecture,
        Self::Gotcha,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Convention => "convention",
            Self::Architecture => "architecture",
            Self::Gotcha => "gotcha",
        }
    }

    pub fn label_pt(self) -> &'static str {
        match self {
            Self::Command => "Comando",
            Self::Convention => "Convenção",
            Self::Architecture => "Arquitetura",
            Self::Gotcha => "Cuidado",
        }
    }

    pub fn parse_str(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "command" | "cmd" | "comando" | "comandos" => Some(Self::Command),
            "convention" | "conv" | "convencao" | "convenção" | "estilo" => Some(Self::Convention),
            "architecture" | "arch" | "arquitetura" => Some(Self::Architecture),
            "gotcha" | "pitfall" | "aviso" | "cuidado" => Some(Self::Gotcha),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq, Ord, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum MemorySource {
    User,
    Auto,
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct MemoryEntry {
    pub id: String,
    pub kind: MemoryKind,
    pub text: String,
    pub source: MemorySource,
    pub updated_at_ms: u64,
    pub hits: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct ProjectMemory {
    pub schema_version: u32,
    pub project_root: String,
    pub entries: Vec<MemoryEntry>,
}

impl Default for ProjectMemory {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            project_root: String::new(),
            entries: Vec::new(),
        }
    }
}

pub use extract::persist_scratchpad_learnings;
pub use render::render_for_prompt;
pub use store::{
    add_memory_entry, clear_project_memory, load_project_memory, remove_memory_entry,
    save_project_memory,
};
