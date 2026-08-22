use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use slim_core::OperatingMode;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NormalizedKey {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
    pub kind: KeyEventKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnterIntent {
    Submit,
    Newline,
    Steer,
    Ignore,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DecodedInput {
    Text(String),
    Paste(String),
}

#[derive(Clone, Debug, Default)]
pub struct BracketedPasteDecoder {
    buffer: Vec<u8>,
    pasting: bool,
}

impl BracketedPasteDecoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<DecodedInput> {
        const START: &[u8] = b"\x1b[200~";
        const END: &[u8] = b"\x1b[201~";
        self.buffer.extend_from_slice(bytes);
        let mut decoded = Vec::new();
        loop {
            if self.pasting {
                let Some(end) = find_subslice(&self.buffer, END) else {
                    break;
                };
                let payload = self.buffer.drain(..end).collect::<Vec<_>>();
                self.buffer.drain(..END.len());
                decoded.push(DecodedInput::Paste(
                    String::from_utf8_lossy(&payload).into(),
                ));
                self.pasting = false;
                continue;
            }
            let Some(start) = find_subslice(&self.buffer, START) else {
                let keep = longest_suffix_prefix(&self.buffer, START);
                let emit_len = self.buffer.len().saturating_sub(keep);
                if emit_len > 0 {
                    let text = self.buffer.drain(..emit_len).collect::<Vec<_>>();
                    decoded.push(DecodedInput::Text(String::from_utf8_lossy(&text).into()));
                }
                break;
            };
            if start > 0 {
                let text = self.buffer.drain(..start).collect::<Vec<_>>();
                decoded.push(DecodedInput::Text(String::from_utf8_lossy(&text).into()));
            }
            self.buffer.drain(..START.len());
            self.pasting = true;
        }
        decoded
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn longest_suffix_prefix(buffer: &[u8], marker: &[u8]) -> usize {
    (1..=buffer.len().min(marker.len().saturating_sub(1)))
        .rev()
        .find(|length| buffer[buffer.len() - length..] == marker[..*length])
        .unwrap_or(0)
}

pub fn normalize(key: KeyEvent) -> NormalizedKey {
    NormalizedKey {
        code: key.code,
        modifiers: key.modifiers,
        kind: key.kind,
    }
}

pub fn is_submit(key: NormalizedKey) -> bool {
    classify_enter(key, false) == EnterIntent::Submit
}

pub fn classify_enter(key: NormalizedKey, run_active: bool) -> EnterIntent {
    if key.kind != KeyEventKind::Press || key.code != KeyCode::Enter {
        return EnterIntent::Ignore;
    }
    if key.modifiers.contains(KeyModifiers::ALT) && run_active {
        return EnterIntent::Steer;
    }
    if key
        .modifiers
        .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL)
    {
        return EnterIntent::Newline;
    }
    EnterIntent::Submit
}

pub fn cycle_mode(mode: OperatingMode) -> OperatingMode {
    mode.next()
}
