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
