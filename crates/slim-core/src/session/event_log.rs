use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::events::SessionEvent;

use super::{SessionHeader, SessionLine};

pub struct SessionWriter {
    path: PathBuf,
    file: File,
    last_seq: Option<u64>,
}

impl SessionWriter {
    pub fn create(path: impl AsRef<Path>, id: &str, cwd: &str) -> io::Result<Self> {
        Self::create_with_header(path, SessionHeader::new(id, cwd, None, None))
    }

    pub(crate) fn create_with_header(
        path: impl AsRef<Path>,
        header: SessionHeader,
    ) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let is_new = !path.exists() || fs::metadata(&path)?.len() == 0;
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let last_seq = if is_new {
            None
        } else {
            super::recovery::recover(&path)?
                .events
                .last()
                .map(|event| event.seq)
        };
        if is_new {
            write_line(&mut file, &SessionLine::from(header))?;
        }
        Ok(Self {
            path,
            file,
            last_seq,
        })
    }

    pub fn append(&mut self, event: &SessionEvent) -> io::Result<()> {
        if self.last_seq.is_some_and(|last_seq| event.seq <= last_seq) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session event sequence must increase",
            ));
        }
        write_line(
            &mut self.file,
            &SessionLine::Event {
                seq: event.seq,
                event: event.clone(),
            },
        )?;
        self.file.sync_data()?;
        self.last_seq = Some(event.seq);
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn write_line(file: &mut File, line: &SessionLine) -> io::Result<()> {
    let bytes = serde_json::to_vec(line).map_err(io::Error::other)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    Ok(())
}
