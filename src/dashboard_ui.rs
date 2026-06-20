//! Serve the React+TS+Vite dashboard SPA embedded into the binary at compile
//! time (D8).
//!
//! `build.rs` guarantees `$OUT_DIR/dashboard_dist/` exists (a node-less stub by
//! default, the real Vite `dist/` when built with `LLMCONDUIT_BUILD_DASHBOARD=1`),
//! so `include_dir!` always compiles. The `static DASHBOARD_DIST: Dir<'static>`
//! binding is REQUIRED — a bare `include_dir!(concat!(env!("OUT_DIR"), …))` does
//! not type-check.
//!
//! Two routes (registered by `http.rs` only when `--with-debug-ui` is set; D7
//! later wraps them with auth):
//! - `GET /dashboard` → the SPA shell (`index.html`). The SPA is a hash router,
//!   so deep links live in the fragment and need no server-side rewrite.
//! - `GET /dashboard/assets/{*path}` → a static asset under `dist/assets/`, with
//!   `Content-Type` inferred from the extension; `404` for a missing path.

use axum::extract::Path;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::IntoResponse;
use axum::response::Response;
use include_dir::Dir;
use include_dir::include_dir;

/// The embedded dashboard build. Backed by `$OUT_DIR/dashboard_dist/`, which
/// `build.rs` always materializes (stub or real). The `Dir<'static>` type on
/// this `static` is what makes `include_dir!` type-check.
static DASHBOARD_DIST: Dir<'static> = include_dir!("$OUT_DIR/dashboard_dist");

/// `GET /dashboard` — serve the SPA shell. Missing only if the embedded build
/// is malformed (the stub and a real Vite build both emit `index.html`), in
/// which case we surface a 500 rather than silently 404.
pub async fn dashboard_index() -> Response {
    match DASHBOARD_DIST.get_file("index.html") {
        Some(file) => serve_file("index.html", file.contents()),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "dashboard index.html missing from embedded build",
        )
            .into_response(),
    }
}

/// `GET /dashboard/assets/{*path}` — serve a static asset from `dist/assets/`.
/// The captured `path` is the portion AFTER `assets/`; we look it up under
/// `assets/` in the embedded tree and 404 if absent.
pub async fn dashboard_asset(Path(path): Path<String>) -> Response {
    let asset_path = format!("assets/{path}");
    match DASHBOARD_DIST.get_file(&asset_path) {
        Some(file) => serve_file(&asset_path, file.contents()),
        None => (StatusCode::NOT_FOUND, "asset not found").into_response(),
    }
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
    use super::content_type_for;

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
}
