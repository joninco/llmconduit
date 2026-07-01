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
//! **Bounded memory (Codex HIGH #2).** Each section (`inbound_request`,
//! `served_response`, and -- in F1d/F1e -- `upstream_request`/
//! `upstream_response`) streams incrementally to a per-turn temp file under
//! `<dir>/.work/<api_call_id>/` via a background writer task fed over an
//! ordered channel; only small metadata (`{bytes, partial, closed}`) lives in
//! memory. NO `HashMap<_, full-body>` ever forms -- the bytes go to disk. The
//! final single JSON is assembled later (F1f) by STREAMING those temp files, so
//! a 256 MiB turn is never held in RAM at once.
//!
//! **Task boundaries.**
//! - F1b (this task): HTTP own-gate + inbound-request capture + served-response
//!   `Body` tee; real `start()` (work dir + section writers + registry).
//! - F1c: engine terminal integration (`engine_done`) + RAII finalize + the
//!   both-`done` assembly barrier (which also EVICTS the registry entry).
//! - F1d: upstream-request capture (carrier on `BackendChatRequest`).
//! - F1e: raw upstream-response capture + final failed HTTP body.
//! - F1f: streaming JSON assembly, atomicity, rotation.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;
use tokio::sync::mpsc;

/// Handle to the turn-capture sink. Cheap to `Clone` (the enabled variant
/// clones an inner `Arc`); threads through DI (`lib.rs`) into the `Gateway`
/// (`Gateway::turn_capture`) and, in later tasks, the upstream client
/// (`BackendChatRequest`).
#[derive(Clone, Debug)]
pub struct TurnCapture {
    /// `None` when disabled (no `turn_capture_dir` configured) -- every
    /// method below short-circuits before doing any work. `Some` carries the
    /// resolved directory every enabled turn writes under plus the per-turn
    /// registry F1c's `engine_done` looks a live turn up in.
    inner: Option<Arc<TurnCaptureInner>>,
}

#[derive(Debug)]
struct TurnCaptureInner {
    dir: PathBuf,
    /// Live per-turn states keyed by `api_call_id`, populated by [`start`] so a
    /// later seam that only has the id (the engine terminal in F1c, the
    /// upstream request/response taps in F1d/F1e) can reach the SAME state.
    /// F1c evicts an entry on the both-`done` assembly barrier; until then a
    /// started turn's entry lives for the turn's duration.
    registry: Mutex<HashMap<String, Arc<TurnCaptureState>>>,
}

impl TurnCapture {
    /// Disabled sink (no `turn_capture_dir` configured). Every method is a
    /// zero-op: no thread, no allocation, no filesystem access, ever.
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    /// Enabled sink that will write artifacts under `dir` (the resolved
    /// `turn_capture_dir`). Constructing an enabled sink does NOT touch the
    /// filesystem -- only [`start`] (per turn) creates the work dir + section
    /// files.
    pub fn enabled(dir: PathBuf) -> Self {
        Self {
            inner: Some(Arc::new(TurnCaptureInner {
                dir,
                registry: Mutex::new(HashMap::new()),
            })),
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

    /// Starts capturing a turn. DISABLED ⇒ `None`, no allocation, no fs touch,
    /// never panics. ENABLED ⇒ opens the per-turn work dir
    /// `<dir>/.work/<api_call_id>/`, spins up the `inbound_request` +
    /// `served_response` section writers (background tasks streaming to temp
    /// files -- so this must run inside a tokio runtime, which every caller
    /// seam does), registers the state under `api_call_id`, and returns the
    /// `Arc`. All filesystem IO (mkdir + file create/write/flush) happens on
    /// the section writer tasks via `tokio::fs`, never synchronously here, so
    /// `start` itself does no blocking IO on the runtime (AGENTS.md).
    pub fn start(
        &self,
        api_call_id: &str,
        model_requested: Option<String>,
        started_ms: u128,
    ) -> Option<Arc<TurnCaptureState>> {
        let inner = self.inner.as_ref()?;
        let work_dir = inner.dir.join(".work").join(api_call_id);
        let state = Arc::new(TurnCaptureState::new(
            work_dir,
            api_call_id.to_string(),
            model_requested,
            started_ms,
        ));
        inner
            .registry
            .lock()
            .expect("turn-capture registry lock")
            .insert(api_call_id.to_string(), Arc::clone(&state));
        Some(state)
    }

    /// The live state for `api_call_id`, if a turn is currently captured. The
    /// registry-lookup seam F1c's `engine_done` and F1d/F1e's upstream taps use
    /// to reach the SAME per-turn state from the engine/upstream layers. `None`
    /// when disabled or no such turn is live.
    pub fn state(&self, api_call_id: &str) -> Option<Arc<TurnCaptureState>> {
        self.inner
            .as_ref()?
            .registry
            .lock()
            .expect("turn-capture registry lock")
            .get(api_call_id)
            .cloned()
    }

    /// F1c fills this: the engine terminal seam (completed/incomplete/
    /// failed/cancelled, including the pre-spawn failure path) reports the
    /// turn's outcome here, keyed by `api_call_id`, then -- once BOTH the engine
    /// and served sides have reported -- assembles the artifact and evicts the
    /// registry entry. No-op stub so the crate compiles; not yet wired into
    /// `engine.rs`.
    #[allow(unused_variables)]
    pub fn engine_done(&self, api_call_id: &str, status: &str, reason: Option<&str>) {
        // F1c fills this.
    }
}

/// Per-turn capture state. Owns the per-section incremental writers (temp files
/// under `<dir>/.work/<api_call_id>/`) plus small outcome metadata. Held behind
/// `Arc` so the HTTP served-body tee, the engine terminal (F1c), and the
/// failover/routing rebuild path (`BackendChatRequest`, Codex MED #5) all share
/// ONE state instead of re-deriving it per attempt.
#[derive(Debug)]
pub struct TurnCaptureState {
    pub api_call_id: String,
    pub model_requested: Option<String>,
    pub started_ms: u128,
    work_dir: PathBuf,
    /// The redacted inbound Anthropic/OpenAI request body (F1b).
    inbound_request: Section,
    /// The exact bytes served to the client, teed off the response `Body` (F1b).
    served_response: Section,
    /// Idempotency latch for [`served_done`] -- the tee's `Drop` is the only
    /// F1b caller, but F1c makes `served_done`/`engine_done` idempotent so the
    /// both-`done` barrier fires exactly once.
    served_reported: AtomicBool,
}

impl TurnCaptureState {
    fn new(
        work_dir: PathBuf,
        api_call_id: String,
        model_requested: Option<String>,
        started_ms: u128,
    ) -> Self {
        let inbound_request = Section::new(work_dir.join("inbound_request"));
        let served_response = Section::new(work_dir.join("served_response"));
        Self {
            api_call_id,
            model_requested,
            started_ms,
            work_dir,
            inbound_request,
            served_response,
            served_reported: AtomicBool::new(false),
        }
    }

    /// The per-turn work directory (`<dir>/.work/<api_call_id>/`). F1f assembles
    /// the final artifact from the section files under it, then removes it.
    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    /// Path of the `inbound_request` section temp file.
    pub fn inbound_request_path(&self) -> &Path {
        self.inbound_request.path()
    }

    /// Path of the `served_response` section temp file.
    pub fn served_response_path(&self) -> &Path {
        self.served_response.path()
    }

    /// Whether the `served_response` section closed `partial` (the served stream
    /// did not reach a clean end -- client disconnect / mid-stream error / a
    /// section write error). `false` for a cleanly-completed response.
    pub fn served_partial(&self) -> bool {
        self.served_response.partial()
    }

    /// Bytes appended to the `served_response` section so far.
    pub fn served_bytes(&self) -> u64 {
        self.served_response.bytes()
    }

    /// Await the `inbound_request` section writer draining + flushing to disk
    /// (used by F1f's assembly barrier and by tests reading the file directly).
    pub async fn await_inbound_closed(&self) {
        self.inbound_request.await_closed().await;
    }

    /// Await the `served_response` section writer draining + flushing to disk.
    pub async fn await_served_closed(&self) {
        self.served_response.await_closed().await;
    }

    /// F1b: copy the redacted inbound request body into the `inbound_request`
    /// section, then close it. The caller redacts (secret keys and image URIs)
    /// BEFORE calling; `bytes` is a redacted COPY, never a slice of the 256 MiB
    /// middleware buffer (AGENTS.md). Written once per turn.
    pub fn write_inbound_request(&self, bytes: &[u8]) {
        self.inbound_request.append(bytes);
        // The whole (redacted) body is in hand, so the section is complete.
        self.inbound_request.close(false);
    }

    /// F1d fills this: the final on-wire OpenAI request (redacted,
    /// last-writer-wins across shrink-retry/failover) into the
    /// `upstream_request` section. No-op in F1b (that section is not created
    /// until F1d).
    #[allow(unused_variables)]
    pub fn write_upstream_request(&self, bytes: &[u8]) {
        // F1d fills this.
    }

    /// F1e fills this: raw upstream response bytes, streamed incrementally, into
    /// the `upstream_response` section. No-op in F1b (that section is not created
    /// until F1e).
    #[allow(unused_variables)]
    pub fn write_upstream_response(&self, bytes: &[u8]) {
        // F1e fills this.
    }

    /// F1b: append served response bytes (called incrementally by the response
    /// `Body` tee, once per DATA frame). Each call COPIES the frame to disk; no
    /// slice of the frame's backing allocation is retained.
    pub fn write_served_response(&self, bytes: &[u8]) {
        self.served_response.append(bytes);
    }

    /// F1b: mark the `served_response` section closed. `partial` is `true` when
    /// the served stream did NOT reach a clean end (client disconnect, mid-stream
    /// error). Idempotent (F1c relies on that for the both-`done` barrier): only
    /// the FIRST call closes the section.
    pub fn served_done(&self, partial: bool) {
        if self.served_reported.swap(true, Ordering::AcqRel) {
            return;
        }
        self.served_response.close(partial);
    }
}

/// One incremental capture section. Bytes stream to a per-turn temp file on a
/// background writer task WITHOUT buffering the whole body: [`append`] copies the
/// frame and hands it to the task over an ordered (FIFO) channel, so the caller
/// (including the sync `poll_frame` tee) never blocks and no full-body map forms.
/// Only small metadata lives in memory.
///
/// [`append`]: Section::append
#[derive(Debug)]
struct Section {
    /// `None` once [`close`](Section::close)d -- the writer task then drains the
    /// remaining queued chunks and flushes. A dropped `Section` (abandoned turn)
    /// also drops the sender, so the task always terminates (no hang; AGENTS.md
    /// cancellation rule).
    tx: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
    meta: Arc<SectionMeta>,
}

#[derive(Debug)]
struct SectionMeta {
    path: PathBuf,
    bytes: AtomicU64,
    partial: AtomicBool,
    closed: AtomicBool,
    /// Notified once, when the writer task has drained + flushed + set `closed`.
    done: Notify,
}

impl Section {
    fn new(path: PathBuf) -> Self {
        let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let meta = Arc::new(SectionMeta {
            path,
            bytes: AtomicU64::new(0),
            partial: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            done: Notify::new(),
        });
        tokio::spawn(section_writer(rx, Arc::clone(&meta)));
        Self {
            tx: Mutex::new(Some(tx)),
            meta,
        }
    }

    fn path(&self) -> &Path {
        &self.meta.path
    }

    fn bytes(&self) -> u64 {
        self.meta.bytes.load(Ordering::Acquire)
    }

    fn partial(&self) -> bool {
        self.meta.partial.load(Ordering::Acquire)
    }

    /// Append `bytes` to the section. COPIES into an owned `Vec` (never retains a
    /// slice of the caller's buffer -- AGENTS.md) and hands it to the writer task.
    /// Non-blocking + ordered; a no-op once closed or on an empty frame.
    fn append(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let Ok(guard) = self.tx.lock()
            && let Some(tx) = guard.as_ref()
        {
            // Send failure only if the writer task is already gone; nothing to do.
            let _ = tx.send(bytes.to_vec());
        }
    }

    /// Close the section. Records `partial` and drops the sender so the writer
    /// task drains remaining chunks, flushes, and finishes. Idempotent.
    fn close(&self, partial: bool) {
        if partial {
            self.meta.partial.store(true, Ordering::Release);
        }
        if let Ok(mut guard) = self.tx.lock() {
            *guard = None;
        }
    }

    /// Await the writer task draining + flushing + setting `closed`. Uses the
    /// register-before-check pattern so a `notify_waiters` racing the flag read
    /// is never lost.
    async fn await_closed(&self) {
        loop {
            let notified = self.meta.done.notified();
            tokio::pin!(notified);
            // Register the waiter NOW, before re-reading `closed`, so a wake that
            // fires between the check and the await is not missed.
            notified.as_mut().enable();
            if self.meta.closed.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

/// The background writer for one [`Section`]: creates the work dir + section file
/// (all fs IO via `tokio::fs`, i.e. off the runtime threads), appends each queued
/// chunk in order, and -- when the sender drops (section closed or turn dropped)
/// -- flushes, fsyncs, and marks the section `closed`. A file-create/write error
/// marks the section `partial` and is logged, never propagated (a diagnostic
/// artifact must never fail or hang the turn).
async fn section_writer(mut rx: mpsc::UnboundedReceiver<Vec<u8>>, meta: Arc<SectionMeta>) {
    // Idempotent + race-tolerant across the turn's sibling sections both racing
    // to create the shared work dir.
    if let Some(parent) = meta.path.parent()
        && let Err(err) = tokio::fs::create_dir_all(parent).await
    {
        tracing::warn!(
            path = %meta.path.display(),
            error = %err,
            "turn-capture: failed to create section work dir"
        );
        meta.partial.store(true, Ordering::Release);
    }

    let mut file = match tokio::fs::File::create(&meta.path).await {
        Ok(file) => Some(file),
        Err(err) => {
            tracing::warn!(
                path = %meta.path.display(),
                error = %err,
                "turn-capture: failed to create section file"
            );
            meta.partial.store(true, Ordering::Release);
            None
        }
    };

    while let Some(chunk) = rx.recv().await {
        let Some(file) = file.as_mut() else {
            // File never opened: keep draining the channel so senders don't wedge,
            // but the section is already flagged partial.
            continue;
        };
        match file.write_all(&chunk).await {
            Ok(()) => {
                meta.bytes.fetch_add(chunk.len() as u64, Ordering::AcqRel);
            }
            Err(err) => {
                tracing::warn!(
                    path = %meta.path.display(),
                    error = %err,
                    "turn-capture: failed to append to section file"
                );
                meta.partial.store(true, Ordering::Release);
            }
        }
    }

    if let Some(mut file) = file {
        let _ = file.flush().await;
        let _ = file.sync_all().await;
    }
    meta.closed.store(true, Ordering::Release);
    meta.done.notify_waiters();
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
        // The disabled sink has no configured directory at all; prove it never
        // reaches the filesystem (no runtime needed either -- it never spawns a
        // writer task) even across repeated calls and the forward-declared
        // `engine_done` stub.
        let probe_dir = temp_dir_path("disabled-probe");
        assert!(!probe_dir.exists());

        let capture = TurnCapture::disabled();
        for _ in 0..5 {
            assert!(capture.start("api_xyz", None, 42).is_none());
        }
        capture.engine_done("api_xyz", "failed", Some("client_disconnect"));
        assert!(capture.state("api_xyz").is_none());

        assert!(
            !probe_dir.exists(),
            "a disabled TurnCapture must never create files/dirs"
        );
    }

    #[test]
    fn enabled_constructor_does_no_filesystem_io() {
        // Constructing an enabled sink must not itself touch the filesystem --
        // only `start()` (per turn) creates the work dir. (Sync test: no runtime,
        // and `enabled()`/`is_enabled()`/`dir()` never spawn.)
        let dir = temp_dir_path("enabled-ctor");
        assert!(!dir.exists());
        let capture = TurnCapture::enabled(dir.clone());
        assert!(capture.is_enabled());
        assert_eq!(capture.dir(), Some(dir.as_path()));
        assert!(!dir.exists(), "enabled() must not perform filesystem IO");
    }

    #[tokio::test]
    async fn enabled_start_registers_state_and_writes_inbound_section() {
        let dir = temp_dir_path("enabled-start");
        let capture = TurnCapture::enabled(dir.clone());

        let state = capture
            .start("api_abc123", Some("claude-opus".to_string()), 1_000)
            .expect("an enabled TurnCapture returns a state from start()");
        assert_eq!(state.api_call_id, "api_abc123");
        assert_eq!(state.model_requested.as_deref(), Some("claude-opus"));
        assert_eq!(state.started_ms, 1_000);

        // The registry lets a later seam (F1c/F1d) reach the SAME state by id.
        let looked_up = capture.state("api_abc123").expect("state registered");
        assert!(std::sync::Arc::ptr_eq(&state, &looked_up));

        // The inbound section streams to `<dir>/.work/<id>/inbound_request`.
        state.write_inbound_request(b"{\"model\":\"claude-opus\"}");
        state.await_inbound_closed().await;
        let contents = std::fs::read(state.inbound_request_path()).expect("inbound section file");
        assert_eq!(contents, b"{\"model\":\"claude-opus\"}");
        assert_eq!(
            state.work_dir(),
            dir.join(".work").join("api_abc123").as_path()
        );
    }

    #[tokio::test]
    async fn enabled_start_with_no_model_requested_is_none() {
        let capture = TurnCapture::enabled(temp_dir_path("no-model"));
        let state = capture
            .start("api_no_model", None, 7)
            .expect("state present");
        assert_eq!(state.model_requested, None);
    }

    #[tokio::test]
    async fn served_section_captures_bytes_and_marks_clean() {
        let capture = TurnCapture::enabled(temp_dir_path("served-clean"));
        let state = capture.start("api_served", None, 0).expect("state");

        state.write_served_response(b"event: message_start\n\n");
        state.write_served_response(b"event: message_stop\n\n");
        state.served_done(false);
        state.await_served_closed().await;

        let contents = std::fs::read(state.served_response_path()).expect("served section file");
        assert_eq!(contents, b"event: message_start\n\nevent: message_stop\n\n");
        assert_eq!(state.served_bytes(), contents.len() as u64);
        assert!(
            !state.served_partial(),
            "a cleanly-closed served section is not partial"
        );
    }

    #[tokio::test]
    async fn served_done_partial_is_recorded_and_idempotent() {
        let capture = TurnCapture::enabled(temp_dir_path("served-partial"));
        let state = capture.start("api_partial", None, 0).expect("state");

        state.write_served_response(b"event: message_start\n\n");
        // Mid-stream disconnect: partial close, then a duplicate close is a no-op.
        state.served_done(true);
        state.served_done(false);
        state.await_served_closed().await;

        assert!(
            state.served_partial(),
            "a partial close must stick even if a later served_done(false) races"
        );
        let contents = std::fs::read(state.served_response_path()).expect("served section file");
        assert_eq!(contents, b"event: message_start\n\n");
    }

    #[tokio::test]
    async fn forward_declared_stubs_are_callable_and_never_panic() {
        let capture = TurnCapture::enabled(temp_dir_path("stubs"));
        let state = capture.start("api_stub", None, 0).expect("state");
        // F1d/F1e sections are not created yet -- these are no-ops.
        state.write_upstream_request(b"{}");
        state.write_upstream_response(b"data: [DONE]\n\n");
        state.write_inbound_request(b"{}");
        state.write_served_response(b"event: message_stop\n\n");
        state.served_done(true);
        capture.engine_done("api_stub", "cancelled", Some("client_disconnect"));
        state.await_inbound_closed().await;
        state.await_served_closed().await;
    }
}
