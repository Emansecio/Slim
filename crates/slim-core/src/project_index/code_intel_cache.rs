//! Project-wide epoch-validated LRU cache for read-only `code_intel` outcomes
//! (`definition`, `references`, `hover`, `symbols`, `diagnostics`).

use std::collections::HashMap;

use crate::tools::{CodeIntelPresentation, ToolResult};

const MAX_CODE_INTEL_CACHE_ENTRIES: usize = 512;

#[derive(Clone, Debug)]
pub(crate) struct CachedCodeIntelOutcome {
    pub(crate) result: ToolResult,
    pub(crate) presentation: Option<CodeIntelPresentation>,
}

#[derive(Debug)]
struct Entry {
    fs_epoch: u64,
    last_used: u64,
    outcome: CachedCodeIntelOutcome,
}

/// Shared LRU cache keyed by canonical `code_intel` query fingerprint and
/// validated in O(1) against [`super::ProjectIndexDaemon::fs_epoch`].
#[derive(Debug, Default)]
pub(crate) struct CodeIntelLru {
    entries: HashMap<String, Entry>,
    clock: u64,
    hits: u64,
    misses: u64,
}

impl CodeIntelLru {
    pub(crate) fn get(&mut self, key: &str, current_epoch: u64) -> Option<CachedCodeIntelOutcome> {
        let valid = self
            .entries
            .get(key)
            .is_some_and(|entry| entry.fs_epoch == current_epoch);
        if !valid {
            self.entries.remove(key);
            self.misses = self.misses.saturating_add(1);
            return None;
        }
        self.clock = self.clock.saturating_add(1);
        let clock = self.clock;
        if let Some(entry) = self.entries.get_mut(key) {
            entry.last_used = clock;
            self.hits = self.hits.saturating_add(1);
            return Some(entry.outcome.clone());
        }
        None
    }

    pub(crate) fn insert(
        &mut self,
        key: String,
        fs_epoch: u64,
        outcome: CachedCodeIntelOutcome,
    ) {
        if self.entries.len() >= MAX_CODE_INTEL_CACHE_ENTRIES && !self.entries.contains_key(&key) {
            if let Some(oldest_key) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
            {
                self.entries.remove(&oldest_key);
            }
        }
        self.clock = self.clock.saturating_add(1);
        self.entries.insert(
            key,
            Entry {
                fs_epoch,
                last_used: self.clock,
                outcome,
            },
        );
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }

    pub(crate) fn stats(&self) -> (usize, u64, u64) {
        (self.entries.len(), self.hits, self.misses)
    }
}
