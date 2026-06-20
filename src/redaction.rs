//! Image-URI redaction primitives shared across every logging/echoing surface.
//!
//! These are intentionally NOT vision-specific: the inbound request trace
//! (`http::redact_payload_secrets`), the upstream JSONL request log, the debug
//! monitor + `/debug/ws` broadcast, and the vision success/error text all route
//! through the SAME redactor so a `data:` image payload or a signed image URL
//! cannot leak to any sink (AGENTS.md redact rule). Keeping the URI semantics in
//! one module means there is exactly one definition of "what is a sensitive URI
//! run" to audit.
//!
//! The vision module re-exports [`redact_image_uris`], [`redact_image_uris_in_value`],
//! and [`redact_vision_text`] so existing `crate::vision::*` call sites keep
//! resolving; new code should prefer `crate::redaction::*`.

/// Cap on the redacted vision text snippet that becomes model-visible/logged.
const VISION_TEXT_REDACT_LIMIT: usize = 4096;

/// URI prefixes that could carry raw image data or a signed image URL, in BOTH
/// raw and JSON-escaped (`\/`) forms (G4 round-3 #4). Matched case-insensitively
/// (round-2 #2). `data:` base64 payloads have no slash to escape. Order matters
/// only for disambiguation; we pick the earliest match regardless.
const SENSITIVE_URI_PREFIXES: [&str; 5] = [
    "data:",
    "https://",
    "http://",
    "https:\\/\\/",
    "http:\\/\\/",
];

/// Whether `c` ends a sensitive URI run. A `data:` base64 URL contains a `,`
/// SEPARATING the media type from the payload, so `,` must not terminate it;
/// only whitespace/quote/bracket bounds a `data:` run. For an `http(s)` URL a
/// comma/paren also bounds it in prose/JSON. A backslash is NOT a delimiter so
/// JSON-escaped `\/` inside a URL is consumed as part of the run.
fn is_uri_run_delimiter(c: char, is_data: bool) -> bool {
    c.is_whitespace()
        || matches!(c, '"' | '\'' | ']' | '}' | '<' | '>')
        || (!is_data && matches!(c, ')' | ','))
}

/// THE single image-redaction primitive (G4 round-4 consolidation). Replaces
/// every `data:` and `http(s)` URI run — case-insensitive, raw AND JSON-escaped
/// (`\/`) form, including signed-URL query tokens — with `<redacted uri>`. This
/// is the one place the URI semantics live; ALL logging/echoing surfaces route
/// through it (inbound trace, upstream JSONL, debug monitor + `/debug/ws`, and
/// vision success/error text), so request image bytes / signed URLs cannot leak
/// to any sink (AGENTS.md redact rule). Does NOT truncate — callers that need a
/// length cap layer it on (see [`redact_vision_text`]).
pub fn redact_image_uris(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    while cursor < text.len() {
        // Earliest sensitive URI start at/after `cursor` (case-insensitive via
        // the lowercased copy, which preserves byte offsets for ASCII prefixes).
        let next = SENSITIVE_URI_PREFIXES
            .iter()
            .filter_map(|prefix| {
                lower[cursor..]
                    .find(prefix)
                    .map(|rel| (cursor + rel, *prefix))
            })
            .min_by_key(|(pos, _)| *pos);
        let Some((start, prefix)) = next else {
            out.push_str(&text[cursor..]);
            break;
        };
        out.push_str(&text[cursor..start]);
        out.push_str("<redacted uri>");
        let is_data = prefix.starts_with("data:");
        let after = &text[start + prefix.len()..];
        let end = after
            .find(|c: char| is_uri_run_delimiter(c, is_data))
            .unwrap_or(after.len());
        cursor = start + prefix.len() + end;
    }
    out
}

/// Recursively redact image URIs in every string within a JSON value (G4
/// round-4 consolidation). Used by the request-logging surfaces (inbound trace
/// `redact_payload_secrets`, upstream JSONL) so a `data:`/signed `image_url`
/// anywhere in the body — string field, content-part, nested object/array — is
/// stripped before serialization, regardless of key name.
pub fn redact_image_uris_in_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => {
            let redacted = redact_image_uris(text);
            if redacted != *text {
                *text = redacted;
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_image_uris_in_value(item);
            }
        }
        serde_json::Value::Object(map) => {
            for (_, item) in map.iter_mut() {
                redact_image_uris_in_value(item);
            }
        }
        _ => {}
    }
}

/// Vision text that becomes model-visible or logged — the successful
/// `VisionOutcome.text` (round-3 #3) and error bodies/messages (review #3,
/// round-2 #2, round-3 #4): image URIs redacted via [`redact_image_uris`], then
/// UTF-8-safely capped so only a bounded, image-free, token-free remainder
/// survives.
pub fn redact_vision_text(text: &str) -> String {
    let trimmed = redact_image_uris(text);
    let trimmed = trimmed.trim();
    if trimmed.chars().count() > VISION_TEXT_REDACT_LIMIT {
        let end = trimmed
            .char_indices()
            .nth(VISION_TEXT_REDACT_LIMIT)
            .map(|(idx, _)| idx)
            .unwrap_or(trimmed.len());
        format!("{}…[truncated]", &trimmed[..end])
    } else {
        trimmed.to_string()
    }
}

// ===========================================================================
// Secret-key authority + the capped, redacting STREAMING capture primitive.
//
// These were consolidated here from `http.rs` / `dashboard_flow.rs` (D1 R1 #10)
// so there is ONE definition of "what key is sensitive" and ONE capped+redacting
// body serializer, reused by the inbound trace logger and the dashboard FlowStore
// capture seam. The capture primitive is O(CAP): it never materializes the whole
// body (no full `serde_json::Value`), redacts secrets inline (sensitive keys →
// `"[redacted]"`, image/data URIs stripped INCLUDING `\uXXXX`-escaped forms,
// over-long scalars + keys capped), and on malformed/too-deep/non-UTF8 input
// falls back to `redact_image_uris` over a CAP-bounded lossy prefix only.
// ===========================================================================

/// THE single authority for "is this object key / header name sensitive" (its
/// value must never be retained). Used by the inbound-trace logger
/// (`http::redact_payload_secrets`) and the dashboard capture seam alike.
pub(crate) fn is_sensitive_payload_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['-', '_'], "");
    matches!(
        normalized.as_str(),
        "apikey"
            | "xapikey"
            | "authorization"
            | "password"
            | "passwd"
            | "secret"
            | "clientsecret"
            | "accesstoken"
            | "refreshtoken"
            | "authtoken"
            | "bearertoken"
            // `openai-beta` carries feature-gating tokens; redact its value in
            // captured headers (D1 R1 #2). Also covers a JSON `openai_beta` field.
            | "openaibeta"
    )
}

/// Capped + redacting capture of a request/response body into an owned, redacted
/// `Vec<u8>` of at most `body_cap` bytes. Secrets are redacted INLINE; peak heap
/// use is O(`body_cap` + `scalar_cap`), never O(body) — a 10 MiB body is never
/// copied in full. Never retains a slice of `raw`. Callers wrap the result in an
/// `Arc<[u8]>` as needed (the dashboard FlowStore does).
pub(crate) fn capture_capped_redacted(raw: &[u8], body_cap: usize, scalar_cap: usize) -> Vec<u8> {
    // Common path: a single streaming pass over the bytes into a hard-capped
    // writer that pre-reserves only `min(raw.len(), body_cap)` and stops at the
    // cap. No `serde_json::Value` is ever built.
    let mut writer = CappedWriter::new(raw.len(), body_cap);
    if let Ok(text) = std::str::from_utf8(raw) {
        let mut parser = JsonRedactor::new(text, scalar_cap);
        parser.skip_ws();
        if parser.redact_value(&mut writer, 0, false) && parser.at_trailing_end() {
            return writer.into_vec();
        }
    }
    // Fallback (RARE — malformed / non-JSON / non-UTF8 / too-deep): redact image
    // URIs over a `body_cap`-bounded lossy prefix ONLY. This is still O(CAP) — it
    // never parses a `serde_json::Value` (which would be O(body)). The lossy
    // conversion already yields valid UTF-8, so the post-redaction truncate handles
    // the char boundary; bound the slice taken from `raw` first to stay O(CAP).
    let prefix = &raw[..raw.len().min(body_cap)];
    let lossy = String::from_utf8_lossy(prefix);
    let redacted = redact_image_uris(&lossy);
    let mut bytes = redacted.into_bytes();
    truncate_bytes_to_cap(&mut bytes, body_cap);
    bytes
}

/// Redact header VALUES whose normalized name is sensitive (via
/// [`is_sensitive_payload_key`]) to `"[redacted]"`; for every other header, cap the
/// value to `scalar_cap` bytes (char boundary) AND run [`redact_image_uris`] over
/// it (a signed/`data:` URI can appear in ANY header value — D1 R1 #2). The header
/// NAME is preserved (not a secret).
pub(crate) fn redact_headers_capped(
    headers: &axum::http::HeaderMap,
    scalar_cap: usize,
) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            let name = name.as_str().to_string();
            if is_sensitive_payload_key(&name) {
                return (name, "[redacted]".to_string());
            }
            let raw = value.to_str().unwrap_or("<non-utf8>");
            let capped = cap_str_on_char_boundary(raw, scalar_cap);
            // Strip data:/signed image URIs from the (capped) value.
            (name, redact_image_uris(capped))
        })
        .collect()
}

/// Cap `text` to at most `cap` bytes on a UTF-8 char boundary (no allocation when
/// already within cap).
fn cap_str_on_char_boundary(text: &str, cap: usize) -> &str {
    if text.len() <= cap {
        return text;
    }
    let bytes = text.as_bytes();
    let mut end = cap;
    while end > 0 && (bytes[end] & 0xC0) == 0x80 {
        end -= 1;
    }
    &text[..end]
}

/// Truncate an owned byte buffer to `cap` on a UTF-8 char boundary in place.
fn truncate_bytes_to_cap(bytes: &mut Vec<u8>, cap: usize) {
    if bytes.len() > cap {
        let mut end = cap;
        while end > 0 && (bytes[end] & 0xC0) == 0x80 {
            end -= 1;
        }
        bytes.truncate(end);
    }
}

/// A `Vec<u8>` that accepts writes only until it reaches its cap; once full it
/// silently drops further writes and records `full = true`. Pre-reserves only
/// `min(hint, cap)` so peak allocation is bounded by the cap.
struct CappedWriter {
    buf: Vec<u8>,
    cap: usize,
    full: bool,
}

impl CappedWriter {
    fn new(size_hint: usize, cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(size_hint.min(cap)),
            cap,
            full: false,
        }
    }

    /// Append `bytes`, stopping at the cap. Returns `false` once the writer is full.
    fn write(&mut self, bytes: &[u8]) -> bool {
        if self.full {
            return false;
        }
        let remaining = self.cap - self.buf.len();
        if bytes.len() <= remaining {
            self.buf.extend_from_slice(bytes);
            if self.buf.len() == self.cap {
                self.full = true;
            }
            true
        } else {
            self.buf.extend_from_slice(&bytes[..remaining]);
            self.full = true;
            false
        }
    }

    fn write_byte(&mut self, byte: u8) -> bool {
        self.write(&[byte])
    }

    fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

/// Forward-pass recursive-descent JSON redactor over a `&str`. Walks the value at
/// the cursor writing a redacted copy into a [`CappedWriter`], never building a
/// `Value`. Returns `false` on malformed input or depth overflow so the caller can
/// fall back. `scalar_cap` bounds retained string VALUES and object KEYS.
struct JsonRedactor<'a> {
    bytes: &'a [u8],
    pos: usize,
    scalar_cap: usize,
}

/// Recursion-depth limit — bounds the call stack on adversarial nesting; deeper
/// input falls back to the best-effort path.
const MAX_JSON_DEPTH: usize = 128;

impl<'a> JsonRedactor<'a> {
    fn new(text: &'a str, scalar_cap: usize) -> Self {
        Self {
            bytes: text.as_bytes(),
            pos: 0,
            scalar_cap,
        }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() {
            match self.bytes[self.pos] {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    /// After parsing the top-level value, only whitespace may remain.
    fn at_trailing_end(&mut self) -> bool {
        self.skip_ws();
        self.pos >= self.bytes.len()
    }

    /// Redact one JSON value at the cursor into `out`. When `sensitive` is set the
    /// value belongs to a sensitive key, so it is replaced wholesale with
    /// `"[redacted]"` and parse-skipped. Returns `false` on malformed input or when
    /// the depth limit is exceeded.
    fn redact_value(&mut self, out: &mut CappedWriter, depth: usize, sensitive: bool) -> bool {
        if depth > MAX_JSON_DEPTH {
            return false;
        }
        self.skip_ws();
        let Some(byte) = self.peek() else {
            return false;
        };
        if sensitive {
            out.write(b"\"[redacted]\"");
            return self.skip_value(depth);
        }
        match byte {
            b'{' => self.redact_object(out, depth),
            b'[' => self.redact_array(out, depth),
            b'"' => self.redact_string(out),
            _ => self.copy_scalar(out),
        }
    }

    fn redact_object(&mut self, out: &mut CappedWriter, depth: usize) -> bool {
        self.pos += 1; // consume '{'
        out.write_byte(b'{');
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            out.write_byte(b'}');
            return true;
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return false;
            }
            // Emit the key CAPPED (D1 R1 #4a: a huge key must not allocate O(body))
            // and decode only a `scalar_cap`-bounded prefix for the sensitivity test.
            let key = match self.read_key_string(out) {
                Some(key) => key,
                None => return false,
            };
            self.skip_ws();
            if self.peek() != Some(b':') {
                return false;
            }
            self.pos += 1;
            out.write_byte(b':');
            let sensitive = is_sensitive_payload_key(&key);
            if !self.redact_value(out, depth + 1, sensitive) {
                return false;
            }
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    out.write_byte(b',');
                }
                Some(b'}') => {
                    self.pos += 1;
                    out.write_byte(b'}');
                    return true;
                }
                _ => return false,
            }
        }
    }

    fn redact_array(&mut self, out: &mut CappedWriter, depth: usize) -> bool {
        self.pos += 1; // consume '['
        out.write_byte(b'[');
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            out.write_byte(b']');
            return true;
        }
        loop {
            if !self.redact_value(out, depth + 1, false) {
                return false;
            }
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    out.write_byte(b',');
                }
                Some(b']') => {
                    self.pos += 1;
                    out.write_byte(b']');
                    return true;
                }
                _ => return false,
            }
        }
    }

    /// Read a JSON string key. Emits a CAPPED, re-encoded key (so a multi-MiB key
    /// allocates only `scalar_cap`) and returns the decoded+capped key text for the
    /// sensitivity test. Cursor lands just past the closing quote.
    fn read_key_string(&mut self, out: &mut CappedWriter) -> Option<String> {
        let range = self.scan_string_raw()?;
        let raw = self.token_str(range);
        let inner = &raw[1..raw.len() - 1];
        // Decode only a bounded prefix of the key (keys are short in practice; this
        // bounds the worst case at `scalar_cap`).
        let capped_inner = cap_json_string_inner(inner, self.scalar_cap);
        let decoded = decode_json_string_inner(capped_inner);
        // Re-encode the (decoded, bounded) key as a valid JSON string for output.
        out.write(encode_json_string(&decoded).as_bytes());
        Some(decoded)
    }

    /// Redact a JSON string VALUE: decode a `scalar_cap`-bounded prefix (so
    /// `\uXXXX`-escaped `data:`/`https:` URIs are de-escaped — D1 R1 #3), run
    /// [`redact_image_uris`] over the DECODED text, then re-encode as a valid JSON
    /// string. The token is a zero-copy slice — a 10 MiB value allocates only the
    /// bounded prefix. Cursor lands just past the closing quote.
    fn redact_string(&mut self, out: &mut CappedWriter) -> bool {
        let Some(range) = self.scan_string_raw() else {
            return false;
        };
        let raw = self.token_str(range);
        let inner = &raw[1..raw.len() - 1];
        let capped_inner = cap_json_string_inner(inner, self.scalar_cap);
        let decoded = decode_json_string_inner(capped_inner);
        let redacted = redact_image_uris(&decoded);
        out.write(encode_json_string(&redacted).as_bytes());
        true
    }

    /// Copy a JSON scalar (number / `true` / `false` / `null`) through verbatim.
    fn copy_scalar(&mut self, out: &mut CappedWriter) -> bool {
        let start = self.pos;
        while self.pos < self.bytes.len() {
            match self.bytes[self.pos] {
                b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r' => break,
                _ => self.pos += 1,
            }
        }
        if self.pos == start {
            return false;
        }
        out.write(&self.bytes[start..self.pos]);
        true
    }

    /// Parse-skip a JSON value WITHOUT emitting it (a sensitive value whose
    /// redaction marker was already written). Returns `false` on malformed input or
    /// depth overflow.
    fn skip_value(&mut self, depth: usize) -> bool {
        if depth > MAX_JSON_DEPTH {
            return false;
        }
        self.skip_ws();
        match self.peek() {
            Some(b'{') => {
                self.pos += 1;
                self.skip_ws();
                if self.peek() == Some(b'}') {
                    self.pos += 1;
                    return true;
                }
                loop {
                    self.skip_ws();
                    if self.scan_string_raw().is_none() {
                        return false;
                    }
                    self.skip_ws();
                    if self.peek() != Some(b':') {
                        return false;
                    }
                    self.pos += 1;
                    if !self.skip_value(depth + 1) {
                        return false;
                    }
                    self.skip_ws();
                    match self.peek() {
                        Some(b',') => self.pos += 1,
                        Some(b'}') => {
                            self.pos += 1;
                            return true;
                        }
                        _ => return false,
                    }
                }
            }
            Some(b'[') => {
                self.pos += 1;
                self.skip_ws();
                if self.peek() == Some(b']') {
                    self.pos += 1;
                    return true;
                }
                loop {
                    if !self.skip_value(depth + 1) {
                        return false;
                    }
                    self.skip_ws();
                    match self.peek() {
                        Some(b',') => self.pos += 1,
                        Some(b']') => {
                            self.pos += 1;
                            return true;
                        }
                        _ => return false,
                    }
                }
            }
            Some(b'"') => self.scan_string_raw().is_some(),
            Some(_) => {
                let start = self.pos;
                while self.pos < self.bytes.len() {
                    match self.bytes[self.pos] {
                        b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r' => break,
                        _ => self.pos += 1,
                    }
                }
                self.pos != start
            }
            None => false,
        }
    }

    /// Scan a JSON string token INCLUDING the surrounding quotes, honoring `\"`/`\\`
    /// escapes. Returns the `start..end` BYTE RANGE (zero-copy) or `None` if
    /// unterminated. Cursor lands just past the closing quote. Returning a RANGE
    /// (not an owned `String`) is what keeps capture O(CAP): a 10 MiB value is never
    /// copied in full.
    fn scan_string_raw(&mut self) -> Option<(usize, usize)> {
        if self.peek() != Some(b'"') {
            return None;
        }
        let start = self.pos;
        self.pos += 1; // opening quote
        while self.pos < self.bytes.len() {
            match self.bytes[self.pos] {
                b'\\' => self.pos += 2,
                b'"' => {
                    self.pos += 1;
                    return Some((start, self.pos));
                }
                _ => self.pos += 1,
            }
        }
        None
    }

    /// The validated `&str` for a scanned string-token range (valid UTF-8: the
    /// bytes came from a `&str` and we split only on ASCII boundaries).
    fn token_str(&self, range: (usize, usize)) -> &'a str {
        std::str::from_utf8(&self.bytes[range.0..range.1]).unwrap_or("")
    }
}

/// Cap the INNER text (between the quotes) of a JSON string to `cap` bytes without
/// splitting a UTF-8 sequence or a trailing JSON escape (`\`): if the cut lands
/// right after a lone backslash, back off one byte so decoding stays well-formed.
fn cap_json_string_inner(inner: &str, cap: usize) -> &str {
    if inner.len() <= cap {
        return inner;
    }
    let bytes = inner.as_bytes();
    let mut end = cap;
    while end > 0 && (bytes[end] & 0xC0) == 0x80 {
        end -= 1;
    }
    // Drop a dangling odd backslash run so the trailing escape is not truncated.
    let mut backslashes = 0;
    let mut i = end;
    while i > 0 && bytes[i - 1] == b'\\' {
        backslashes += 1;
        i -= 1;
    }
    if backslashes % 2 == 1 {
        end -= 1;
    }
    &inner[..end]
}

/// Decode the INNER text of a JSON string (escapes → chars), enough for image-URI
/// redaction + key sensitivity. Handles the standard JSON escapes incl. `\uXXXX`
/// (with surrogate-pair joining) so an escaped `data:`/`https:` scheme de-escapes
/// to its raw form and is caught by [`redact_image_uris`] (D1 R1 #3). Unknown
/// escapes pass through.
fn decode_json_string_inner(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('b') => out.push('\u{0008}'),
            Some('f') => out.push('\u{000C}'),
            Some('u') => {
                let Some(hi) = take_hex4(&mut chars) else {
                    continue;
                };
                // High surrogate: try to join a following `\uXXXX` low surrogate.
                if (0xD800..=0xDBFF).contains(&hi) {
                    if chars.peek() == Some(&'\\') {
                        let mut lookahead = chars.clone();
                        lookahead.next(); // backslash
                        if lookahead.peek() == Some(&'u') {
                            lookahead.next(); // 'u'
                            if let Some(lo) = take_hex4(&mut lookahead)
                                && (0xDC00..=0xDFFF).contains(&lo)
                            {
                                let c = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                                if let Some(decoded) = char::from_u32(c) {
                                    out.push(decoded);
                                }
                                chars = lookahead;
                                continue;
                            }
                        }
                    }
                    // Unpaired high surrogate: emit replacement char.
                    out.push('\u{FFFD}');
                } else if let Some(decoded) = char::from_u32(hi) {
                    out.push(decoded);
                }
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// Consume exactly 4 hex digits from `chars`, returning their value, or `None`.
fn take_hex4(chars: &mut impl Iterator<Item = char>) -> Option<u32> {
    let mut value = 0u32;
    for _ in 0..4 {
        let digit = chars.next()?.to_digit(16)?;
        value = (value << 4) | digit;
    }
    Some(value)
}

/// Encode `text` as a JSON string literal INCLUDING surrounding quotes. The input
/// is already bounded (≤ scalar_cap), so this small `serde_json` call is cheap and
/// produces correct escaping for control chars / quotes / backslashes.
fn encode_json_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn is_sensitive_payload_key_covers_aliases_and_openai_beta() {
        for key in [
            "api_key",
            "API-KEY",
            "x-api-key",
            "Authorization",
            "openai-beta",
            "openai_beta",
            "client_secret",
            "refresh_token",
        ] {
            assert!(is_sensitive_payload_key(key), "{key} must be sensitive");
        }
        for key in ["model", "content", "messages", "x-request-id"] {
            assert!(
                !is_sensitive_payload_key(key),
                "{key} must NOT be sensitive"
            );
        }
    }

    #[test]
    fn capture_redacts_sensitive_keys_and_escaped_uris() {
        // Sensitive key value redacted; a `\uXXXX`-escaped image scheme de-escaped
        // then stripped; structure preserved.
        let esc_data: String = "data"
            .chars()
            .map(|c| format!("\\u{:04x}", c as u32))
            .collect();
        let body = format!(
            r#"{{"model":"m","api_key":"sk-LEAK","img":"{esc_data}:image/png;base64,ESCLEAK x"}}"#
        );
        let out = capture_capped_redacted(body.as_bytes(), 128 * 1024, 4 * 1024);
        let text = String::from_utf8_lossy(&out);
        assert!(!text.contains("sk-LEAK"), "api_key redacted");
        assert!(!text.contains("ESCLEAK"), "escaped data: uri redacted");
        assert!(text.contains("[redacted]"));
        assert!(text.contains("<redacted uri>"));
        // Output is still valid JSON.
        let _: serde_json::Value = serde_json::from_slice(&out).expect("valid json out");
    }

    #[test]
    fn capture_respects_configurable_caps() {
        // A tiny body_cap bounds the OUTPUT; a tiny scalar_cap bounds the retained
        // string. Both are honored (the primitive is parameterized for reuse).
        let body = format!("{{\"s\":\"{}\"}}", "z".repeat(10_000));
        let out = capture_capped_redacted(body.as_bytes(), 256, 64);
        assert!(out.len() <= 256, "body_cap honored: {} > 256", out.len());
    }

    #[test]
    fn capture_fallback_redacts_non_json_without_value() {
        // Malformed/non-JSON input → image URIs still stripped over a bounded prefix.
        let raw = b"not json data:image/png;base64,RAWLEAK trailing";
        let out = capture_capped_redacted(raw, 128 * 1024, 4 * 1024);
        let text = String::from_utf8_lossy(&out);
        assert!(!text.contains("RAWLEAK"), "non-json image uri redacted");
        assert!(text.contains("<redacted uri>"));
    }

    #[test]
    fn redact_headers_capped_redacts_sensitive_and_uri_values() {
        use axum::http::HeaderMap;
        use axum::http::HeaderName;
        use axum::http::HeaderValue;
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer HDRLEAK"),
        );
        headers.insert(
            HeaderName::from_static("openai-beta"),
            HeaderValue::from_static("tok=BETALEAK"),
        );
        headers.insert(
            HeaderName::from_static("x-cb"),
            HeaderValue::from_static("https://x/y?sig=URLLEAK"),
        );
        let out = redact_headers_capped(&headers, 4 * 1024);
        let dumped = format!("{out:?}");
        assert!(!dumped.contains("HDRLEAK"));
        assert!(!dumped.contains("BETALEAK"), "openai-beta value redacted");
        assert!(
            !dumped.contains("URLLEAK"),
            "uri-bearing header value redacted"
        );
    }

    #[test]
    fn redact_vision_text_strips_data_uris_and_caps_length() {
        let body = "error before data:image/png;base64,AAAABBBBCCCC after";
        let redacted = redact_vision_text(body);
        assert!(
            !redacted.contains("AAAABBBBCCCC"),
            "base64 payload stripped"
        );
        assert!(!redacted.contains("data:image"), "data uri stripped");
        assert!(redacted.contains("<redacted uri>"));
        assert!(redacted.contains("error before"));
        assert!(redacted.contains("after"));

        // UTF-8-safe length cap (limit + the truncation marker).
        let long = "x".repeat(VISION_TEXT_REDACT_LIMIT + 500);
        let capped = redact_vision_text(&long);
        assert!(capped.ends_with("…[truncated]"));
        assert!(
            capped.chars().count() <= VISION_TEXT_REDACT_LIMIT + "…[truncated]".chars().count()
        );
        // Truncation lands on a char boundary even with multi-byte content.
        let multibyte = "é".repeat(VISION_TEXT_REDACT_LIMIT + 10);
        let capped_mb = redact_vision_text(&multibyte);
        assert!(capped_mb.is_char_boundary(capped_mb.len()));

        // A JSON error message embedding a data URL inside quotes is bounded.
        let json_err = "{\"detail\":\"bad input data:image/jpeg;base64,ZZZZ\"}";
        let r = redact_vision_text(json_err);
        assert!(!r.contains("ZZZZ"));
    }

    #[test]
    fn redact_vision_text_is_case_insensitive_and_strips_http_image_urls() {
        // Round-2 #2: uppercase DATA: and http(s) image URLs with signed-URL
        // query tokens must all be redacted.
        let upper = "oops DATA:IMAGE/PNG;BASE64,SECRETPAYLOAD trailing";
        let r = redact_vision_text(upper);
        assert!(!r.contains("SECRETPAYLOAD"), "uppercase data: stripped");
        assert!(r.contains("<redacted uri>"));
        assert!(r.contains("trailing"));

        let signed =
            "fetch failed for https://cdn.example.com/img.png?sig=ABCSECRET123&exp=999 oh no";
        let r = redact_vision_text(signed);
        assert!(!r.contains("ABCSECRET123"), "signed-url token stripped");
        assert!(!r.contains("cdn.example.com"), "image host stripped");
        assert!(r.contains("fetch failed for"));
        assert!(r.contains("oh no"));

        let mixed_case_http = "HTTPS://Host/Path?token=ZZZ done";
        let r = redact_vision_text(mixed_case_http);
        assert!(!r.contains("ZZZ"), "uppercase https stripped");
        assert!(r.contains("done"));
    }

    #[test]
    fn redact_vision_text_strips_json_escaped_signed_urls() {
        // Round-3 #4: a raw non-2xx body often contains JSON-escaped slashes
        // (`https:\/\/...`); the escaped form must be redacted too.
        let escaped =
            r#"{"error":"could not load https:\/\/cdn.example.com\/i.png?sig=ESCAPEDTOKEN&x=1"}"#;
        let r = redact_vision_text(escaped);
        assert!(
            !r.contains("ESCAPEDTOKEN"),
            "escaped signed-url token stripped"
        );
        assert!(
            !r.contains("cdn.example.com"),
            "escaped image host stripped"
        );
        assert!(r.contains("<redacted uri>"));
        assert!(r.contains("could not load"));

        // Escaped http (no TLS) too, uppercase scheme.
        let escaped_http = r#"HTTP:\/\/host\/p?tok=ABC end"#;
        let r = redact_vision_text(escaped_http);
        assert!(!r.contains("ABC"), "escaped http token stripped");
        assert!(r.contains("end"));

        // A successful description echoing a data URL is redacted the same way.
        let success = "Here is your image data:image/png;base64,REALPAYLOAD and analysis.";
        let r = redact_vision_text(success);
        assert!(!r.contains("REALPAYLOAD"));
        assert!(r.contains("and analysis."));
    }

    #[test]
    fn redact_image_uris_in_value_strips_nested_image_fields() {
        // Round-4 #2/#3: the shared JSON redactor used by inbound trace + upstream
        // JSONL must strip data:/signed image URLs anywhere in the body —
        // including nested content-part arrays and object fields — by VALUE, not
        // by key name, leaving non-image content intact.
        let mut value = serde_json::json!({
            "model": "glm-5.1",
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": "keep this prose" },
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,NESTEDLEAK" } },
                    { "type": "image_url", "image_url": { "url": "https://cdn.x/i.png?sig=SIGLEAK" } }
                ]
            }],
            "note": "see https:\\/\\/cdn.x\\/e.png?tok=ESCLEAK now"
        });
        redact_image_uris_in_value(&mut value);
        let dumped = serde_json::to_string(&value).expect("serialize");
        assert!(
            !dumped.contains("NESTEDLEAK"),
            "nested data: payload stripped"
        );
        assert!(
            !dumped.contains("SIGLEAK"),
            "nested signed-url token stripped"
        );
        assert!(
            !dumped.contains("ESCLEAK"),
            "escaped signed-url token stripped"
        );
        assert!(!dumped.contains("cdn.x"), "image host stripped");
        assert!(
            dumped.contains("keep this prose"),
            "non-image text preserved"
        );
        assert!(dumped.contains("glm-5.1"), "model preserved");
        assert!(dumped.contains("<redacted uri>"));
    }

    #[test]
    fn redact_image_uris_handles_multibyte_around_uris() {
        // Round-5: the core redactor walks UNTRUSTED text; multibyte chars
        // adjacent to / before / inside a URI must not panic and must preserve
        // non-image content. `é`/`☕`/`café` straddle byte boundaries near the
        // `data:`/`http` scan points.
        let text = "café ☕ before data:image/png;base64,PAYLÖAD/é+= after — déjà vu";
        let r = redact_image_uris(text);
        assert!(
            !r.contains("PAYL"),
            "base64 payload (with multibyte) stripped"
        );
        assert!(r.contains("<redacted uri>"));
        assert!(r.contains("café ☕ before "));
        assert!(r.contains(" after — déjà vu"));

        // A multibyte run immediately preceding `https://` must not corrupt the
        // boundary handling.
        let text2 = "señor https://hôst.x/p?tok=ZZZé done ☕";
        let r2 = redact_image_uris(text2);
        assert!(!r2.contains("ZZZ"), "signed-url token stripped");
        assert!(r2.contains("señor "));
        assert!(r2.contains(" done ☕"));

        // Pure multibyte with no URI is returned intact (no panic, no change).
        let plain = "完全に日本語のテキスト ☕ café";
        assert_eq!(redact_image_uris(plain), plain);
    }
}
