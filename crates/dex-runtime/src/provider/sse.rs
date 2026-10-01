//! Incremental Server-Sent Events parsing.
//!
//! Split out from the HTTP client so the part that is easy to get wrong, a
//! frame arriving across a chunk boundary, can be tested without a network.
//!
//! The rules that matter here: a frame is dispatched on a blank line; a frame
//! may carry several `data:` lines, which are joined with newlines; a line
//! beginning with `:` is a comment and is ignored; and anything after `data:`
//! has exactly one optional leading space removed, not all of them.

/// A decoded SSE frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field, or `"message"` when absent.
    pub event: String,
    /// The joined `data:` lines. Empty for a frame that carried none.
    pub data: String,
}

/// Accumulates bytes and yields complete frames.
#[derive(Debug, Default)]
pub struct SseParser {
    /// Raw bytes, not text. A multi-byte character can straddle a chunk
    /// boundary, and decoding eagerly would turn the half of it into a
    /// replacement character. Newlines cannot occur inside a multi-byte
    /// sequence, so splitting on them first is always safe.
    buffer: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
    /// A frame was seen but is still waiting for its blank line.
    open: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes, returning every frame that completed.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        // A partial UTF-8 sequence at a chunk boundary would panic on
        // `from_utf8`, so anything incomplete stays buffered for the next call.
        self.buffer.extend_from_slice(bytes);
        self.drain()
    }

    /// A server that closes mid-frame still has a complete event if the data
    /// line was terminated. This flushes anything left over.
    pub fn finish(&mut self) -> Option<SseEvent> {
        // Anything left without a newline is a final unterminated line.
        if self.buffer.is_empty() {
            return if self.open { self.dispatch() } else { None };
        }
        let line: Vec<u8> = std::mem::take(&mut self.buffer);
        let text = String::from_utf8_lossy(&line).into_owned();
        let line = text.trim_end_matches('\r');
        self.line(line);
        if self.open {
            self.dispatch()
        } else {
            None
        }
    }

    fn drain(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        // A line is complete once its newline has arrived, and a newline can
        // never be part of a multi-byte character, so each line decodes on its
        // own without risk of splitting a codepoint.
        while let Some(index) = self.buffer.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=index).collect();
            line.pop(); // the newline itself
            // Normalise CRLF so a proxy rewriting line endings cannot swallow
            // every frame.
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let text = String::from_utf8_lossy(&line).into_owned();
            if let Some(event) = self.line(&text) {
                out.push(event);
            }
        }
        out
    }

    /// Handle one line, returning a frame if the line completed one.
    fn line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            // A blank line dispatches, but only if a frame was actually open.
            return if self.open { self.dispatch() } else { None };
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value),
            // A line with no colon is a field with an empty value.
            None => (line, ""),
        };
        // Exactly one optional space is stripped, per the spec.
        let value = value.strip_prefix(' ').unwrap_or(value);

        match field {
            "event" => {
                self.event = Some(value.to_string());
                self.open = true;
            }
            "data" => {
                self.data.push(value.to_string());
                self.open = true;
            }
            // `id` and `retry` are not meaningful to a completion stream.
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        let event = self.event.take().unwrap_or_else(|| "message".to_string());
        let data = std::mem::take(&mut self.data).join("\n");
        self.open = false;
        Some(SseEvent { event, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(parser: &mut SseParser, text: &str) -> Vec<SseEvent> {
        parser.push(text.as_bytes())
    }

    #[test]
    fn a_single_frame_is_decoded() {
        let mut p = SseParser::new();
        assert_eq!(
            frames(&mut p, "data: {\"a\":1}\n\n"),
            vec![SseEvent {
                event: "message".into(),
                data: "{\"a\":1}".into()
            }]
        );
    }

    #[test]
    fn several_frames_in_one_chunk_are_all_returned() {
        let mut p = SseParser::new();
        let got = frames(&mut p, "data: one\n\ndata: two\n\ndata: three\n\n");
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].data, "one");
        assert_eq!(got[2].data, "three");
    }

    #[test]
    fn a_frame_split_across_chunks_is_reassembled() {
        let mut p = SseParser::new();
        // The interesting split is mid-line and mid-blank-line.
        assert!(frames(&mut p, "data: {\"content\":\"Hel").is_empty());
        assert!(frames(&mut p, "lo\"}\n").is_empty());
        let got = frames(&mut p, "\n");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].data, "{\"content\":\"Hello\"}");
    }

    #[test]
    fn a_frame_split_one_byte_at_a_time_still_works() {
        let mut p = SseParser::new();
        let mut all = Vec::new();
        for byte in b"data: hello\n\n" {
            all.extend(p.push(&[*byte]));
        }
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].data, "hello");
    }

    #[test]
    fn multi_byte_utf8_split_across_chunks_survives_intact() {
        let full = "data: héllo wörld ✓\n\n";
        let bytes = full.as_bytes();
        // Split at every possible offset: wherever the cuts fall, the frame
        // must reassemble byte-for-byte rather than gaining replacement
        // characters.
        for cut in 1..bytes.len() {
            let mut p = SseParser::new();
            let mut all = p.push(&bytes[..cut]);
            all.extend(p.push(&bytes[cut..]));
            all.extend(p.finish());
            assert_eq!(all.len(), 1, "split at {cut} produced {all:?}");
            assert_eq!(all[0].data, "héllo wörld ✓", "split at {cut}");
        }
    }

    #[test]
    fn comments_and_unknown_fields_are_ignored() {
        let mut p = SseParser::new();
        let got = frames(&mut p, ": keep-alive\nid: 7\nretry: 1000\ndata: payload\n\n");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].data, "payload");
    }

    #[test]
    fn a_named_event_is_reported() {
        let mut p = SseParser::new();
        let got = frames(&mut p, "event: delta\ndata: x\n\n");
        assert_eq!(got[0].event, "delta");
    }

    #[test]
    fn several_data_lines_are_joined_with_newlines() {
        let mut p = SseParser::new();
        let got = frames(&mut p, "data: one\ndata: two\n\n");
        assert_eq!(got[0].data, "one\ntwo");
    }

    #[test]
    fn only_one_leading_space_is_stripped() {
        let mut p = SseParser::new();
        let got = frames(&mut p, "data:  two spaces\n\n");
        assert_eq!(got[0].data, " two spaces");
    }

    #[test]
    fn crlf_line_endings_are_handled() {
        let mut p = SseParser::new();
        let got = frames(&mut p, "data: x\r\n\r\n");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].data, "x");
    }

    #[test]
    fn a_field_with_no_colon_is_an_empty_value() {
        let mut p = SseParser::new();
        let got = frames(&mut p, "data\n\n");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].data, "");
    }

    #[test]
    fn blank_lines_without_a_frame_produce_nothing() {
        let mut p = SseParser::new();
        assert!(frames(&mut p, "\n\n\n").is_empty());
    }

    #[test]
    fn finish_flushes_a_frame_whose_terminator_never_arrived() {
        let mut p = SseParser::new();
        assert!(frames(&mut p, "data: tail\n").is_empty());
        let got = p.finish().expect("flushed");
        assert_eq!(got.data, "tail");
        assert!(p.finish().is_none(), "only once");
    }
}