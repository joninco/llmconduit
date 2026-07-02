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
//! temp file under `<dir>/.work/<api_call_id>/` via a background writer task.
//! Append-only sections (`inbound_request`, `served_response`, and the future
//! `upstream_response`) are fed over a BOUNDED, ordered channel
//! ([`SECTION_CHANNEL_CAPACITY`] frames); the high-volume served-body tee
//! reserves a channel slot BEFORE pulling each frame and yields `Poll::Pending`
//! when the writer is behind, so a slow disk / large streamed body throttles the
//! served stream to disk pace instead of piling the whole body into RAM.
//! `upstream_request`'s whole-body last-writer-wins write
//! ([`Section::replace`]) instead OVERWRITES a single coalescing slot (never
//! queued, so a full/slow channel can never drop the FINAL replace -- review
//! r1); memory there is bounded to exactly ONE pending section-sized `Vec`, not
//! a per-attempt queue. Only small metadata (`{bytes, partial, closed}`) plus
//! that one optional pending `Vec` lives in memory. NO `HashMap<_, full-body>`
//! ever forms -- the bytes go to disk. The final single JSON is assembled later
//! (F1f) by STREAMING those temp files, so a 256 MiB turn is never held in RAM
//! at once.
//!
//! **Task boundaries.**
//! - F1b: HTTP own-gate + inbound-request capture + served-response `Body` tee;
//!   real `start()` (work dir + section writers + registry).
//! - F1c (this task): engine terminal integration ([`TurnCaptureState::engine_done`])
//!   via the RAII [`CaptureGuard`] (+ the [`MiddlewareCaptureGuard`] backstop for a
//!   turn that never reached the engine); the both-`done` assembly barrier
//!   ([`TurnCaptureState::engine_done`] + [`TurnCaptureState::served_done`] are
//!   idempotent latches → when BOTH have fired, FLUSH the section writers,
//!   stream-assemble `<dir>/<api_call_id>.json`, then EVICT the registry entry
//!   (infallible) + BEST-EFFORT delete the `.work/<id>/` dir -- a failed delete
//!   leaves a sweepable orphan for F1f, never blocking the publish). Assembly is
//!   BOUNDED: each section streams from
//!   its temp file through a JSON escaper (never a whole-section RAM load).
//! - F1d: upstream-request capture (carrier on `BackendChatRequest`), written into
//!   the SAME [`TurnCaptureState`] via [`TurnCaptureState::write_upstream_request`].
//! - F1e: raw upstream-response capture + final failed HTTP body, via
//!   [`TurnCaptureState::write_upstream_response`].
//! - F1f: rotation wiring, orphan `.work` sweep, docs, and the base64/atomicity
//!   hardening tests (the streaming assembly + atomic tmp→rename land here in F1c).

use std::collections::HashMap;
use std::io::Read;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
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
        let capture_dir = inner.dir.clone();
        let inner_weak = Arc::downgrade(inner);
        // `new_cyclic` hands the state a `Weak` reference to ITSELF so the both-`done`
        // barrier (`spawn_assembly`) can, from a plain `&self` method, upgrade to an
        // `Arc<Self>` and spawn the async flush/assemble/evict task -- without the tee
        // or the engine guard having to hand the `Arc` back in.
        let state = Arc::new_cyclic(|self_weak| {
            TurnCaptureState::new(
                work_dir,
                capture_dir,
                api_call_id.to_string(),
                model_requested,
                started_ms,
                inner_weak,
                self_weak.clone(),
            )
        });
        // F1c eviction: the both-`done` barrier removes this entry once the artifact is
        // assembled (`engine_done` + `served_done` → assemble → infallible `registry.remove`
        // + best-effort delete `.work/<id>/`). Until then a started turn's entry lives for its
        // duration; body bytes never enter this map (they stream to disk), so the
        // bounded-memory invariant holds regardless.
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

    /// F1c: the engine terminal seam (completed/incomplete/failed/cancelled,
    /// including the pre-spawn failure path) reports the turn's outcome here,
    /// keyed by `api_call_id`. Looks the live state up in the registry and
    /// delegates to [`TurnCaptureState::engine_done`] (the idempotent latch that,
    /// once BOTH the engine and served sides have reported, assembles the artifact
    /// and evicts the registry entry). A no-op when disabled or when no such turn
    /// is live -- the engine normally holds the state directly (via the
    /// [`CaptureGuard`]) and calls the state method; this id-keyed convenience seam
    /// exists for callers that only have the id.
    pub fn engine_done(&self, api_call_id: &str, status: &str, reason: Option<&str>) {
        if let Some(state) = self.state(api_call_id) {
            state.engine_done(status, reason);
        }
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
    /// The base capture dir (`turn_capture_dir`) the final `<api_call_id>.json`
    /// artifact is assembled into (distinct from `work_dir` =
    /// `<capture_dir>/.work/<api_call_id>/`, which holds the temp section files).
    capture_dir: PathBuf,
    /// Back-reference to the owning `TurnCaptureInner` so the both-`done` barrier can
    /// EVICT this turn's registry entry after assembly. `Weak` breaks the
    /// `Inner → registry → Arc<State>` cycle; a failed upgrade (handle dropped at
    /// shutdown) just skips eviction (the whole map is going away anyway).
    inner: Weak<TurnCaptureInner>,
    /// A `Weak` to this very `Arc<TurnCaptureState>` (set via `Arc::new_cyclic`), so
    /// [`spawn_assembly`](Self::spawn_assembly) can upgrade to an `Arc<Self>` and
    /// spawn the async assembly task from a synchronous `&self` latch method.
    self_weak: Weak<TurnCaptureState>,
    /// The served/backend model, stamped by the engine once resolution settles
    /// (`set_model_served`). `None` until then / for a turn that failed before
    /// resolution -- an absent field, never a fabricated empty (don't-lie-with-zeros).
    model_served: Mutex<Option<String>>,
    /// The redacted inbound Anthropic/OpenAI request body (F1b).
    inbound_request: Section,
    /// F1d: the redacted, FINAL on-wire OpenAI `ChatCompletionRequest` (post
    /// profile/lowering/sanitize). Written WHOLE per dispatch attempt via
    /// [`Section::replace`] (last-writer-wins), NOT streamed -- so a shrink-retry
    /// or a failover rebuild's later attempt reflects as the recorded section,
    /// never a concatenation of every attempt tried. Stays OPEN across the
    /// turn's dispatch attempts; closed at the both-`done` barrier
    /// ([`finalize_and_assemble`](Self::finalize_and_assemble)), like the other
    /// sections.
    upstream_request: Section,
    /// F1d (don't-lie-with-zeros): whether
    /// [`write_upstream_request`](Self::write_upstream_request) was EVER called
    /// for this turn. A turn that never dispatches upstream (e.g. a pre-spawn
    /// validation failure) never sets this, so [`write_artifact_file`] OMITS the
    /// `upstream_request` key from the artifact entirely -- absent, never a
    /// fabricated `bytes:0` "measured empty" section.
    ///
    /// [`write_artifact_file`]: Self::write_artifact_file
    upstream_request_written: AtomicBool,
    /// The exact bytes served to the client, teed off the response `Body` (F1b).
    served_response: Section,
    /// Set synchronously by [`CaptureGuard::new`] the instant the engine takes
    /// ownership of the turn (before it returns the stream). The
    /// [`MiddlewareCaptureGuard`] backstop reads it at `Drop`: an UNCLAIMED turn
    /// never reached the engine (a `Json`/extractor rejection, a `convert_request`
    /// error) so the backstop finalizes it `failed`; a claimed turn is left to the
    /// engine's own terminal / `Drop` fallback.
    engine_claimed: AtomicBool,
    /// Set by [`mark_served_tee_installed`](Self::mark_served_tee_installed) the
    /// instant the HTTP served-body tee is installed over the response
    /// (`tee_served_body`). The [`MiddlewareCaptureGuard`] SERVED backstop reads it at
    /// `Drop`: if NO tee was ever installed -- `next.run` unwound/returned AFTER
    /// [`start`](TurnCapture::start) but BEFORE the tee -- the served side would
    /// otherwise never latch and the both-`done` barrier would hang forever, leaking
    /// the registry entry and `.work` dir (finding #2). The backstop then fires a
    /// partial [`served_done`](Self::served_done) itself; when the tee IS installed it
    /// owns `served_done`, so the backstop stays inert.
    served_tee_installed: AtomicBool,
    /// The both-`done` assembly barrier, consolidated under ONE mutex (finding #1):
    /// the two `done` latches, the engine `outcome`, and the exactly-once `finalized`
    /// trigger all live here, so an observer that sees the engine side "done" ALSO
    /// sees the stored outcome, and the "both done -> assemble" winner is decided
    /// INSIDE this lock. Assembly (spawned OUTSIDE the lock) therefore always reads a
    /// fully-stored outcome -- the publish-before-store race is eliminated. The mutex
    /// is NEVER held across an await or blocking IO (only the small latch/outcome
    /// bookkeeping runs under it; the flush + file assembly run after the lock drops).
    barrier: Mutex<AssemblyBarrier>,
    /// Epoch-ms stamped when the both-`done` barrier resolves (the later of
    /// `engine_done`/`served_done`, plus the section flush) -- the turn's finish
    /// time for the `finished_ms` outcome field.
    finished_ms: AtomicU64,
}

impl TurnCaptureState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        work_dir: PathBuf,
        capture_dir: PathBuf,
        api_call_id: String,
        model_requested: Option<String>,
        started_ms: u128,
        inner: Weak<TurnCaptureInner>,
        self_weak: Weak<TurnCaptureState>,
    ) -> Self {
        let inbound_request = Section::new(work_dir.join("inbound_request"));
        let upstream_request = Section::new(work_dir.join("upstream_request"));
        let served_response = Section::new(work_dir.join("served_response"));
        Self {
            api_call_id,
            model_requested,
            started_ms,
            work_dir,
            capture_dir,
            inner,
            self_weak,
            model_served: Mutex::new(None),
            inbound_request,
            upstream_request,
            upstream_request_written: AtomicBool::new(false),
            served_response,
            engine_claimed: AtomicBool::new(false),
            served_tee_installed: AtomicBool::new(false),
            barrier: Mutex::new(AssemblyBarrier::default()),
            finished_ms: AtomicU64::new(0),
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

    /// Path of the `upstream_request` section temp file (F1d).
    pub fn upstream_request_path(&self) -> &Path {
        self.upstream_request.path()
    }

    /// Path of the `served_response` section temp file.
    pub fn served_response_path(&self) -> &Path {
        self.served_response.path()
    }

    /// Whether the `served_response` section is `partial` (the served stream did
    /// not reach a clean end -- client disconnect / mid-stream error / a section
    /// write error / a [`mark_served_degraded`](Self::mark_served_degraded) mark).
    /// `false` for a cleanly-completed response. STICKY: once set, no later call
    /// (including a clean [`served_done`](Self::served_done)`(false)`) can ever
    /// flip it back to `false` -- see [`mark_served_degraded`](Self::mark_served_degraded).
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

    /// F1d: replace the `upstream_request` section with `bytes` -- the redacted,
    /// FINAL on-wire OpenAI `ChatCompletionRequest` for THIS dispatch attempt
    /// (post profile/lowering/sanitize). Called once per attempt
    /// (`upstream.rs::dispatch_chat_stream` → `logged_send_chat_request`), so a
    /// shrink-and-retry or a failover rebuild's later attempt REPLACES an earlier
    /// attempt's bytes (last-writer-wins, GUARANTEED even under backpressure --
    /// review r1) rather than concatenating onto them -- the section is captured
    /// WHOLE per attempt, never streamed. `bytes` is an owned, already-redacted
    /// COPY -- never a slice of a larger buffer (AGENTS.md line 144); the caller
    /// redacts BEFORE calling (AGENTS.md line 137). `upstream_request_written`
    /// (gating whether the artifact embeds this section at all -- don't-lie-
    /// with-zeros) is set ONLY once [`Section::replace`] confirms the bytes were
    /// actually accepted, so a turn whose sole call raced an already-closed
    /// section never fabricates the key.
    pub fn write_upstream_request(&self, bytes: &[u8]) {
        if self.upstream_request.replace(bytes) {
            self.upstream_request_written.store(true, Ordering::Release);
        }
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
    /// the FIRST call closes the section, and only the FIRST call can complete the
    /// both-`done` barrier (`spawn_assembly`).
    pub fn served_done(&self, partial: bool) {
        let trigger = {
            let mut barrier = self.barrier.lock().expect("turn-capture barrier lock");
            if barrier.served_reported {
                return;
            }
            barrier.served_reported = true;
            barrier.take_trigger()
        };
        // Close the section OUTSIDE the barrier lock (this only drops the writer
        // sender -- no IO under the mutex). The `served_reported` latch above
        // guarantees the close + trigger run exactly once.
        self.served_response.close(partial);
        if trigger {
            self.spawn_assembly();
        }
    }

    /// F1b review r2 (don't-lie-with-zeros): mark the `served_response` section
    /// `partial` immediately -- independent of, and possibly well BEFORE,
    /// [`served_done`](Self::served_done). The HTTP served-body tee calls this
    /// the instant its [`ServedSink`] closes early (`SinkClosed` on
    /// `poll_reserve`, or a dropped [`send`](ServedSink::send)): from that point
    /// on, served bytes are still being forwarded to the client (never
    /// interrupted -- a diagnostic failure must never break the served stream)
    /// but are no longer reaching this section, so the capture is truncated
    /// regardless of how the outer response stream itself later ends. Without
    /// this mark, a later clean end-of-stream would make
    /// [`served_done`](Self::served_done)`(false)` the only recorded outcome,
    /// falsely reporting a truncated capture as complete.
    ///
    /// STICKY by construction: [`Section::close`] only ever SETS `partial`,
    /// never clears it, so a later `served_done(false)` cannot erase this mark
    /// -- [`served_partial`](Self::served_partial) reflects it immediately and
    /// permanently. Idempotent and cheap (a single atomic store); safe to call
    /// any number of times, from any point in the stream, even after the
    /// section has already closed.
    pub fn mark_served_degraded(&self) {
        self.served_response.mark_partial();
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

    /// F1c: stamp the served/backend model onto the outcome metadata. The engine
    /// calls this once model resolution settles (via [`CaptureGuard::set_model_served`]).
    /// Last-writer-wins; a turn that fails before resolution simply never sets it,
    /// so `model_served` is ABSENT from the artifact rather than a fabricated empty.
    pub fn set_model_served(&self, model: &str) {
        *self
            .model_served
            .lock()
            .expect("turn-capture model_served lock") = Some(model.to_string());
    }

    /// F1c: mark that the engine has taken ownership of this turn. Called
    /// synchronously by [`CaptureGuard::new`] BEFORE the engine returns the stream,
    /// so the [`MiddlewareCaptureGuard`] backstop (which drops after `next.run`
    /// returns) sees the claim and stays inert for any turn that reached the engine.
    fn mark_engine_claimed(&self) {
        self.engine_claimed.store(true, Ordering::Release);
    }

    /// F1c: whether the engine claimed this turn (see [`mark_engine_claimed`]).
    /// Read by the [`MiddlewareCaptureGuard`] backstop.
    fn is_engine_claimed(&self) -> bool {
        self.engine_claimed.load(Ordering::Acquire)
    }

    /// F1c (finding #2): mark that the HTTP served-body tee has been installed over
    /// the response. Called by `tee_served_body` (the ONLY installer) BEFORE the
    /// middleware backstop can drop, so a claimed+teed turn leaves `served_done`
    /// entirely to the tee's `Drop`. See [`served_tee_installed`](Self#field).
    pub fn mark_served_tee_installed(&self) {
        self.served_tee_installed.store(true, Ordering::Release);
    }

    /// F1c (finding #2): whether the served-body tee was installed (see
    /// [`mark_served_tee_installed`](Self::mark_served_tee_installed)). Read by the
    /// [`MiddlewareCaptureGuard`] served backstop to decide whether it must fire a
    /// partial `served_done` itself (no tee ⇒ a pre-tee unwind ⇒ back it up).
    fn is_served_tee_installed(&self) -> bool {
        self.served_tee_installed.load(Ordering::Acquire)
    }

    /// F1c: the ENGINE terminal seam reports the turn's outcome here. `status` is
    /// the artifact status (`completed`/`incomplete`/`failed`/`cancelled`) and
    /// `reason` the terminal reason -- BOTH sourced from the engine (never the tee).
    /// IDEMPOTENT (first-writer-wins): the FIRST call (the terminal seam, the
    /// [`CaptureGuard`] `Drop` fallback, or the [`MiddlewareCaptureGuard`] backstop)
    /// records the outcome; later calls are inert, so a `Drop` fallback can never
    /// overwrite the real status. Completes the both-`done` barrier when the served
    /// side has also reported.
    pub fn engine_done(&self, status: &str, reason: Option<&str>) {
        let trigger = {
            let mut barrier = self.barrier.lock().expect("turn-capture barrier lock");
            if barrier.engine_reported {
                return;
            }
            // Store the outcome UNDER THE SAME LOCK that flips `engine_reported`, so
            // any observer that later sees the engine side "done" is guaranteed to see
            // the stored outcome too (finding #1: no publish-before-store window). The
            // both-`done` winner is then chosen inside this same critical section.
            barrier.engine_reported = true;
            barrier.outcome = Some(EngineOutcome {
                status: status.to_string(),
                reason: reason.map(str::to_string),
            });
            barrier.take_trigger()
        };
        if trigger {
            self.spawn_assembly();
        }
    }

    /// F1c both-`done` barrier: spawn the flush → assemble → evict task exactly once.
    /// The caller has already WON the trigger under the barrier lock
    /// ([`AssemblyBarrier::take_trigger`]), so this runs OUTSIDE the lock -- the mutex
    /// is never held across the await / blocking IO in
    /// [`finalize_and_assemble`](Self::finalize_and_assemble). Fires on whichever side
    /// reports SECOND. A turn that never reached the engine still resolves: the
    /// [`MiddlewareCaptureGuard`] backstop supplies the engine side AND (finding #2)
    /// the served side, so the barrier can never wait forever.
    fn spawn_assembly(&self) {
        let Some(state) = self.self_weak.upgrade() else {
            return;
        };
        // Assembly is async (await the section flushes) + does blocking file IO, so
        // it must run on the runtime. `start()` only ever runs inside one (the HTTP
        // seam), so `try_current` succeeds for every real turn; the guard keeps the
        // latch total off-runtime rather than panicking.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    state.finalize_and_assemble().await;
                });
            }
            Err(_) => {
                tracing::warn!(
                    api_call_id = %state.api_call_id,
                    "turn-capture: no tokio runtime to assemble artifact; \
                     leaving .work dir for the orphan sweep"
                );
            }
        }
    }

    /// F1c: flush every section writer, then hand off to the blocking assembler
    /// (streaming JSON escape + atomic rename + work-dir delete + registry evict).
    /// Awaiting the `await_*_closed` barriers first guarantees the temp files are
    /// fully drained + fsynced before the assembler reads them (queued disk writes
    /// can lag the stream). Bounded-time: each barrier resolves as soon as its
    /// writer drains its BOUNDED channel, so a cancelled/abandoned turn finalizes
    /// without hanging.
    async fn finalize_and_assemble(self: Arc<Self>) {
        self.inbound_request.await_closed().await;
        // F1d: `upstream_request` stays OPEN across the turn's dispatch attempts
        // (each is a `replace`, last-writer-wins) -- close it now, at the
        // both-`done` barrier, so the writer task drains + flushes the FINAL
        // attempt's bytes before assembly reads the file.
        self.upstream_request.close(false);
        self.upstream_request.await_closed().await;
        self.served_response.await_closed().await;
        // F1e adds its `upstream_response` section flush here (mirror the lines
        // above) once that section exists.
        // Stamp `finished_ms` AFTER the flush barriers so it reflects the turn's true
        // end (both `done`s + the section drain), and is `>= started_ms`.
        self.finished_ms.store(now_ms(), Ordering::Release);
        let state = Arc::clone(&self);
        // The assembly reads the section temp files, JSON-escapes them, writes the
        // artifact, renames, and deletes the work dir -- all synchronous std::fs, so
        // it runs on the blocking pool (AGENTS.md: no blocking IO on the runtime).
        if let Err(err) = tokio::task::spawn_blocking(move || state.assemble_blocking()).await {
            tracing::warn!(
                api_call_id = %self.api_call_id,
                error = %err,
                "turn-capture: assembly task panicked"
            );
        }
    }

    /// F1c (blocking): fully build the tmp artifact by STREAMING every section into
    /// `<capture_dir>/<api_call_id>.json.tmp`, then EVICT the registry entry
    /// (in-memory, infallible), BEST-EFFORT delete the `.work/<id>/` dir, and
    /// PUBLISH via atomic rename LAST. Diagnostic-only: every fs error is logged,
    /// never propagated (a capture artifact must never fail or hang the turn it
    /// describes).
    ///
    /// INVARIANT (F1c review r2): a visible `<id>.json` ⇒ a COMPLETE, valid
    /// artifact AND the registry entry evicted. The `.work/<id>/` dir is deleted
    /// BEST-EFFORT -- if that delete fails the orphan lingers and is reclaimed by
    /// F1f's age-based `.work` sweep (the documented backstop). A work-dir-delete
    /// failure NEVER blocks publishing a valid capture nor the registry eviction,
    /// and a tmp-build failure NEVER publishes a partial/empty final.
    fn assemble_blocking(&self) {
        if let Err(err) = std::fs::create_dir_all(&self.capture_dir) {
            tracing::warn!(
                dir = %self.capture_dir.display(),
                error = %err,
                "turn-capture: failed to create capture dir"
            );
        }
        let tmp = self
            .capture_dir
            .join(format!("{}.json.tmp", self.api_call_id));
        let final_path = self.capture_dir.join(format!("{}.json", self.api_call_id));
        // Ordering (see the INVARIANT above):
        //   1. build the tmp artifact FULLY (all sections streamed in);
        //   2. evict the registry (in-memory, infallible) -- ALWAYS, before publish;
        //   3. best-effort remove the work dir (failure logged, non-blocking);
        //   4. publish LAST via atomic rename tmp -> final.
        match self.write_artifact_file(&tmp) {
            Ok(()) => {
                // (2) Registry eviction is in-memory and infallible; do it BEFORE the
                // publish so a visible artifact ALWAYS implies an evicted entry.
                self.evict_registry();
                // (3) Best-effort work-dir delete: on the happy path the dir is gone
                // before the artifact is visible; a failure is logged and the orphan
                // left for F1f's `.work` sweep. It must NOT gate the publish below.
                self.remove_work_dir_best_effort();
                // (4) Publish LAST: atomic rename of the fully-written tmp over the
                // final name so a reader (or the sweep) never sees a half-written file.
                if let Err(err) = std::fs::rename(&tmp, &final_path) {
                    tracing::warn!(
                        path = %final_path.display(),
                        error = %err,
                        "turn-capture: failed to publish artifact (rename)"
                    );
                    let _ = std::fs::remove_file(&tmp);
                }
            }
            Err(err) => {
                // (1) failed: NEVER publish a partial/empty final. Drop the tmp, but
                // still evict (no memory leak) + best-effort clean the work dir.
                tracing::warn!(
                    path = %tmp.display(),
                    error = %err,
                    "turn-capture: failed to write artifact"
                );
                let _ = std::fs::remove_file(&tmp);
                self.evict_registry();
                self.remove_work_dir_best_effort();
            }
        }
    }

    /// F1c: EVICT this turn's registry entry (closes F1b's deferred leak).
    /// In-memory and INFALLIBLE -- a `HashMap` remove cannot fail -- so it is
    /// performed UNCONDITIONALLY on every finalize path (success OR write-error),
    /// gated on NOTHING external: there is never a registry (memory) leak
    /// regardless of disk state. A failed `Weak` upgrade just means the capture
    /// handle (and its whole registry) is already gone at shutdown -- nothing to
    /// evict. A poisoned lock is recovered (a panic in an unrelated turn must not
    /// strand this eviction), so eviction truly never fails.
    fn evict_registry(&self) {
        if let Some(inner) = self.inner.upgrade() {
            inner
                .registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.api_call_id);
        }
    }

    /// F1c: BEST-EFFORT delete the per-turn `.work/<id>/` section dir (the artifact
    /// now embeds its bytes). A failure is LOGGED, never propagated, and does NOT
    /// gate publishing or eviction -- the orphan is reclaimed by F1f's age-based
    /// `.work` sweep (the documented backstop). `NotFound` counts as success
    /// (already gone). Diagnostic-only.
    fn remove_work_dir_best_effort(&self) {
        if let Err(err) = std::fs::remove_dir_all(&self.work_dir)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                dir = %self.work_dir.display(),
                error = %err,
                "turn-capture: failed to remove work dir (best-effort); \
                 leaving orphan for the F1f .work sweep"
            );
        }
    }

    /// F1c: write the single-JSON artifact to `tmp`, STREAMING each section from its
    /// temp file through a bounded JSON escaper -- never loading a whole section into
    /// RAM (spec Design #2/#5; AGENTS.md bounded-memory invariant). Outcome metadata
    /// is small and written directly; each section embeds as a `{bytes, partial,
    /// encoding, content}` object where `content` is a JSON value (a request that
    /// parses), a JSON string (valid UTF-8), or a base64 string (non-UTF-8).
    fn write_artifact_file(&self, tmp: &Path) -> std::io::Result<()> {
        let file = std::fs::File::create(tmp)?;
        let mut w = std::io::BufWriter::new(file);

        w.write_all(b"{\"api_call_id\":")?;
        write_json_string(&mut w, &self.api_call_id)?;
        if let Some(model) = &self.model_requested {
            w.write_all(b",\"model_requested\":")?;
            write_json_string(&mut w, model)?;
        }
        if let Some(model) = self
            .model_served
            .lock()
            .expect("turn-capture model_served lock")
            .as_deref()
        {
            w.write_all(b",\"model_served\":")?;
            write_json_string(&mut w, model)?;
        }
        write!(w, ",\"started_ms\":{}", self.started_ms)?;
        write!(
            w,
            ",\"finished_ms\":{}",
            self.finished_ms.load(Ordering::Acquire)
        )?;
        let outcome = self
            .barrier
            .lock()
            .expect("turn-capture barrier lock")
            .outcome
            .clone();
        let (status, reason) = match outcome {
            Some(outcome) => (outcome.status, outcome.reason),
            // Assembly is only ever triggered from inside the barrier lock AFTER
            // `engine_done` stored the outcome (finding #1), so `outcome` is always
            // `Some` here; default defensively rather than fabricate a success.
            None => ("failed".to_string(), Some("no_engine_outcome".to_string())),
        };
        w.write_all(b",\"status\":")?;
        write_json_string(&mut w, &status)?;
        if let Some(reason) = &reason {
            w.write_all(b",\"terminal_reason\":")?;
            write_json_string(&mut w, reason)?;
        }

        // Section-agnostic: F1e appends its section to this list once it exists.
        // An ABSENT section is simply omitted (never a fabricated empty measured
        // value). Requests may embed as a JSON value; responses embed as strings.
        w.write_all(b",\"sections\":{")?;
        write_section(&mut w, "inbound_request", &self.inbound_request, true)?;
        // F1d: `upstream_request` is gated on `upstream_request_written` -- a turn
        // that never dispatched upstream (e.g. a pre-spawn validation failure)
        // never wrote it, so the key is OMITTED entirely rather than emitted with
        // `bytes:0` (don't-lie-with-zeros: absent, not empty-measured).
        if self.upstream_request_written.load(Ordering::Acquire) {
            w.write_all(b",")?;
            write_section(&mut w, "upstream_request", &self.upstream_request, true)?;
        }
        w.write_all(b",")?;
        write_section(&mut w, "served_response", &self.served_response, false)?;
        w.write_all(b"}}")?;

        w.flush()?;
        // fsync the artifact before the caller renames it into place (durability +
        // a torn artifact is never published).
        w.into_inner()
            .map_err(std::io::IntoInnerError::into_error)?
            .sync_all()?;
        Ok(())
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
    inner: PollSender<SectionChunk>,
}

/// The served-section writer is gone -- its bounded channel closed (typically
/// after a section write error dropped the receiver). The tee drops the sink on
/// this and keeps serving the client byte-for-byte WITHOUT capture (a diagnostic
/// failure must never break the served stream -- AGENTS.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SinkClosed;

impl ServedSink {
    fn new(inner: PollSender<SectionChunk>) -> Self {
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
        self.inner
            .send_item(SectionChunk::Append(bytes))
            .map_err(|_| SinkClosed)
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
/// served tee back-pressures on `Poll::Pending` once the channel is full. NOTE:
/// [`Section::replace`]'s whole-body write does NOT use this channel (review
/// r1) -- see [`SectionMeta::pending_replace`] -- so a full channel can never
/// cause a dropped `replace`.
const SECTION_CHANNEL_CAPACITY: usize = 16;

/// One item queued to a section's background writer over the bounded, ordered
/// channel: the streaming/incremental write (the served-body tee, the
/// single-shot inbound-request write). F1d's last-writer-wins whole-body write
/// (`upstream_request`, via [`Section::replace`]) does NOT flow through this
/// channel or this type at all -- a bounded channel can silently DROP a queued
/// item when full, which would leave a STALE attempt's bytes as the committed
/// content instead of the final one (review r1's bug). See
/// [`SectionMeta::pending_replace`] for the guaranteed-delivery mechanism it
/// uses instead.
#[derive(Debug)]
enum SectionChunk {
    Append(Vec<u8>),
}

/// One incremental capture section. Bytes stream to a per-turn temp file on a
/// background writer task WITHOUT buffering the whole body. Two write paths feed
/// it, with DIFFERENT delivery guarantees:
/// - Low-volume, single-shot writers ([`append`], used for the inbound-request
///   body and tests) `try_send` a copy over a BOUNDED, ordered (FIFO) channel of
///   at most [`SECTION_CHANNEL_CAPACITY`] frames -- they never fill it. The
///   high-volume served-body tee takes a back-pressured [`ServedSink`]
///   ([`TurnCaptureState::served_sink`]) over the SAME channel: it reserves a
///   slot before each frame and yields `Poll::Pending` when the writer lags, so
///   a slow disk throttles the served stream rather than piling the whole body
///   into RAM (finding #1).
/// - F1d's whole-body last-writer-wins writer ([`replace`], used for
///   `upstream_request`) does NOT use that channel (review r1: a bounded
///   channel can DROP a queued item when full, which would silently commit a
///   STALE attempt's bytes instead of the final one). Instead it OVERWRITES the
///   [`SectionMeta::pending_replace`] slot (O(1), never dropped -- a newer
///   replace simply supersedes an undrained older one) and wakes the writer,
///   which applies the LATEST slot value the next time it looks.
///
/// Either way only small metadata (plus, for `replace`, at most one pending
/// section-sized `Vec`) lives in memory and no full-body map forms.
///
/// [`append`]: Section::append
/// [`replace`]: Section::replace
#[derive(Debug)]
struct Section {
    /// `None` once [`close`](Section::close)d -- the writer task then drains the
    /// remaining queued chunks and flushes. A dropped `Section` (abandoned turn)
    /// also drops the sender(s), so the task always terminates (no hang; AGENTS.md
    /// cancellation rule). Also gates [`replace`](Section::replace): a closed
    /// section rejects a late replace (mirrors `append`'s no-op-once-closed).
    tx: Mutex<Option<mpsc::Sender<SectionChunk>>>,
    meta: Arc<SectionMeta>,
}

/// The coalescing last-writer-wins slot for [`Section::replace`] PLUS its
/// accept-gate, held together under ONE mutex ([`SectionMeta::pending_replace`])
/// so "accept a replace" and the writer's close-side "take the final value +
/// seal" are MUTUALLY EXCLUSIVE (F1d review r2 -- closes the accept-vs-close
/// race). Without this atomicity a `replace` could pass its not-yet-closed
/// check, `close` + the writer's final post-close slot-check could then run
/// (observe an EMPTY slot) and mark the section `closed`, and only THEN would
/// the racing `replace` store its bytes -- ORPHANED in a slot no one drains, yet
/// reported as a successful write while `await_closed` resolved on a stale value.
#[derive(Debug, Default)]
struct ReplaceSlot {
    /// The latest whole-body replace the writer has not yet applied, or `None`.
    /// `replace` overwrites it (O(1), last-writer-wins -- a newer replace simply
    /// supersedes an undrained older one); the writer takes it (truncate + write
    /// exactly these bytes).
    pending: Option<Vec<u8>>,
    /// Set `true` by the writer's FINAL (close-side) take, ATOMICALLY with taking
    /// the last `pending` value under this shared mutex. Once set, no further
    /// `replace` is accepted -- the writer has committed the final value and is
    /// about to flush + mark the section `closed`, so a later store would be
    /// orphaned. This flag IS the accept-vs-close barrier (review r2).
    sealed: bool,
}

#[derive(Debug)]
struct SectionMeta {
    path: PathBuf,
    bytes: AtomicU64,
    partial: AtomicBool,
    closed: AtomicBool,
    /// Notified once, when the writer task has drained + flushed + set `closed`.
    done: Notify,
    /// F1d review r1/r2: the coalescing last-writer-wins slot for
    /// [`Section::replace`], PLUS the accept-gate (`sealed`) that makes accepting
    /// a replace and the writer's final close-side take MUTUALLY EXCLUSIVE under
    /// this ONE mutex (r2). `replace` OVERWRITES the pending value in O(1) (never
    /// drops a write -- a newer replace supersedes an undrained older one, so
    /// memory is bounded to exactly ONE pending `Vec` regardless of attempt
    /// count) IFF the slot is not yet sealed; the writer applies the pending
    /// value (file truncate + write of exactly those bytes) whenever it observes
    /// it, and on close ATOMICALLY takes the FINAL value AND sets `sealed` so any
    /// later `replace` is rejected (`false`) instead of orphaned. See
    /// [`ReplaceSlot`].
    pending_replace: Mutex<ReplaceSlot>,
    /// Wakes the writer task when `pending_replace` is freshly set. Purely a
    /// "go look" signal -- the DATA lives in `pending_replace`, so coalescing
    /// several `notify_one` calls (from several rapid `replace`s arriving
    /// before the writer catches up) into one wake is correct: whenever the
    /// writer next looks, it reads whatever currently sits in the slot, which
    /// is always the LATEST replace.
    replace_ready: Notify,
}

impl Section {
    fn new(path: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel::<SectionChunk>(SECTION_CHANNEL_CAPACITY);
        let meta = Arc::new(SectionMeta {
            path,
            bytes: AtomicU64::new(0),
            partial: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            done: Notify::new(),
            pending_replace: Mutex::new(ReplaceSlot::default()),
            replace_ready: Notify::new(),
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
            match tx.try_send(SectionChunk::Append(bytes.to_vec())) {
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

    /// F1d review r1: replace the section's ENTIRE content with `bytes` --
    /// GUARANTEED last-writer-wins, even under backpressure. Used by
    /// `upstream_request`: the on-wire request is captured WHOLE per dispatch
    /// attempt (never streamed incrementally), so a shrink-retry or a failover
    /// rebuild's later attempt must DISCARD an earlier attempt's bytes rather
    /// than concatenate onto them (unlike [`append`](Section::append)).
    ///
    /// Unlike the old `try_send`-onto-the-bounded-channel design (which could
    /// silently DROP a replace when the channel was full, leaving a STALE
    /// attempt's bytes as the committed content -- the exact bug this fixes),
    /// this OVERWRITES the [`SectionMeta::pending_replace`] slot in O(1): a
    /// newer `replace` simply supersedes an undrained older one, so no write is
    /// EVER dropped and memory stays bounded to exactly one pending
    /// section-sized `Vec` (never a queue that grows with attempt count).
    /// COPIES `bytes` into an owned `Vec` (never retains a caller slice --
    /// AGENTS.md line 144).
    ///
    /// ACCEPT vs CLOSE is ATOMIC (review r2): the sealed-check and the slot store
    /// run under ONE lock -- the SAME [`ReplaceSlot`] mutex the writer's
    /// close-side final take seals -- so a `replace` either lands before that take
    /// (accepted, returns `true`, and its bytes are GUARANTEED the committed
    /// content, flushed before [`await_closed`](Section::await_closed) resolves)
    /// or arrives after the seal (rejected, returns `false`, bytes NEVER stored,
    /// so they can't be orphaned in an already-drained slot). It is IMPOSSIBLE to
    /// return `true` yet not be committed, or to be accepted after the section
    /// sealed. The caller gates "written" bookkeeping on the return value so a
    /// rejected write is never recorded as if it landed (don't-lie-with-zeros).
    fn replace(&self, bytes: &[u8]) -> bool {
        {
            let mut slot = self
                .meta
                .pending_replace
                .lock()
                .expect("turn-capture pending-replace lock");
            if slot.sealed {
                // The writer's close-side final take has already SEALED the slot
                // (it took the FINAL value and is flushing / has marked the
                // section closed). Storing now would ORPHAN these bytes -- never
                // committed, never awaited -- so reject honestly with `false`
                // (the caller then never records a never-written attempt;
                // don't-lie-with-zeros) rather than returning a false-true.
                return false;
            }
            // Not sealed: overwrite the pending value (last-writer-wins, O(1)).
            // Because this sealed-check + store share the slot lock with the
            // writer's seal, an accepted store is GUARANTEED to be taken and
            // committed before `await_closed` resolves -- it can never be
            // orphaned by a close racing between the check and the store (the
            // exact review-r2 bug this fixes). `tx` is intentionally untouched:
            // the seal, not the sender, is the accept-gate.
            slot.pending = Some(bytes.to_vec());
        }
        // Wake the writer OUTSIDE the slot lock: a pure "go look" signal (the
        // DATA is already durably in the slot), so coalescing several notifies is
        // correct -- the writer reads the LATEST value when it next looks.
        self.meta.replace_ready.notify_one();
        true
    }

    /// A back-pressured [`PollSender`] over this section's bounded writer channel,
    /// for the HTTP served-body tee. Cloning the sender lets the tee `poll_reserve`
    /// a slot from its synchronous `poll_frame` seam and yield `Poll::Pending` when
    /// the writer is behind -- bounding capture memory to
    /// [`SECTION_CHANNEL_CAPACITY`] frames regardless of body size (finding #1).
    /// The tee's clone plus this section's retained sender both drop at turn end
    /// (tee `Drop` → `served_done` → [`close`](Section::close)), so the writer
    /// always terminates. `None` once the section is closed.
    fn poll_sender(&self) -> Option<PollSender<SectionChunk>> {
        self.tx
            .lock()
            .ok()?
            .as_ref()
            .map(|tx| PollSender::new(tx.clone()))
    }

    /// Close the section. Records `partial` and drops the sender so the writer
    /// task drains remaining chunks, flushes, and finishes. Idempotent. NOTE:
    /// only ever SETS `partial` (never clears it back to `false`), which is what
    /// makes [`mark_partial`](Section::mark_partial) sticky against a later
    /// `close(false)`.
    fn close(&self, partial: bool) {
        if partial {
            self.meta.partial.store(true, Ordering::Release);
        }
        if let Ok(mut guard) = self.tx.lock() {
            *guard = None;
        }
    }

    /// Mark the section `partial` right now, independent of [`close`](Section::close)
    /// (the section stays open -- the sender is untouched). Backs
    /// [`TurnCaptureState::mark_served_degraded`]; see there for the "why" and
    /// the stickiness guarantee. A single atomic store.
    fn mark_partial(&self) {
        self.meta.partial.store(true, Ordering::Release);
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
/// (all fs IO via `tokio::fs`, i.e. off the runtime threads), then loops,
/// PRIORITIZING the coalescing [`SectionMeta::pending_replace`] slot (F1d
/// review r1's guaranteed last-writer-wins whole-body write -- applied by
/// truncating the file and writing exactly the slot's bytes) ahead of draining
/// the next queued [`SectionChunk::Append`] chunk. When the sender drops
/// (section closed or turn dropped) it ATOMICALLY takes one final pending replace
/// that raced the close AND seals the slot (so a `replace` racing after is
/// rejected, never orphaned -- review r2), then flushes, fsyncs, and marks the
/// section `closed` -- so [`Section::await_closed`] always observes the LAST
/// accepted replace's bytes, never a stale earlier one. A file-create/write error
/// marks the section `partial`
/// and is logged, never propagated (a diagnostic artifact must never fail or
/// hang the turn).
async fn section_writer(mut rx: mpsc::Receiver<SectionChunk>, meta: Arc<SectionMeta>) {
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

    loop {
        // Register interest in the next replace-wake BEFORE checking the slot
        // (register-before-check, mirroring `Section::await_closed`), so a
        // `replace` landing concurrently with this check is never missed.
        let notified = meta.replace_ready.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        // The coalescing slot always wins over the next queued `Append`: apply
        // it in one shot -- truncate + write the LATEST bytes only, never a
        // queue of superseded attempts -- then loop back around.
        if let Some(bytes) = take_pending_replace(&meta) {
            apply_replace(&mut file, &meta, bytes).await;
            continue;
        }

        tokio::select! {
            maybe_chunk = rx.recv() => {
                match maybe_chunk {
                    Some(SectionChunk::Append(bytes)) => {
                        write_append(&mut file, &meta, bytes).await;
                    }
                    // Sender dropped (section closed or turn dropped): stop
                    // draining the channel and fall through to the final
                    // pending-replace check + flush below.
                    None => break,
                }
            }
            _ = notified.as_mut() => {
                // Woken by a fresh replace; loop back to the top and apply it.
            }
        }
    }

    // Channel closed: ATOMICALLY take the FINAL pending replace that raced the
    // close AND seal the slot (under one lock), so any `replace` arriving after
    // this point is rejected (`false`) rather than orphaned in a slot no one
    // drains. Take-under-lock, then write AFTER releasing the lock (never hold
    // the mutex across the disk write). Assembly reads the file only AFTER
    // `await_closed` resolves (below), so it always sees this FINAL value.
    if let Some(bytes) = seal_and_take_pending_replace(&meta) {
        apply_replace(&mut file, &meta, bytes).await;
    }

    if let Some(mut file) = file {
        let _ = file.flush().await;
        let _ = file.sync_all().await;
    }
    meta.closed.store(true, Ordering::Release);
    meta.done.notify_waiters();
}

/// In-loop take: remove the [`SectionMeta::pending_replace`] slot's pending value
/// (if any) WITHOUT sealing, so replaces arriving while the writer is still
/// draining are still accepted. Under the slot's own mutex.
fn take_pending_replace(meta: &SectionMeta) -> Option<Vec<u8>> {
    meta.pending_replace
        .lock()
        .expect("turn-capture pending-replace lock")
        .pending
        .take()
}

/// Close-side final take + SEAL (review r2): under ONE lock, atomically take the
/// LAST pending value AND set `sealed`, making ACCEPT ([`Section::replace`]) and
/// this take mutually exclusive -- a racing replace either lands before this (its
/// bytes are the value returned here, guaranteed committed before `meta.closed`
/// is set) or after (rejected with `false`, never orphaned). Called exactly once,
/// after the writer's channel closed. Returns the taken bytes; the caller writes
/// them AFTER the lock is released (never holds the mutex across the disk write).
fn seal_and_take_pending_replace(meta: &SectionMeta) -> Option<Vec<u8>> {
    let mut slot = meta
        .pending_replace
        .lock()
        .expect("turn-capture pending-replace lock");
    slot.sealed = true;
    slot.pending.take()
}

/// Write one queued `Append` chunk to the currently-open file. A no-op besides
/// draining if the file was never opened (or a prior replace's re-create just
/// failed) -- the section is already flagged `partial` in that case.
async fn write_append(file: &mut Option<tokio::fs::File>, meta: &SectionMeta, bytes: Vec<u8>) {
    let Some(open_file) = file.as_mut() else {
        return;
    };
    match open_file.write_all(&bytes).await {
        Ok(()) => {
            meta.bytes.fetch_add(bytes.len() as u64, Ordering::AcqRel);
        }
        Err(err) => {
            tracing::warn!(
                path = %meta.path.display(),
                error = %err,
                "turn-capture: failed to write to section file"
            );
            meta.partial.store(true, Ordering::Release);
        }
    }
}

/// Apply one whole-body replace: truncate (re-create) the section file, reset
/// the `bytes` counter to 0, then write `bytes` as its ENTIRE content -- F1d's
/// last-writer-wins whole-body write, so the section reflects only THIS
/// replacement's length, never a concatenation with an earlier attempt. On a
/// re-create failure the bytes are dropped (nothing to write into) and the
/// section is flagged `partial`, mirroring the original on-full-replace-error
/// handling.
async fn apply_replace(file: &mut Option<tokio::fs::File>, meta: &SectionMeta, bytes: Vec<u8>) {
    match tokio::fs::File::create(&meta.path).await {
        Ok(new_file) => {
            *file = Some(new_file);
            meta.bytes.store(0, Ordering::Release);
        }
        Err(err) => {
            tracing::warn!(
                path = %meta.path.display(),
                error = %err,
                "turn-capture: failed to re-create section file for replace"
            );
            meta.partial.store(true, Ordering::Release);
            *file = None;
            return;
        }
    }
    write_append(file, meta, bytes).await;
}

/// The both-`done` assembly barrier state, consolidated under ONE mutex on
/// [`TurnCaptureState`] (finding #1). Holding the two `done` latches, the engine
/// `outcome`, and the exactly-once `finalized` trigger together closes the
/// publish-before-store race: [`TurnCaptureState::engine_done`] stores `outcome` in
/// the SAME critical section that flips `engine_reported`, and the "both done ->
/// assemble" winner is chosen inside the lock via [`take_trigger`](Self::take_trigger),
/// so the spawned assembly always reads a fully-stored outcome. Cheap bookkeeping
/// only; the lock is never held across a flush/await or file IO.
#[derive(Debug, Default)]
struct AssemblyBarrier {
    /// The served side has latched its `done` (the tee's `Drop`, or the
    /// [`MiddlewareCaptureGuard`] served backstop). First-writer-wins.
    served_reported: bool,
    /// The engine side has latched its terminal (the engine seam, the
    /// [`CaptureGuard`] `Drop` fallback, or the [`MiddlewareCaptureGuard`] backstop).
    /// First-writer-wins; whenever this is `true`, `outcome` is guaranteed `Some`
    /// (both are written together under the barrier lock).
    engine_reported: bool,
    /// The terminal outcome recorded by the FIRST `engine_done`. `status` is the
    /// artifact status (`completed`/`incomplete`/`failed`/`cancelled`), mapped from
    /// the engine's `FlowStatus` at the terminal seam (NEVER from the tee).
    outcome: Option<EngineOutcome>,
    /// Exactly-once assembly trigger: set `true` by whichever `done` first observes
    /// BOTH sides reported; every later [`take_trigger`](Self::take_trigger) is inert.
    finalized: bool,
}

impl AssemblyBarrier {
    /// Decide, UNDER the barrier lock, whether THIS `done` is the one that completes
    /// the barrier: `true` for the single caller that observes BOTH sides reported
    /// with assembly not yet triggered, flipping `finalized` so every later call is
    /// inert. The caller spawns assembly OUTSIDE the lock. Because the winning caller
    /// held the lock -- where `engine_reported == true` implies a stored `outcome` --
    /// the spawned assembly is guaranteed to read a fully-stored outcome (finding #1).
    fn take_trigger(&mut self) -> bool {
        if self.engine_reported && self.served_reported && !self.finalized {
            self.finalized = true;
            true
        } else {
            false
        }
    }
}

/// The terminal outcome the engine reports via [`TurnCaptureState::engine_done`].
/// `status` is already the artifact status string (mapped from the engine's
/// `FlowStatus` at the seam), so assembly never re-derives it.
#[derive(Debug, Clone)]
struct EngineOutcome {
    status: String,
    reason: Option<String>,
}

/// Current wall-clock time as epoch milliseconds (`u64`; epoch-ms fits until well
/// past year 2100). Matches the `started_ms` clock the HTTP seam stamps.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// F1c RAII **engine** capture guard: owns the engine side of the both-`done`
/// barrier for a turn whose capture is active. Constructed early in
/// `stream_responses_with_api_call_id` (so it covers the pre-spawn `?` paths) and
/// moved into the terminal `tokio::spawn`, mirroring the dashboard's
/// `TelemetryGuard` (`dashboard_flow.rs`). `new` CLAIMS the turn synchronously so
/// the [`MiddlewareCaptureGuard`] backstop stays inert for any turn that reached the
/// engine. The engine calls [`finalize`](Self::finalize) on every terminal
/// (completed/incomplete/failed/cancelled, incl. pre-spawn); its `Drop` is the
/// fallback that finalizes an abandoned/panicked turn (`failed`, or `cancelled` if
/// the abort token fired). All calls funnel through the idempotent
/// [`TurnCaptureState::engine_done`], so the explicit terminal always wins and a
/// later `Drop` is inert.
pub struct CaptureGuard {
    state: Arc<TurnCaptureState>,
    abort_token: CancellationToken,
}

impl CaptureGuard {
    /// Build the guard and CLAIM the turn (synchronously, before the engine returns
    /// its stream) so the middleware backstop knows the engine took ownership.
    pub fn new(state: Arc<TurnCaptureState>, abort_token: CancellationToken) -> Self {
        state.mark_engine_claimed();
        Self { state, abort_token }
    }

    /// Explicitly finalize with the engine's terminal `status` + `reason`. Idempotent
    /// first-writer-wins (a later `Drop` fallback no-ops).
    pub fn finalize(&self, status: &str, reason: Option<&str>) {
        self.state.engine_done(status, reason);
    }

    /// Stamp the served/backend model onto the outcome metadata (once resolution
    /// settles). Forwards to [`TurnCaptureState::set_model_served`].
    pub fn set_model_served(&self, model: &str) {
        self.state.set_model_served(model);
    }
}

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        // Fallback for a turn abandoned without an explicit terminal (a panic in
        // `run_turn`, an early path that forgot to finalize): `cancelled` iff the
        // abort token fired, else `failed`. Idempotent -- inert if the engine already
        // finalized (the CAS-equivalent `engine_reported` latch).
        let status = if self.abort_token.is_cancelled() {
            "cancelled"
        } else {
            "failed"
        };
        self.state.engine_done(status, Some("dropped"));
    }
}

/// F1c RAII **middleware** backstop: closes BOTH "one side never fired" holes in the
/// both-`done` barrier. Held by `log_api_call` across `next.run` (mirroring the
/// dashboard's `MiddlewareGuard`), so its `Drop` fires when the middleware returns OR
/// unwinds.
///
/// - **Engine side.** If the turn NEVER reached the engine (a `Json` extractor /
///   `convert_request` rejection returns before `stream_responses_with_api_call_id`),
///   no [`CaptureGuard`] was built and the turn is UNCLAIMED at `Drop`, so this
///   finalizes the engine side `failed`/`"unhandled"`. A CLAIMED turn is left to the
///   engine's own terminal / `CaptureGuard` `Drop`.
/// - **Served side (finding #2).** If `next.run` unwinds (panics) or returns AFTER
///   [`TurnCapture::start`] but BEFORE `tee_served_body` installs the served tee, the
///   served side would otherwise never latch -- the barrier would wait forever and
///   `.work/<id>/` + the registry entry would leak. So if NO tee was installed
///   ([`is_served_tee_installed`](TurnCaptureState::is_served_tee_installed) is
///   false), this fires a partial `served_done` itself. When the tee IS installed it
///   owns `served_done`, so this served backstop stays inert on the normal path.
///
/// Both backstops are IDEMPOTENT (the barrier latches first-writer-wins), so on the
/// normal path -- claimed engine + installed tee -- this whole `Drop` is inert.
pub struct MiddlewareCaptureGuard {
    state: Arc<TurnCaptureState>,
}

impl MiddlewareCaptureGuard {
    /// Build the backstop over the turn's shared state (a cheap `Arc` clone).
    pub fn new(state: Arc<TurnCaptureState>) -> Self {
        Self { state }
    }
}

impl Drop for MiddlewareCaptureGuard {
    fn drop(&mut self) {
        // Engine-side backstop: an UNCLAIMED turn never reached the engine, so no
        // `CaptureGuard` will finalize it -- do it here.
        if !self.state.is_engine_claimed() {
            self.state.engine_done("failed", Some("unhandled"));
        }
        // Served-side backstop (finding #2): if no served tee was ever installed
        // (`next.run` unwound/returned after `start()` but before `tee_served_body`),
        // fire a partial `served_done` so the both-`done` barrier resolves instead of
        // leaking the registry entry + `.work` dir forever. Idempotent: on the normal
        // path the tee set `served_tee_installed` before this guard drops and owns
        // `served_done`, so this is inert.
        if !self.state.is_served_tee_installed() {
            self.state.served_done(true);
        }
    }
}

/// Write a JSON string literal for `value` (quotes + full escaping) via serde_json,
/// so the small outcome-metadata strings are escaped exactly like any JSON string.
fn write_json_string<W: std::io::Write>(w: &mut W, value: &str) -> std::io::Result<()> {
    // serde_json escapes into an owned `String`; these are SMALL metadata values
    // (ids/model names/reasons), never a section body -- the bounded-memory rule
    // applies to the section streams below, which never go through here.
    let encoded = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string());
    w.write_all(encoded.as_bytes())
}

/// Embed one section as `"<name>":{"bytes":N,"partial":B,"encoding":E,"content":C}`,
/// STREAMING the section temp file for `content` so a multi-MiB section is never
/// held in RAM. `is_request` allows a request that parses to embed as a raw JSON
/// VALUE; a response always embeds as a string. A missing/unreadable file yields
/// `content:null` + `encoding:"absent"` (don't-lie-with-zeros -- never a fabricated
/// empty string that reads as "measured empty").
fn write_section<W: std::io::Write>(
    w: &mut W,
    name: &str,
    section: &Section,
    is_request: bool,
) -> std::io::Result<()> {
    let path = section.path();
    let bytes = section.bytes();
    let partial = section.partial();

    w.write_all(b"\"")?;
    w.write_all(name.as_bytes())?;
    w.write_all(b"\":{\"bytes\":")?;
    write!(w, "{bytes}")?;
    w.write_all(b",\"partial\":")?;
    w.write_all(if partial { b"true" } else { b"false" })?;

    if !path.exists() {
        w.write_all(b",\"encoding\":\"absent\",\"content\":null}")?;
        return Ok(());
    }

    // Classify by STREAMING the file (bounded): a request that parses as one JSON
    // value embeds raw; else valid UTF-8 embeds as a JSON string; else base64.
    let encoding = if is_request && section_is_valid_json(path) {
        "json"
    } else if section_is_valid_utf8(path) {
        "utf8"
    } else {
        "base64"
    };
    w.write_all(b",\"encoding\":\"")?;
    w.write_all(encoding.as_bytes())?;
    w.write_all(b"\",\"content\":")?;
    match encoding {
        // The file bytes ARE a valid JSON value -- copy them in verbatim (bounded).
        "json" => stream_file_raw(path, w)?,
        // Valid UTF-8: emit as a JSON string, escaping as we stream (bounded).
        "utf8" => {
            w.write_all(b"\"")?;
            stream_file_json_escaped(path, w)?;
            w.write_all(b"\"")?;
        }
        // Non-UTF-8: base64 string + the `base64` encoding marker (bounded stream).
        _ => {
            w.write_all(b"\"")?;
            stream_file_base64(path, w)?;
            w.write_all(b"\"")?;
        }
    }
    w.write_all(b"}")?;
    Ok(())
}

/// Streaming, BOUNDED check that the file at `path` is exactly one JSON value (only
/// trailing whitespace after). `serde_json::from_reader` over `IgnoredAny` skips
/// tokens without building a `Value`, so memory is O(nesting depth), not O(size);
/// invalid UTF-8 surfaces as a parse error (returns `false`).
fn section_is_valid_json(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let reader = std::io::BufReader::new(file);
    serde_json::from_reader::<_, serde::de::IgnoredAny>(reader).is_ok()
}

/// Streaming, BOUNDED UTF-8 validation: read in fixed chunks, validating with a
/// carry of at most 3 bytes for a multibyte sequence split across a chunk boundary.
/// Memory is O(chunk), never O(file size).
fn section_is_valid_utf8(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut reader = std::io::BufReader::new(file);
    let mut carry: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => return false,
        };
        carry.extend_from_slice(&buf[..n]);
        match std::str::from_utf8(&carry) {
            Ok(_) => carry.clear(),
            Err(err) => {
                // A genuine invalid sequence (has `error_len`) is a hard fail; an
                // incomplete trailing sequence (no `error_len`) carries to the next
                // chunk. Drain the valid prefix so `carry` stays bounded (<= 3 bytes).
                if err.error_len().is_some() {
                    return false;
                }
                let valid = err.valid_up_to();
                carry.drain(..valid);
            }
        }
    }
    // Any leftover carry at EOF is a truncated multibyte sequence -> not valid UTF-8.
    carry.is_empty()
}

/// Copy the section file verbatim into `w` (for the already-valid-JSON case), in
/// bounded chunks via `std::io::copy`.
fn stream_file_raw<W: std::io::Write>(path: &Path, w: &mut W) -> std::io::Result<()> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    std::io::copy(&mut reader, w)?;
    Ok(())
}

/// Stream the section file into `w` as the BODY of a JSON string (no surrounding
/// quotes), escaping JSON-special bytes as we go. The caller has validated UTF-8, so
/// high bytes (>= 0x80) pass through byte-for-byte to form the original multibyte
/// UTF-8 -- only ASCII control chars / `"` / `\` are escaped. Bounded (chunked).
fn stream_file_json_escaped<W: std::io::Write>(path: &Path, w: &mut W) -> std::io::Result<()> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut buf = [0u8; 8192];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for &byte in &buf[..n] {
            match byte {
                b'"' => w.write_all(b"\\\"")?,
                b'\\' => w.write_all(b"\\\\")?,
                b'\n' => w.write_all(b"\\n")?,
                b'\r' => w.write_all(b"\\r")?,
                b'\t' => w.write_all(b"\\t")?,
                0x08 => w.write_all(b"\\b")?,
                0x0c => w.write_all(b"\\f")?,
                0x00..=0x1f => write!(w, "\\u{byte:04x}")?,
                other => w.write_all(&[other])?,
            }
        }
    }
    Ok(())
}

/// Stream the section file into `w` as a base64 string body (no surrounding quotes),
/// via base64's incremental `EncoderWriter` -- bounded, never a whole-file load.
fn stream_file_base64<W: std::io::Write>(path: &Path, w: &mut W) -> std::io::Result<()> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut encoder =
        base64::write::EncoderWriter::new(w, &base64::engine::general_purpose::STANDARD);
    std::io::copy(&mut reader, &mut encoder)?;
    encoder.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::TurnCapture;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

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

    /// F1b review r2 (don't-lie-with-zeros): reproduces the HTTP tee's early-close
    /// scenario -- the served sink's writer/receiver closes early mid-stream (the
    /// `SinkClosed` path at `http.rs`'s `poll_frame`), which the tee handles by
    /// calling `mark_served_degraded()` right there and then continuing to serve
    /// the client byte-for-byte from `inner` WITHOUT capture. The outer response
    /// stream can still reach a clean end-of-stream afterward (the client got the
    /// full body), so the tee's `Drop` reports that as `served_done(false)`. Prove
    /// the sticky latch wins over that later clean close: `served_partial()` is
    /// true immediately after the mark (no need to wait for `served_done`), and
    /// STAYS true through the subsequent `served_done(false)` -- a truncated
    /// capture must never be reported complete.
    #[tokio::test]
    async fn served_sink_closed_early_marks_partial_sticky_through_clean_done() {
        let capture = TurnCapture::enabled(temp_dir_path("served-degraded"));
        let state = capture.start("api_degraded", None, 0).expect("state");

        // Some bytes reached the section before the writer/receiver died.
        state.write_served_response(b"event: message_start\n\n");

        // Simulate the tee observing `SinkClosed` mid-stream (the section writer
        // task died): it marks the section degraded immediately, well before the
        // turn's `served_done` ever runs.
        state.mark_served_degraded();
        assert!(
            state.served_partial(),
            "mark_served_degraded must be visible immediately, before served_done runs"
        );

        // The client-facing stream still reaches a clean end (the tee kept
        // forwarding `inner` unchanged after dropping its sink), so `Drop`
        // reports a clean close -- exactly the buggy input that used to overwrite
        // the truth.
        state.served_done(false);
        state.await_served_closed().await;

        assert!(
            state.served_partial(),
            "a served capture marked degraded mid-stream must stay partial even \
             though the outer stream later reported a clean served_done(false)"
        );
    }

    #[tokio::test]
    async fn forward_declared_stub_is_callable_and_never_panics() {
        let capture = TurnCapture::enabled(temp_dir_path("stubs"));
        let state = capture.start("api_stub", None, 0).expect("state");
        // F1e's section is not created yet -- this is still a no-op.
        state.write_upstream_response(b"data: [DONE]\n\n");
        state.write_inbound_request(b"{}");
        state.write_served_response(b"event: message_stop\n\n");
        state.served_done(true);
        capture.engine_done("api_stub", "cancelled", Some("client_disconnect"));
        state.await_inbound_closed().await;
        state.await_served_closed().await;
    }

    /// F1d: `write_upstream_request` is real -- a single write is captured and
    /// embedded as a parsed JSON value (mirrors `inbound_request`'s encoding).
    #[tokio::test]
    async fn upstream_request_section_captures_bytes_when_written() {
        let dir = temp_dir_path("upstream-request-written");
        let capture = TurnCapture::enabled(dir.clone());
        let state = capture.start("api_ur", None, 0).expect("state");
        state.write_inbound_request(b"{}");
        state.write_upstream_request(br#"{"model":"served-model","max_tokens":123}"#);
        state.write_served_response(b"ok");
        state.served_done(false);
        state.engine_done("completed", None);

        let artifact = wait_for_artifact(&dir.join("api_ur.json")).await;
        let section = &artifact["sections"]["upstream_request"];
        assert_eq!(
            section["encoding"], "json",
            "a parseable request embeds raw"
        );
        assert_eq!(section["content"]["model"], "served-model");
        assert_eq!(section["content"]["max_tokens"], 123);
        assert_eq!(section["partial"], false);
    }

    /// F1d last-writer-wins: a SECOND `write_upstream_request` call REPLACES the
    /// first attempt's bytes entirely -- the section must never grow into a
    /// concatenation of every attempt tried (which would corrupt the embedded
    /// JSON value at assembly).
    #[tokio::test]
    async fn upstream_request_second_write_replaces_not_appends() {
        let dir = temp_dir_path("upstream-request-replace");
        let capture = TurnCapture::enabled(dir.clone());
        let state = capture.start("api_ur_replace", None, 0).expect("state");
        state.write_inbound_request(b"{}");
        // First (oversized) attempt, then a shrink-retry attempt -- exactly the
        // `dispatch_chat_stream` shrink-and-retry shape.
        state.write_upstream_request(br#"{"model":"m","max_tokens":64000}"#);
        state.write_upstream_request(br#"{"model":"m","max_tokens":63652}"#);
        state.write_served_response(b"ok");
        state.served_done(false);
        state.engine_done("completed", None);

        let artifact = wait_for_artifact(&dir.join("api_ur_replace.json")).await;
        let section = &artifact["sections"]["upstream_request"];
        assert_eq!(
            section["content"]["max_tokens"], 63652,
            "the LAST write wins: {section}"
        );
        // Bytes reflect ONLY the second write's length, not a concatenation.
        let expected_bytes = br#"{"model":"m","max_tokens":63652}"#.len() as u64;
        assert_eq!(
            section["bytes"], expected_bytes,
            "bytes count is the replacement's length, not first+second: {section}"
        );
    }

    /// F1d review r1 (BLOCKING fix): the OLD `replace` `try_send`d onto the
    /// bounded channel and DROPPED the write when the channel was full, so a
    /// burst of rapid attempts (shrink-retry + failover) racing ahead of a slow
    /// writer could leave a STALE, non-final attempt as the committed section
    /// instead of the truly LAST one -- silently recording the wrong on-wire
    /// request. Proves the fix: issue far more rapid `write_upstream_request`
    /// calls than `SECTION_CHANNEL_CAPACITY` with NO `.await` between them, so
    /// on this current-thread test runtime the background writer task cannot
    /// be scheduled to service ANY of them mid-burst -- every call must
    /// coalesce into the pending-replace slot before the writer ever looks.
    /// The committed artifact must equal the LAST attempt, never an earlier
    /// one, and the section must never be marked `partial` just because many
    /// attempts raced ahead of the writer (guaranteed delivery, not a dropped
    /// write recorded as partial).
    #[tokio::test]
    async fn upstream_request_replace_survives_backpressure_last_writer_wins() {
        let dir = temp_dir_path("upstream-request-backpressure");
        let capture = TurnCapture::enabled(dir.clone());
        let state = capture
            .start("api_ur_backpressure", None, 0)
            .expect("state");
        state.write_inbound_request(b"{}");

        // Far more than `SECTION_CHANNEL_CAPACITY` (16) rapid replaces, all
        // issued synchronously with NO `.await` in between.
        const ATTEMPTS: usize = 200;
        const { assert!(ATTEMPTS > super::SECTION_CHANNEL_CAPACITY * 4) };
        for i in 0..ATTEMPTS {
            let body = format!(r#"{{"model":"m","attempt":{i}}}"#);
            state.write_upstream_request(body.as_bytes());
        }
        let last_attempt = ATTEMPTS - 1;

        state.write_served_response(b"ok");
        state.served_done(false);
        state.engine_done("completed", None);

        let artifact = wait_for_artifact(&dir.join("api_ur_backpressure.json")).await;
        let section = &artifact["sections"]["upstream_request"];
        assert_eq!(
            section["content"]["attempt"].as_u64(),
            Some(last_attempt as u64),
            "the FINAL replace must win even though {ATTEMPTS} rapid replaces \
             raced a writer that could not service any of them until this task \
             yielded: {section}"
        );
        let expected_bytes = format!(r#"{{"model":"m","attempt":{last_attempt}}}"#).len() as u64;
        assert_eq!(
            section["bytes"], expected_bytes,
            "bytes reflect ONLY the final replacement's length, never an \
             earlier attempt or a concatenation of several: {section}"
        );
        assert_eq!(
            section["partial"], false,
            "guaranteed-delivery replace must never mark the section partial \
             just because many attempts raced ahead of the writer: {section}"
        );
    }

    /// F1d review r2 (BLOCKING fix): the ACCEPT-vs-CLOSE race. The OLD `replace`
    /// released the tx-lock BEFORE storing into the pending slot, so a `close`
    /// (and the writer's final post-close slot-check + flush + mark-closed) could
    /// slot in between -- leaving the racing `replace`'s bytes ORPHANED in a slot
    /// no one drains, yet `replace` still returned `true` and `await_closed`
    /// resolved on a stale/missing value. The fix seals the slot ATOMICALLY with
    /// the writer's final take, so accept and close are mutually exclusive.
    ///
    /// Stress the boundary: each iteration commits a baseline, then races a
    /// `replace` against a `close` on the SAME section (both spawned so the
    /// scheduler decides the winner). The invariant that must hold for EVERY
    /// iteration, regardless of who wins: EITHER the replace was accepted
    /// (returned `true`) and its bytes are EXACTLY the committed section content,
    /// OR it was rejected (`false`) and the section reflects the PRIOR committed
    /// baseline -- never an orphaned slot (true-but-not-committed), a torn value,
    /// or a lost final.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn replace_and_close_are_atomic_no_orphaned_slot() {
        use super::Section;

        // Each outer iteration drives a HOT LOOP of `replace`s that straddles a
        // CONCURRENT `close`/seal, so a replace is guaranteed to be executing at
        // the exact moment the writer seals -- precisely the accept-vs-close
        // boundary. The invariant checked afterward: the committed section content
        // MUST equal the LAST replace that returned `true` (last-writer-wins).
        // On the buggy split-lock `replace`, the boundary replace can read
        // `sealed == false`, the writer then seals + commits an EARLIER value, and
        // the boundary replace stores its bytes into the sealed slot -- ORPHANED:
        // it returned `true` yet is never committed, so `committed != last_accepted`
        // and this asserts. The atomic fix makes accept + seal mutually exclusive,
        // so an accepted replace is ALWAYS the committed value.
        const OUTER: u32 = 1200;
        const INNER_CAP: u32 = 20_000;
        for iter in 0..OUTER {
            let dir = temp_dir_path(&format!("replace-close-race-{iter}"));
            let path = dir.join("upstream_request");
            let section = Arc::new(Section::new(path.clone()));

            // A committed baseline so `last_accepted` is well-defined even if the
            // writer seals before the very first hot-loop replace is accepted.
            let baseline = format!(r#"{{"which":"baseline-{iter}"}}"#).into_bytes();
            assert!(
                section.replace(&baseline),
                "iter {iter}: the pre-race (unsealed) baseline replace must be accepted"
            );

            // Seal concurrently on another worker thread while the hot loop runs.
            let s_close = Arc::clone(&section);
            let close_task = tokio::spawn(async move { s_close.close(false) });

            // Hot loop: keep replacing until the slot seals (`replace` -> false),
            // tracking the LAST accepted bytes. `sealed` is monotonic, so the first
            // `false` means sealed-for-good and we stop.
            let mut last_accepted = baseline.clone();
            let mut k = 0u32;
            loop {
                let candidate = format!(r#"{{"which":"racer-{iter}-{k}"}}"#).into_bytes();
                if section.replace(&candidate) {
                    last_accepted = candidate;
                    k += 1;
                    if k >= INNER_CAP {
                        // Writer hasn't sealed yet (didn't straddle this round);
                        // stop spinning -- the invariant still holds below.
                        break;
                    }
                } else {
                    break;
                }
            }

            close_task.await.expect("close task joined");
            // Resolves only AFTER the writer drained + flushed the FINAL value.
            section.await_closed().await;
            let committed = std::fs::read(&path).expect("section file readable");

            assert_eq!(
                committed, last_accepted,
                "iter {iter}: the committed section MUST equal the LAST `replace` \
                 that returned true (last-writer-wins). A mismatch means a replace \
                 returned true yet its bytes were orphaned in a sealed slot -- the \
                 accept-vs-close race this fix closes."
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// F1d (don't-lie-with-zeros): a turn that never dispatches upstream (the
    /// pre-spawn-failure shape) never calls `write_upstream_request`, so the
    /// section is OMITTED from the artifact entirely -- absent, never a
    /// fabricated `bytes:0` "measured empty" value.
    #[tokio::test]
    async fn upstream_request_absent_when_never_written() {
        let dir = temp_dir_path("upstream-request-absent");
        let capture = TurnCapture::enabled(dir.clone());
        let state = capture.start("api_ur_absent", None, 0).expect("state");
        state.write_inbound_request(b"{}");
        state.write_served_response(b"error body");
        state.served_done(false);
        state.engine_done("failed", Some("pre_spawn_validation"));

        let artifact = wait_for_artifact(&dir.join("api_ur_absent.json")).await;
        assert!(
            artifact["sections"].get("upstream_request").is_none(),
            "a never-dispatched turn has NO upstream_request key: {artifact}"
        );
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

    /// Poll for the assembled artifact (assembly is spawned async), failing after a
    /// bounded wait -- so a test that "never assembles" fails loudly instead of
    /// hanging, and a passing test PROVES the barrier resolved in bounded time.
    async fn wait_for_artifact(path: &std::path::Path) -> serde_json::Value {
        for _ in 0..300 {
            if let Ok(bytes) = std::fs::read(path) {
                return serde_json::from_slice(&bytes).unwrap_or_else(|err| {
                    panic!("artifact at {} is not valid JSON: {err}", path.display())
                });
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("artifact never appeared at {}", path.display());
    }

    /// F1c both-`done` barrier: engine + served `done`s → assemble a single JSON
    /// artifact (inbound as a JSON VALUE, served as a UTF-8 string), then EVICT the
    /// registry entry + delete the `.work/<id>/` dir. Closes F1b's deferred leak.
    #[tokio::test]
    async fn both_done_barrier_assembles_evicts_and_cleans_up() {
        let dir = temp_dir_path("barrier-assemble");
        let capture = TurnCapture::enabled(dir.clone());
        let state = capture
            .start("api_barrier", Some("model-x".to_string()), 1_000)
            .expect("state");
        state.write_inbound_request(br#"{"model":"model-x","messages":[]}"#);
        state.write_served_response(b"event: message_start\n\nevent: message_stop\n\n");
        state.set_model_served("backend-y");

        // Both sides report (order does not matter) → the SECOND fires assembly once.
        state.served_done(false);
        capture.engine_done("api_barrier", "completed", Some("response.completed"));

        let artifact = wait_for_artifact(&dir.join("api_barrier.json")).await;
        assert_eq!(artifact["api_call_id"], "api_barrier");
        assert_eq!(artifact["status"], "completed");
        assert_eq!(artifact["terminal_reason"], "response.completed");
        assert_eq!(artifact["model_requested"], "model-x");
        assert_eq!(artifact["model_served"], "backend-y");
        // inbound_request PARSES → embeds as a JSON value (encoding "json").
        assert_eq!(artifact["sections"]["inbound_request"]["encoding"], "json");
        assert_eq!(
            artifact["sections"]["inbound_request"]["content"]["model"],
            "model-x"
        );
        assert_eq!(artifact["sections"]["inbound_request"]["partial"], false);
        // served_response embeds as a UTF-8 string.
        assert_eq!(artifact["sections"]["served_response"]["encoding"], "utf8");
        assert!(
            artifact["sections"]["served_response"]["content"]
                .as_str()
                .expect("served content string")
                .contains("message_start")
        );
        assert_eq!(artifact["sections"]["served_response"]["partial"], false);
        // Outcome timing is honest: finished stamped, and not before started.
        let started = artifact["started_ms"].as_u64().expect("started_ms");
        let finished = artifact["finished_ms"].as_u64().expect("finished_ms");
        assert!(finished > 0, "finished_ms is stamped");
        assert!(finished >= started, "finished_ms is not before started_ms");
        // F1c has no upstream sections yet — ABSENT, not a fabricated empty-measured.
        assert!(artifact["sections"].get("upstream_request").is_none());
        assert!(artifact["sections"].get("upstream_response").is_none());
        // Registry evicted + work dir deleted (no leak).
        assert!(
            capture.state("api_barrier").is_none(),
            "registry entry is evicted after assembly"
        );
        assert!(
            !state.work_dir().exists(),
            ".work/<id>/ dir is deleted after assembly"
        );
        // No .tmp residue in the capture dir.
        assert!(!dir.join("api_barrier.json.tmp").exists());
    }

    /// F1c review r2 (robust cleanup ordering): a work-dir-delete FAILURE must
    /// still (i) publish a valid artifact and (ii) evict the registry entry -- a
    /// transient fs hiccup on the best-effort `.work/<id>/` delete can NEVER strand
    /// a valid capture or leak the registry entry (the orphan is left for F1f's
    /// `.work` sweep). Simulates the failure DETERMINISTICALLY (holds even as root)
    /// by replacing the work dir with a plain FILE, so `remove_dir_all` fails with
    /// `NotADirectory`. Drives `assemble_blocking` directly with ONLY the served
    /// side reported, so the both-`done` barrier does not also auto-assemble.
    #[tokio::test]
    async fn work_dir_delete_failure_still_publishes_and_evicts() {
        let dir = temp_dir_path("workdir-delete-fail");
        let capture = TurnCapture::enabled(dir.clone());
        let state = capture
            .start("api_wdfail", Some("m".to_string()), 5)
            .expect("state");
        state.write_inbound_request(b"{\"model\":\"m\"}");
        state.write_served_response(b"ok");
        // Close + flush both sections; report ONLY the served side so the barrier
        // does NOT auto-assemble -- we drive assembly manually after the sabotage.
        state.served_done(false);
        state.await_inbound_closed().await;
        state.await_served_closed().await;

        // Sabotage the work dir: replace it (+ its now-flushed section files) with a
        // regular FILE, so the upcoming `remove_dir_all` fails deterministically with
        // `NotADirectory` -- a simulated fs hiccup that holds even when tests run as
        // root (a chmod-based read-only parent would simply be bypassed by root).
        std::fs::remove_dir_all(state.work_dir()).expect("drop the real work dir");
        std::fs::write(state.work_dir(), b"x").expect("place a file where the dir was");
        assert!(
            state.work_dir().is_file(),
            "work dir path is now un-removable"
        );

        // Assemble on the blocking pool, mirroring `finalize_and_assemble`.
        let assemble_state = Arc::clone(&state);
        tokio::task::spawn_blocking(move || assemble_state.assemble_blocking())
            .await
            .expect("assemble task");

        // (i) A valid artifact IS published despite the work-dir delete failure.
        let bytes = std::fs::read(dir.join("api_wdfail.json")).expect("artifact published");
        let artifact: serde_json::Value =
            serde_json::from_slice(&bytes).expect("published artifact is valid JSON");
        assert_eq!(artifact["api_call_id"], "api_wdfail");
        assert!(
            !dir.join("api_wdfail.json.tmp").exists(),
            "the tmp is renamed away -- no residue"
        );

        // (ii) Registry evicted -- NO leak -- even though the best-effort delete failed.
        assert!(
            capture.state("api_wdfail").is_none(),
            "registry eviction is unconditional; a work-dir delete failure never leaks it"
        );
        // The best-effort delete failed, so the orphan survives for F1f's sweep.
        assert!(
            state.work_dir().exists(),
            "a failed best-effort work-dir delete leaves a sweepable orphan (F1f)"
        );
    }

    /// `engine_done` is a first-writer-wins latch: the engine's real terminal is
    /// never overwritten by a later `Drop`-fallback `failed`.
    #[tokio::test]
    async fn engine_done_is_idempotent_first_writer_wins() {
        let dir = temp_dir_path("engine-idempotent");
        let capture = TurnCapture::enabled(dir.clone());
        let state = capture.start("api_idem", None, 0).expect("state");
        state.write_inbound_request(b"{}");
        state.engine_done("completed", Some("response.completed"));
        // A later (Drop-style) terminal must be inert.
        state.engine_done("failed", Some("dropped"));
        state.write_served_response(b"ok");
        state.served_done(false);

        let artifact = wait_for_artifact(&dir.join("api_idem.json")).await;
        assert_eq!(artifact["status"], "completed", "first engine_done wins");
        assert_eq!(artifact["terminal_reason"], "response.completed");
    }

    /// Encoding contract: a non-UTF-8 served section round-trips via base64 + the
    /// `"encoding":"base64"` marker (bounded streaming base64 encoder).
    #[tokio::test]
    async fn assembly_embeds_non_utf8_served_as_base64() {
        let dir = temp_dir_path("base64-served");
        let capture = TurnCapture::enabled(dir.clone());
        let state = capture.start("api_b64", None, 0).expect("state");
        state.write_inbound_request(b"{}");
        let raw = vec![0xff, 0xfe, 0x00, 0x01, 0x80, 0x7f];
        state.write_served_response(&raw);
        state.served_done(false);
        state.engine_done("completed", None);

        let artifact = wait_for_artifact(&dir.join("api_b64.json")).await;
        assert_eq!(
            artifact["sections"]["served_response"]["encoding"],
            "base64"
        );
        // No `terminal_reason` key when the engine passed `None`.
        assert!(artifact.get("terminal_reason").is_none());
        let content = artifact["sections"]["served_response"]["content"]
            .as_str()
            .expect("base64 content string");
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(content)
            .expect("valid base64");
        assert_eq!(decoded, raw, "base64 round-trips the exact bytes");
    }

    /// The middleware backstop resolves the barrier for a turn that NEVER reached the
    /// engine (unclaimed → `failed`/`unhandled`), and is INERT for a claimed turn (the
    /// engine's status wins). This is the "served side is the only latch" case that
    /// would otherwise wait forever + leak the registry entry.
    #[tokio::test]
    async fn middleware_backstop_finalizes_unclaimed_but_is_inert_when_claimed() {
        // Unclaimed: engine never took ownership → backstop Drop finalizes failed.
        // The rejection response IS teed in reality (the tee wraps the whole
        // `next.run` result), so mark the tee installed and let the explicit
        // `served_done` own the served side; this isolates the ENGINE-side backstop.
        let dir = temp_dir_path("backstop-unclaimed");
        let capture = TurnCapture::enabled(dir.clone());
        let state = capture.start("api_unclaimed", None, 0).expect("state");
        state.write_inbound_request(b"{\"bad\":true}");
        state.write_served_response(b"{\"error\":\"bad request\"}");
        state.mark_served_tee_installed();
        drop(super::MiddlewareCaptureGuard::new(Arc::clone(&state)));
        state.served_done(false);

        let artifact = wait_for_artifact(&dir.join("api_unclaimed.json")).await;
        assert_eq!(artifact["status"], "failed");
        assert_eq!(artifact["terminal_reason"], "unhandled");
        assert!(
            capture.state("api_unclaimed").is_none(),
            "an unhandled (never-reached-engine) turn is still evicted — no leak"
        );

        // Claimed: a CaptureGuard took ownership → backstop is inert, engine wins.
        let dir2 = temp_dir_path("backstop-claimed");
        let capture2 = TurnCapture::enabled(dir2.clone());
        let state2 = capture2.start("api_claimed", None, 0).expect("state");
        state2.write_inbound_request(b"{}");
        state2.mark_served_tee_installed();
        let guard = super::CaptureGuard::new(Arc::clone(&state2), CancellationToken::new());
        drop(super::MiddlewareCaptureGuard::new(Arc::clone(&state2)));
        guard.finalize("completed", Some("response.completed"));
        state2.write_served_response(b"ok");
        state2.served_done(false);

        let artifact2 = wait_for_artifact(&dir2.join("api_claimed.json")).await;
        assert_eq!(
            artifact2["status"], "completed",
            "a claimed turn keeps the engine's status; the backstop is inert"
        );
        // The guard's own Drop fallback is idempotent (inert after the explicit terminal).
        drop(guard);
    }

    /// Finding #2 (pre-tee unwind backstop): `start()` ran, but `next.run` unwound /
    /// returned BEFORE `tee_served_body` installed the served tee — so NO tee ever
    /// fires `served_done`. Without the served-side backstop the both-`done` barrier
    /// would wait forever and `.work/<id>/` + the registry entry would leak. Prove the
    /// `MiddlewareCaptureGuard` `Drop` fires BOTH sides (engine `failed`/`unhandled`
    /// AND a partial `served_done`), so the artifact is written + evicted + the work
    /// dir deleted in bounded time — no leak, no hang.
    #[tokio::test]
    async fn middleware_backstop_fires_served_done_when_tee_never_installed() {
        let dir = temp_dir_path("backstop-no-tee");
        let capture = TurnCapture::enabled(dir.clone());
        let state = capture.start("api_no_tee", None, 0).expect("state");
        state.write_inbound_request(b"{\"model\":\"m\"}");
        // No `mark_served_tee_installed()`, no `served_done`, no tee — exactly the
        // pre-tee unwind state. The ONLY thing that drops is the middleware backstop.
        drop(super::MiddlewareCaptureGuard::new(Arc::clone(&state)));

        let artifact = wait_for_artifact(&dir.join("api_no_tee.json")).await;
        assert_eq!(artifact["status"], "failed");
        assert_eq!(artifact["terminal_reason"], "unhandled");
        // Served section present but partial (no tee ever captured any bytes).
        assert_eq!(
            artifact["sections"]["served_response"]["partial"], true,
            "a turn whose tee never installed has a partial served section"
        );
        // Registry evicted + work dir deleted — the leak finding #2 describes is gone.
        assert!(
            capture.state("api_no_tee").is_none(),
            "a pre-tee-unwind turn is still evicted — no registry leak"
        );
        assert!(
            !state.work_dir().exists(),
            ".work/<id>/ is deleted even when the tee never installed — no leak"
        );
        assert!(!dir.join("api_no_tee.json.tmp").exists());
    }

    /// The RAII `CaptureGuard` Drop finalizes an abandoned turn: `cancelled` when the
    /// abort token fired, `failed` otherwise — with whatever sections closed.
    #[tokio::test]
    async fn capture_guard_drop_finalizes_cancelled_when_abort_fired() {
        let dir = temp_dir_path("guard-drop-cancel");
        let capture = TurnCapture::enabled(dir.clone());
        let state = capture.start("api_abort", None, 0).expect("state");
        state.write_inbound_request(b"{}");
        state.write_served_response(b"partial");
        // Served cut short (client gone) → partial; engine terminal only via Drop.
        state.served_done(true);
        let token = CancellationToken::new();
        token.cancel();
        let guard = super::CaptureGuard::new(Arc::clone(&state), token);
        drop(guard); // no explicit finalize → Drop maps the fired abort to cancelled.

        let artifact = wait_for_artifact(&dir.join("api_abort.json")).await;
        assert_eq!(artifact["status"], "cancelled");
        assert_eq!(artifact["terminal_reason"], "dropped");
        assert_eq!(artifact["sections"]["served_response"]["partial"], true);
    }
}
