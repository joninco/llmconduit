//! Serve the React+TS+Vite dashboard SPA embedded into the binary at compile
//! time (D8), with the D7 auth gate.
//!
//! `build.rs` guarantees `$OUT_DIR/dashboard_dist/` exists (a node-less stub by
//! default, the real Vite `dist/` when built with `LLMCONDUIT_BUILD_DASHBOARD=1`),
//! so `include_dir!` always compiles. The `static DASHBOARD_DIST: Dir<'static>`
//! binding is REQUIRED — a bare `include_dir!(concat!(env!("OUT_DIR"), …))` does
//! not type-check.
//!
//! Routes (registered by `http.rs` only when `--with-debug-ui` is set AND the
//! D7 startup decision permits it):
//! - `GET /dashboard` → the SPA shell (`index.html`) when authenticated, with an
//!   injected bootstrap `<script>` (carrying the CSRF token + mutation flag) and
//!   a `llmconduit_csrf` cookie; a small **login shell** (token form) when not.
//!   The SPA is a hash router, so deep links live in the fragment and need no
//!   server-side rewrite.
//! - `GET /dashboard/assets/{*path}` → a static asset under `dist/assets/`, with
//!   Brotli/gzip negotiation, representation ETags, immutable caching, and
//!   `Content-Type` inferred from the canonical extension; `404` when missing.
//!
//! ## CSP-safe bootstrap injection
//! The dashboard CSP is `script-src 'self'` (no `'unsafe-inline'`). The SPA's
//! own `<script src=…>` tags are covered by `'self'`; the ONLY inline script is
//! the server-injected bootstrap, which we authorize with a per-response
//! `'nonce-<n>'` added to `script-src`. The frontend reads
//! `window.__LLMCONDUIT_DASHBOARD__` for its CSRF token + mutation flag.

use crate::dashboard_auth::AuthSession;
use crate::dashboard_auth::CSRF_COOKIE;
use crate::dashboard_auth::DashboardAuth;
use crate::dashboard_auth::SESSION_TTL_SECS;
use crate::dashboard_contracts::DASHBOARD_SCHEMA_VERSION;
use crate::dashboard_contracts::DashboardBootstrap;
use axum::Extension;
use axum::extract::Path;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::IntoResponse;
use axum::response::Response;
use include_dir::Dir;
use include_dir::include_dir;
use sha2::Digest;
use sha2::Sha256;
use std::sync::Arc;
use uuid::Uuid;

/// The embedded dashboard build. Backed by `$OUT_DIR/dashboard_dist/`, which
/// `build.rs` always materializes (stub or real). The `Dir<'static>` type on
/// this `static` is what makes `include_dir!` type-check.
static DASHBOARD_DIST: Dir<'static> = include_dir!("$OUT_DIR/dashboard_dist");

/// Base CSP for `/dashboard` (the `script-src` gets a per-response nonce appended
/// for the bootstrap inline script). Matches the D7 spec exactly.
const DASHBOARD_CSP_BASE: &str = "default-src 'self'; script-src 'self'{NONCE}; \
     connect-src 'self' ws: wss:; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
     object-src 'none'; base-uri 'self'; frame-ancestors 'none'";

/// The minimal login shell served to an UNauthenticated `/dashboard` client: a
/// token-entry form POSTing JSON to `/dashboard/login`, then reloading. All
/// scripting is via a nonce'd inline script (no external asset needed, so the
/// login page works even before the SPA assets load). Styling is inline
/// (`style-src 'unsafe-inline'`).
const LOGIN_SHELL_TEMPLATE: &str = include_str!("dashboard_login.html");

/// `GET /dashboard` — auth-aware shell. Authenticated → the embedded SPA with an
/// injected bootstrap script + a refreshed CSRF cookie. Unauthenticated → the
/// login shell. Always carries the dashboard CSP + security headers + `no-store`
/// (transcripts/credentials must not be cached).
pub async fn dashboard_index(
    Extension(auth): Extension<Arc<DashboardAuth>>,
    session: Option<AuthSession>,
) -> Response {
    let nonce = new_nonce();
    if session.is_some() {
        serve_authenticated_shell(&auth, &nonce)
    } else {
        serve_login_shell(&nonce)
    }
}

/// Build the authenticated SPA response: inject the bootstrap script into
/// `index.html`, set a fresh `llmconduit_csrf` cookie, and stamp the CSP +
/// headers.
fn serve_authenticated_shell(auth: &DashboardAuth, nonce: &str) -> Response {
    let Some(file) = DASHBOARD_DIST.get_file("index.html") else {
        return security_headers(
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "dashboard index.html missing from embedded build",
            )
                .into_response(),
            None,
        );
    };
    let csrf = auth.issue_csrf_token();
    let bootstrap_value = serde_json::to_string(&DashboardBootstrap {
        authenticated: true,
        csrf_token: csrf.clone(),
        mutations_enabled: auth.mutations_enabled(),
        schema_version: DASHBOARD_SCHEMA_VERSION,
    })
    .expect("dashboard bootstrap is serializable");
    let bootstrap = format!(
        "<script nonce=\"{nonce}\">window.__LLMCONDUIT_DASHBOARD__={bootstrap_value};</script>"
    );
    let html = inject_before_head_close(&String::from_utf8_lossy(file.contents()), &bootstrap);

    let mut response = ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response();
    let secure = auth.secure_cookies();
    if let Ok(cookie) = HeaderValue::from_str(&csrf_cookie(&csrf, secure)) {
        response.headers_mut().append(header::SET_COOKIE, cookie);
    }
    security_headers(response, Some(nonce))
}

/// Build the login-shell response (unauthenticated `/dashboard`).
fn serve_login_shell(nonce: &str) -> Response {
    let html = LOGIN_SHELL_TEMPLATE.replace("{NONCE}", nonce);
    let response = ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response();
    security_headers(response, Some(nonce))
}

/// `GET /dashboard/assets/{*path}` — serve a static asset from `dist/assets/`.
/// The build writes `.br`/`.gz` siblings for compressible assets; negotiate the
/// best accepted representation while keeping the canonical URL/content type.
/// Every successful representation carries its own strong ETag and one-year
/// immutable caching. Direct sidecar URLs stay private implementation details.
pub async fn dashboard_asset(Path(path): Path<String>, headers: HeaderMap) -> Response {
    if path.ends_with(".br") || path.ends_with(".gz") {
        return asset_security_headers((StatusCode::NOT_FOUND, "asset not found").into_response());
    }
    let asset_path = format!("assets/{path}");
    let Some(identity) = DASHBOARD_DIST.get_file(&asset_path) else {
        return asset_security_headers((StatusCode::NOT_FOUND, "asset not found").into_response());
    };
    let brotli_path = format!("{asset_path}.br");
    let gzip_path = format!("{asset_path}.gz");
    let brotli = DASHBOARD_DIST.get_file(&brotli_path);
    let gzip = DASHBOARD_DIST.get_file(&gzip_path);

    let Some(encoding) = select_content_encoding(&headers, brotli.is_some(), gzip.is_some()) else {
        return asset_security_headers(
            (StatusCode::NOT_ACCEPTABLE, "no acceptable asset encoding").into_response(),
        );
    };
    let contents = match encoding {
        ContentEncoding::Brotli => brotli.expect("selected only when embedded").contents(),
        ContentEncoding::Gzip => gzip.expect("selected only when embedded").contents(),
        ContentEncoding::Identity => identity.contents(),
    };
    serve_asset(&asset_path, contents, encoding, &headers)
}

/// Apply the dashboard CSP (with the bootstrap nonce when `nonce` is `Some`) plus
/// `X-Frame-Options: DENY`, `nosniff`, `no-referrer`, and `Cache-Control: no-store`.
fn security_headers(mut response: Response, nonce: Option<&str>) -> Response {
    let headers = response.headers_mut();
    let csp = match nonce {
        Some(nonce) => DASHBOARD_CSP_BASE.replace("{NONCE}", &format!(" 'nonce-{nonce}'")),
        None => DASHBOARD_CSP_BASE.replace("{NONCE}", ""),
    };
    if let Ok(value) = HeaderValue::from_str(&csp) {
        headers.insert(header::CONTENT_SECURITY_POLICY, value);
    }
    apply_common_security_headers(headers);
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Static-asset variant: common hardening plus the representation-negotiation
/// cache key. Successful responses add the immutable policy in `serve_asset`.
fn asset_security_headers(mut response: Response) -> Response {
    apply_common_security_headers(response.headers_mut());
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    response
}

const IMMUTABLE_CACHE_CONTROL: &str = "public,max-age=31536000,immutable";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ContentEncoding {
    Brotli,
    Gzip,
    Identity,
}

impl ContentEncoding {
    fn header_value(self) -> Option<HeaderValue> {
        match self {
            Self::Brotli => Some(HeaderValue::from_static("br")),
            Self::Gzip => Some(HeaderValue::from_static("gzip")),
            Self::Identity => None,
        }
    }
}

/// Honor q-values and prefer Brotli over gzip over identity when qualities tie.
/// Identity remains acceptable by default unless explicitly excluded.
fn select_content_encoding(
    headers: &HeaderMap,
    has_brotli: bool,
    has_gzip: bool,
) -> Option<ContentEncoding> {
    let raw = headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect::<Vec<_>>()
        .join(",");
    if raw.trim().is_empty() {
        return Some(ContentEncoding::Identity);
    }
    let preferences = EncodingPreferences::parse(&raw);
    let candidates = [
        (
            ContentEncoding::Brotli,
            has_brotli,
            preferences.quality("br"),
        ),
        (ContentEncoding::Gzip, has_gzip, preferences.quality("gzip")),
        (
            ContentEncoding::Identity,
            true,
            preferences.identity_quality(),
        ),
    ];
    candidates
        .into_iter()
        .filter(|(_, available, quality)| *available && *quality > 0.0)
        // `max_by` would pick the LAST tie; compare in reverse so array order is
        // the explicit br > gzip > identity tiebreak.
        .fold(None, |best, candidate| match best {
            None => Some(candidate),
            Some(current) if candidate.2 > current.2 => Some(candidate),
            Some(current) => Some(current),
        })
        .map(|(encoding, _, _)| encoding)
}

#[derive(Default)]
struct EncodingPreferences {
    values: Vec<(String, f32)>,
}

impl EncodingPreferences {
    fn parse(raw: &str) -> Self {
        let mut values = Vec::new();
        for item in raw.split(',') {
            let mut pieces = item.trim().split(';');
            let name = pieces
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            if name.is_empty() {
                continue;
            }
            let mut quality = 1.0;
            for parameter in pieces {
                let Some((key, value)) = parameter.trim().split_once('=') else {
                    continue;
                };
                if key.trim().eq_ignore_ascii_case("q") {
                    quality = value
                        .trim()
                        .parse::<f32>()
                        .ok()
                        .filter(|q| q.is_finite())
                        .map(|q| q.clamp(0.0, 1.0))
                        .unwrap_or(0.0);
                }
            }
            values.push((name, quality));
        }
        Self { values }
    }

    fn explicit_quality(&self, coding: &str) -> Option<f32> {
        self.values
            .iter()
            .rev()
            .find_map(|(name, quality)| (name == coding).then_some(*quality))
    }

    fn quality(&self, coding: &str) -> f32 {
        self.explicit_quality(coding)
            .or_else(|| self.explicit_quality("*"))
            .unwrap_or(0.0)
    }

    fn identity_quality(&self) -> f32 {
        self.explicit_quality("identity").unwrap_or_else(|| {
            if self.explicit_quality("*") == Some(0.0) {
                0.0
            } else {
                1.0
            }
        })
    }
}

fn serve_asset(
    canonical_path: &str,
    contents: &'static [u8],
    encoding: ContentEncoding,
    request_headers: &HeaderMap,
) -> Response {
    let etag = strong_etag(contents);
    let not_modified = if_none_match_matches(request_headers, &etag);
    let mut response = if not_modified {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        serve_file(canonical_path, contents)
    };
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(IMMUTABLE_CACHE_CONTROL),
    );
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&etag).expect("SHA-256 ETag is a valid header"),
    );
    if let Some(value) = encoding.header_value() {
        headers.insert(header::CONTENT_ENCODING, value);
    }
    asset_security_headers(response)
}

fn strong_etag(contents: &[u8]) -> String {
    format!("\"{}\"", hex::encode(Sha256::digest(contents)))
}

fn if_none_match_matches(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        // If-None-Match uses weak comparison for GET/HEAD, so a client-supplied
        // weak marker for these exact bytes also validates the cached response.
        .any(|candidate| {
            candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
        })
}

fn apply_common_security_headers(headers: &mut HeaderMap) {
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
}

/// Build the non-`HttpOnly` double-submit CSRF cookie (mirrors
/// `dashboard_auth`'s policy; duplicated here only because the shell sets a
/// FRESH token per page-load while `dashboard_auth` owns the login-time one).
fn csrf_cookie(value: &str, secure: bool) -> String {
    let mut cookie =
        format!("{CSRF_COOKIE}={value}; SameSite=Strict; Path=/; Max-Age={SESSION_TTL_SECS}");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Insert `snippet` immediately before the first `</head>` (case-insensitive),
/// falling back to prepending it if the document has no head close tag (the
/// node-less stub `index.html` may be minimal).
fn inject_before_head_close(html: &str, snippet: &str) -> String {
    if let Some(idx) = find_ci(html, "</head>") {
        let mut out = String::with_capacity(html.len() + snippet.len());
        out.push_str(&html[..idx]);
        out.push_str(snippet);
        out.push_str(&html[idx..]);
        out
    } else {
        format!("{snippet}{html}")
    }
}

/// Case-insensitive search for `needle` in `haystack`, returning the byte index.
fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    let hay = haystack.to_ascii_lowercase();
    let need = needle.to_ascii_lowercase();
    hay.find(&need)
}

/// A fresh random nonce for the per-response CSP `script-src`.
fn new_nonce() -> String {
    Uuid::new_v4().simple().to_string()
}

/// Path (relative to `DASHBOARD_DIST`, e.g. `assets/index-DEADBEEF.js`) of the
/// first file embedded under `assets/`, or `None` if that directory is empty.
///
/// Test-support: lets the `tests/` integration suite exercise the
/// `/dashboard/assets/{*path}` route against an asset that is REALLY embedded in
/// the current build, instead of hard-coding a name. The node-less stub embeds
/// `assets/stub.txt`, while a real `LLMCONDUIT_BUILD_DASHBOARD=1` build embeds
/// content-hashed Vite assets whose names are unknowable at source-edit time, so
/// the same test stays green under BOTH build modes. Not `#[cfg(test)]` because
/// integration tests link the library compiled WITHOUT `cfg(test)`; `doc(hidden)`
/// keeps it out of the public API surface. The captured `{*path}` is the portion
/// after `assets/`, so callers strip that prefix before requesting.
#[doc(hidden)]
pub fn first_embedded_asset_path() -> Option<String> {
    DASHBOARD_DIST
        .get_dir("assets")
        .and_then(|assets| {
            assets.files().find(|file| {
                let path = file.path().to_string_lossy();
                !path.ends_with(".br") && !path.ends_with(".gz")
            })
        })
        .map(|file| file.path().to_string_lossy().into_owned())
}

/// Build a `200 OK` body for an embedded file, tagging `Content-Type` from the
/// path's extension (falling back to `application/octet-stream`).
fn serve_file(path: &str, contents: &'static [u8]) -> Response {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static(content_type_for(path)),
        )],
        contents,
    )
        .into_response()
}

/// Map a file extension to a `Content-Type`. Covers the asset kinds Vite emits
/// for this SPA (JS/CSS/HTML, source maps, fonts, images); anything else is
/// served as `application/octet-stream`.
fn content_type_for(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().filter(|ext| *ext != path);
    match ext {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") | Some("map") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("ttf") => "font/ttf",
        Some("txt") => "text/plain; charset=utf-8",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::ContentEncoding;
    use super::IMMUTABLE_CACHE_CONTROL;
    use super::content_type_for;
    use super::select_content_encoding;
    use super::serve_asset;
    use axum::http::HeaderMap;
    use axum::http::HeaderValue;
    use axum::http::StatusCode;
    use axum::http::header;

    #[test]
    fn maps_known_vite_asset_extensions() {
        assert_eq!(content_type_for("index.html"), "text/html; charset=utf-8");
        assert_eq!(
            content_type_for("assets/index-DEADBEEF.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            content_type_for("assets/index-DEADBEEF.css"),
            "text/css; charset=utf-8"
        );
        assert_eq!(content_type_for("assets/logo.svg"), "image/svg+xml");
        assert_eq!(content_type_for("assets/font.woff2"), "font/woff2");
    }

    #[test]
    fn unknown_and_extensionless_paths_are_octet_stream() {
        assert_eq!(
            content_type_for("assets/data.bin"),
            "application/octet-stream"
        );
        // No extension: `rsplit('.')` yields the whole string, which we reject.
        assert_eq!(content_type_for("noext"), "application/octet-stream");
    }

    #[test]
    fn negotiates_precompressed_assets_with_q_values_and_safe_fallbacks() {
        let mut headers = HeaderMap::new();
        assert_eq!(
            select_content_encoding(&headers, true, true),
            Some(ContentEncoding::Identity),
            "a client that sends no Accept-Encoding gets the canonical bytes"
        );

        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("gzip, br"),
        );
        assert_eq!(
            select_content_encoding(&headers, true, true),
            Some(ContentEncoding::Brotli),
            "Brotli wins an equal-quality tie"
        );

        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("gzip;q=1, br;q=0.4"),
        );
        assert_eq!(
            select_content_encoding(&headers, true, true),
            Some(ContentEncoding::Gzip),
            "client q-values take precedence over server preference"
        );

        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("br, gzip;q=0.5, identity;q=0"),
        );
        assert_eq!(
            select_content_encoding(&headers, false, true),
            Some(ContentEncoding::Gzip),
            "an unavailable Brotli sidecar falls through to gzip"
        );

        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("br;q=0, gzip;q=0, identity;q=0"),
        );
        assert_eq!(select_content_encoding(&headers, true, true), None);
    }

    #[test]
    fn asset_response_has_representation_etag_and_supports_conditional_304() {
        static CONTENTS: &[u8] = b"const answer = 42;";
        let request_headers = HeaderMap::new();
        let response = serve_asset(
            "assets/index-ABC.js",
            CONTENTS,
            ContentEncoding::Brotli,
            &request_headers,
        );
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_ENCODING),
            Some(&HeaderValue::from_static("br"))
        );
        assert_eq!(
            response.headers().get(header::VARY),
            Some(&HeaderValue::from_static("Accept-Encoding"))
        );
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL),
            Some(&HeaderValue::from_static(IMMUTABLE_CACHE_CONTROL))
        );
        let etag = response.headers().get(header::ETAG).expect("ETag").clone();
        let etag_text = etag.to_str().expect("ASCII ETag");
        assert!(etag_text.starts_with('"') && etag_text.ends_with('"'));
        assert!(!etag_text.starts_with("W/"), "ETag must be strong");

        let mut conditional = HeaderMap::new();
        conditional.insert(header::IF_NONE_MATCH, etag.clone());
        let not_modified = serve_asset(
            "assets/index-ABC.js",
            CONTENTS,
            ContentEncoding::Brotli,
            &conditional,
        );
        assert_eq!(not_modified.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(not_modified.headers().get(header::ETAG), Some(&etag));
        assert_eq!(
            not_modified.headers().get(header::CACHE_CONTROL),
            Some(&HeaderValue::from_static(IMMUTABLE_CACHE_CONTROL))
        );
        assert_eq!(
            not_modified.headers().get(header::CONTENT_ENCODING),
            Some(&HeaderValue::from_static("br"))
        );
    }
}
