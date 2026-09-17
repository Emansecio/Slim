use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
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
                let payload = String::from_utf8_lossy(&self.buffer[..end]).into_owned();
                self.buffer.drain(..end + END.len());
                decoded.push(DecodedInput::Paste(payload));
                self.pasting = false;
                continue;
            }
            let Some(start) = find_subslice(&self.buffer, START) else {
                let keep = longest_suffix_prefix(&self.buffer, START);
                let emit_len = self.buffer.len().saturating_sub(keep);
                if emit_len > 0 {
                    let text = String::from_utf8_lossy(&self.buffer[..emit_len]).into_owned();
                    self.buffer.drain(..emit_len);
                    decoded.push(DecodedInput::Text(text));
                }
                break;
            };
            if start > 0 {
                let text = String::from_utf8_lossy(&self.buffer[..start]).into_owned();
                self.buffer.drain(..start);
                decoded.push(DecodedInput::Text(text));
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DecodedEvent {
    Event(Event),
    Paste(String),
}

/// Decodes paste bursts from the terminal event stream. Crossterm only emits
/// `Event::Paste` on Unix. Measured on Windows (ConPTY probe
/// `tests/paste_conpty_probe.rs`, 16/09/2026): the console input layer absorbs
/// the `Esc [ 2 0 0 ~` / `Esc [ 2 0 1 ~` markers entirely — no `Esc` key event
/// and no `Event::Paste` reaches the app — and a paste arrives as plain text
/// interleaved with `Enter` presses whose `poll(0)` reports more queued input.
/// The burst heuristic below is the path that actually runs there; the marker
/// state machine is retained for terminals that do forward the markers, so a
/// paste never becomes a submit either way (spec §15.3: "paste multiline nunca
/// envia automaticamente").
#[derive(Clone, Debug, Default)]
pub struct PasteStreamDecoder {
    pasting: bool,
    /// Progress (1..=5 matched so far) through the marker being tracked:
    /// `\x1b[200~` outside a paste, `\x1b[201~` inside one.
    marker_progress: usize,
    /// Press events held while a marker is undecided (at most 5).
    pending: Vec<KeyEvent>,
    payload: String,
    /// `payload.chars().count()` kept incrementally: a per-char scan would
    /// make a ConPTY paste burst quadratic (one key event per pasted char).
    payload_chars: usize,
    /// Text already seen in the current input burst; an `Enter` that follows
    /// it (or precedes more queued input) is a paste newline, not a submit.
    burst_text: bool,
}

impl PasteStreamDecoder {
    pub fn feed(&mut self, event: Event, more_input: bool) -> Vec<DecodedEvent> {
        let Event::Key(key) = event else {
            return vec![DecodedEvent::Event(event)];
        };
        if key.kind != KeyEventKind::Press {
            return vec![DecodedEvent::Event(event)];
        }
        if self.marker_progress > 0 {
            return self.feed_marker(key, more_input);
        }
        if self.pasting {
            return self.feed_paste(key);
        }
        self.feed_ground(key, more_input)
    }

    /// Called when the input queue drained. A burst boundary resets the
    /// typed-text heuristic and flushes a marker prefix that turned out to be
    /// literal input (e.g. a lone `Esc`).
    pub fn end_of_input(&mut self) -> Vec<DecodedEvent> {
        self.burst_text = false;
        if self.pasting || self.marker_progress == 0 {
            return Vec::new();
        }
        self.marker_progress = 0;
        self.pending
            .drain(..)
            .map(|key| DecodedEvent::Event(Event::Key(key)))
            .collect()
    }

    fn feed_ground(&mut self, key: KeyEvent, more_input: bool) -> Vec<DecodedEvent> {
        if is_marker_key(key, 0, self.pasting) {
            self.pending.push(key);
            self.marker_progress = 1;
            return Vec::new();
        }
        if key.code == KeyCode::Enter
            && key.modifiers == KeyModifiers::NONE
            && (more_input || self.burst_text)
        {
            self.burst_text = true;
            return vec![DecodedEvent::Event(Event::Key(KeyEvent::new(
                KeyCode::Char('\n'),
                KeyModifiers::NONE,
            )))];
        }
        self.burst_text |= matches!(key.code, KeyCode::Char(_) | KeyCode::Tab)
            && !key.modifiers.contains(KeyModifiers::CONTROL);
        vec![DecodedEvent::Event(Event::Key(key))]
    }

    fn feed_marker(&mut self, key: KeyEvent, more_input: bool) -> Vec<DecodedEvent> {
        if is_marker_key(key, self.marker_progress, self.pasting) {
            self.pending.push(key);
            self.marker_progress += 1;
            if self.marker_progress == MARKER_LEN {
                self.marker_progress = 0;
                self.pending.clear();
                if self.pasting {
                    self.pasting = false;
                    self.payload_chars = 0;
                    return vec![DecodedEvent::Paste(std::mem::take(&mut self.payload))];
                }
                self.pasting = true;
                self.payload.clear();
                self.payload_chars = 0;
            }
            return Vec::new();
        }
        // Mismatch: the held prefix is literal input, then the current event
        // is re-evaluated (it may itself open a new marker).
        let mut flushed = Vec::new();
        if self.pasting {
            for held in self.pending.drain(..) {
                if let Some(character) = paste_char(held.code) {
                    self.payload.push(character);
                    self.payload_chars += 1;
                }
            }
        } else {
            flushed.extend(
                self.pending
                    .drain(..)
                    .map(|held| DecodedEvent::Event(Event::Key(held))),
            );
        }
        self.marker_progress = 0;
        flushed.extend(self.feed(Event::Key(key), more_input));
        flushed
    }

    fn feed_paste(&mut self, key: KeyEvent) -> Vec<DecodedEvent> {
        if is_marker_key(key, 0, self.pasting) {
            self.pending.push(key);
            self.marker_progress = 1;
            return Vec::new();
        }
        match paste_char(key.code) {
            Some(character) => {
                self.payload.push(character);
                self.payload_chars += 1;
                if self.payload_chars >= crate::composer::MAX_DRAFT_CHARS {
                    self.pasting = false;
                    self.payload_chars = 0;
                    return vec![DecodedEvent::Paste(std::mem::take(&mut self.payload))];
                }
                Vec::new()
            }
            None => vec![DecodedEvent::Event(Event::Key(key))],
        }
    }
}

const MARKER_LEN: usize = 6;

/// `\x1b[200~` opens a paste, `\x1b[201~` closes it; the sequences differ only
/// at index 4. Matching requires a plain Press so user chorded keys never
/// register as marker bytes.
fn is_marker_key(key: KeyEvent, index: usize, closing: bool) -> bool {
    if key.modifiers != KeyModifiers::NONE {
        return false;
    }
    key.code
        == match index {
            0 => KeyCode::Esc,
            1 => KeyCode::Char('['),
            2 => KeyCode::Char('2'),
            3 => KeyCode::Char('0'),
            4 if closing => KeyCode::Char('1'),
            4 => KeyCode::Char('0'),
            5 => KeyCode::Char('~'),
            _ => unreachable!(),
        }
}

fn paste_char(code: KeyCode) -> Option<char> {
    match code {
        KeyCode::Char(character) => Some(character),
        KeyCode::Enter => Some('\n'),
        KeyCode::Tab => Some('\t'),
        KeyCode::Backspace => Some('\x08'),
        KeyCode::Esc => Some('\x1b'),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{BracketedPasteDecoder, DecodedEvent, DecodedInput, PasteStreamDecoder};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn bracketed_paste_decodes_across_partial_feeds() {
        let mut decoder = BracketedPasteDecoder::default();
        // The marker tail is held back until the rest arrives.
        assert_eq!(
            decoder.feed(b"hello \x1b[20"),
            vec![DecodedInput::Text("hello ".into())]
        );
        assert_eq!(
            decoder.feed(b"0~payload\nlines\x1b[201~ tail"),
            vec![
                DecodedInput::Paste("payload\nlines".into()),
                DecodedInput::Text(" tail".into())
            ]
        );
    }

    #[test]
    fn key_paste_roundtrips_through_markers() {
        let mut decoder = PasteStreamDecoder::default();
        for code in [
            KeyCode::Esc,
            KeyCode::Char('['),
            KeyCode::Char('2'),
            KeyCode::Char('0'),
            KeyCode::Char('0'),
            KeyCode::Char('~'),
        ] {
            assert!(decoder.feed(Event::Key(key(code)), false).is_empty());
        }
        for event in [KeyCode::Char('a'), KeyCode::Enter, KeyCode::Char('b')] {
            assert!(decoder.feed(Event::Key(key(event)), false).is_empty());
        }
        let mut pasted = None;
        for code in [
            KeyCode::Esc,
            KeyCode::Char('['),
            KeyCode::Char('2'),
            KeyCode::Char('0'),
            KeyCode::Char('1'),
            KeyCode::Char('~'),
        ] {
            for decoded in decoder.feed(Event::Key(key(code)), false) {
                if let DecodedEvent::Paste(payload) = decoded {
                    pasted = Some(payload);
                }
            }
        }
        assert_eq!(pasted.as_deref(), Some("a\nb"));
    }

    #[test]
    fn key_paste_cap_counts_chars_incrementally() {
        let mut decoder = PasteStreamDecoder::default();
        for code in [
            KeyCode::Esc,
            KeyCode::Char('['),
            KeyCode::Char('2'),
            KeyCode::Char('0'),
            KeyCode::Char('0'),
            KeyCode::Char('~'),
        ] {
            decoder.feed(Event::Key(key(code)), false);
        }
        let mut emitted = None;
        for _ in 0..crate::composer::MAX_DRAFT_CHARS {
            for decoded in decoder.feed(Event::Key(key(KeyCode::Char('a'))), false) {
                if let DecodedEvent::Paste(payload) = decoded {
                    emitted = Some(payload);
                }
            }
        }
        let payload = emitted.expect("draft-size cap flushes the paste");
        assert_eq!(
            payload.chars().count(),
            crate::composer::MAX_DRAFT_CHARS,
            "payload ends exactly at the cap"
        );
        // The decoder recovered: fresh input flows as normal events again.
        let press = Event::Key(key(KeyCode::Char('a')));
        assert_eq!(
            decoder.feed(press.clone(), false),
            vec![DecodedEvent::Event(press)]
        );
    }
}
