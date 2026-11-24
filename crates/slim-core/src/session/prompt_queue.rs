use super::{DurableFact, DurableRecord, DurableRepo, DurableSessionHeader, JsonlRepo};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{io, path::Path};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptQueueSnapshot {
    pub pending: Vec<String>,
    // Retained until a terminal event: a crash must never imply safe replay.
    pub in_flight: Option<String>,
    /// One cancelled direct prompt restored in the composer. This is a
    /// recovery marker for that accepted prompt, not general draft history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_draft: Option<String>,
}

pub struct PromptQueueJournal {
    repo: JsonlRepo,
    snapshot: PromptQueueSnapshot,
}

impl PromptQueueJournal {
    pub fn open(workspace: &Path, session: &str) -> io::Result<Self> {
        let directory = workspace.join(".slim/queues");
        std::fs::create_dir_all(&directory)?;
        let key = format!("{:x}", Sha256::digest(session.as_bytes()));
        let path = directory.join(format!("{key}.jsonl"));
        let repo = if path.exists() {
            JsonlRepo::open(&path)?
        } else {
            JsonlRepo::create(
                &path,
                DurableSessionHeader::new(session, "", workspace.to_string_lossy(), None, None),
            )?
        };
        let snapshot = repo
            .records()
            .last()
            .map(|record| match record {
                DurableRecord::Fact { fact, .. } if fact.namespace == "prompt_queue.v1" => {
                    serde_json::from_value(fact.value.clone()).map_err(io::Error::other)
                }
                _ => Err(io::Error::other("invalid prompt queue record")),
            })
            .transpose()?
            .unwrap_or_default();
        Ok(Self { repo, snapshot })
    }

    pub fn snapshot(&self) -> &PromptQueueSnapshot {
        &self.snapshot
    }

    pub fn save(&mut self, snapshot: PromptQueueSnapshot) -> io::Result<()> {
        if self.snapshot == snapshot {
            return Ok(());
        }
        self.repo.append(DurableRecord::Fact {
            seq: self.repo.next_seq()?,
            fact: DurableFact {
                namespace: "prompt_queue.v1".into(),
                key: "snapshot".into(),
                value: serde_json::to_value(&snapshot).map_err(io::Error::other)?,
            },
        })?;
        self.snapshot = snapshot;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "slim-prompt-queue-{label}-{}-{stamp}-{seq}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn open_save_open_roundtrip() {
        let root = TempDir::new("roundtrip");
        let mut journal = PromptQueueJournal::open(&root.0, "session-1").expect("open fresh");
        assert_eq!(journal.snapshot(), &PromptQueueSnapshot::default());

        let saved = PromptQueueSnapshot {
            pending: vec!["queued-1".into(), "queued-2".into()],
            in_flight: Some("active".into()),
            recovery_draft: Some("draft".into()),
        };
        journal.save(saved.clone()).expect("save snapshot");
        assert_eq!(journal.snapshot(), &saved);
        drop(journal);

        let reopened = PromptQueueJournal::open(&root.0, "session-1").expect("reopen");
        assert_eq!(reopened.snapshot(), &saved);
    }

    #[test]
    fn save_unchanged_snapshot_is_noop() {
        let root = TempDir::new("noop");
        let mut journal = PromptQueueJournal::open(&root.0, "session-noop").expect("open fresh");
        journal
            .save(PromptQueueSnapshot::default())
            .expect("default save is no-op");
        assert_eq!(journal.repo.records().len(), 0);

        let snapshot = PromptQueueSnapshot {
            pending: vec!["one".into()],
            in_flight: None,
            recovery_draft: None,
        };
        journal.save(snapshot.clone()).expect("first save");
        assert_eq!(journal.repo.records().len(), 1);
        let len_after_first = std::fs::metadata(journal.repo.path())
            .expect("metadata")
            .len();

        journal.save(snapshot.clone()).expect("duplicate save");
        assert_eq!(journal.repo.records().len(), 1);
        let len_after_duplicate = std::fs::metadata(journal.repo.path())
            .expect("metadata")
            .len();
        assert_eq!(len_after_first, len_after_duplicate);
        drop(journal);

        let reopened = PromptQueueJournal::open(&root.0, "session-noop").expect("reopen");
        assert_eq!(reopened.snapshot(), &snapshot);
    }
}
