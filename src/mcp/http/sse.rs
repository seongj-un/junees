//! Server-Sent Events, as far as an MCP response stream needs them: the
//! `data` of `message` events, read piece by piece (WHATWG HTML, "Event
//! stream interpretation"). `id` and `retry` are ignored: Junees does not
//! resume streams. Lines end with LF or CRLF; a lone CR is not a line end.

/// Collects events from the bytes of a stream.
#[derive(Default)]
pub struct Parser {
    /// The unfinished line.
    line: Vec<u8>,
    data: Vec<u8>,
    has_data: bool,
    event: Vec<u8>,
}

impl Parser {
    /// Reads `bytes` and appends the data of each finished `message`
    /// event to `events`.
    pub fn feed(&mut self, mut bytes: &[u8], events: &mut Vec<Vec<u8>>) {
        while let Some(end) = bytes.iter().position(|&b| b == b'\n') {
            self.line.extend_from_slice(&bytes[..end]);
            bytes = &bytes[end + 1..];
            let mut line = std::mem::take(&mut self.line);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.field(&line, events);
        }
        self.line.extend_from_slice(bytes);
    }

    fn field(&mut self, line: &[u8], events: &mut Vec<Vec<u8>>) {
        if line.is_empty() {
            self.dispatch(events);
            return;
        }
        if line[0] == b':' {
            return;
        }
        let (name, value) = match line.iter().position(|&b| b == b':') {
            Some(colon) => {
                let value = &line[colon + 1..];
                (&line[..colon], value.strip_prefix(b" ").unwrap_or(value))
            }
            None => (line, &[][..]),
        };
        match name {
            b"data" => {
                self.data.extend_from_slice(value);
                self.data.push(b'\n');
                self.has_data = true;
            }
            b"event" => self.event = value.to_vec(),
            _ => {}
        }
    }

    fn dispatch(&mut self, events: &mut Vec<Vec<u8>>) {
        let mut data = std::mem::take(&mut self.data);
        let event = std::mem::take(&mut self.event);
        if !std::mem::take(&mut self.has_data) {
            return;
        }
        data.pop();
        if (event.is_empty() || event == b"message") && !data.is_empty() {
            events.push(data);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(chunks: &[&[u8]]) -> Vec<String> {
        let mut parser = Parser::default();
        let mut events = Vec::new();
        for chunk in chunks {
            parser.feed(chunk, &mut events);
        }
        events
            .into_iter()
            .map(|event| String::from_utf8(event).unwrap())
            .collect()
    }

    #[test]
    fn events_are_read_across_chunks() {
        let stream: &[u8] = b"id: 1\ndata:\n\n: keep-alive\r\n\r\nevent: message\r\ndata: {\"a\":\r\ndata:1}\r\n\r\ndata: tail without end";
        // Every split point gives the same events.
        for cut in 0..stream.len() {
            assert_eq!(
                parse(&[&stream[..cut], &stream[cut..]]),
                ["{\"a\":\n1}"],
                "cut at {cut}"
            );
        }
        let bytes: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(parse(&bytes), ["{\"a\":\n1}"]);
    }

    #[test]
    fn other_events_and_fields_are_skipped() {
        assert_eq!(
            parse(&[b"event: ping\ndata: x\n\nretry: 10\ndata:y\nfoo: z\n\ndata\n\n"]),
            ["y"]
        );
        // Only one space after the colon is dropped; Hangul passes whole.
        assert_eq!(parse(&["data:  가\n\n".as_bytes()]), [" 가"]);
    }
}
