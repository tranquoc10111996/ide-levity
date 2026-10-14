//! Incremental Server-Sent Events parser (WHATWG `text/event-stream`).

/// One dispatched SSE event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    /// The `event:` field, if present.
    pub event: Option<String>,
    /// The concatenated `data:` lines.
    pub data: String,
}

/// Byte-oriented incremental parser; feed arbitrary chunks with [`SseParser::push`].
#[derive(Debug, Default)]
pub(crate) struct SseParser {
    buffer: Vec<u8>,
    event: Option<String>,
    data: Option<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds a chunk and returns every event completed by it.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        let mut start = 0;
        while let Some(pos) = self.buffer[start..].iter().position(|&b| b == b'\n') {
            let end = start + pos;
            let mut line = &self.buffer[start..end];
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            let line = String::from_utf8_lossy(line).into_owned();
            start = end + 1;
            if let Some(event) = self.process_line(&line) {
                events.push(event);
            }
        }
        self.buffer.drain(..start);
        events
    }

    /// Flushes a trailing event when the stream ends without a blank line.
    pub fn finish(&mut self) -> Option<SseEvent> {
        if !self.buffer.is_empty() {
            let rest = std::mem::take(&mut self.buffer);
            let line = String::from_utf8_lossy(&rest);
            let line = line.trim_end_matches('\r').to_string();
            if let Some(event) = self.process_line(&line) {
                return Some(event);
            }
        }
        self.dispatch()
    }

    fn process_line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => match &mut self.data {
                Some(data) => {
                    data.push('\n');
                    data.push_str(value);
                }
                None => self.data = Some(value.to_string()),
            },
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        let event = self.event.take();
        self.data.take().map(|data| SseEvent { event, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events_split_across_chunks_with_crlf_and_comments() {
        let mut p = SseParser::new();
        assert!(p.push(b": keep-alive\r\nevent: content_block_delta\r\nda").is_empty());
        let events = p.push(b"ta: {\"a\":1}\r\n\r\ndata: x\ndata: y\n\n");
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: Some("content_block_delta".into()),
                    data: "{\"a\":1}".into()
                },
                SseEvent {
                    event: None,
                    data: "x\ny".into()
                }
            ]
        );
    }

    #[test]
    fn utf8_split_inside_a_line_is_preserved_and_trailing_event_flushed() {
        let mut p = SseParser::new();
        let text = "data: Xin chào 🌍".as_bytes();
        let (a, b) = text.split_at(text.len() - 2);
        assert!(p.push(a).is_empty());
        assert!(p.push(b).is_empty());
        assert_eq!(p.finish().unwrap().data, "Xin chào 🌍");
        assert_eq!(p.finish(), None);
    }
}
