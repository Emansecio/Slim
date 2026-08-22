use serde_json::Value;

#[derive(Default)]
pub struct JsonLineFramer {
    buffer: Vec<u8>,
}

impl JsonLineFramer {
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Value>, serde_json::Error> {
        self.buffer.extend_from_slice(chunk);
        let mut messages = Vec::new();
        while let Some(index) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=index).collect();
            let trimmed = line.strip_suffix(b"\n").unwrap_or(&line);
            if trimmed.is_empty() {
                continue;
            }
            messages.push(serde_json::from_slice(trimmed)?);
        }
        Ok(messages)
    }
}
