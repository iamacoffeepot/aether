//! Raw bytes plus a line-span index. Only painted (or searched) rows parse.

use std::str;

use super::event;

/// Default retained complete lines. Configurable via [`LineBuffer::with_cap`].
pub const DEFAULT_CAP: usize = 8_192;

struct LineSpan {
    start: usize,
    end: usize,
}

/// Capped byte buffer with lazy per-line collapse.
pub struct LineBuffer {
    bytes: Vec<u8>,
    spans: Vec<LineSpan>,
    parsed: Vec<Option<String>>,
    cap: usize,
    dropped: usize,
    parse_count: usize,
}

impl LineBuffer {
    #[must_use]
    pub fn new() -> Self {
        Self::with_cap(DEFAULT_CAP)
    }

    #[must_use]
    pub fn with_cap(cap: usize) -> Self {
        Self { bytes: Vec::new(), spans: Vec::new(), parsed: Vec::new(), cap: cap.max(1), dropped: 0, parse_count: 0 }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    #[must_use]
    pub fn dropped(&self) -> usize {
        self.dropped
    }

    /// How many lines have been collapsed. Zero until a caller paints or searches.
    #[must_use]
    pub fn parse_count(&self) -> usize {
        self.parse_count
    }

    /// Absolute id of `index`, stable across a head trim.
    #[must_use]
    pub fn abs_id(&self, index: usize) -> u64 {
        u64::try_from(self.dropped.saturating_add(index)).unwrap_or(u64::MAX)
    }

    #[must_use]
    pub fn index_of(&self, id: u64) -> Option<usize> {
        let floor = u64::try_from(self.dropped).unwrap_or(u64::MAX);
        let index = usize::try_from(id.checked_sub(floor)?).ok()?;
        (index < self.spans.len()).then_some(index)
    }

    #[must_use]
    pub fn last_id(&self) -> Option<u64> {
        (!self.spans.is_empty()).then(|| self.abs_id(self.spans.len() - 1))
    }

    /// Banner when the cap has trimmed the head. Absent until a line is dropped.
    #[must_use]
    pub fn banner(&self) -> Option<String> {
        (self.dropped > 0).then(|| format!("{} earlier lines dropped", self.dropped))
    }

    pub fn push_line(&mut self, line: &str) {
        let start = self.bytes.len();
        self.bytes.extend_from_slice(line.as_bytes());
        let end = self.bytes.len();
        self.bytes.push(b'\n');
        self.spans.push(LineSpan { start, end });
        self.parsed.push(None);
        self.trim();
    }

    #[must_use]
    pub fn raw(&self, index: usize) -> Option<&str> {
        let span = self.spans.get(index)?;
        str::from_utf8(&self.bytes[span.start..span.end]).ok()
    }

    /// Collapse `index` on first call; later calls reuse the cache.
    pub fn collapsed(&mut self, index: usize) -> Option<&str> {
        if self.parsed.get(index)?.is_none() {
            let raw = self.raw(index)?;
            let text = event::collapse(raw);
            self.parsed[index] = Some(text);
            self.parse_count = self.parse_count.saturating_add(1);
        }
        self.parsed.get(index).and_then(Option::as_deref)
    }

    #[must_use]
    pub fn expanded(&self, index: usize) -> Option<String> {
        self.raw(index).map(event::expand)
    }

    fn trim(&mut self) {
        let extra = self.spans.len().saturating_sub(self.cap);
        if extra == 0 {
            return;
        }
        let cut = self.spans[extra].start;
        self.bytes.drain(..cut);
        self.spans.drain(..extra);
        self.parsed.drain(..extra);
        for span in &mut self.spans {
            span.start = span.start.saturating_sub(cut);
            span.end = span.end.saturating_sub(cut);
        }
        self.dropped = self.dropped.saturating_add(extra);
    }
}

impl Default for LineBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::LineBuffer;

    #[test]
    fn parsing_happens_only_for_painted_spans() {
        // The plausible bug: ingest JSON-decodes every line, so a megabyte
        // transcript pays a full decode before the first frame.
        let mut buffer = LineBuffer::with_cap(8_000);
        for index in 0..5_000 {
            buffer.push_line(&format!(
                r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{index}"}}]}}}}"#
            ));
        }
        assert_eq!(buffer.parse_count(), 0, "ingest must not parse");
        assert_eq!(buffer.len(), 5_000);

        let painted = [10_usize, 11, 12];
        for index in painted {
            let _ = buffer.collapsed(index);
        }
        assert_eq!(buffer.parse_count(), painted.len(), "only the painted span may parse");
        let _ = buffer.collapsed(10);
        assert_eq!(buffer.parse_count(), painted.len(), "a second paint of the same row is a cache hit");
    }

    #[test]
    fn a_malformed_line_renders_raw_and_the_cap_names_what_it_dropped() {
        // The plausible bug: a non-JSON or truncated tail is skipped, so a
        // killed lane's last line vanishes; a silent cap looks complete.
        let mut buffer = LineBuffer::with_cap(3);
        buffer.push_line(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"keep"}]}}"#);
        buffer.push_line("not-json {");
        buffer.push_line(r#"{"type":"mystery","payload":1}"#);
        buffer.push_line(r#"{"incomplete":"#);

        assert_eq!(buffer.dropped(), 1);
        assert_eq!(buffer.banner().as_deref(), Some("1 earlier lines dropped"));
        assert_eq!(buffer.collapsed(0), Some("not-json {"));
        assert_eq!(buffer.collapsed(1), Some("mystery"));
        assert_eq!(buffer.collapsed(2).map(str::to_owned).as_deref(), Some(r#"{"incomplete":"#));
    }
}
