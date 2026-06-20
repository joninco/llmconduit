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

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

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
