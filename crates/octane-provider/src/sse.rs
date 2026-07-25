//! Server-sent events.
//!
//! Every streaming format here is SSE, so the framing is parsed once and the
//! per-format codecs only see decoded payloads.
//!
//! Written as a push parser over a byte buffer rather than a line iterator
//! because HTTP chunks split wherever they like — mid-event, mid-line, and
//! mid-UTF-8-sequence. A parser that assumes it receives whole lines works
//! against every provider until one of them sends a 4 KB chunk boundary through
//! the middle of a tool call.

/// One decoded event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Event {
    /// `event:` field. Anthropic uses it; OpenAI does not.
    pub name: Option<String>,
    /// `data:` field, with multiple lines joined by newline per the spec.
    pub data: String,
}

impl Event {
    /// Whether this is the `[DONE]` sentinel OpenAI-shaped endpoints send.
    ///
    /// It is not JSON, so handing it to a parser produces a confusing error at
    /// the very end of an otherwise successful stream.
    pub fn is_done(&self) -> bool {
        self.data.trim() == "[DONE]"
    }
}

/// Accumulates bytes and yields complete events.
#[derive(Debug, Default)]
pub struct Parser {
    buffer: Vec<u8>,
}

impl Parser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk, returning every event it completed.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Event> {
        self.buffer.extend_from_slice(chunk);

        let mut events = Vec::new();
        // Events are separated by a blank line. Both CRLF and LF appear in the
        // wild — some proxies rewrite line endings.
        while let Some((end, skip)) = find_separator(&self.buffer) {
            let raw = self.buffer.drain(..end + skip).collect::<Vec<u8>>();
            if let Some(event) = decode(&raw[..end]) {
                events.push(event);
            }
        }
        events
    }

    /// Anything left after the stream ends.
    ///
    /// A final event without its trailing blank line is common enough that
    /// dropping it silently loses the last token of a response.
    pub fn finish(&mut self) -> Option<Event> {
        if self.buffer.is_empty() {
            return None;
        }
        let raw = std::mem::take(&mut self.buffer);
        decode(&raw)
    }
}

/// Index of the first event separator, and how many bytes it occupies.
fn find_separator(buffer: &[u8]) -> Option<(usize, usize)> {
    let mut index = 0;
    while index < buffer.len() {
        if buffer[index..].starts_with(b"\n\n") {
            return Some((index, 2));
        }
        if buffer[index..].starts_with(b"\r\n\r\n") {
            return Some((index, 4));
        }
        index += 1;
    }
    None
}

fn decode(raw: &[u8]) -> Option<Event> {
    // Lossy rather than strict: a chunk boundary can split a multi-byte
    // character, and losing a replacement character beats dropping the event.
    let text = String::from_utf8_lossy(raw);

    let mut event = Event::default();
    let mut data_lines: Vec<&str> = Vec::new();

    for line in text.lines() {
        let line = line.trim_end_matches('\r');

        // Comments and keep-alives. Proxies send bare `:` to hold the
        // connection open; treating one as an event produces a JSON error.
        if line.is_empty() || line.starts_with(':') {
            continue;
        }

        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };

        match field {
            "event" => event.name = Some(value.to_string()),
            "data" => data_lines.push(value),
            // `id` and `retry` are spec fields nothing here needs.
            _ => {}
        }
    }

    if data_lines.is_empty() && event.name.is_none() {
        return None;
    }
    event.data = data_lines.join("\n");
    Some(event)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(chunks: &[&str]) -> Vec<Event> {
        let mut parser = Parser::new();
        let mut events: Vec<Event> = chunks
            .iter()
            .flat_map(|chunk| parser.push(chunk.as_bytes()))
            .collect();
        events.extend(parser.finish());
        events
    }

    #[test]
    fn a_single_event_decodes() {
        let events = parse(&["data: {\"a\":1}\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, r#"{"a":1}"#);
    }

    #[test]
    fn the_event_name_is_captured() {
        // Anthropic dispatches on it; OpenAI omits it entirely.
        let events = parse(&["event: content_block_delta\ndata: {}\n\n"]);
        assert_eq!(events[0].name.as_deref(), Some("content_block_delta"));
    }

    #[test]
    fn a_chunk_boundary_mid_event_is_reassembled() {
        // The failure this parser exists to prevent.
        let events = parse(&["data: {\"tex", "t\":\"hel", "lo\"}\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, r#"{"text":"hello"}"#);
    }

    #[test]
    fn a_chunk_boundary_inside_the_separator_is_handled() {
        let events = parse(&["data: {}\n", "\n"]);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn several_events_in_one_chunk_all_come_out() {
        let events = parse(&["data: 1\n\ndata: 2\n\ndata: 3\n\n"]);
        assert_eq!(events.len(), 3);
        assert_eq!(events[2].data, "3");
    }

    #[test]
    fn crlf_line_endings_work() {
        // Some proxies rewrite them.
        let events = parse(&["event: x\r\ndata: {}\r\n\r\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name.as_deref(), Some("x"));
    }

    #[test]
    fn multi_line_data_is_joined_with_newlines() {
        let events = parse(&["data: line one\ndata: line two\n\n"]);
        assert_eq!(events[0].data, "line one\nline two");
    }

    #[test]
    fn comments_and_keepalives_are_ignored() {
        // A bare `:` holds the connection open; parsing it as JSON errors.
        let events = parse(&[": keep-alive\n\n", "data: {}\n\n"]);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn a_trailing_event_without_its_blank_line_is_not_lost() {
        // Otherwise the last token of a response silently disappears.
        let events = parse(&["data: {\"last\":true}"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, r#"{"last":true}"#);
    }

    #[test]
    fn the_done_sentinel_is_recognized_rather_than_parsed() {
        let events = parse(&["data: [DONE]\n\n"]);
        assert!(events[0].is_done());
        assert!(!Event { name: None, data: "{}".into() }.is_done());
    }

    #[test]
    fn a_split_utf8_sequence_does_not_panic() {
        let mut parser = Parser::new();
        let text = "data: \"héllo\"\n\n".as_bytes();
        // Split inside the two-byte é.
        let split = text.iter().position(|b| *b == 0xC3).unwrap() + 1;
        let mut events = parser.push(&text[..split]);
        events.extend(parser.push(&text[split..]));
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn a_value_without_a_leading_space_still_parses() {
        assert_eq!(parse(&["data:{}\n\n"])[0].data, "{}");
    }

    #[test]
    fn an_empty_stream_yields_nothing() {
        assert!(parse(&[]).is_empty());
        assert!(parse(&["\n\n"]).is_empty());
    }
}
