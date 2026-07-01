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
//! **Bounded memory (Codex HIGH #2 + F1b review #1).** Each section
//! (`inbound_request`, `served_response`, and -- in F1d/F1e --
//! `upstream_request`/`upstream_response`) streams incrementally to a per-turn
//! temp file under `<dir>/.work/<api_call_id>/` via a background writer task fed
//! over a BOUNDED, ordered channel ([`SECTION_CHANNEL_CAPACITY`] frames). The
//! high-volume served-body tee reserves a channel slot BEFORE pulling each frame
//! and yields `Poll::Pending` when the writer is behind, so a slow disk / large
//! streamed body throttles the served stream to disk pace instead of piling the
//! whole body into RAM. Only small metadata (`{bytes, partial, closed}`) lives in
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
use std::task::Context;
use std::task::Poll;

use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio_util::sync::PollSender;

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
        // EVICTION IS OWNED BY F1c (finding #2, deferred): this entry lives for
        // the turn's duration and is removed by F1c's both-`done` assembly barrier
        // (`engine_done` + `served_done` → assemble → `registry.remove`). F1b has no
        // engine terminal seam yet, so building the barrier here would half-implement
        // F1c; until F1c lands, a long-lived process capturing many turns grows this
        // map by one small `Arc` per turn (documented in `.ralph/IMPLEMENTATION_PLAN.md`
        // "Known deferred (F1c)"). Not a leak of body bytes -- those stream to disk,
        // not this map (bounded-memory invariant holds regardless).
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

    /// F1b: a bounded, back-pressured [`ServedSink`] over the `served_response`
    /// section for the HTTP served-body tee. The tee reserves a channel slot
    /// BEFORE pulling each served frame and yields `Poll::Pending` while the
    /// writer is behind, so a slow disk / large streamed body throttles the
    /// served stream instead of accumulating in RAM -- capture memory stays
    /// bounded to [`SECTION_CHANNEL_CAPACITY`] frames regardless of body size
    /// (finding #1; AGENTS.md bounded-memory invariant). This is the ONLY
    /// high-volume served writer; [`write_served_response`] is the low-volume /
    /// test path (its `try_send` would drop-on-full, unacceptable for a stream).
    /// `None` once the served section is closed.
    ///
    /// [`write_served_response`]: TurnCaptureState::write_served_response
    pub fn served_sink(&self) -> Option<ServedSink> {
        self.served_response.poll_sender().map(ServedSink::new)
    }
}

/// A bounded, back-pressured sink into the `served_response` section's writer
/// channel, handed to the HTTP served-body tee by
/// [`TurnCaptureState::served_sink`]. Wraps a [`PollSender`] so the tee can drive
/// it from a synchronous `poll_frame`: [`poll_reserve`] a slot BEFORE pulling the
/// next served frame, then [`send`] the copied frame into the reserved slot. When
/// the writer is behind, `poll_reserve` yields `Poll::Pending` (registering the
/// task's waker, re-woken as the writer drains) so the served stream is throttled
/// to disk pace and capture memory stays bounded to [`SECTION_CHANNEL_CAPACITY`]
/// frames -- never the whole body (finding #1). A closed channel (writer gone)
/// surfaces as `Err`; the tee then stops capturing but keeps serving the client
/// byte-for-byte unchanged (a diagnostic failure must never break the served
/// stream -- AGENTS.md).
///
/// [`poll_reserve`]: ServedSink::poll_reserve
/// [`send`]: ServedSink::send
#[derive(Debug)]
pub struct ServedSink {
    inner: PollSender<Vec<u8>>,
}

/// The served-section writer is gone -- its bounded channel closed (typically
/// after a section write error dropped the receiver). The tee drops the sink on
/// this and keeps serving the client byte-for-byte WITHOUT capture (a diagnostic
/// failure must never break the served stream -- AGENTS.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SinkClosed;

impl ServedSink {
    fn new(inner: PollSender<Vec<u8>>) -> Self {
        Self { inner }
    }

    /// Reserve one slot for the next served frame. `Poll::Ready(Ok(()))` once a
    /// slot is held (then call [`send`] exactly once); `Poll::Pending` applies
    /// back-pressure (the whole tee yields, throttling the served stream to the
    /// writer's pace -- bounded memory); `Poll::Ready(Err(SinkClosed))` once the
    /// writer is gone. Must return `Ready(Ok(()))` before each [`send`] (the
    /// `PollSender` contract).
    ///
    /// [`send`]: ServedSink::send
    pub fn poll_reserve(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), SinkClosed>> {
        self.inner.poll_reserve(cx).map_err(|_| SinkClosed)
    }

    /// Send one copied served frame into the slot reserved by a preceding
    /// successful [`poll_reserve`]. `Err(SinkClosed)` once the writer is gone.
    ///
    /// [`poll_reserve`]: ServedSink::poll_reserve
    pub fn send(&mut self, bytes: Vec<u8>) -> Result<(), SinkClosed> {
        self.inner.send_item(bytes).map_err(|_| SinkClosed)
    }

    /// The bounded capacity of the underlying writer channel: at most this many
    /// frames are ever in flight, independent of the served body's total size.
    /// Exposed so the bounded-memory invariant (finding #1) is asserted
    /// STRUCTURALLY in tests (the channel cannot exceed this) rather than via a
    /// heap probe.
    pub fn max_capacity(&self) -> usize {
        self.inner.get_ref().map_or(0, |tx| tx.max_capacity())
    }
}

/// Bounded in-flight cap for a section's writer channel (finding #1: the served
/// tee must NOT accumulate the whole body in RAM). At most this many frames (plus
/// one reserved permit) are ever queued toward the background writer, independent
/// of the served body's total size -- so capture memory stays O(CAP), not O(N).
/// A small constant keeps a little pipelining without unbounding memory; the
/// served tee back-pressures on `Poll::Pending` once the channel is full.
const SECTION_CHANNEL_CAPACITY: usize = 16;

/// One incremental capture section. Bytes stream to a per-turn temp file on a
/// background writer task WITHOUT buffering the whole body: they are handed to the
/// task over a BOUNDED, ordered (FIFO) channel of at most
/// [`SECTION_CHANNEL_CAPACITY`] frames. Two write paths feed it:
/// - Low-volume, single-shot writers ([`append`], used for the inbound-request
///   body and tests) `try_send` a copy -- they never fill the channel.
/// - The high-volume served-body tee takes a back-pressured [`ServedSink`]
///   ([`TurnCaptureState::served_sink`]): it reserves a slot before each frame and
///   yields `Poll::Pending` when the writer lags, so a slow disk throttles the
///   served stream rather than piling the whole body into RAM (finding #1).
///
/// Either way only small metadata lives in memory and no full-body map forms.
///
/// [`append`]: Section::append
#[derive(Debug)]
struct Section {
    /// `None` once [`close`](Section::close)d -- the writer task then drains the
    /// remaining queued chunks and flushes. A dropped `Section` (abandoned turn)
    /// also drops the sender(s), so the task always terminates (no hang; AGENTS.md
    /// cancellation rule).
    tx: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
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
        let (tx, rx) = mpsc::channel::<Vec<u8>>(SECTION_CHANNEL_CAPACITY);
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
    /// This is the LOW-VOLUME, single-shot path (the inbound-request body + tests);
    /// the high-volume served tee uses [`Section::poll_sender`] /
    /// [`TurnCaptureState::served_sink`] for real back-pressure instead. Non-blocking
    /// + ordered; a no-op once closed or on an empty frame.
    fn append(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let Ok(guard) = self.tx.lock()
            && let Some(tx) = guard.as_ref()
        {
            match tx.try_send(bytes.to_vec()) {
                Ok(()) => {}
                // Unreached by the single-shot callers (one item into a fresh
                // bounded channel); if it ever were, record it honestly as partial
                // rather than silently dropping bytes (don't-lie-with-zeros).
                Err(mpsc::error::TrySendError::Full(_)) => {
                    self.meta.partial.store(true, Ordering::Release);
                }
                // Writer task already gone; nothing to do.
                Err(mpsc::error::TrySendError::Closed(_)) => {}
            }
        }
    }

    /// A back-pressured [`PollSender`] over this section's bounded writer channel,
    /// for the HTTP served-body tee. Cloning the sender lets the tee `poll_reserve`
    /// a slot from its synchronous `poll_frame` seam and yield `Poll::Pending` when
    /// the writer is behind -- bounding capture memory to
    /// [`SECTION_CHANNEL_CAPACITY`] frames regardless of body size (finding #1).
    /// The tee's clone plus this section's retained sender both drop at turn end
    /// (tee `Drop` → `served_done` → [`close`](Section::close)), so the writer
    /// always terminates. `None` once the section is closed.
    fn poll_sender(&self) -> Option<PollSender<Vec<u8>>> {
        self.tx
            .lock()
            .ok()?
            .as_ref()
            .map(|tx| PollSender::new(tx.clone()))
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
async fn section_writer(mut rx: mpsc::Receiver<Vec<u8>>, meta: Arc<SectionMeta>) {
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

    /// Finding #1 (bounded memory): streaming a large served body through the
    /// back-pressured `ServedSink` keeps the in-flight footprint bounded to
    /// `SECTION_CHANNEL_CAPACITY` frames -- O(CAP), not O(N) -- while EVERY byte
    /// still reaches the section file. The sink is EXACTLY the mechanism the HTTP
    /// tee drives (reserve-before-send from a `poll_frame` seam), so this exercises
    /// the real back-pressure path: `poll_reserve` yields `Pending` whenever the
    /// writer lags and is re-woken as it drains, so a slow disk throttles the
    /// stream instead of accumulating the whole body in RAM.
    #[tokio::test]
    async fn served_sink_streams_large_body_with_bounded_memory() {
        let capture = TurnCapture::enabled(temp_dir_path("bounded-served"));
        let state = capture.start("api_bounded", None, 0).expect("state");
        let mut sink = state.served_sink().expect("served sink");

        // The bound is STRUCTURAL: the channel holds at most this many frames, so
        // no matter how large the body, in-flight memory is O(CAP), not O(N)
        // (assert via the bounded structure's capacity, not a heap probe).
        assert_eq!(
            sink.max_capacity(),
            super::SECTION_CHANNEL_CAPACITY,
            "the served sink is a fixed-capacity channel (bounded memory)"
        );
        // The in-flight cap is a small constant, not proportional to body size.
        const { assert!(super::SECTION_CHANNEL_CAPACITY <= 64) };

        // Stream far more frames than the channel can hold at once (2048 frames of
        // 4 KiB = 8 MiB across many frames), driving the SAME reserve-before-send
        // dance the tee uses. Because the channel is bounded, at most CAP frames are
        // ever queued regardless of the 2048 total; `poll_reserve` back-pressures
        // (Pending) whenever the writer is behind.
        const FRAMES: usize = 2048;
        const FRAME_LEN: usize = 4096;
        let mut expected_len: u64 = 0;
        for i in 0..FRAMES {
            std::future::poll_fn(|cx| sink.poll_reserve(cx))
                .await
                .expect("writer alive");
            let frame = vec![(i % 251) as u8; FRAME_LEN];
            expected_len += frame.len() as u64;
            sink.send(frame).expect("send frame into the reserved slot");
        }
        // Drop the tee's sink, then close the section (mirrors the tee `Drop` →
        // `served_done` order) so the writer drains and terminates.
        drop(sink);
        state.served_done(false);
        state.await_served_closed().await;

        // Every streamed byte reached the section file -- back-pressure throttled,
        // never dropped.
        let contents = std::fs::read(state.served_response_path()).expect("served section file");
        assert_eq!(
            contents.len() as u64,
            expected_len,
            "all streamed bytes reached the section file despite back-pressure"
        );
        assert_eq!(state.served_bytes(), expected_len);
        assert!(
            !state.served_partial(),
            "a cleanly-closed served stream is not partial"
        );
        // Content fidelity: first and last frame patterns survived byte-for-byte.
        assert_eq!(&contents[..FRAME_LEN], vec![0u8; FRAME_LEN].as_slice());
        let last = ((FRAMES - 1) % 251) as u8;
        assert_eq!(
            &contents[contents.len() - FRAME_LEN..],
            vec![last; FRAME_LEN].as_slice()
        );
    }
}
