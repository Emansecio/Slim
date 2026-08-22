use std::path::{Path, PathBuf};

use super::ToolError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchHit {
    pub path: PathBuf,
    pub line: usize,
    pub text: String,
}

pub fn search_literal(root: impl AsRef<Path>, query: &str) -> Result<Vec<SearchHit>, ToolError> {
    if query.is_empty() {
        return Err(ToolError::InvalidInput {
            message: "search query cannot be empty".into(),
        });
    }
    let mut hits = Vec::new();
    visit(root.as_ref(), query, &mut hits)?;
    Ok(hits)
}

fn visit(path: &Path, query: &str, hits: &mut Vec<SearchHit>) -> Result<(), ToolError> {
    let metadata = std::fs::metadata(path)?;
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path)? {
            visit(&entry?.path(), query, hits)?;
        }
        return Ok(());
    }

    let Ok(content) = std::fs::read_to_string(path) else {
        return Ok(());
    };
    for (index, line) in content.lines().enumerate() {
        if line.contains(query) {
            hits.push(SearchHit {
                path: path.to_path_buf(),
                line: index + 1,
                text: line.into(),
            });
        }
    }
    Ok(())
}
