use companion_core::GeminiUsageMetadata;

pub const DEFAULT_MAX_SSE_EVENT_BYTES: usize = 2 * 1024 * 1024;
const RETAIN_BUFFER_CAPACITY_LIMIT: usize = 64 * 1024;
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

#[derive(Debug)]
pub struct SseUsageTracker {
    max_event_bytes: usize,
    line_buf: Vec<u8>,
    event_data: Vec<u8>,
    has_data_field: bool,
    pending_cr: bool,
    first_line_seen: bool,
    line_discarded: bool,
    event_discarded: bool,
    last_usage: Option<GeminiUsageMetadata>,
}

impl Default for SseUsageTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl SseUsageTracker {
    pub fn new() -> Self {
        Self::with_max_event_bytes(DEFAULT_MAX_SSE_EVENT_BYTES)
    }

    pub fn with_max_event_bytes(max_bytes: usize) -> Self {
        Self {
            max_event_bytes: max_bytes,
            line_buf: Vec::new(),
            event_data: Vec::new(),
            has_data_field: false,
            pending_cr: false,
            first_line_seen: false,
            line_discarded: false,
            event_discarded: false,
            last_usage: None,
        }
    }

    pub fn push_chunk(&mut self, mut chunk: &[u8]) {
        while !chunk.is_empty() {
            if self.pending_cr {
                self.pending_cr = false;
                if chunk[0] == b'\n' {
                    chunk = &chunk[1..];
                    continue;
                }
            }

            match chunk.iter().position(|&b| b == b'\n' || b == b'\r') {
                Some(delim_idx) => {
                    let segment = &chunk[..delim_idx];
                    let delim = chunk[delim_idx];
                    self.append_line_bytes(segment);
                    if delim == b'\r' {
                        self.pending_cr = true;
                    }
                    self.finish_line();
                    chunk = &chunk[delim_idx + 1..];
                }
                None => {
                    self.append_line_bytes(chunk);
                    break;
                }
            }
        }
    }

    pub fn last_usage(&self) -> Option<GeminiUsageMetadata> {
        self.last_usage
    }

    pub fn finish(&mut self) -> Option<GeminiUsageMetadata> {
        self.pending_cr = false;
        if self.line_discarded {
            self.first_line_seen = true;
            self.line_discarded = false;
        } else if !self.line_buf.is_empty() {
            let line = std::mem::take(&mut self.line_buf);
            self.process_line(&line);
        }
        self.flush_event();
        self.last_usage
    }

    fn append_line_bytes(&mut self, segment: &[u8]) {
        if self.line_discarded || segment.is_empty() {
            return;
        }

        if self.line_buf.len().saturating_add(segment.len()) > self.max_event_bytes {
            let mut prefix = [0u8; 4];
            let mut count = 0;
            for &b in self.line_buf.iter().chain(segment.iter()).take(4) {
                prefix[count] = b;
                count += 1;
            }
            let slice = &prefix[..count];
            let effective = if !self.first_line_seen {
                slice.strip_prefix(&UTF8_BOM).unwrap_or(slice)
            } else {
                slice
            };
            let is_comment = effective.first() == Some(&b':');

            self.line_buf = Vec::new();
            self.line_discarded = true;
            if !is_comment {
                self.event_data = Vec::new();
                self.has_data_field = false;
                self.event_discarded = true;
            }
            return;
        }

        self.line_buf.extend_from_slice(segment);
    }

    fn finish_line(&mut self) {
        if self.line_discarded {
            self.first_line_seen = true;
            self.line_discarded = false;
            return;
        }

        let mut line = std::mem::take(&mut self.line_buf);
        self.process_line(&line);
        if line.capacity() <= RETAIN_BUFFER_CAPACITY_LIMIT {
            line.clear();
            self.line_buf = line;
        }
    }

    fn process_line(&mut self, raw_line: &[u8]) {
        let line = if !self.first_line_seen {
            self.first_line_seen = true;
            raw_line.strip_prefix(&UTF8_BOM).unwrap_or(raw_line)
        } else {
            raw_line
        };

        if line.is_empty() {
            self.flush_event();
            return;
        }

        if line[0] == b':' {
            return;
        }

        let (field_name, field_value) = match line.iter().position(|&b| b == b':') {
            Some(colon) => {
                let raw_val = &line[colon + 1..];
                let val = raw_val.strip_prefix(b" ").unwrap_or(raw_val);
                (&line[..colon], val)
            }
            None => (line, &[][..]),
        };

        if field_name == b"data" {
            if self.event_discarded {
                return;
            }

            let extra = usize::from(self.has_data_field).saturating_add(field_value.len());
            if self.event_data.len().saturating_add(extra) > self.max_event_bytes {
                self.event_data = Vec::new();
                self.has_data_field = false;
                self.event_discarded = true;
                return;
            }

            if self.has_data_field {
                self.event_data.push(b'\n');
            }
            self.event_data.extend_from_slice(field_value);
            self.has_data_field = true;
        }
    }

    fn flush_event(&mut self) {
        if self.has_data_field
            && !self.event_discarded
            && !self.event_data.is_empty()
            && let Some(usage) = GeminiUsageMetadata::parse_from_json(&self.event_data)
        {
            self.last_usage = Some(usage);
        }

        if self.event_data.capacity() > RETAIN_BUFFER_CAPACITY_LIMIT {
            self.event_data = Vec::new();
        } else {
            self.event_data.clear();
        }
        self.has_data_field = false;
        self.event_discarded = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_usage_from_split_utf8_sse_chunks() {
        let payload = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello 🌍 世界 café\"}]}}],",
            "\"usageMetadata\":{\"promptTokenCount\":120,\"candidatesTokenCount\":30,",
            "\"cachedContentTokenCount\":80,\"thoughtsTokenCount\":12}}\n\n"
        )
        .as_bytes();

        // Split into 1-byte chunks so every multi-byte UTF-8 sequence is split across chunks.
        let mut tracker = SseUsageTracker::new();
        let mut reassembled = Vec::with_capacity(payload.len());
        for byte in payload {
            let chunk = std::slice::from_ref(byte);
            reassembled.extend_from_slice(chunk);
            tracker.push_chunk(chunk);
        }

        assert_eq!(reassembled.as_slice(), payload);
        let usage = tracker.finish().expect("usage metadata extracted");
        assert_eq!(
            usage,
            GeminiUsageMetadata {
                prompt_token_count: 120,
                candidates_token_count: 30,
                cached_content_token_count: 80,
                thoughts_token_count: 12,
            }
        );
    }

    #[test]
    fn retains_only_last_usage_metadata_never_summing_chunks() {
        let mut tracker = SseUsageTracker::new();
        tracker.push_chunk(
            b"data: {\"usageMetadata\":{\"promptTokenCount\":100,\"candidatesTokenCount\":10,\"cachedContentTokenCount\":50}}\n\n",
        );
        assert_eq!(
            tracker.last_usage(),
            Some(GeminiUsageMetadata {
                prompt_token_count: 100,
                candidates_token_count: 10,
                cached_content_token_count: 50,
                thoughts_token_count: 0,
            })
        );

        tracker.push_chunk(
            b"data: {\"usageMetadata\":{\"promptTokenCount\":100,\"candidatesTokenCount\":42,\"cachedContentTokenCount\":50,\"thoughtsTokenCount\":7}}\n\n",
        );
        // Trailing metadata-less chunk and [DONE] sentinel must preserve the last observed usage.
        tracker.push_chunk(b"data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n");
        tracker.push_chunk(b"data: [DONE]\n\n");

        let final_usage = tracker.finish().expect("last usage preserved");
        assert_eq!(
            final_usage,
            GeminiUsageMetadata {
                prompt_token_count: 100,
                candidates_token_count: 42,
                cached_content_token_count: 50,
                thoughts_token_count: 7,
            }
        );
    }

    #[test]
    fn handles_crlf_split_cr_standalone_cr_and_utf8_bom() {
        let mut tracker = SseUsageTracker::new();
        // Split UTF-8 BOM across chunks + CRLF split across chunks + comments + event/id/retry + standalone \r.
        tracker.push_chunk(&[0xEF]);
        tracker.push_chunk(&[0xBB, 0xBF]);
        tracker.push_chunk(b": keep-alive comment\r");
        tracker.push_chunk(b"\nevent: message\r\nid: 42\r\nretry: 1000\r\n");
        tracker.push_chunk(
            b"data: {\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":1}}\r",
        );
        tracker.push_chunk(b"\n\r\n");
        // Standalone \r delimiters for second event.
        tracker.push_chunk(
            b"data: {\"usageMetadata\":{\"promptTokenCount\":20,\"candidatesTokenCount\":9,\"cachedContentTokenCount\":15}}\r\r",
        );

        let usage = tracker.finish().expect("usage parsed across CRLF and BOM");
        assert_eq!(
            usage,
            GeminiUsageMetadata {
                prompt_token_count: 20,
                candidates_token_count: 9,
                cached_content_token_count: 15,
                thoughts_token_count: 0,
            }
        );
    }

    #[test]
    fn handles_no_space_multiline_data_and_unterminated_eof_idempotently() {
        let mut tracker = SseUsageTracker::new();
        // No space after colon.
        tracker.push_chunk(
            b"data:{\"usageMetadata\":{\"promptTokenCount\":11,\"candidatesTokenCount\":2}}\n\n",
        );
        assert_eq!(tracker.last_usage().map(|u| u.prompt_token_count), Some(11));

        // Multi-line data joined by '\n' with final line unterminated by newline at EOF.
        tracker.push_chunk(b"data: {\n");
        tracker.push_chunk(b"data:   \"usageMetadata\": {\n");
        tracker.push_chunk(b"data:\"promptTokenCount\": 99,\n");
        tracker.push_chunk(b"data: \"candidatesTokenCount\": 33\n");
        tracker.push_chunk(b"data:   }\n");
        tracker.push_chunk(b"data: }");

        let expected = Some(GeminiUsageMetadata {
            prompt_token_count: 99,
            candidates_token_count: 33,
            cached_content_token_count: 0,
            thoughts_token_count: 0,
        });
        assert_eq!(tracker.finish(), expected);
        // Calling finish() a second time must be idempotent.
        assert_eq!(tracker.finish(), expected);
    }

    #[test]
    fn enforces_bounded_buffer_on_oversized_data_and_resyncs_cleanly() {
        let mut tracker = SseUsageTracker::with_max_event_bytes(128);

        // 1. Oversized single data line (> 128 bytes) streamed in chunks must be dropped without OOM.
        tracker.push_chunk(b"data: ");
        for _ in 0..50 {
            tracker.push_chunk(b"0123456789abcdef");
        }
        tracker.push_chunk(b"\n");
        // Another data line in the same corrupted event must still be discarded until blank line.
        tracker.push_chunk(b"data: {\"usageMetadata\":{\"promptTokenCount\":999}}\n\n");
        assert_eq!(tracker.last_usage(), None);

        // 2. Subsequent valid event after blank line resyncs and parses cleanly.
        tracker.push_chunk(
            b"data: {\"usageMetadata\":{\"promptTokenCount\":55,\"candidatesTokenCount\":12}}\n\n",
        );
        assert_eq!(
            tracker.last_usage(),
            Some(GeminiUsageMetadata {
                prompt_token_count: 55,
                candidates_token_count: 12,
                cached_content_token_count: 0,
                thoughts_token_count: 0,
            })
        );

        // 3. Oversized multi-line event (each line <= 128, combined > 128) drops only itself and preserves prior usage.
        for _ in 0..5 {
            tracker.push_chunk(b"data: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n");
        }
        tracker.push_chunk(b"\n");
        assert_eq!(tracker.finish().map(|u| u.prompt_token_count), Some(55));
    }

    #[test]
    fn oversized_comment_does_not_corrupt_current_event() {
        let mut tracker = SseUsageTracker::with_max_event_bytes(96);

        // Leading BOM + oversized comment on line 1.
        tracker.push_chunk(&UTF8_BOM);
        tracker.push_chunk(b":");
        for _ in 0..30 {
            tracker.push_chunk(b"comment-padding-");
        }
        tracker.push_chunk(b"\n");

        // Multi-line event interrupted in the middle by an oversized comment line.
        tracker.push_chunk(b"data: {\"usageMetadata\":\n");
        tracker.push_chunk(b":");
        for _ in 0..30 {
            tracker.push_chunk(b"mid-event-comment-");
        }
        tracker.push_chunk(b"\n");
        tracker.push_chunk(b"data: {\"promptTokenCount\":77,\"candidatesTokenCount\":19}}\n\n");

        assert_eq!(
            tracker.finish(),
            Some(GeminiUsageMetadata {
                prompt_token_count: 77,
                candidates_token_count: 19,
                cached_content_token_count: 0,
                thoughts_token_count: 0,
            })
        );
    }
}
