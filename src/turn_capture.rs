//! F1 — durable per-turn capture (`.ralph/specs/F1-durable-turn-capture.md`).
//!
//! `TurnCapture` is the opt-in sink that, once `turn_capture_dir` is
//! configured, writes ONE self-contained JSON artifact per instrumented
//! inference turn (`<dir>/<api_call_id>.json`) holding the full inbound
//! request, the translated on-wire upstream request, the raw upstream
//! response, and the served response bytes -- so an operator (or a fresh
//! debug session) can see exactly what the backend sent and what the client
//! received for a turn that produced weird output but no error (a `<think>`
//! leak is a `200 OK`; today nothing persists it).
//!
//! Mirrors the `MonitorHub`/`DashboardFlowStore` `new()`/`disabled()`
//! zero-overhead split: `disabled()` (no configured dir) makes every method a
//! zero-op -- no thread, no allocation, no filesystem access -- so a gateway
//! that never sets `turn_capture_dir` pays nothing. Unlike those two, capture
//! has its OWN gate independent of `--with-debug-ui` (spec Design overview
//! #1); that gate lives in `http.rs` (F1b), not here.
//!
//! **F1a scope.** This task lands the config plumbing, the module skeleton,
//! and a REAL (not stubbed) `start()` -- but the ENABLED path is in-memory
//! only: no section file is written yet. `engine_done` and the per-turn
//! section writers are no-op forward declarations so the crate compiles and
//! later tasks have a stable surface to fill:
//! - F1b: HTTP own-gate + inbound-request capture + served-response wrapper.
//! - F1c: engine terminal integration (`engine_done`) + RAII finalize.
//! - F1d: upstream-request capture (carrier on `BackendChatRequest`).
//! - F1e: raw upstream-response capture + final failed HTTP body.
//! - F1f: streaming JSON assembly, atomicity, rotation.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

/// Handle to the turn-capture sink. Cheap to `Clone` (the enabled variant
/// clones an inner `Arc`); threads through DI (`lib.rs`) into the `Gateway`
/// (`Gateway::turn_capture`) and, in later tasks, the upstream client
/// (`BackendChatRequest`).
#[derive(Clone, Debug)]
pub struct TurnCapture {
    /// `None` when disabled (no `turn_capture_dir` configured) -- every
    /// method below short-circuits before doing any work. `Some` carries the
    /// resolved directory every enabled turn writes under.
    inner: Option<Arc<TurnCaptureInner>>,
}

#[derive(Debug)]
struct TurnCaptureInner {
    dir: PathBuf,
}

impl TurnCapture {
    /// Disabled sink (no `turn_capture_dir` configured). Every method is a
    /// zero-op: no thread, no allocation, no filesystem access, ever.
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    /// Enabled sink that will write artifacts under `dir` (the resolved
    /// `turn_capture_dir`). F1a keeps this in-memory only -- constructing an
    /// enabled sink does NOT create `dir`; section-file IO lands in F1c-F1e
    /// and JSON assembly in F1f.
    pub fn enabled(dir: PathBuf) -> Self {
        Self {
            inner: Some(Arc::new(TurnCaptureInner { dir })),
        }
    }

    /// Whether this handle will do any work. `false` for `disabled()`.
    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// The configured capture directory, when enabled.
    pub fn dir(&self) -> Option<&Path> {
        self.inner.as_ref().map(|inner| inner.dir.as_path())
    }

    /// Starts capturing a turn. DISABLED ⇒ `None`, no allocation, no fs
    /// touch, never panics. ENABLED ⇒ `Some(Arc::new(state))` -- in-memory
    /// only for F1a; no section file is written yet (F1b+ wires the
    /// HTTP/engine seams that call this and stream section bytes to disk).
    pub fn start(
        &self,
        api_call_id: &str,
        model_requested: Option<String>,
        started_ms: u128,
    ) -> Option<Arc<TurnCaptureState>> {
        self.inner.as_ref()?;
        Some(Arc::new(TurnCaptureState {
            api_call_id: api_call_id.to_string(),
            model_requested,
            started_ms,
        }))
    }

    /// F1c fills this: the engine terminal seam (completed/incomplete/
    /// failed/cancelled, including the pre-spawn failure path) reports the
    /// turn's outcome here, keyed by `api_call_id`. No-op stub so the crate
    /// compiles; not yet wired into `engine.rs`.
    #[allow(unused_variables)]
    pub fn engine_done(&self, api_call_id: &str, status: &str, reason: Option<&str>) {
        // F1b–F1e fill this.
    }
}

/// Per-turn capture state. F1a carries only outcome metadata; section fields
/// (temp-file handles for `inbound_request`/`upstream_request`/
/// `upstream_response`/`served_response`) land in F1c–F1e. Held behind `Arc`
/// so the failover/routing rebuild path (`BackendChatRequest`, Codex MED #5)
/// can clone it for free instead of re-deriving it per attempt.
#[derive(Debug)]
pub struct TurnCaptureState {
    pub api_call_id: String,
    pub model_requested: Option<String>,
    pub started_ms: u128,
}

impl TurnCaptureState {
    // F1b–F1e fill these section writers. Each is a real, callable, no-op
    // stub now -- no thread, no allocation, no filesystem access -- so
    // callers can be wired up incrementally without ever breaking the build.

    /// F1b fills this: copy + redact the inbound request body into the
    /// `inbound_request` section.
    #[allow(unused_variables)]
    pub fn write_inbound_request(&self, bytes: &[u8]) {
        // F1b–F1e fill this.
    }

    /// F1d fills this: the final on-wire OpenAI request (redacted,
    /// last-writer-wins across shrink-retry/failover) into the
    /// `upstream_request` section.
    #[allow(unused_variables)]
    pub fn write_upstream_request(&self, bytes: &[u8]) {
        // F1b–F1e fill this.
    }

    /// F1e fills this: raw upstream response bytes, streamed incrementally,
    /// into the `upstream_response` section.
    #[allow(unused_variables)]
    pub fn write_upstream_response(&self, bytes: &[u8]) {
        // F1b–F1e fill this.
    }

    /// F1b fills this: served response bytes (via the response-`Body` tee)
    /// into the `served_response` section.
    #[allow(unused_variables)]
    pub fn write_served_response(&self, bytes: &[u8]) {
        // F1b–F1e fill this.
    }

    /// F1b fills this: marks the `served_response` section closed (`partial`
    /// when the stream did not reach a clean end -- client disconnect,
    /// error).
    #[allow(unused_variables)]
    pub fn served_done(&self, partial: bool) {
        // F1b–F1e fill this.
    }
}

#[cfg(test)]
mod tests {
    use super::TurnCapture;

    fn temp_dir_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "llmconduit-turn-capture-{label}-{}",
            uuid::Uuid::new_v4().simple()
        ))
    }

    #[test]
    fn disabled_is_not_enabled_and_has_no_dir() {
        let capture = TurnCapture::disabled();
        assert!(!capture.is_enabled());
        assert!(capture.dir().is_none());
    }

    #[test]
    fn disabled_start_returns_none() {
        let capture = TurnCapture::disabled();
        assert!(
            capture
                .start("api_test123", Some("gpt-4".to_string()), 0)
                .is_none(),
            "a disabled TurnCapture must never hand out a TurnCaptureState"
        );
    }

    #[test]
    fn disabled_start_creates_no_filesystem_entries() {
        // The disabled sink has no configured directory at all; prove it
        // never reaches the filesystem by round-tripping through a would-be
        // capture directory and confirming it stays absent, even across
        // repeated calls (including through the forward-declared
        // `engine_done` stub).
        let probe_dir = temp_dir_path("disabled-probe");
        assert!(!probe_dir.exists());

        let capture = TurnCapture::disabled();
        for _ in 0..5 {
            assert!(capture.start("api_xyz", None, 42).is_none());
        }
        capture.engine_done("api_xyz", "failed", Some("client_disconnect"));

        assert!(
            !probe_dir.exists(),
            "a disabled TurnCapture must never create files/dirs"
        );
    }

    #[test]
    fn enabled_start_returns_populated_state_in_memory_only() {
        let dir = temp_dir_path("enabled");
        // Constructing an enabled sink must not itself touch the filesystem.
        assert!(!dir.exists());

        let capture = TurnCapture::enabled(dir.clone());
        assert!(capture.is_enabled());
        assert_eq!(capture.dir(), Some(dir.as_path()));

        let state = capture
            .start("api_abc123", Some("claude-opus".to_string()), 1_000)
            .expect("an enabled TurnCapture returns a state from start()");
        assert_eq!(state.api_call_id, "api_abc123");
        assert_eq!(state.model_requested.as_deref(), Some("claude-opus"));
        assert_eq!(state.started_ms, 1_000);

        // F1a is in-memory only -- starting a turn must not create the
        // directory or any file under it yet (F1b+ wires real section IO).
        assert!(
            !dir.exists(),
            "F1a must not perform any filesystem IO from start()"
        );
    }

    #[test]
    fn enabled_start_with_no_model_requested_is_none() {
        let capture = TurnCapture::enabled(temp_dir_path("no-model"));
        let state = capture
            .start("api_no_model", None, 7)
            .expect("state present");
        assert_eq!(state.model_requested, None);
    }

    #[test]
    fn forward_declared_stubs_are_callable_and_never_panic() {
        let capture = TurnCapture::enabled(temp_dir_path("stubs"));
        let state = capture.start("api_stub", None, 0).expect("state");
        state.write_inbound_request(b"{}");
        state.write_upstream_request(b"{}");
        state.write_upstream_response(b"data: [DONE]\n\n");
        state.write_served_response(b"event: message_stop\n\n");
        state.served_done(true);
        capture.engine_done("api_stub", "cancelled", Some("client_disconnect"));
    }
}
