//! Incremental Server-Sent Events decoder for MCP's Streamable HTTP
//! transport. Bytes are fed as they arrive; complete events come out one at a
//! time so a caller can stop at the event it was waiting for.
//!
//! Hostile-server bounds live here: one event's `data` and the unterminated
//! tail are each capped, and only complete lines are decoded as UTF-8 (a
//! multibyte character can straddle a network chunk, and `\n` never occurs
//! inside one).

use std::time::Duration;

use crate::mcp::spec::McpError;

/// One dispatched event. `event` is the SSE event type (`None` for the
/// default, which is `message`).
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

impl SseEvent {
    /// Only default/`message` events carry JSON-RPC; other types are ignored.
    pub fn is_message(&self) -> bool {
        self.event.as_deref().is_none_or(|name| name == "message")
    }
}

/// Longest event id kept: it is replayed in a `Last-Event-ID` header.
const MAX_EVENT_ID_BYTES: usize = 1024;

pub(crate) struct SseDecoder {
    buffer: Vec<u8>,
    /// Start of the first byte not yet consumed as a whole line.
    start: usize,
    /// Where the next newline search resumes, so a long unterminated line is
    /// never rescanned.
    scan: usize,
    data: String,
    has_data: bool,
    event: Option<String>,
    pending_id: Option<String>,
    max_bytes: usize,
    last_event_id: Option<String>,
    retry: Option<Duration>,
    events_seen: bool,
}

impl SseDecoder {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            buffer: Vec::new(),
            start: 0,
            scan: 0,
            data: String::new(),
            has_data: false,
            event: None,
            pending_id: None,
            max_bytes,
            last_event_id: None,
            retry: None,
            events_seen: false,
        }
    }

    /// Appends received bytes. Fails when the not-yet-consumed input exceeds
    /// the per-message cap.
    pub fn push(&mut self, chunk: &[u8]) -> Result<(), McpError> {
        if self.start > 0 {
            self.buffer.drain(..self.start);
            self.scan -= self.start;
            self.start = 0;
        }
        self.buffer.extend_from_slice(chunk);
        if self.buffer.len() > self.max_bytes {
            return Err(too_large());
        }
        Ok(())
    }

    /// Next complete event, or `None` when more input is needed. Events whose
    /// `data` is empty (resumption priming events, keep-alives) only update
    /// the cursor and are not returned.
    pub fn next_event(&mut self) -> Result<Option<SseEvent>, McpError> {
        loop {
            let Some(relative) = self.buffer[self.scan..].iter().position(|b| *b == b'\n') else {
                self.scan = self.buffer.len();
                return Ok(None);
            };
            let end = self.scan + relative;
            let line = std::str::from_utf8(&self.buffer[self.start..end])
                .map_err(|_| McpError::Protocol("invalid UTF-8 in SSE message".into()))?
                .trim_end_matches('\r')
                .to_owned();
            self.start = end + 1;
            self.scan = self.start;
            if let Some(event) = self.process_line(&line)? {
                return Ok(Some(event));
            }
        }
    }

    /// Last `id:` the server dispatched; the value for `Last-Event-ID` when
    /// the stream has to be resumed.
    pub fn last_event_id(&self) -> Option<&str> {
        self.last_event_id.as_deref()
    }

    /// Reconnection delay the server asked for with `retry:`.
    pub fn retry(&self) -> Option<Duration> {
        self.retry
    }

    /// Whether any event (including a priming event) was dispatched.
    pub fn events_seen(&self) -> bool {
        self.events_seen
    }

    fn process_line(&mut self, line: &str) -> Result<Option<SseEvent>, McpError> {
        if line.is_empty() {
            return Ok(self.dispatch());
        }
        if line.starts_with(':') {
            return Ok(None);
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "data" => {
                // Every `data:` line contributes at least its newline, so
                // empty lines cannot bypass the cap.
                if self.data.len() + value.len() + 1 > self.max_bytes {
                    return Err(too_large());
                }
                self.data.push_str(value);
                self.data.push('\n');
                self.has_data = true;
            }
            "event" => self.event = Some(value.to_owned()),
            // The id is applied when the event is dispatched; it reaches a
            // request header, so anything but plain visible text is dropped.
            "id" if !value.is_empty()
                && value.len() <= MAX_EVENT_ID_BYTES
                && value.chars().all(|c| !c.is_control()) =>
            {
                self.pending_id = Some(value.to_owned());
            }
            "retry" if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) => {
                if let Ok(millis) = value.parse::<u64>() {
                    self.retry = Some(Duration::from_millis(millis));
                }
            }
            _ => {}
        }
        Ok(None)
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        if let Some(id) = self.pending_id.take() {
            self.last_event_id = Some(id);
        }
        let event = self.event.take();
        if !self.has_data {
            return None;
        }
        self.has_data = false;
        self.events_seen = true;
        let mut data = std::mem::take(&mut self.data);
        data.pop();
        Some(SseEvent { event, data })
    }
}

fn too_large() -> McpError {
    McpError::Protocol("sse message exceeds 16 MiB".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoder() -> SseDecoder {
        SseDecoder::new(1024)
    }

    fn drain(decoder: &mut SseDecoder) -> Vec<SseEvent> {
        let mut events = Vec::new();
        while let Some(event) = decoder.next_event().expect("decode") {
            events.push(event);
        }
        events
    }

    #[test]
    fn decodes_events_across_chunks_with_crlf_and_comments() {
        let mut decoder = decoder();
        decoder
            .push(b": keepalive\r\nid: 7\r\ndata: {\"a\"")
            .unwrap();
        assert!(drain(&mut decoder).is_empty());
        decoder.push(b":1}\r\n\r\ndata: x\ndata: y\n\n").unwrap();
        let events = drain(&mut decoder);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "{\"a\":1}");
        assert!(events[0].is_message());
        assert_eq!(events[1].data, "x\ny");
        assert_eq!(decoder.last_event_id(), Some("7"));
        assert!(decoder.events_seen());
    }

    #[test]
    fn only_one_leading_space_is_stripped_from_values() {
        let mut decoder = decoder();
        decoder.push(b"data:   spaced\n\n").unwrap();
        assert_eq!(drain(&mut decoder)[0].data, "  spaced");
    }

    #[test]
    fn multibyte_characters_split_across_chunks_survive() {
        let mut decoder = decoder();
        let bytes = "data: ação🐾\n\n".as_bytes();
        for chunk in bytes.chunks(1) {
            decoder.push(chunk).unwrap();
        }
        assert_eq!(drain(&mut decoder)[0].data, "ação🐾");
    }

    #[test]
    fn priming_event_updates_the_cursor_without_producing_a_message() {
        let mut decoder = decoder();
        decoder.push(b"id: 0\nretry: 250\ndata:\n\n").unwrap();
        // The empty `data:` line yields an event without content for the
        // caller to skip; the cursor still advanced.
        let primed = drain(&mut decoder);
        assert_eq!(primed.len(), 1);
        assert!(primed[0].data.is_empty());
        assert_eq!(decoder.last_event_id(), Some("0"));
        // An id with no data line at all produces nothing.
        decoder.push(b"id: 5\n\n").unwrap();
        assert!(drain(&mut decoder).is_empty());
        assert_eq!(decoder.last_event_id(), Some("5"));
        assert_eq!(decoder.retry(), Some(Duration::from_millis(250)));
    }

    #[test]
    fn id_is_applied_at_dispatch_not_before() {
        let mut decoder = decoder();
        decoder.push(b"id: 1\ndata: a\n\nid: 2\ndata: b").unwrap();
        assert_eq!(drain(&mut decoder).len(), 1);
        // The second event never completed, so resuming repeats it.
        assert_eq!(decoder.last_event_id(), Some("1"));
    }

    #[test]
    fn named_events_are_not_messages_and_bad_fields_are_ignored() {
        let mut decoder = decoder();
        decoder
            .push(b"event: ping\ndata: {}\n\nretry: soon\nid: a\x01b\ndata: m\n\n")
            .unwrap();
        let events = drain(&mut decoder);
        assert!(!events[0].is_message());
        assert!(events[1].is_message());
        assert_eq!(decoder.retry(), None);
        assert_eq!(decoder.last_event_id(), None);
    }

    #[test]
    fn ids_that_could_not_travel_in_a_header_are_ignored() {
        let mut decoder = SseDecoder::new(8 * 1024);
        let long = "x".repeat(MAX_EVENT_ID_BYTES + 1);
        decoder
            .push(format!("id: {long}\ndata: a\n\n").as_bytes())
            .unwrap();
        assert_eq!(drain(&mut decoder).len(), 1);
        assert_eq!(decoder.last_event_id(), None);
        let longest = "y".repeat(MAX_EVENT_ID_BYTES);
        decoder
            .push(format!("id: {longest}\ndata: b\n\n").as_bytes())
            .unwrap();
        assert_eq!(drain(&mut decoder).len(), 1);
        assert_eq!(decoder.last_event_id(), Some(longest.as_str()));
    }

    #[test]
    fn oversized_input_and_data_are_rejected() {
        let mut decoder = SseDecoder::new(32);
        assert!(decoder.push(&[b'x'; 33]).is_err());
        let mut decoder = SseDecoder::new(32);
        decoder.push(b"data: 0123456789012345678\n").unwrap();
        assert!(decoder.next_event().unwrap().is_none());
        decoder.push(b"data: 0123456789012345678\n").unwrap();
        let error = decoder.next_event().expect_err("data cap");
        assert!(error.to_string().contains("16 MiB"), "{error}");
        // Empty data lines count too.
        let mut decoder = SseDecoder::new(8);
        let rejected = (0..10).any(|_| {
            decoder.push(b"data:\n").unwrap();
            decoder.next_event().is_err()
        });
        assert!(rejected, "empty data lines must still count toward the cap");
    }

    #[test]
    fn invalid_utf8_lines_are_rejected() {
        let mut decoder = decoder();
        decoder.push(b"data: \xff\n\n").unwrap();
        let error = decoder.next_event().expect_err("utf8");
        assert!(error.to_string().contains("UTF-8"), "{error}");
    }
}
