use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::{
    MemoryEntry, MemoryKind, MemorySource, ProjectMemory, MAX_ENTRY_CHARS, MAX_MEMORY_ENTRIES,
    SCHEMA_VERSION,
};
use crate::checkpoint::{now_ms, project_hash, slim_data_root};

fn memory_file_path(cwd: &Path) -> Option<(PathBuf, String)> {
    let root = slim_data_root()?;
    let canonical = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let hash = project_hash(&canonical);
    let path = root.join("memory").join(format!("{hash}.json"));
    Some((path, canonical.to_string_lossy().into_owned()))
}

fn normalize_text(raw: &str) -> Option<String> {
    let collapsed: String = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .chars()
        .take(MAX_ENTRY_CHARS)
        .collect();
    let trimmed = collapsed.trim().to_owned();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn normalize_key(text: &str) -> String {
    text.trim().to_ascii_lowercase()
}

fn entry_id(kind: MemoryKind, text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(kind.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(normalize_key(text).as_bytes());
    let digest = hasher.finalize();
    format!(
        "{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
}

fn evict_lru_one(entries: &mut Vec<MemoryEntry>) {
    if entries.is_empty() {
        return;
    }
    let has_auto = entries
        .iter()
        .any(|entry| entry.source == MemorySource::Auto);
    let idx = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| !has_auto || entry.source == MemorySource::Auto)
        .min_by_key(|(_, entry)| (entry.hits, entry.updated_at_ms, entry.id.clone()))
        .map(|(idx, _)| idx);
    if let Some(idx) = idx {
        entries.remove(idx);
    }
}

pub fn load_project_memory(cwd: &Path) -> ProjectMemory {
    let Some((path, root_str)) = memory_file_path(cwd) else {
        return ProjectMemory::default();
    };
    let Ok(raw) = fs::read_to_string(&path) else {
        return ProjectMemory {
            schema_version: SCHEMA_VERSION,
            project_root: root_str,
            entries: Vec::new(),
        };
    };
    match serde_json::from_str::<ProjectMemory>(&raw) {
        Ok(mut mem) if mem.schema_version == SCHEMA_VERSION => {
            if mem.project_root.is_empty() {
                mem.project_root = root_str;
            }
            mem
        }
        _ => ProjectMemory {
            schema_version: SCHEMA_VERSION,
            project_root: root_str,
            entries: Vec::new(),
        },
    }
}

pub fn save_project_memory(cwd: &Path, memory: &ProjectMemory) -> Result<(), String> {
    let Some((path, root_str)) = memory_file_path(cwd) else {
        return Err("Diretório home do Slim indisponível para salvar memória.".to_owned());
    };
    let Some(parent) = path.parent() else {
        return Err("Caminho de memória inválido.".to_owned());
    };
    fs::create_dir_all(parent)
        .map_err(|error| format!("Não foi possível criar diretório de memória: {error}"))?;

    let mut normalized = memory.clone();
    normalized.schema_version = SCHEMA_VERSION;
    if normalized.project_root.is_empty() {
        normalized.project_root = root_str;
    }
    while normalized.entries.len() > MAX_MEMORY_ENTRIES {
        evict_lru_one(&mut normalized.entries);
    }

    let payload = serde_json::to_string_pretty(&normalized)
        .map_err(|error| format!("Falha ao serializar memória do projeto: {error}"))?;
    let tmp = parent.join(format!(
        "{}.tmp.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("memory.json"),
        std::process::id()
    ));
    fs::write(&tmp, payload)
        .map_err(|error| format!("Falha ao gravar arquivo temporário de memória: {error}"))?;
    if let Err(error) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("Falha ao salvar memória do projeto: {error}"));
    }
    Ok(())
}

pub fn add_memory_entry(
    cwd: &Path,
    kind: MemoryKind,
    text: &str,
    source: MemorySource,
) -> Result<MemoryEntry, String> {
    let text = normalize_text(text)
        .ok_or_else(|| "O texto da memória não pode ser vazio.".to_owned())?;
    let key = normalize_key(&text);
    let now = now_ms();
    let mut mem = load_project_memory(cwd);

    if let Some(existing) = mem
        .entries
        .iter_mut()
        .find(|entry| entry.kind == kind && normalize_key(&entry.text) == key)
    {
        existing.hits = existing.hits.saturating_add(1);
        existing.updated_at_ms = now;
        if source == MemorySource::User {
            existing.source = MemorySource::User;
            existing.text = text;
        }
        let updated = existing.clone();
        save_project_memory(cwd, &mem)?;
        return Ok(updated);
    }

    while mem.entries.len() >= MAX_MEMORY_ENTRIES {
        evict_lru_one(&mut mem.entries);
    }

    let entry = MemoryEntry {
        id: entry_id(kind, &text),
        kind,
        text,
        source,
        updated_at_ms: now,
        hits: 1,
    };
    mem.entries.push(entry.clone());
    save_project_memory(cwd, &mem)?;
    Ok(entry)
}

pub fn remove_memory_entry(cwd: &Path, id_or_prefix: &str) -> Result<MemoryEntry, String> {
    let needle = id_or_prefix.trim();
    if needle.is_empty() {
        return Err("Informe o ID da memória a remover.".to_owned());
    }
    let mut mem = load_project_memory(cwd);
    if let Some(pos) = mem.entries.iter().position(|entry| entry.id == needle) {
        let removed = mem.entries.remove(pos);
        save_project_memory(cwd, &mem)?;
        return Ok(removed);
    }
    let matches: Vec<usize> = mem
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.id.starts_with(needle))
        .map(|(idx, _)| idx)
        .collect();
    match matches.as_slice() {
        [pos] => {
            let removed = mem.entries.remove(*pos);
            save_project_memory(cwd, &mem)?;
            Ok(removed)
        }
        [] => Err(format!("Nenhuma memória encontrada com ID \"{needle}\".")),
        _ => Err(format!(
            "Prefixo \"{needle}\" é ambíguo; informe mais caracteres do ID."
        )),
    }
}

pub fn clear_project_memory(cwd: &Path) -> Result<usize, String> {
    let mut mem = load_project_memory(cwd);
    let count = mem.entries.len();
    if count == 0 {
        return Ok(0);
    }
    mem.entries.clear();
    save_project_memory(cwd, &mem)?;
    Ok(count)
}
