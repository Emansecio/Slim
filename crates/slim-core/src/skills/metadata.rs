use std::fs;
use std::io;
use std::path::Path;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
}

pub fn read_metadata(path: impl AsRef<Path>) -> io::Result<SkillMetadata> {
    let text = fs::read_to_string(path)?;
    let (frontmatter, _) = split_document(&text)?;
    let mut name = None;
    let mut description = None;
    for line in frontmatter.lines() {
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
    let text = fs::read_to_string(path)?;
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
