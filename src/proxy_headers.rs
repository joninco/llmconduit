//! Canonical hop-by-hop header filter shared by both halves of the
//! `/v1/completions` raw proxy (U6).
//!
//! The raw proxy strips hop-by-hop headers in two independent directions: the
//! outbound request (`upstream::should_proxy_request_header`) and the inbound
//! response (`http::should_proxy_response_header`). Both directions must filter
//! the *same* RFC 7230 §6.1 connection-header set; keeping two private copies of
//! the list let them silently drift so request vs. response stripping could
//! diverge. Hoisting one canonical `is_hop_by_hop_header`/`header_name_eq` pair
//! here makes that drift impossible by construction while the wire behavior
//! stays byte-identical. Each direction keeps its own extra filters
//! (request also drops `authorization`/`host`/`content-length`; response drops
//! `content-length`) at its own call site.

use http::HeaderName;

/// `true` when `name` is one of the RFC 7230 §6.1 hop-by-hop headers that a
/// proxy MUST NOT forward. The list contents and order are FINAL for this task
/// and shared verbatim by both proxy directions.
pub(crate) fn is_hop_by_hop_header(name: &HeaderName) -> bool {
    [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ]
    .iter()
    .any(|header| header_name_eq(name, header))
}

/// ASCII-case-insensitive compare of a header name against a lowercase literal.
pub(crate) fn header_name_eq(name: &HeaderName, other: &str) -> bool {
    name.as_str().eq_ignore_ascii_case(other)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The full RFC 7230 §6.1 hop-by-hop set every proxy direction strips.
    const HOP_BY_HOP: [&str; 8] = [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ];

    #[test]
    fn canonical_strips_full_hop_by_hop_set() {
        for header in HOP_BY_HOP {
            let name = HeaderName::from_bytes(header.as_bytes()).unwrap();
            assert!(
                is_hop_by_hop_header(&name),
                "{header} must be classified hop-by-hop",
            );
        }
    }

    #[test]
    fn canonical_is_ascii_case_insensitive() {
        let name = HeaderName::from_static("transfer-encoding");
        assert!(is_hop_by_hop_header(&name));
        // mixed/upper-case spellings of the same header still match
        assert!(header_name_eq(&name, "Transfer-Encoding"));
        assert!(header_name_eq(&name, "TRANSFER-ENCODING"));
    }

    #[test]
    fn canonical_passes_representative_passthrough_header() {
        let name = HeaderName::from_static("content-type");
        assert!(!is_hop_by_hop_header(&name));
    }
}
