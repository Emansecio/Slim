use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

const MAX_SKILL_FRONTMATTER_BYTES: usize = 64 * 1024;
const MAX_SKILL_FILE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
}

pub fn read_metadata(path: impl AsRef<Path>) -> io::Result<SkillMetadata> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file).take((MAX_SKILL_FRONTMATTER_BYTES + 1) as u64);
    let mut line = Vec::new();
    let mut consumed = reader.read_until(b'\n', &mut line)?;
    if consumed > MAX_SKILL_FRONTMATTER_BYTES {
        return Err(invalid(
            "skill frontmatter exceeds the 65536-byte safety limit",
        ));
    }
    if std::str::from_utf8(&line).map_err(invalid_utf8)?.trim() != "---" {
        return Err(invalid("missing frontmatter"));
    }
    let mut name = None;
    let mut description = None;
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            return Err(invalid("unterminated frontmatter"));
        }
        consumed = consumed.saturating_add(read);
        if consumed > MAX_SKILL_FRONTMATTER_BYTES {
            return Err(invalid(
                "skill frontmatter exceeds the 65536-byte safety limit",
            ));
        }
        let line = std::str::from_utf8(&line).map_err(invalid_utf8)?;
        if line.trim() == "---" {
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "name" => name = Some(value.trim().to_owned()),
            "description" => description = Some(value.trim().to_owned()),
            _ => {}
        }
    }
    Ok(SkillMetadata {
        name: name.ok_or_else(|| invalid("missing name"))?,
        description: description.ok_or_else(|| invalid("missing description"))?,
    })
}

pub fn read_body(path: impl AsRef<Path>) -> io::Result<String> {
    let file = File::open(path)?;
    let mut text = String::new();
    file.take((MAX_SKILL_FILE_BYTES + 1) as u64)
        .read_to_string(&mut text)?;
    if text.len() > MAX_SKILL_FILE_BYTES {
        return Err(invalid("skill file exceeds the 1048576-byte safety limit"));
    }
    Ok(split_document(&text)?.1.trim_start_matches('\n').to_owned())
}

fn split_document(text: &str) -> io::Result<(&str, &str)> {
    let mut lines = text.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return Err(invalid("empty skill"));
    };
    if first.trim() != "---" {
        return Err(invalid("missing frontmatter"));
    }
    let mut frontmatter_end = first.len();
    for line in lines {
        if line.trim() == "---" {
            return Ok((
                &text[first.len()..frontmatter_end],
                &text[frontmatter_end + line.len()..],
            ));
        }
        frontmatter_end += line.len();
    }
    Err(invalid("unterminated frontmatter"))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn invalid_utf8(error: std::str::Utf8Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
