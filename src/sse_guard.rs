//! Upstream SSE per-frame DoS guard (G6).
//!
//! The `eventsource-stream` parser used to read the upstream SSE response
//! buffers every byte it receives until it sees an event boundary (a blank
//! line) and does NOT cap that buffer, so a hostile/buggy upstream streaming an
//! oversized or never-terminated frame would grow memory without bound. This
//! module is the focused, pure byte-accounting guard that bounds the bytes
//! accumulated between SSE event boundaries before they reach the parser, plus
//! the thin async stream adapter (`bounded_sse_byte_stream`) that drives it over
//! a real `bytes_stream()`. `upstream::stream_success_response` does the
//! call-site wiring; everything here is provider-agnostic and unit-testable with
//! raw byte slices.

use crate::error::AppError;
use axum::body::Bytes;
use futures::Stream;
use futures::StreamExt;

/// 8 MiB default upstream SSE per-frame ceiling. Comfortably above any sane
/// single model-output SSE event (typical chunks are well under 1 MiB) so
/// normal streaming is never affected, while still bounding a hostile/
/// unterminated frame far below the memory a single oversized accumulation
/// could exhaust. This is the single source of truth for the default;
/// `config::default_max_sse_frame_bytes` and `sse_guard::default_max_sse_frame_bytes`
/// both return it.
pub(crate) const DEFAULT_MAX_SSE_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// 8 MiB default upstream SSE per-frame ceiling. Mirrors
/// `config::default_max_sse_frame_bytes`; kept here so `ReqwestUpstreamClient::new`
/// (which does not take a cap) has a sane default without depending on config.
pub(crate) fn default_max_sse_frame_bytes() -> usize {
    DEFAULT_MAX_SSE_FRAME_BYTES
}

/// Pure, synchronous per-frame byte-accounting guard for the upstream SSE read
/// (G6). Feed it each incoming byte chunk in order; it tracks the number of
/// bytes accumulated **since the last SSE event boundary** and returns an
/// `AppError` the moment that running count exceeds the cap — i.e. as soon as an
/// oversized or never-terminated (no blank-line) frame would force the
/// downstream `eventsource-stream` parser to over-buffer.
///
/// Kept pure (no async, no `reqwest`) so it is unit/integration-testable with
/// raw byte slices; `bounded_sse_byte_stream` is the thin async wrapper that
/// drives it over a real `bytes_stream()`.
#[derive(Debug)]
pub(crate) struct SseFrameGuard {
    max_frame_bytes: usize,
    /// INVARIANT: `since_boundary` = bytes of the current in-progress frame that
    /// are CONFIRMED not part of a pending boundary (every such byte counted
    /// exactly once). `carry` = the trailing <=3 bytes of the stream so far that
    /// form a (possibly empty) PREFIX of an SSE boundary and are therefore NOT
    /// yet charged: on the next chunk they either complete a boundary (→ reset,
    /// never charged) or are disambiguated as ordinary frame bytes (→ charged
    /// then). Holding the ambiguous tail uncharged is what makes the verdict
    /// chunking-INDEPENDENT.
    since_boundary: usize,
    /// Deferred boundary-prefix tail (`\n`, `\r`, `\r\n`, or `\r\n\r`). Fixed tiny
    /// window — never grows beyond 3 bytes; uncharged until disambiguated.
    carry: Vec<u8>,
    /// Set when the stream is currently INSIDE a maximal run of consecutive EOLs
    /// that began at a completed frame boundary (Codex round-4 LOW). After a blank
    /// line (boundary = two EOLs) any ADDITIONAL consecutive EOLs are extra empty
    /// lines that `eventsource-stream` dispatches as empty events / skips, so they
    /// belong to NO data frame and must be charged to neither. When this is true,
    /// a leading EOL on the next chunk continues that empty-line run (consumed,
    /// uncharged) rather than being charged into the next frame; the `carry` then
    /// holds the run's trailing partial EOL (a lone `\r` whose CR/CRLF nature is
    /// still ambiguous) instead of a boundary-prefix. Cleared the moment a
    /// non-EOL byte (real frame content) ends the run.
    in_eol_run: bool,
}

impl SseFrameGuard {
    /// Build a guard with the given per-frame ceiling (floored at 1 KiB so a
    /// misconfigured tiny cap cannot reject every normal frame).
    ///
    /// `in_eol_run` starts TRUE: the stream begins AS IF immediately after a frame
    /// boundary, so any LEADING EOLs (an empty/blank-line SSE event, or stray
    /// separators before the first `data:`) are an empty-line run charged to NO
    /// frame — exactly like extra blank lines BETWEEN frames (Codex round-4 LOW).
    /// Starting false instead charged a leading EOL into the first real frame, a
    /// false reject for a frame otherwise exactly at cap (Codex round-5 LOW). A
    /// stream that opens directly on real content ends the (zero-length) leading run
    /// at byte 0, so this is a no-op for the common case.
    pub fn new(max_frame_bytes: usize) -> Self {
        Self {
            max_frame_bytes: max_frame_bytes.max(1024),
            since_boundary: 0,
            carry: Vec::new(),
            in_eol_run: true,
        }
    }

    /// The effective (floored) per-frame cap this guard enforces. Only the guard's
    /// own unit tests read it today (the production path threads the cap in via
    /// `new`); kept as a first-class accessor so the floor is observable and so the
    /// `Bytes`-specialization follow-up (T5) has the field exposed without re-widening
    /// visibility.
    #[allow(dead_code)]
    pub(crate) fn max_frame_bytes(&self) -> usize {
        self.max_frame_bytes
    }

    /// Account for one incoming chunk. Returns `Err` the moment ANY single SSE
    /// frame — the bytes between two boundaries, terminated or not — would exceed
    /// the cap (the caller must stop and surface the error — never silently
    /// truncate). Each boundary resets the running count, so a well-formed (even
    /// large-but-bounded-per-event) stream always passes.
    ///
    /// The scan searches `carry + chunk` (so a boundary straddling the chunk edge
    /// is detected). `carry` is the previously-DEFERRED boundary-prefix tail
    /// (uncharged), so the scan both charges it (when it turns out to be ordinary
    /// frame bytes) and re-derives a fresh deferred tail — all in one pass. This
    /// keeps the verdict independent of how the stream is split into chunks.
    /// See [`scan_frames_since_boundary`].
    pub fn accept(&mut self, chunk: &[u8]) -> Result<(), AppError> {
        self.scan(chunk, false)
    }

    /// Finalize accounting when the upstream byte stream ENDS. Any bytes still held
    /// in the deferred boundary-prefix carry could not be completed into a frame
    /// boundary (no more bytes will arrive), so a dangling single EOL is charged as
    /// part of the still-open, unterminated frame and a final cap check is emitted
    /// (Codex round-3 Finding 2: an unterminated frame must not slip past the cap by
    /// a trailing `\n`/`\r`/`\r\n`/`\r\n\r` just because EOF arrived before the carry
    /// was disambiguated). A trailing carry that is itself a complete boundary
    /// (`\n\r`, `\r\r`, `\r\n\r`, resolving the final CR at EOF) resets instead of
    /// being charged. Idempotent: after a successful call the carry is empty, so a
    /// second call is a no-op.
    pub fn finish(&mut self) -> Result<(), AppError> {
        self.scan(&[], true)
    }

    fn scan(&mut self, chunk: &[u8], at_eof: bool) -> Result<(), AppError> {
        let ScanState {
            since_boundary,
            carry,
            in_eol_run,
        } = scan_frames_since_boundary(
            ScanState {
                since_boundary: self.since_boundary,
                carry: std::mem::take(&mut self.carry),
                in_eol_run: self.in_eol_run,
            },
            chunk,
            self.max_frame_bytes,
            at_eof,
        )
        .map_err(|observed| {
            AppError::upstream(format!(
                "upstream SSE frame exceeded {} bytes before an event boundary \
                         (saw {observed}); rejecting to bound memory (G6)",
                self.max_frame_bytes
            ))
        })?;
        self.since_boundary = since_boundary;
        self.carry = carry;
        self.in_eol_run = in_eol_run;
        Ok(())
    }
}

/// Wrap an upstream byte stream so the bytes accumulated **between SSE event
/// boundaries** never exceed `max_frame_bytes` before being handed to the
/// `eventsource-stream` parser (G6 DoS guard).
///
/// SSE events are separated by a blank line (`\n\n`, `\r\n\r\n`, or `\r\r`). The
/// `eventsource-stream` parser buffers everything it receives until it sees such
/// a separator, so the only thing that can grow its buffer without bound is a
/// frame that never terminates (or a single oversized frame). The
/// [`SseFrameGuard`] tracks the byte count since the last separator and we reject
/// as soon as it exceeds the cap — *before* forwarding the offending chunk — so
/// the downstream parser buffer is itself bounded by `max_frame_bytes` (plus one
/// in-flight chunk).
///
/// The rejection is yielded as a `std::io::Error` so it travels the transport
/// (`EventStreamError::Transport`) channel of `eventsource()`; its message is the
/// `AppError`'s, and `stream_success_response` re-wraps it into an `AppError`.
/// Normal-sized streaming is untouched: each well-formed event resets the
/// counter at its boundary.
///
/// On stream END the adapter FINALIZES the guard ([`SseFrameGuard::finish`]):
/// any pending boundary-prefix carry is charged and a final cap check is emitted,
/// so an unterminated over-cap frame is rejected even if EOF arrives before a
/// trailing separator byte could be disambiguated (Codex round-3 Finding 2). This
/// is why a plain `.map` is insufficient — the adapter must be able to act on
/// end-of-stream — so it is a stateful `async_stream` that drives the guard and
/// emits one trailing error item when finalization trips the cap.
///
/// Cancellation is preserved: this is a lazy stream adapter that only advances
/// when polled. The caller's `tx.closed()`/timeout selects still cancel the
/// whole chain by dropping it; nothing here blocks or spawns. The raw `*.delta`
/// path and AppError-not-truncation contract are unchanged: every rejection still
/// travels the transport-error channel as an `std::io::Error` whose message is the
/// `AppError`'s, which `stream_success_response` re-wraps — output is never
/// silently truncated.
pub(crate) fn bounded_sse_byte_stream<S, B>(
    stream: S,
    max_frame_bytes: usize,
) -> impl Stream<Item = Result<Bytes, std::io::Error>>
where
    S: Stream<Item = Result<B, reqwest::Error>>,
    B: AsRef<[u8]>,
{
    async_stream::stream! {
        let mut guard = SseFrameGuard::new(max_frame_bytes);
        let mut stream = std::pin::pin!(stream);
        while let Some(result) = stream.next().await {
            let bytes = match result {
                Ok(bytes) => Bytes::copy_from_slice(bytes.as_ref()),
                Err(err) => {
                    yield Err(std::io::Error::other(format!(
                        "failed to read upstream SSE bytes: {err}"
                    )));
                    return;
                }
            };
            // Reject BEFORE forwarding so the parser never sees the over-cap bytes.
            if let Err(err) = guard.accept(bytes.as_ref()) {
                yield Err(std::io::Error::other(err.to_string()));
                return;
            }
            yield Ok(bytes);
        }
        // Upstream ended: charge any deferred carry and cap-check the (possibly
        // unterminated) final frame. A clean end stays clean; an over-cap dangling
        // frame surfaces as a trailing transport error rather than a silent EOF.
        if let Err(err) = guard.finish() {
            yield Err(std::io::Error::other(err.to_string()));
        }
    }
}

/// Length of the EOL token starting at `buf[i]`, tokenized **exactly like**
/// `eventsource-stream`'s `end-of-line = ( cr lf / cr / lf )` (CRLF matched
/// greedily, longest-first). Returns:
///   * `EolToken::Complete(len)` — a fully-determined EOL of `len` bytes;
///   * `EolToken::IncompleteCr` — `buf[i]` is a CR that is the LAST byte of `buf`
///     and `at_eof` is false, so we cannot yet tell `\r` (CR) from `\r\n` (CRLF);
///   * `EolToken::None` — `buf[i]` is not an EOL byte.
///
/// At end-of-stream (`at_eof`) a trailing lone CR is resolved as a 1-byte CR EOL,
/// because the parser will never receive the following byte that could make it a
/// CRLF (mirrors the parser leaving a trailing `\r` `Incomplete` forever).
enum EolToken {
    Complete(usize),
    IncompleteCr,
    None,
}

fn eol_token_at(buf: &[u8], i: usize, at_eof: bool) -> EolToken {
    match buf.get(i) {
        Some(b'\r') => match buf.get(i + 1) {
            Some(b'\n') => EolToken::Complete(2),    // CRLF, greedy.
            Some(_) => EolToken::Complete(1),        // lone CR proven by a following byte.
            None if at_eof => EolToken::Complete(1), // no more bytes: CR resolves to CR.
            None => EolToken::IncompleteCr,          // could still become CRLF.
        },
        Some(b'\n') => EolToken::Complete(1), // LF never coalesces forward.
        _ => EolToken::None,
    }
}

/// Carried byte-accounting state of the SSE frame guard between chunks. Bundled
/// into one value so the maximal-EOL-run flag (`in_eol_run`) travels alongside
/// the running count and the deferred-prefix carry without an ever-widening
/// tuple. See [`SseFrameGuard`] for the field invariants.
#[derive(Debug, Clone)]
struct ScanState {
    since_boundary: usize,
    carry: Vec<u8>,
    in_eol_run: bool,
}

/// Advance past a MAXIMAL run of complete EOL tokens in `buf` starting at `from`,
/// returning `(end, stop)` where `end` is the index just past the last complete
/// EOL consumed and `stop` says WHY the run ended:
///   * [`EolRunStop::Content`] — `buf[end]` is a non-EOL byte (real frame content);
///   * [`EolRunStop::BufferEnd`] — the run reached the end of `buf` cleanly (the
///     last token was a complete EOL); a leading EOL on the NEXT chunk continues it;
///   * [`EolRunStop::IncompleteCr`] — the run stopped on a trailing lone `\r` whose
///     CR-vs-CRLF nature is unresolved mid-stream (`!at_eof`); that `\r` is itself
///     another empty-line EOL and is deferred uncharged into the carry.
///
/// Every byte consumed here is an empty-line EOL that belongs to NO data frame, so
/// the caller charges none of them (Codex round-4 LOW).
fn eol_run_end(buf: &[u8], from: usize, at_eof: bool) -> (usize, EolRunStop) {
    let mut i = from;
    loop {
        match eol_token_at(buf, i, at_eof) {
            EolToken::Complete(len) => i += len,
            EolToken::IncompleteCr => return (i, EolRunStop::IncompleteCr),
            EolToken::None => {
                return if i >= buf.len() {
                    (i, EolRunStop::BufferEnd)
                } else {
                    (i, EolRunStop::Content)
                };
            }
        }
    }
}

enum EolRunStop {
    Content,
    BufferEnd,
    IncompleteCr,
}

/// Single robust pass that bounds EVERY SSE frame in `carry + chunk` and returns
/// the updated [`ScanState`] (running count, freshly-deferred tail, and whether we
/// ended inside an empty-line EOL run).
///
/// A frame boundary is a BLANK LINE = two consecutive `end-of-line`s, tokenized
/// exactly like the `eventsource-stream` parser (`end-of-line = cr lf / cr / lf`,
/// CRLF greedy). So the boundary byte-sequences are, by length: `\n\n`, `\n\r`,
/// `\r\r` (2); `\n\r\n`, `\r\n\n`, `\r\n\r`, `\r\r\n` (3); `\r\n\r\n` (4). The old
/// guard recognized only `\n\n`/`\r\r`/`\r\n\r\n` and so mis-detected the mixed
/// combos (Codex round-3 Finding 1).
///
/// `carry` is the tail DEFERRED by the previous call (uncharged). We rebuild
/// `buf = carry + chunk` so a boundary straddling the chunk edge is detected, then
/// walk it boundary by boundary:
///   * For each completed boundary, the bytes of `buf` since the current frame
///     started are now CONFIRMED frame bytes (a boundary follows them): charge
///     them to `since_boundary` and check the cap, then reset `since_boundary` to
///     0 for the next frame. (This naturally subsumes the old `carry` bytes — they
///     are charged here exactly once, the first time they are confirmed.)
///   * Immediately AFTER each boundary, consume the MAXIMAL run of additional
///     consecutive EOLs (Codex round-4 LOW): those are extra empty lines that the
///     parser dispatches as empty events / skips, so they belong to no frame and
///     resume scanning from the end of the run with `since_boundary` still 0. A run
///     that straddles the chunk edge is finished on the next chunk via `in_eol_run`
///     (a leading EOL there continues it, uncharged) so it is never charged.
///   * After the last boundary/run, the trailing segment is split: when `!at_eof`,
///     its longest suffix that is a PROPER PREFIX of a boundary is deferred
///     uncharged into the new carry and the remainder is charged; when `at_eof`
///     there is no future byte to disambiguate, so the entire trailing segment is
///     charged (a dangling single EOL is part of the still-open, unterminated
///     frame) — UNLESS we are still inside an EOL run, in which case a trailing EOL
///     is one more empty line and stays uncharged (Finding 2 vs. round-4 LOW: an
///     unterminated *frame* must be charged at EOF, but an inter-frame empty line
///     must not).
///
/// Correctness properties:
///   * Finding 1 — a trailing byte that merely STARTS a boundary is never charged
///     until the next chunk disambiguates it, so an ambiguous tail cannot trip the
///     cap (and the verdict does not depend on the chunk split).
///   * Finding 2 — a deferred boundary-prefix carry that never completes is charged
///     to the unterminated frame at EOF.
///   * Round-4 LOW — extra/empty blank-line EOLs are charged to no frame, with the
///     run consumed even when split across a chunk edge (carry = run tail,
///     `in_eol_run = true`).
///
/// Returns the new `ScanState`, or `Err(observed)` — the count that first exceeded
/// the cap — so the caller can format the error.
fn scan_frames_since_boundary(
    state: ScanState,
    chunk: &[u8],
    cap: usize,
    at_eof: bool,
) -> Result<ScanState, usize> {
    let ScanState {
        mut since_boundary,
        carry,
        in_eol_run,
    } = state;
    debug_assert!(
        carry.len() <= 3 && boundary_prefix_suffix_len(&carry) == carry.len(),
        "carry must be a pure boundary prefix of <=3 bytes"
    );
    let mut buf = Vec::with_capacity(carry.len() + chunk.len());
    buf.extend_from_slice(&carry);
    buf.extend_from_slice(chunk);

    // `seg_start` is the `buf` index where the current in-progress frame begins;
    // `scan` is how far we have searched for the next boundary.
    let mut seg_start = 0usize;
    let mut scan = 0usize;

    // If the previous chunk ended inside an empty-line EOL run, finish consuming it
    // FIRST: a leading EOL here is one more empty line (charged to nothing), not the
    // first byte of the next frame. Only when the run ends do we begin the frame.
    if in_eol_run {
        match eol_run_end(&buf, 0, at_eof) {
            (end, EolRunStop::IncompleteCr) => {
                // Still mid-run: defer the trailing lone `\r` (another empty-line
                // EOL whose CR/CRLF nature is unresolved) and stay in the run.
                let new_carry = buf[end..].to_vec();
                debug_assert!(new_carry.len() <= 1, "in-run carry is a lone CR");
                return Ok(ScanState {
                    since_boundary: 0,
                    carry: new_carry,
                    in_eol_run: true,
                });
            }
            (_end, EolRunStop::BufferEnd) => {
                // Run consumed the whole buffer cleanly; the next chunk's leading
                // EOLs (if any) continue it. Nothing is charged.
                return Ok(ScanState {
                    since_boundary: 0,
                    carry: Vec::new(),
                    in_eol_run: true,
                });
            }
            (end, EolRunStop::Content) => {
                // The run ended at real frame content: the next frame starts here,
                // and we fall through to the normal boundary scan below.
                seg_start = end;
                scan = end;
            }
        }
    }

    while let Some((bs, be)) = next_boundary(&buf, scan, at_eof) {
        // No boundary is ever double-counted: `scan`/`seg_start` only advance, so
        // each reported boundary starts at/after the current frame's start, and a
        // mid-stream `carry` is never itself a complete boundary (it was deferred
        // precisely because its trailing CR was an unresolved last byte, i.e.
        // `next_boundary(carry, 0, false) == None`). A boundary may now legitimately
        // END at `carry.len()` when the FIRST chunk byte merely RESOLVES that
        // trailing CR (e.g. carry `\r\r` + chunk `d` → boundary `[0,2)`), which is a
        // first detection, not a re-reset.
        debug_assert!(
            bs >= seg_start,
            "boundary start {bs} precedes frame start {seg_start} — double reset"
        );
        // Bytes [seg_start, bs) are now confirmed frame bytes (a boundary follows).
        let confirmed = bs.saturating_sub(seg_start);
        since_boundary = since_boundary.saturating_add(confirmed);
        if since_boundary > cap {
            return Err(since_boundary);
        }
        // Boundary terminates the frame: the count resets for the next frame. Then
        // consume any ADDITIONAL consecutive EOLs (extra empty lines) so their bytes
        // are charged to no frame (Codex round-4 LOW).
        since_boundary = 0;
        match eol_run_end(&buf, be, at_eof) {
            (end, EolRunStop::IncompleteCr) => {
                let new_carry = buf[end..].to_vec();
                debug_assert!(new_carry.len() <= 1, "in-run carry is a lone CR");
                return Ok(ScanState {
                    since_boundary: 0,
                    carry: new_carry,
                    in_eol_run: true,
                });
            }
            (_end, EolRunStop::BufferEnd) => {
                return Ok(ScanState {
                    since_boundary: 0,
                    carry: Vec::new(),
                    in_eol_run: true,
                });
            }
            (end, EolRunStop::Content) => {
                seg_start = end;
                scan = end;
            }
        }
    }

    // Trailing unterminated segment after the final boundary/run (or the whole
    // buffer if there was none). Mid-stream we defer its boundary-prefix suffix
    // uncharged and charge the rest; at EOF nothing more can arrive to complete a
    // boundary, so the whole segment is charged as part of the unterminated frame.
    let tail = &buf[seg_start..];
    let defer = if at_eof {
        0
    } else {
        boundary_prefix_suffix_len(tail)
    };
    let charged = tail.len() - defer;
    since_boundary = since_boundary.saturating_add(charged);
    if since_boundary > cap {
        return Err(since_boundary);
    }
    let new_carry = tail[charged..].to_vec();
    debug_assert!(new_carry.len() <= 3, "deferred carry must stay <=3 bytes");
    Ok(ScanState {
        since_boundary,
        carry: new_carry,
        in_eol_run: false,
    })
}

/// Length of the longest suffix of `buf` that is a PROPER prefix of an SSE
/// blank-line boundary (two `end-of-line`s) — i.e. bytes that might still grow
/// into / complete a boundary on the next chunk and so must be deferred
/// uncharged. With CRLF-greedy EOL tokenization the proper boundary prefixes,
/// longest-first, are:
///   * `\r\n\r` (3) — one CRLF EOL plus a pending CR (→ `\r\n\r\n` or `\r\n`+`\r`);
///   * `\r\n` (2) — one CRLF EOL, second EOL not yet seen;
///   * `\n\r` (2) — LF EOL plus a pending CR (→ `\n\r\n` or `\n`+`\r`);
///   * `\r\r` (2) — CR EOL plus a pending CR (→ `\r\r\n` or `\r`+`\r`);
///   * a lone trailing `\r` or `\n` (1).
///
/// A two-EOL boundary that is already COMPLETE and unambiguous (`\n\n`, `\r\n\n`,
/// `\n\r\n`, `\r\r\n`, `\r\n\r\n`) is consumed by [`next_boundary`] before the
/// tail is examined, so it never reaches here. The ambiguous-length boundaries
/// (`\n\r`, `\r\r`, `\r\n\r`) are deferred here precisely because a trailing CR
/// could still extend the separator — deferring them keeps the byte verdict
/// chunking-independent; they are resolved (as complete boundaries that reset, or
/// as charged frame bytes) on the next chunk or at EOF.
fn boundary_prefix_suffix_len(buf: &[u8]) -> usize {
    let n = buf.len();
    // 3-byte prefix `\r\n\r` of `\r\n\r\n`.
    if n >= 3 && &buf[n - 3..] == b"\r\n\r" {
        return 3;
    }
    // 2-byte ambiguous/partial prefixes: one EOL plus a pending CR, or a partial
    // CRLF, that could still complete or extend a boundary on the next chunk.
    if n >= 2 {
        let last2 = &buf[n - 2..];
        if last2 == b"\r\n" || last2 == b"\n\r" || last2 == b"\r\r" {
            return 2;
        }
    }
    // 1-byte prefix: a lone trailing `\r` (start of `\r\r`/`\r\n...`) or `\n`
    // (start of `\n\n`/`\n\r`).
    if n >= 1 && (buf[n - 1] == b'\r' || buf[n - 1] == b'\n') {
        return 1;
    }
    0
}

/// Find the next SSE blank-line boundary in `buf` at or after `from`, returning
/// its `(start, end)` byte range, or `None` if none completes. A boundary is two
/// consecutive `end-of-line`s, each tokenized greedily as `cr lf / cr / lf` (see
/// [`eol_token_at`]); the `(start, end)` range covers BOTH EOLs (so the bytes of
/// the separator itself are never charged to either adjacent frame). A trailing
/// lone CR that cannot yet be disambiguated (`!at_eof`) does not complete a
/// boundary — it is deferred into the carry instead.
fn next_boundary(buf: &[u8], from: usize, at_eof: bool) -> Option<(usize, usize)> {
    let n = buf.len();
    let mut i = from;
    while i < n {
        // First EOL of the candidate blank line.
        let first_len = match eol_token_at(buf, i, at_eof) {
            EolToken::Complete(len) => len,
            // A lone trailing CR mid-stream cannot start a confirmed boundary yet.
            EolToken::IncompleteCr => return None,
            EolToken::None => {
                i += 1;
                continue;
            }
        };
        // Second consecutive EOL → the line between them is empty → boundary.
        match eol_token_at(buf, i + first_len, at_eof) {
            EolToken::Complete(second_len) => {
                return Some((i, i + first_len + second_len));
            }
            // The second EOL is an unresolved trailing CR (mid-stream): the
            // boundary is not yet complete; defer (it lives in the carry).
            EolToken::IncompleteCr => return None,
            // First byte was an EOL but the next is ordinary content: not a blank
            // line. Resume scanning AFTER this EOL (the content may yet end in a
            // real boundary).
            EolToken::None => {
                i += first_len;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A complete, well-formed SSE event terminated by the blank-line boundary.
    fn frame(data: &str) -> String {
        format!("data: {data}\n\n")
    }

    // --- Boundary-detection internals (white-box: pokes module-private items) ---

    #[test]
    fn next_boundary_tokenizes_every_eol_combo_like_the_parser() {
        // No boundary.
        assert_eq!(super::next_boundary(b"data: hi", 0, false), None);
        // Two consecutive EOLs = blank line = boundary; (start, end) spans BOTH
        // EOLs so separator bytes are charged to neither frame. All combos of
        // `cr lf / cr / lf` x `cr lf / cr / lf` (CRLF greedy) are recognized.
        // Followed by `b` so any trailing CR is disambiguated (not incomplete).
        assert_eq!(super::next_boundary(b"a\n\nb", 0, false), Some((1, 3))); // LF LF
        assert_eq!(super::next_boundary(b"a\r\rb", 0, false), Some((1, 3))); // CR CR
        assert_eq!(super::next_boundary(b"a\r\n\r\nb", 0, false), Some((1, 5))); // CRLF CRLF
        assert_eq!(super::next_boundary(b"a\r\n\nb", 0, false), Some((1, 4))); // CRLF LF
        assert_eq!(super::next_boundary(b"a\n\rb", 0, false), Some((1, 3))); // LF CR
        assert_eq!(super::next_boundary(b"a\r\n\rb", 0, false), Some((1, 4))); // CRLF CR
        assert_eq!(super::next_boundary(b"a\n\r\nb", 0, false), Some((1, 4))); // LF CRLF
        assert_eq!(super::next_boundary(b"a\r\r\nb", 0, false), Some((1, 4))); // CR CRLF
        // A trailing lone CR mid-stream is INCOMPLETE: it might still become CRLF,
        // so no boundary is reported until disambiguated (or EOF).
        assert_eq!(super::next_boundary(b"a\n\r", 0, false), None);
        assert_eq!(super::next_boundary(b"a\r\r", 0, false), None);
        // ...but at EOF the trailing CR resolves to a CR EOL and the boundary completes.
        assert_eq!(super::next_boundary(b"a\n\r", 0, true), Some((1, 3)));
        assert_eq!(super::next_boundary(b"a\r\r", 0, true), Some((1, 3)));
        // A single EOL followed by ordinary content is NOT a blank line; scanning
        // resumes and finds the real boundary later.
        assert_eq!(super::next_boundary(b"a\nb\n\nc", 0, false), Some((3, 5)));
        // The FIRST boundary at/after `from` is returned; `from` skips earlier ones.
        assert_eq!(super::next_boundary(b"a\n\nb\n\nc", 0, false), Some((1, 3)));
        assert_eq!(super::next_boundary(b"a\n\nb\n\nc", 3, false), Some((4, 6)));
    }

    /// Thin test shim: drive `scan_frames_since_boundary` with the legacy
    /// positional args and collapse the returned `ScanState` to the
    /// `(since_boundary, carry)` tuple the older assertions read. `in_eol_run`
    /// defaults to false on input (the round-4 LOW cases assert it explicitly via
    /// `scan_state`). Returns `Err(observed)` unchanged.
    fn scan(
        since: usize,
        carry: &[u8],
        chunk: &[u8],
        cap: usize,
        eof: bool,
    ) -> Result<(usize, Vec<u8>), usize> {
        scan_state(since, carry, false, chunk, cap, eof).map(|s| (s.since_boundary, s.carry))
    }

    /// Like [`scan`] but also threads/returns the `in_eol_run` flag so the
    /// round-4 LOW (maximal-EOL-run) cases can assert run state.
    fn scan_state(
        since: usize,
        carry: &[u8],
        in_eol_run: bool,
        chunk: &[u8],
        cap: usize,
        eof: bool,
    ) -> Result<super::ScanState, usize> {
        super::scan_frames_since_boundary(
            super::ScanState {
                since_boundary: since,
                carry: carry.to_vec(),
                in_eol_run,
            },
            chunk,
            cap,
            eof,
        )
    }

    #[test]
    fn scan_frames_charges_confirmed_frame_bytes_and_defers_prefix_tail() {
        let cap = 1024;
        // No carry, boundary mid-chunk: "ab" confirmed+reset, tail "cd" charged,
        // nothing deferred.
        assert_eq!(scan(0, b"", b"ab\n\ncd", cap, false), Ok((2, vec![])));
        // No boundary, no prefix tail: whole new chunk extends the frame and the
        // carried-in count; the (uncharged) carry "\r" is now confirmed & charged.
        assert_eq!(scan(5, b"\r", b"abc", cap, false), Ok((9, vec![])));
        // Boundary straddling carry/chunk edge: carry "\r\n" + chunk "\r\nz" =>
        // boundary completes & resets, tail "z" charged.
        assert_eq!(scan(7, b"\r\n", b"\r\nz", cap, false), Ok((1, vec![])));
        // A MIXED-separator boundary mid-chunk is recognized (Finding 1): carry-in
        // count 6, chunk "..\r\n\ncd" => the `\r\n\n` blank line resets (the `\r` is
        // NOT charged as a frame byte), tail "cd" charged.
        assert_eq!(scan(6, b"", b"\r\n\ncd", cap, false), Ok((2, vec![])));
        // A trailing boundary-PREFIX is DEFERRED, not charged (Finding 1): "ab"
        // charged, trailing "\r\n\r" held uncharged in the returned carry.
        assert_eq!(
            scan(0, b"", b"ab\r\n\r", cap, false),
            Ok((2, b"\r\n\r".to_vec()))
        );
        // A lone trailing "\n" is deferred (could start "\n\n"); count unchanged.
        assert_eq!(scan(3, b"", b"\n", cap, false), Ok((3, b"\n".to_vec())));
        // An ambiguous-length 2-byte prefix ("\n\r": LF + pending CR) is deferred
        // whole, mid-stream, so the verdict stays chunk-independent.
        assert_eq!(scan(3, b"", b"\n\r", cap, false), Ok((3, b"\n\r".to_vec())));
    }

    #[test]
    fn scan_frames_consumes_maximal_eol_run_after_boundary() {
        // Codex round-4 LOW: extra blank-line EOLs after a boundary belong to no
        // frame and must be charged to neither side.
        let cap = 4;
        // Three consecutive LFs: the first two are the boundary, the THIRD is an
        // extra empty line. "ab" charged+reset, the extra "\n" consumed (charged to
        // nothing), tail "cd" charged => since=2. The OLD code charged the extra
        // "\n" into "cd" (since=3).
        let s = scan_state(0, b"", false, b"ab\n\n\ncd", cap, false).expect("ok");
        assert_eq!(
            (s.since_boundary, s.carry, s.in_eol_run),
            (2, vec![], false)
        );
        // FOUR LFs (two boundaries' worth) collapse the same way: only "cd" counts.
        let s = scan_state(0, b"", false, b"ab\n\n\n\ncd", cap, false).expect("ok");
        assert_eq!(s.since_boundary, 2);
        // The exact Codex sequence: a frame at cap, then `\n\n\n`, then a second
        // frame of EXACTLY cap content. The extra `\n` must NOT be charged into the
        // second frame, so it stays at cap (accepted), not cap+1.
        let mut data = Vec::new();
        data.extend_from_slice(b"x".repeat(cap).as_slice());
        data.extend_from_slice(b"\n\n\n");
        data.extend_from_slice(b"y".repeat(cap).as_slice());
        let s = scan_state(0, b"", false, &data, cap, false).expect("at-cap second frame accepted");
        assert_eq!(s.since_boundary, cap);
        // Mixed run `\r\n\n` (boundary) + `\r\r` (two more empty-line EOLs): all
        // consumed, nothing charged from the run; tail "z" charged.
        let s = scan_state(0, b"", false, b"ab\r\n\n\r\rz", cap, false).expect("ok");
        assert_eq!((s.since_boundary, s.carry), (1, vec![]));
    }

    #[test]
    fn scan_frames_eol_run_straddling_chunk_edge_is_fully_consumed() {
        // The maximal-EOL run can straddle a chunk edge; it must still be consumed
        // (never charged), matching the in-chunk verdict (Codex round-4 LOW).
        let cap = 8;
        // Chunk 1 ends mid-run with a trailing lone `\r` (after boundary `\n\n` +
        // `\r`): the `\r` is deferred and we stay `in_eol_run`.
        let s = scan_state(0, b"", false, b"ab\n\n\r", cap, false).expect("ok");
        assert_eq!(
            (s.since_boundary, s.carry, s.in_eol_run),
            (0, b"\r".to_vec(), true)
        );
        // Chunk 2 resolves it as a CR EOL (`\r` + content): the run's `\r` is one
        // more empty line (NOT charged) and "cd" begins the next frame.
        let s = scan_state(0, b"\r", true, b"cd", cap, false).expect("ok");
        assert_eq!(
            (s.since_boundary, s.carry, s.in_eol_run),
            (2, vec![], false)
        );
        // If chunk 2 instead resolves it as CRLF (`\r\n`) followed by content, the
        // `\r\n` is still ONE empty-line EOL (uncharged) and "cd" begins the frame.
        let s = scan_state(0, b"\r", true, b"\ncd", cap, false).expect("ok");
        assert_eq!((s.since_boundary, s.in_eol_run), (2, false));
        // `in_eol_run` with a leading EOL that itself ends the chunk stays in-run.
        let s = scan_state(0, b"", true, b"\n", cap, false).expect("ok");
        assert_eq!((s.since_boundary, s.carry, s.in_eol_run), (0, vec![], true));
        // `in_eol_run` ending at EOF on a dangling `\r`: that final CR is one last
        // empty line, charged to nothing (NOT to a frame).
        let s = scan_state(0, b"\r", true, b"", cap, true).expect("ok");
        assert_eq!(s.since_boundary, 0);
    }

    #[test]
    fn scan_frames_finalizes_carry_on_eof() {
        let cap = 4;
        // EOF with a dangling single EOL carry charges it as the unterminated
        // frame's bytes (Finding 2): since=4 + carry "\n" => 5 > 4 => reject.
        assert_eq!(scan(cap, b"\n", b"", cap, true), Err(5));
        // EOF where the carry is itself a complete boundary ("\r\r", resolving the
        // final CR) resets instead of charging: the frame WAS terminated.
        assert_eq!(scan(3, b"\r\r", b"", cap, true), Ok((0, vec![])));
        // EOF with `\r\n\r` carry: that is `\r\n` EOL + a final CR EOL = boundary,
        // so it resets (no charge).
        assert_eq!(scan(3, b"\r\n\r", b"", cap, true), Ok((0, vec![])));
        // EOF with `\r\n` carry: one EOL, no second => unterminated frame; charge
        // both bytes. since=3 + 2 => 5 > 4 => reject.
        assert_eq!(scan(3, b"\r\n", b"", cap, true), Err(5));
    }

    #[test]
    fn scan_frames_caps_pre_boundary_segment() {
        let cap = 4;
        // A TERMINATED but oversized segment ("xxxxx" = 5 > 4) is rejected even
        // though the post-boundary tail is empty (Finding 1 sibling: confirmed
        // pre-boundary bytes are still capped).
        assert_eq!(scan(0, b"", b"xxxxx\n\n", cap, false), Err(5));
        // A pre-boundary segment that, added to the carried-in count, crosses the
        // cap is rejected before the reset (since=4, +"x" before "\n\n" => 5).
        assert_eq!(scan(cap, b"", b"x\n\n", cap, false), Err(5));
    }

    #[test]
    fn carry_completes_boundary_split_across_tiny_chunks() {
        // A `\r\n\r\n` separator arriving as "\r","\n","\r\n" must be detected.
        let mut guard = super::SseFrameGuard::new(1024);
        guard.accept(b"\r").expect("carry \\r");
        guard.accept(b"\n").expect("carry \\r\\n");
        // This completes \r\n\r\n across the chunk edge; the frame resets to 0.
        guard.accept(b"\r\n").expect("boundary completes");
        // Prove the reset: a near-cap frame now fits where it would not have if
        // the prior bytes had still been counted.
        let near_cap = vec![b'q'; 1024];
        guard
            .accept(&near_cap)
            .expect("fresh frame fits after multi-chunk boundary reset");
    }

    #[test]
    fn boundary_prefix_suffix_len_classifies_tails() {
        // Proper, incomplete/ambiguous boundary prefixes, longest-first. (In real
        // use the caller only ever passes a post-final-boundary tail, so an
        // unambiguous COMPLETE boundary like `\n\n` never reaches here.)
        assert_eq!(super::boundary_prefix_suffix_len(b"a\r\n\r"), 3); // CRLF + pending CR
        assert_eq!(super::boundary_prefix_suffix_len(b"a\r\n"), 2); // CRLF, 2nd EOL pending
        // Ambiguous-LENGTH 2-byte separators (one EOL + a pending CR) are deferred
        // whole: a following `\n` extends/redefines the boundary, so charging them
        // early would make the verdict depend on the chunk split.
        assert_eq!(super::boundary_prefix_suffix_len(b"a\n\r"), 2); // LF + pending CR
        assert_eq!(super::boundary_prefix_suffix_len(b"a\r\r"), 2); // CR + pending CR
        assert_eq!(super::boundary_prefix_suffix_len(b"a\r"), 1);
        assert_eq!(super::boundary_prefix_suffix_len(b"a\n"), 1);
        // A trailing `\r` is always a deferrable prefix (it may begin a `\r\r` or
        // `\r\n...` on the next chunk).
        assert_eq!(super::boundary_prefix_suffix_len(b"x\r"), 1);
        // Ordinary bytes defer nothing.
        assert_eq!(super::boundary_prefix_suffix_len(b"abc"), 0);
        assert_eq!(super::boundary_prefix_suffix_len(b""), 0);
    }

    #[test]
    fn guard_floors_tiny_cap_to_1kib() {
        let guard = super::SseFrameGuard::new(10);
        assert_eq!(guard.max_frame_bytes(), 1024);
    }

    #[tokio::test]
    async fn bounded_stream_passes_normal_then_errors_on_oversized() {
        use futures::StreamExt;
        // A normal small frame, then a chunk that blows the (floored) cap.
        // `Bytes` here is the re-exported `bytes::Bytes` already in scope; using
        // `Result<Bytes, reqwest::Error>` matches what `bytes_stream()` yields so
        // the adapter's generic bound is exercised exactly as in production.
        let chunks: Vec<Result<Bytes, reqwest::Error>> = vec![
            Ok(Bytes::from_static(b"data: ok\n\n")),
            Ok(Bytes::from(vec![b'x'; 2048])),
        ];
        let mut stream = Box::pin(super::bounded_sse_byte_stream(
            futures::stream::iter(chunks),
            1024,
        ));
        // First item passes through unchanged.
        let first = stream.next().await.expect("first item").expect("ok bytes");
        assert_eq!(first.as_ref(), b"data: ok\n\n");
        // Second item exceeds the cap and surfaces as a transport error.
        let err = stream
            .next()
            .await
            .expect("second item")
            .expect_err("oversized chunk errors");
        assert!(err.to_string().contains("exceeded"));
    }

    // --------------------------------------------------------------------------
    // Pure guard behavior (claude-relay test_sse.py parity)
    // --------------------------------------------------------------------------

    /// A single unterminated frame of exactly the cap is accepted (no boundary yet,
    /// but the running count has not EXCEEDED the cap). Mirrors `exactly_at_limit`.
    #[test]
    fn frame_exactly_at_cap_is_accepted() {
        let cap = 4096;
        let mut guard = SseFrameGuard::new(cap);
        // `new` floors at 1 KiB; 4096 is above the floor so it is exact.
        assert_eq!(guard.max_frame_bytes(), cap);

        let exact = vec![b'x'; cap];
        assert!(
            guard.accept(&exact).is_ok(),
            "a frame of exactly the cap must be accepted"
        );
    }

    /// One byte over the cap, with no event boundary, is rejected with a clean
    /// `AppError` (not a panic / not OOM). Mirrors `just_over_limit`.
    #[test]
    fn frame_one_byte_over_cap_is_rejected() {
        let cap = 4096;
        let mut guard = SseFrameGuard::new(cap);

        let over = vec![b'x'; cap + 1];
        let err = guard
            .accept(&over)
            .expect_err("a frame one byte over the cap must be rejected");
        let message = err.to_string();
        assert!(
            message.contains("exceeded") && message.contains(&cap.to_string()),
            "rejection error should name the exceeded cap, got: {message}"
        );
    }

    /// An oversized UNTERMINATED frame (never sends a `\n\n`) is rejected as soon as
    /// the running accumulation crosses the cap — i.e. BEFORE the whole hostile
    /// payload is consumed. We feed cap-sized chunks and assert the guard errors on
    /// the chunk that crosses the boundary, never having buffered the full
    /// (notionally unbounded) stream. Mirrors `oversized_unterminated` /
    /// `buffer_overflow`.
    #[test]
    fn oversized_unterminated_frame_is_rejected_before_unbounded_growth() {
        let cap = 1024; // floored minimum; keeps the test cheap.
        let mut guard = SseFrameGuard::new(cap);

        // Simulate a never-terminated frame arriving as many cap-sized chunks. A
        // truly hostile upstream would send these forever; the guard must stop us
        // long before that. None of these chunks contains a boundary.
        let chunk = vec![b'a'; cap];
        // First chunk: accumulation == cap, still accepted.
        assert!(guard.accept(&chunk).is_ok());
        // Second chunk would push accumulation to 2*cap > cap: rejected here, after
        // having seen only 2 chunks (bounded), not the whole infinite stream.
        let err = guard
            .accept(&chunk)
            .expect_err("unterminated frame past the cap must be rejected");
        assert!(err.to_string().contains("exceeded"));
    }

    /// The cap is configurable: a larger cap accepts what a smaller one rejects.
    /// Mirrors `custom_max_buffer`.
    #[test]
    fn cap_is_configurable() {
        let payload = vec![b'z'; 64 * 1024];

        // Small cap rejects it.
        let mut small = SseFrameGuard::new(8 * 1024);
        assert!(
            small.accept(&payload).is_err(),
            "small cap must reject the 64 KiB unterminated frame"
        );

        // Larger cap accepts it.
        let mut large = SseFrameGuard::new(128 * 1024);
        assert!(
            large.accept(&payload).is_ok(),
            "larger cap must accept the same 64 KiB frame"
        );
    }

    /// Normal-sized streaming is unaffected: many well-formed events whose TOTAL
    /// size dwarfs the cap still pass, because each event boundary resets the
    /// running count. This is the load-bearing "do not break normal streaming"
    /// guarantee.
    #[test]
    fn normal_streaming_is_unaffected() {
        let cap = 4096;
        let mut guard = SseFrameGuard::new(cap);

        // 10_000 small frames => total far exceeds the cap, but each frame is tiny
        // and terminated, so the per-frame accumulation never approaches the cap.
        for i in 0..10_000 {
            let event = frame(&format!("chunk number {i} with a little payload"));
            guard
                .accept(event.as_bytes())
                .expect("well-formed small frames must always pass");
        }
    }

    /// A frame split across many chunks that, summed, stays under the cap passes;
    /// the boundary then resets the count so the next frame starts fresh.
    #[test]
    fn chunked_frame_under_cap_passes_and_resets_on_boundary() {
        let cap = 1024;
        let mut guard = SseFrameGuard::new(cap);

        // 8 chunks of 100 bytes each = 800 bytes of one unterminated frame: under
        // the cap, all accepted.
        let part = vec![b'p'; 100];
        for _ in 0..8 {
            guard.accept(&part).expect("partial frame under cap");
        }
        // Now terminate the frame; the boundary resets accounting.
        guard.accept(b"\n\n").expect("boundary accepted");
        // A fresh, equally-large frame is fine because the count reset.
        for _ in 0..8 {
            guard
                .accept(&part)
                .expect("next frame under cap after reset");
        }
    }

    /// An event boundary that straddles two chunks (`...\r\n` then `\r\n...`) is
    /// still detected, so the post-boundary tail — not the whole pre-boundary frame
    /// — is what counts toward the next frame's cap.
    #[test]
    fn boundary_straddling_chunk_edge_is_detected() {
        let cap = 1024;
        let mut guard = SseFrameGuard::new(cap);

        // Fill most of the cap with an unterminated frame.
        let bulk = vec![b'd'; 1000];
        guard.accept(&bulk).expect("bulk under cap");
        // End the frame with a CRLF-CRLF separator split across two chunks.
        guard.accept(b"\r\n").expect("first half of boundary");
        guard
            .accept(b"\r\n")
            .expect("second half completes boundary");
        // The boundary reset the count; another near-cap frame is now fine even
        // though bulk + this would exceed the cap if it had NOT reset.
        guard
            .accept(&bulk)
            .expect("next frame fits because boundary reset the count");
    }

    /// An oversized but well-formed (TERMINATED) frame delivered in a single chunk
    /// must STILL be rejected: the cap bounds each frame — the bytes between
    /// boundaries — terminated or not. Regression for Codex Finding 1, where the
    /// guard reset to the (empty) post-boundary tail without checking the oversized
    /// pre-boundary segment, letting `b"x"*(cap+1) + b"\n\n"` slip through.
    #[test]
    fn oversized_terminated_single_chunk_frame_is_rejected() {
        let cap = 4096;
        let mut guard = SseFrameGuard::new(cap);

        let mut payload = vec![b'x'; cap + 1];
        payload.extend_from_slice(b"\n\n"); // well-formed boundary, but frame is over cap.
        let err = guard
            .accept(&payload)
            .expect_err("an oversized TERMINATED frame must be rejected");
        assert!(
            err.to_string().contains("exceeded"),
            "rejection should name the exceeded cap, got: {err}"
        );
    }

    /// When the running count is already near the cap, a chunk whose PRE-boundary
    /// segment pushes the count over must be rejected before the boundary resets it.
    /// Second half of Codex Finding 1: a boundary later in the chunk previously
    /// masked the over-cap pre-boundary bytes.
    #[test]
    fn pre_boundary_segment_over_cap_is_rejected_even_when_chunk_has_boundary() {
        let cap = 1024; // floored minimum.
        let mut guard = SseFrameGuard::new(cap);

        // Accumulate exactly the cap with an unterminated frame (accepted).
        let fill = vec![b'a'; cap];
        guard.accept(&fill).expect("exactly-at-cap accepted");

        // Now a chunk whose first byte extends the SAME frame to cap+1 BEFORE its
        // `\n\n` boundary: the over-cap pre-boundary segment must be caught.
        let err = guard
            .accept(b"x\n\ndata: next")
            .expect_err("pre-boundary segment over the cap must be rejected");
        assert!(err.to_string().contains("exceeded"), "got: {err}");
    }

    /// A separator (`\r\n\r\n`) split across 3+ tiny chunks (`"\r"`, `"\n"`, `"\r\n"`)
    /// must be detected so the frame counter resets — otherwise a long, perfectly
    /// valid multi-frame stream is falsely rejected once the missed boundaries let
    /// the count grow past the cap. Regression for Codex Finding 2, where the carry
    /// kept only the last <=3 bytes of the CURRENT chunk and lost earlier edge bytes.
    #[test]
    fn boundary_split_across_three_tiny_chunks_resets_and_does_not_falsely_reject() {
        let cap = 1024; // floored minimum.
        let mut guard = SseFrameGuard::new(cap);

        // Emit a long stream of frames, each terminated by a `\r\n\r\n` separator that
        // is dribbled out one/two bytes at a time. If the multi-chunk boundary were
        // missed, the count would never reset and would blow the cap long before the
        // loop ends. The per-frame payload is small, so a correct guard never trips.
        for i in 0..5_000 {
            let line = format!("data: frame {i}");
            guard.accept(line.as_bytes()).expect("payload under cap");
            // Boundary `\r\n\r\n` split across three chunks: "\r", "\n", "\r\n".
            guard.accept(b"\r").expect("boundary byte 1");
            guard.accept(b"\n").expect("boundary byte 2");
            guard
                .accept(b"\r\n")
                .expect("boundary completes; counter must reset");
        }
    }

    // --------------------------------------------------------------------------
    // Chunking-INDEPENDENCE: the accept/reject verdict for a byte stream must be
    // identical no matter how the bytes are split into chunks. The guard defers any
    // trailing boundary-PREFIX uncharged and never re-resets on a boundary it
    // already consumed.
    // --------------------------------------------------------------------------

    /// Feed `data` to a fresh guard split at the given chunk boundaries; return
    /// whether it was ACCEPTED (`true`) or rejected (`false`). When `eof` is set, the
    /// upstream stream is finalized (`SseFrameGuard::finish`) after the last chunk, so
    /// any pending boundary-prefix carry on a still-open frame is charged and
    /// cap-checked (Codex round-3 Finding 2). `splits` are the chunk lengths; their
    /// sum must equal `data.len()`.
    fn verdict_for_split_eof(data: &[u8], cap: usize, splits: &[usize], eof: bool) -> bool {
        assert_eq!(
            splits.iter().sum::<usize>(),
            data.len(),
            "split lengths must cover all bytes"
        );
        let mut guard = SseFrameGuard::new(cap);
        let mut off = 0usize;
        for &len in splits {
            if guard.accept(&data[off..off + len]).is_err() {
                return false;
            }
            off += len;
        }
        if eof && guard.finish().is_err() {
            return false;
        }
        true
    }

    /// Mid-stream verdict (no EOF finalization) — used by the original Findings-1/2
    /// regressions that assert the running, not-yet-terminated accounting.
    fn verdict_for_split(data: &[u8], cap: usize, splits: &[usize]) -> bool {
        verdict_for_split_eof(data, cap, splits, false)
    }

    /// Accept/reject when `data` is delivered as ONE chunk.
    fn verdict_whole(data: &[u8], cap: usize) -> bool {
        verdict_for_split(data, cap, &[data.len()])
    }

    /// Accept/reject when `data` is delivered ONE BYTE at a time.
    fn verdict_byte_by_byte(data: &[u8], cap: usize) -> bool {
        let ones = vec![1usize; data.len()];
        verdict_for_split(data, cap, &ones)
    }

    /// Codex Finding 1 (boundary-prefix bytes must not be charged before they are
    /// disambiguated): with the running count already AT the cap, a valid `\n\n`
    /// boundary delivered as two separate `b"\n"` chunks must NOT be rejected — the
    /// first `\n` is a boundary prefix and must be deferred, not charged. The same
    /// total bytes delivered as one `b"\n\n"` chunk must give the SAME verdict.
    #[test]
    fn boundary_prefix_split_is_not_falsely_rejected_and_is_chunk_independent() {
        let cap = 1024; // floored minimum.

        // Bytes: exactly `cap` frame bytes, then a `\n\n` boundary.
        let mut data = vec![b'a'; cap];
        data.extend_from_slice(b"\n\n");

        // (a) `cap` then "\n" then "\n": the first "\n" is a deferred boundary prefix,
        //     so the count stays at cap (not cap+1) and the second "\n" completes the
        //     boundary. ACCEPTED.
        let split = verdict_for_split(&data, cap, &[cap, 1, 1]);
        assert!(
            split,
            "a valid boundary split as two `\\n` chunks must be accepted (Finding 1)"
        );

        // (b) The exact same bytes with the boundary as a single "\n\n" chunk.
        let whole = verdict_for_split(&data, cap, &[cap, 2]);
        assert!(
            whole,
            "the same boundary as one `\\n\\n` chunk must be accepted"
        );

        // Chunking-independence: identical verdict regardless of the split.
        assert_eq!(
            split, whole,
            "verdict must not depend on how the boundary bytes are chunked"
        );
        assert_eq!(
            verdict_byte_by_byte(&data, cap),
            whole,
            "byte-by-byte delivery must match whole-chunk delivery"
        );
    }

    /// Codex Finding 2 (a boundary wholly inside the carry must not reset twice): the
    /// stream `b"data: {}\n\n"` (a terminated 8-byte frame) followed by `b"y"` starts
    /// a NEW 1-byte frame; then `b"x"*cap` extends it to `cap + 1` unterminated bytes,
    /// which MUST be rejected. A buggy guard re-detects the already-consumed `\n\n`
    /// (now sitting inside the carry) and wrongly resets, accepting `cap + 1` bytes.
    #[test]
    fn carry_internal_boundary_does_not_reset_twice() {
        let cap = 1024; // floored minimum.

        // `data: {}` is 8 bytes; the `\n\n` terminates it. Then "y" opens a new frame.
        let mut guard = SseFrameGuard::new(cap);
        guard
            .accept(b"data: {}\n\ny")
            .expect("terminated frame + 1 byte of next frame is fine");
        // Now drive the NEW frame to cap + 1 unterminated bytes: 1 ("y") + cap ("x").
        let bulk = vec![b'x'; cap];
        let err = guard
            .accept(&bulk)
            .expect_err("a 1025-byte unterminated frame must be rejected (Finding 2)");
        assert!(err.to_string().contains("exceeded"), "got: {err}");

        // And it is chunk-independent: the same bytes as one buffer reject too.
        let mut data = Vec::new();
        data.extend_from_slice(b"data: {}\n\ny");
        data.extend_from_slice(&bulk);
        assert!(
            !verdict_whole(&data, cap),
            "the 1025-byte frame must be rejected as a single chunk as well"
        );
        assert!(!verdict_byte_by_byte(&data, cap), "...and byte-by-byte");
    }

    /// THE SWEEP: take several streams (valid multi-frame; a frame exactly AT cap; a
    /// frame ONE OVER cap; CRLF and CR-CR boundaries) and feed each split at EVERY
    /// 1-byte offset, plus a few pseudo-random groupings. Assert the verdict is
    /// ALWAYS identical to feeding the stream whole. This is the load-bearing
    /// chunking-independence guarantee for the stateful carry accounting.
    #[test]
    fn chunking_independence_sweep_every_offset_and_random_groupings() {
        let cap = 1024; // floored minimum — keeps the sweep cheap.

        // Build the corpus. Each entry: (label, bytes).
        let mut corpus: Vec<(String, Vec<u8>)> = Vec::new();

        // 1) A valid multi-frame stream mixing LF-LF, CRLF-CRLF and CR-CR boundaries.
        {
            let mut s = Vec::new();
            s.extend_from_slice(b"data: one\n\n");
            s.extend_from_slice(b"data: two\r\n\r\n");
            s.extend_from_slice(b"data: three\r\r");
            s.extend_from_slice(b"data: tail-no-final-boundary");
            corpus.push(("valid_multi_frame_mixed_boundaries".into(), s));
        }

        // 2) A frame EXACTLY at cap, terminated, then another short frame. Accepted.
        {
            let mut s = vec![b'a'; cap];
            s.extend_from_slice(b"\n\n");
            s.extend_from_slice(b"data: after\n\n");
            corpus.push(("frame_exactly_at_cap_terminated".into(), s));
        }

        // 3) A frame ONE OVER cap, terminated. Rejected (the pre-boundary segment is
        //    over the cap no matter where the chunk edges fall).
        {
            let mut s = vec![b'b'; cap + 1];
            s.extend_from_slice(b"\n\n");
            corpus.push(("frame_one_over_cap_terminated".into(), s));
        }

        // 4) An unterminated frame ONE OVER cap (worst case for the guard). Rejected.
        {
            let s = vec![b'c'; cap + 1];
            corpus.push(("frame_one_over_cap_unterminated".into(), s));
        }

        // 5) Frames whose boundaries are deliberately adjacent to ambiguous bytes:
        //    content ending in `\r` then a real `\n\n`, exercising prefix deferral.
        {
            let mut s = Vec::new();
            s.extend_from_slice(b"data: ends-with-cr\r\n\n"); // "\r" then "\n\n"
            s.extend_from_slice(b"data: x\n\r\r"); // "\n" then "\r\r"
            s.extend_from_slice(b"data: y");
            corpus.push(("ambiguous_prefix_adjacent_boundaries".into(), s));
        }

        // Simple deterministic LCG so "random" groupings are reproducible without a
        // dependency (Numerical Recipes constants).
        let mut rng: u64 = 0x1234_5678_9abc_def0;
        let mut next_rand = move |bound: usize| -> usize {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            if bound == 0 {
                0
            } else {
                ((rng >> 33) as usize) % bound
            }
        };

        for (label, data) in &corpus {
            let expected = verdict_whole(data, cap);

            // Every single-byte offset is covered by the byte-by-byte split, which is
            // the maximal fragmentation; also test EACH two-chunk split (one cut at
            // every offset) so a boundary or prefix landing on any edge is exercised.
            assert_eq!(
                verdict_byte_by_byte(data, cap),
                expected,
                "[{label}] byte-by-byte verdict diverged from whole-chunk verdict"
            );
            for cut in 1..data.len() {
                let splits = [cut, data.len() - cut];
                assert_eq!(
                    verdict_for_split(data, cap, &splits),
                    expected,
                    "[{label}] two-chunk split at offset {cut} diverged from whole"
                );
            }

            // A handful of pseudo-random multi-chunk groupings.
            for trial in 0..16 {
                let mut splits = Vec::new();
                let mut remaining = data.len();
                while remaining > 0 {
                    // Chunk size in 1..=remaining, biased small to create many edges.
                    let size = 1 + next_rand(remaining.min(7));
                    let size = size.min(remaining);
                    splits.push(size);
                    remaining -= size;
                }
                assert_eq!(
                    verdict_for_split(data, cap, &splits),
                    expected,
                    "[{label}] random grouping #{trial} {splits:?} diverged from whole"
                );
            }
        }
    }

    /// The eight greedily-tokenized SSE blank-line separators — every pairing of an
    /// `end-of-line = ( cr lf / cr / lf )` with the next, CRLF matched greedily. The
    /// guard must treat ALL of them as frame boundaries (Codex round-3 Finding 1); the
    /// old guard recognized only `\n\n`, `\r\r`, `\r\n\r\n`.
    const ALL_BOUNDARY_COMBOS: &[(&str, &[u8])] = &[
        ("lf_lf", b"\n\n"),
        ("cr_cr", b"\r\r"),
        ("crlf_crlf", b"\r\n\r\n"),
        ("crlf_lf", b"\r\n\n"),
        ("lf_cr", b"\n\r"),
        ("crlf_cr", b"\r\n\r"),
        ("lf_crlf", b"\n\r\n"),
        ("cr_crlf", b"\r\r\n"),
    ];

    /// THE EXHAUSTIVE CONVERGENCE GATE (Codex round-3). For EACH of the eight EOL
    /// boundary combos, build a corpus of VALID streams that use that combo as the
    /// frame separator — including a frame EXACTLY at cap and a frame ONE BYTE OVER
    /// cap — and feed each stream (a) whole, (b) split at EVERY byte offset, (c)
    /// byte-by-byte, (d) several seeded-random groupings, AND (e) with a final EOF
    /// after the last byte. The canonical verdict is the one taken WITH EOF
    /// finalization (the real adapter always finalizes on stream end). Assert it is
    /// IDENTICAL across every framing (chunking-independent) and EQUALS the intended
    /// per-frame-cap verdict (exactly-at-cap accepts, over-cap rejects) for each combo.
    #[test]
    fn exhaustive_eol_combo_convergence_gate_every_offset_random_and_eof() {
        let cap = 1024; // floored minimum — keeps the O(n^2) per-offset sweep cheap.

        // Deterministic LCG (Numerical Recipes constants) for reproducible "random"
        // groupings without a dependency.
        let mut rng: u64 = 0x0f0f_0f0f_dead_beef;
        let mut next_rand = move |bound: usize| -> usize {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            if bound == 0 {
                0
            } else {
                ((rng >> 33) as usize) % bound
            }
        };

        for (combo, sep) in ALL_BOUNDARY_COMBOS {
            // Each entry: (label, bytes, intended terminal accept verdict). The intended
            // verdict is by the per-FRAME cap: a frame is the CONTENT between boundaries
            // (separator bytes are charged to neither side); exactly cap accepts, cap+1
            // rejects. Every stream uses `sep` as its ONLY separator.
            let mut corpus: Vec<(String, Vec<u8>, bool)> = Vec::new();

            // 1) A frame EXACTLY at cap, terminated by `sep`, then a short terminated
            //    frame. Both frames are <= cap => ACCEPT.
            {
                let mut s = vec![b'a'; cap];
                s.extend_from_slice(sep);
                s.extend_from_slice(b"data: tail");
                s.extend_from_slice(sep);
                corpus.push((format!("{combo}/at_cap_terminated"), s, true));
            }
            // 2) A frame ONE OVER cap, terminated by `sep` => REJECT (the over-cap
            //    pre-boundary segment is caught no matter where the chunk edges fall).
            {
                let mut s = vec![b'b'; cap + 1];
                s.extend_from_slice(sep);
                corpus.push((format!("{combo}/over_cap_terminated"), s, false));
            }
            // 3) A short terminated frame, then a trailing UNTERMINATED frame whose
            //    content is EXACTLY cap (no final `sep`). At EOF the trailing frame is
            //    finalized at exactly cap => ACCEPT.
            {
                let mut s = Vec::new();
                s.extend_from_slice(b"data: head");
                s.extend_from_slice(sep);
                s.extend_from_slice(&vec![b'c'; cap]);
                corpus.push((format!("{combo}/trailing_unterminated_at_cap"), s, true));
            }
            // 4) A trailing UNTERMINATED frame whose content is ONE OVER cap (no final
            //    `sep`). Rejected during accept (content alone crosses the cap).
            {
                let s = vec![b'd'; cap + 1];
                corpus.push((format!("{combo}/trailing_unterminated_over_cap"), s, false));
            }
            // 5) A frame EXACTLY at cap whose terminator is followed by a DANGLING
            //    partial separator (the first EOL of `sep`) with no second EOL — the
            //    Finding-2 EOF case. The dangling EOL is a separator-prefix carry held
            //    uncharged mid-stream, but at EOF it cannot complete a boundary, so it
            //    is charged to the still-open frame. content cap + 1 dangling byte =>
            //    cap+1 at EOF => REJECT. (Without EOF this would wrongly ACCEPT — proven
            //    by the dedicated Finding-2 test below.)
            {
                let first_eol: &[u8] = if sep[0] == b'\r' && sep.get(1) == Some(&b'\n') {
                    b"\r\n"
                } else {
                    &sep[..1]
                };
                let mut s = vec![b'e'; cap];
                s.extend_from_slice(first_eol);
                corpus.push((format!("{combo}/at_cap_then_dangling_eol"), s, false));
            }

            for (label, data, intended) in &corpus {
                // Canonical terminal verdict: whole stream, finalized at EOF.
                let expected = verdict_for_split_eof(data, cap, &[data.len()], true);
                assert_eq!(
                    expected, *intended,
                    "[{label}] whole+EOF verdict {expected} != intended per-frame-cap verdict {intended}"
                );

                // (c) byte-by-byte, finalized.
                let ones = vec![1usize; data.len()];
                assert_eq!(
                    verdict_for_split_eof(data, cap, &ones, true),
                    expected,
                    "[{label}] byte-by-byte+EOF diverged from whole+EOF"
                );

                // (b) EVERY single-byte cut (two chunks), finalized.
                for cut in 1..data.len() {
                    let splits = [cut, data.len() - cut];
                    assert_eq!(
                        verdict_for_split_eof(data, cap, &splits, true),
                        expected,
                        "[{label}] two-chunk split at offset {cut} + EOF diverged from whole+EOF"
                    );
                }

                // (d) several seeded-random multi-chunk groupings, finalized.
                for trial in 0..24 {
                    let mut splits = Vec::new();
                    let mut remaining = data.len();
                    while remaining > 0 {
                        let size = (1 + next_rand(remaining.min(9))).min(remaining);
                        splits.push(size);
                        remaining -= size;
                    }
                    assert_eq!(
                        verdict_for_split_eof(data, cap, &splits, true),
                        expected,
                        "[{label}] random grouping #{trial} {splits:?} + EOF diverged from whole+EOF"
                    );
                }
            }
        }
    }

    /// Finding 2, isolated: a frame exactly at cap followed by a DANGLING partial
    /// separator (one EOL, no blank line) is wrongly ACCEPTED if the pending carry is
    /// not charged on stream end, and correctly REJECTED once `finish()` finalizes it.
    /// Proven for every combo's leading EOL so the EOF charge is load-bearing, not
    /// incidental. (`b"e"*cap + dangling_eol` is cap+1 unterminated bytes.)
    #[test]
    fn pending_carry_is_charged_on_eof_for_every_combo() {
        let cap = 1024;
        for (combo, sep) in ALL_BOUNDARY_COMBOS {
            let first_eol: &[u8] = if sep[0] == b'\r' && sep.get(1) == Some(&b'\n') {
                b"\r\n"
            } else {
                &sep[..1]
            };
            let mut data = vec![b'e'; cap];
            data.extend_from_slice(first_eol);

            // Mid-stream (no EOF): the dangling EOL is deferred uncharged, so the cap is
            // NOT yet exceeded — accepted.
            assert!(
                verdict_for_split_eof(&data, cap, &[data.len()], false),
                "[{combo}] without EOF the dangling separator prefix must be deferred (accepted)"
            );
            // With EOF: the carry is finalized and charged -> cap+1 -> rejected.
            assert!(
                !verdict_for_split_eof(&data, cap, &[data.len()], true),
                "[{combo}] EOF must charge the pending carry and reject the over-cap unterminated frame"
            );
            // And the EOF rejection is chunk-independent (split right at the dangling EOL).
            let cut = cap;
            assert!(
                !verdict_for_split_eof(&data, cap, &[cut, data.len() - cut], true),
                "[{combo}] EOF rejection must hold when the dangling EOL is its own chunk"
            );
        }
    }

    /// MULTI-blank-line separators between frames (Codex round-4 LOW). A frame
    /// boundary is a single blank line (two EOLs); any ADDITIONAL consecutive EOLs are
    /// extra empty lines that `eventsource-stream` dispatches as empty events / skips,
    /// so their bytes belong to NO data frame and must be charged to neither side.
    /// Each separator here is a run of >2 EOLs (or a mixed run); the frame that
    /// FOLLOWS it is sized to EXACTLY the cap (must ACCEPT — the extra EOLs are not
    /// charged into it) and ONE OVER the cap (must REJECT — its own content alone
    /// crosses the cap). The verdict is taken whole+EOF and asserted IDENTICAL across
    /// every chunking (whole, every single-byte cut, byte-by-byte, seeded-random) so
    /// an EOL run split across a chunk edge is still fully consumed, never charged.
    #[test]
    fn multi_blank_line_separators_charge_extra_eols_to_no_frame_and_are_chunk_independent() {
        let cap = 1024; // floored minimum — keeps the O(n^2) per-offset sweep cheap.

        // Inter-frame EOL RUNS to exercise: >2-EOL homogeneous runs and mixed runs.
        // Each is strictly more than one blank line, so >=1 EOL is an extra empty line
        // that must be charged to neither neighbour.
        let separators: &[(&str, &[u8])] = &[
            ("lf3", b"\n\n\n"),
            ("lf4", b"\n\n\n\n"),
            ("crlf3", b"\r\n\r\n\r\n"),
            ("mixed_lf_crlf_lf", b"\n\r\n\n"),
            ("cr3", b"\r\r\r"),
            ("mixed_crlf_lf_cr_cr", b"\r\n\n\r\r"),
        ];

        let mut rng: u64 = 0x1234_5678_dead_f00d;
        let mut next_rand = move |bound: usize| -> usize {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            if bound == 0 {
                0
            } else {
                ((rng >> 33) as usize) % bound
            }
        };

        for (sep_label, sep) in separators {
            // Each entry: (label, bytes, intended terminal accept verdict). The frame
            // FOLLOWING the multi-EOL separator is the one under test.
            let mut corpus: Vec<(String, Vec<u8>, bool)> = Vec::new();

            // 1) head frame, multi-EOL separator, then a SECOND frame EXACTLY at cap
            //    (terminated). The extra EOLs in `sep` must NOT be charged into the
            //    second frame, so it is exactly cap => ACCEPT. This is the structural
            //    form of the Codex sequence.
            {
                let mut s = Vec::new();
                s.extend_from_slice(b"data: head");
                s.extend_from_slice(sep);
                s.extend_from_slice(&vec![b'a'; cap]);
                s.extend_from_slice(b"\n\n");
                corpus.push((format!("{sep_label}/second_frame_at_cap"), s, true));
            }
            // 2) Same, but the second frame is ONE OVER cap => REJECT (its content alone
            //    crosses the cap, regardless of how the EOL run is charged).
            {
                let mut s = Vec::new();
                s.extend_from_slice(b"data: head");
                s.extend_from_slice(sep);
                s.extend_from_slice(&vec![b'b'; cap + 1]);
                s.extend_from_slice(b"\n\n");
                corpus.push((format!("{sep_label}/second_frame_over_cap"), s, false));
            }
            // 3) A leading multi-EOL run (all empty lines) THEN a frame at cap: the
            //    leading extras are charged to nothing => ACCEPT.
            {
                let mut s = Vec::new();
                s.extend_from_slice(sep);
                s.extend_from_slice(&vec![b'c'; cap]);
                s.extend_from_slice(b"\n\n");
                corpus.push((format!("{sep_label}/leading_run_then_at_cap"), s, true));
            }
            // 4) head frame, multi-EOL separator, then a trailing UNTERMINATED frame of
            //    EXACTLY cap (no closing blank line). At EOF it finalizes at cap and the
            //    separator's extra EOLs are not charged => ACCEPT.
            {
                let mut s = Vec::new();
                s.extend_from_slice(b"data: head");
                s.extend_from_slice(sep);
                s.extend_from_slice(&vec![b'd'; cap]);
                corpus.push((format!("{sep_label}/trailing_unterminated_at_cap"), s, true));
            }
            // 5) Same trailing-unterminated form but ONE OVER cap => REJECT.
            {
                let mut s = Vec::new();
                s.extend_from_slice(b"data: head");
                s.extend_from_slice(sep);
                s.extend_from_slice(&vec![b'e'; cap + 1]);
                corpus.push((
                    format!("{sep_label}/trailing_unterminated_over_cap"),
                    s,
                    false,
                ));
            }

            for (label, data, intended) in &corpus {
                // Canonical terminal verdict: whole stream, finalized at EOF.
                let expected = verdict_for_split_eof(data, cap, &[data.len()], true);
                assert_eq!(
                    expected, *intended,
                    "[{label}] whole+EOF verdict {expected} != intended per-frame-cap verdict {intended}"
                );

                // byte-by-byte, finalized (splits every EOL run across chunk edges).
                let ones = vec![1usize; data.len()];
                assert_eq!(
                    verdict_for_split_eof(data, cap, &ones, true),
                    expected,
                    "[{label}] byte-by-byte+EOF diverged from whole+EOF"
                );

                // EVERY single-byte cut (two chunks), finalized.
                for cut in 1..data.len() {
                    let splits = [cut, data.len() - cut];
                    assert_eq!(
                        verdict_for_split_eof(data, cap, &splits, true),
                        expected,
                        "[{label}] two-chunk split at offset {cut} + EOF diverged from whole+EOF"
                    );
                }

                // Several seeded-random multi-chunk groupings, finalized.
                for trial in 0..24 {
                    let mut splits = Vec::new();
                    let mut remaining = data.len();
                    while remaining > 0 {
                        let size = (1 + next_rand(remaining.min(9))).min(remaining);
                        splits.push(size);
                        remaining -= size;
                    }
                    assert_eq!(
                        verdict_for_split_eof(data, cap, &splits, true),
                        expected,
                        "[{label}] random grouping #{trial} {splits:?} + EOF diverged from whole+EOF"
                    );
                }
            }
        }
    }

    /// The EXACT Codex round-4 reproduction: `b"data: {}\n\n\n" + b"data: " +
    /// b"x"*(cap-6) + b"\n\n"`. The second frame's content is `"data: " + x*(cap-6)` =
    /// EXACTLY cap bytes; the extra `\n` after the first frame's `\n\n` boundary is an
    /// empty line and must be charged to NO frame. Before the fix it was charged into
    /// the second frame, rejecting it as cap+1. It must now be ACCEPTED, and the
    /// verdict must hold under every chunking (whole, byte-by-byte, every cut).
    #[test]
    fn codex_round4_extra_blank_line_before_at_cap_frame_is_accepted() {
        let cap = 1024;
        let mut data = Vec::new();
        data.extend_from_slice(b"data: {}\n\n\n");
        data.extend_from_slice(b"data: ");
        data.extend_from_slice(&vec![b'x'; cap - 6]); // "data: " (6) + this = cap
        data.extend_from_slice(b"\n\n");

        // Whole, finalized: ACCEPTED.
        assert!(
            verdict_for_split_eof(&data, cap, &[data.len()], true),
            "the second frame is exactly at cap; the extra inter-frame `\\n` must not be charged into it"
        );
        // And mid-stream (no EOF) — the second frame's closing `\n\n` already resets it.
        assert!(
            verdict_for_split_eof(&data, cap, &[data.len()], false),
            "exact Codex sequence must be accepted mid-stream too"
        );
        // Chunk-independent: byte-by-byte and every single-byte cut agree.
        let ones = vec![1usize; data.len()];
        assert!(
            verdict_for_split_eof(&data, cap, &ones, true),
            "byte-by-byte delivery of the exact Codex sequence must also accept"
        );
        for cut in 1..data.len() {
            assert!(
                verdict_for_split_eof(&data, cap, &[cut, data.len() - cut], true),
                "two-chunk split at offset {cut} of the exact Codex sequence must accept"
            );
        }
    }

    /// A LEADING empty SSE event (a blank line BEFORE the first `data:`) — its EOL(s)
    /// must be charged to NO frame, exactly like extra blank lines between frames
    /// (Codex round-5 LOW). Concrete false reject before the fix at cap 1024:
    /// `b"\n" + b"data: " + b"x"*(cap-6) + b"\n\n"` parses as an empty event then a
    /// frame whose content is `"data: " + x*(cap-6)` = EXACTLY cap bytes; the guard
    /// (starting `in_eol_run: false`) charged the leading `\n` into that frame and
    /// rejected it as cap+1. It must now ACCEPT, under whole/byte-by-byte/every cut,
    /// both with and without EOF finalization.
    #[test]
    fn leading_empty_event_eol_is_charged_to_no_frame_and_is_accepted() {
        let cap = 1024;
        let mut data = Vec::new();
        data.extend_from_slice(b"\n"); // leading empty SSE event (blank line)
        data.extend_from_slice(b"data: ");
        data.extend_from_slice(&vec![b'x'; cap - 6]); // "data: " (6) + this = cap
        data.extend_from_slice(b"\n\n");

        // Whole, finalized at EOF: ACCEPTED (the leading `\n` is charged to nothing).
        assert!(
            verdict_for_split_eof(&data, cap, &[data.len()], true),
            "the frame after the leading empty event is exactly at cap; the leading `\\n` must not be charged into it"
        );
        // Mid-stream (no EOF) too: the frame's own `\n\n` already resets it.
        assert!(
            verdict_for_split_eof(&data, cap, &[data.len()], false),
            "leading-empty-event case must be accepted mid-stream as well"
        );
        // Split right AFTER the leading EOL (its own chunk) — the boundary of the bug.
        assert!(
            verdict_for_split_eof(&data, cap, &[1, data.len() - 1], true),
            "leading `\\n` delivered as its own chunk must still be charged to no frame"
        );
        // Chunk-independent: byte-by-byte and every single-byte cut agree, with EOF.
        let ones = vec![1usize; data.len()];
        assert!(
            verdict_for_split_eof(&data, cap, &ones, true),
            "byte-by-byte delivery of the leading-empty-event case must accept"
        );
        for cut in 1..data.len() {
            assert!(
                verdict_for_split_eof(&data, cap, &[cut, data.len() - cut], true),
                "two-chunk split at offset {cut} of the leading-empty-event case must accept"
            );
        }
    }

    // --------------------------------------------------------------------------
    // REFERENCE-ORACLE differential gate. The chunking-independence sweeps only
    // assert self-consistency (every split agrees with whole); that pins nothing
    // ABSOLUTE — a position-specific off-by-one (leading / extra / trailing EOL
    // charged to the wrong frame) is self-consistent and slips through. So we add
    // an independent, obviously-correct, WHOLE-BUFFER reference scanner and assert
    // the streaming guard's verdict EQUALS the oracle's for every input. The oracle
    // is trivial to get right because it sees the entire buffer at once:
    //   * tokenize EOLs greedily as CRLF | CR | LF (mirrors `eventsource-stream`'s
    //     `end-of-line = cr lf / cr / lf`, CRLF longest-first);
    //   * a frame BOUNDARY is a BLANK LINE = TWO consecutive EOLs (`eventsource-stream`
    //     buffers the whole event and only dispatches at a blank line); a "frame" is
    //     the RAW bytes BETWEEN blank-line boundaries. A SINGLE EOL that is NOT part of
    //     a blank line is an internal field-line terminator INSIDE the event, so it IS
    //     part of the frame and IS counted (a multi-line event is ONE frame whose size
    //     includes its internal line terminators);
    //   * only EOLs that form a blank-line separator, plus any ADDITIONAL consecutive
    //     EOLs (extra empty lines), plus a LEADING EOL run (leading empty events), plus
    //     a wholly-EOL trailing dangling separator belong to NO frame and are charged to
    //     neither neighbour;
    //   * max_frame_bytes = the largest blank-line-delimited frame's byte length (0 if
    //     there are no frames);
    //   * verdict ACCEPT iff max_frame_bytes <= cap (i.e. reject iff it EXCEEDS cap).
    // This is the EOF/terminal verdict (at end of stream a trailing lone CR resolves to
    // a CR EOL; a dangling SINGLE EOL after the last blank line opens no new frame and
    // is an inter-/trailing-empty line, but a dangling EOL INSIDE an unterminated frame
    // is charged with that frame), so we compare it against the guard finalized with
    // `finish()`.
    // --------------------------------------------------------------------------

    /// Length of the greedily-tokenized EOL at `buf[i]` (`\r\n`=2, lone `\r`=1,
    /// `\n`=1), or `None` if `buf[i]` is not an EOL byte. Whole-buffer / terminal: a
    /// trailing lone `\r` is a CR EOL (no further byte can extend it to CRLF).
    fn oracle_eol_len(buf: &[u8], i: usize) -> Option<usize> {
        match buf.get(i) {
            Some(b'\r') => {
                if buf.get(i + 1) == Some(&b'\n') {
                    Some(2) // CRLF, greedy
                } else {
                    Some(1) // lone CR
                }
            }
            Some(b'\n') => Some(1),
            _ => None,
        }
    }

    /// Advance past a MAXIMAL run of consecutive EOLs in `buf` starting at `from`,
    /// returning the index just past the last EOL (`from` itself if `buf[from]` is not
    /// an EOL). Greedy CRLF tokenization via [`oracle_eol_len`]. Whole-buffer/terminal,
    /// so a trailing lone CR is a CR EOL. Every byte skipped here is a blank-line /
    /// extra-empty-line / leading-empty-event EOL that belongs to NO data frame.
    fn oracle_skip_eol_run(buf: &[u8], from: usize) -> usize {
        let mut i = from;
        while i < buf.len() {
            match oracle_eol_len(buf, i) {
                Some(len) => i += len,
                None => break,
            }
        }
        i
    }

    /// Find the next BLANK-LINE boundary (two consecutive greedy EOLs) at/after `from`,
    /// returning `(start, end)` over BOTH EOLs, or `None` if no blank line completes in
    /// `buf`. A SINGLE EOL followed by content is NOT a boundary — it is an internal
    /// field-line terminator inside the current event, so scanning resumes AFTER it
    /// (its bytes stay inside the frame). Mirrors `next_boundary` at end-of-stream
    /// (`at_eof`): the whole buffer is present, so a trailing lone CR is a resolved CR
    /// EOL.
    fn oracle_next_boundary(buf: &[u8], from: usize) -> Option<(usize, usize)> {
        let mut i = from;
        while i < buf.len() {
            let Some(first) = oracle_eol_len(buf, i) else {
                i += 1; // ordinary frame byte
                continue;
            };
            match oracle_eol_len(buf, i + first) {
                // Two consecutive EOLs => blank line => boundary spanning both.
                Some(second) => return Some((i, i + first + second)),
                // One EOL then content: internal line terminator, not a boundary.
                None => i += first,
            }
        }
        None
    }

    /// REFERENCE ORACLE: the terminal ACCEPT/reject verdict for the WHOLE buffer,
    /// computed directly, faithfully modelling `eventsource-stream`'s per-EVENT
    /// buffering. A frame is the RAW bytes BETWEEN blank-line boundaries (a blank line
    /// = two consecutive EOLs); internal single-EOL field-line terminators ARE part of
    /// the frame and ARE counted, so a multi-line event is ONE frame. A leading EOL run
    /// (leading empty events), the blank-line separators themselves, any extra empty
    /// lines after a boundary, and a wholly-EOL trailing dangling separator belong to
    /// NO frame. ACCEPT iff the largest blank-line-delimited frame is `<= cap`.
    fn oracle_accepts(buf: &[u8], cap: usize) -> bool {
        // Skip a LEADING EOL run (leading empty events): charged to no frame. The guard
        // starts `in_eol_run = true`, so it consumes every leading EOL the same way.
        let mut i = oracle_skip_eol_run(buf, 0);
        let mut max_frame = 0usize;
        while i < buf.len() {
            match oracle_next_boundary(buf, i) {
                // Frame is [i, bs): the raw bytes (incl. internal single EOLs) before the
                // blank line. Its byte length is the frame size. After the boundary, skip
                // any ADDITIONAL consecutive EOLs (extra empty lines) before the next
                // frame begins.
                Some((bs, be)) => {
                    max_frame = max_frame.max(bs - i);
                    i = oracle_skip_eol_run(buf, be);
                }
                // No further blank line: the trailing segment [i, end) is the final
                // unterminated frame. At EOF every one of its bytes is charged (including
                // any dangling internal EOL), so its size is the full remaining length.
                None => {
                    max_frame = max_frame.max(buf.len() - i);
                    break;
                }
            }
        }
        max_frame <= cap
    }

    /// THE DIFFERENTIAL CONVERGENCE GATE (Codex round-5): for a broad corpus —
    /// single/multi/mixed EOL separators, LEADING empty events, TRAILING dangling
    /// EOLs, frames AT and OVER cap — assert the STREAMING guard's finalized verdict
    /// (fed across whole / byte-by-byte / every single-byte cut / seeded-random
    /// splits + EOF) EQUALS the independent whole-buffer reference oracle. Unlike the
    /// self-consistency sweeps, this pins the ABSOLUTE verdict for EVERY EOL position
    /// at once, so a leading/extra/trailing off-by-one cannot pass by being merely
    /// chunk-consistent.
    #[test]
    fn reference_oracle_differential_gate_pins_absolute_verdict_every_eol_position() {
        let cap = 1024; // floored minimum — keeps the O(n^2) per-offset sweeps cheap.

        // EOL separators to thread through the corpus: single blank line, multi-EOL
        // runs (homogeneous + mixed), all greedy combos.
        let seps: &[&[u8]] = &[
            b"\n\n",
            b"\r\r",
            b"\r\n\r\n",
            b"\r\n\n",
            b"\n\r",
            b"\r\n\r",
            b"\n\r\n",
            b"\r\r\n",
            b"\n\n\n",
            b"\n\n\n\n",
            b"\r\n\r\n\r\n",
            b"\n\r\n\n",
            b"\r\r\r",
            b"\r\n\n\r\r",
        ];

        let mut corpus: Vec<(String, Vec<u8>)> = Vec::new();

        for sep in seps {
            let tag = format!("{sep:?}");
            // For each separator, vary the frame sizes around the cap so the oracle and
            // the guard must AGREE on the exact at/over-cap edge for every EOL position.
            for &(sz_label, first, second) in &[
                ("both_small", 10usize, 12usize),
                ("first_at_cap", cap, 12),
                ("first_over_cap", cap + 1, 12),
                ("second_at_cap", 12, cap),
                ("second_over_cap", 12, cap + 1),
            ] {
                // (a) two terminated frames separated by `sep`.
                {
                    let mut s = vec![b'a'; first];
                    s.extend_from_slice(sep);
                    s.extend_from_slice(&vec![b'b'; second]);
                    s.extend_from_slice(b"\n\n");
                    corpus.push((format!("two_frames/{tag}/{sz_label}"), s));
                }
                // (b) LEADING `sep` (empty events) then a frame, terminated.
                {
                    let mut s = Vec::new();
                    s.extend_from_slice(sep);
                    s.extend_from_slice(&vec![b'c'; first]);
                    s.extend_from_slice(b"\n\n");
                    corpus.push((format!("leading_sep/{tag}/{sz_label}"), s));
                }
                // (c) a frame then a TRAILING `sep` (dangling, no further content), at EOF.
                {
                    let mut s = vec![b'd'; first];
                    s.extend_from_slice(sep);
                    corpus.push((format!("trailing_sep/{tag}/{sz_label}"), s));
                }
                // (d) leading `sep`, a frame, `sep`, a trailing UNTERMINATED frame.
                {
                    let mut s = Vec::new();
                    s.extend_from_slice(sep);
                    s.extend_from_slice(&vec![b'e'; first]);
                    s.extend_from_slice(sep);
                    s.extend_from_slice(&vec![b'f'; second]);
                    corpus.push((format!("lead_mid_trail/{tag}/{sz_label}"), s));
                }
            }
        }

        // MULTI-LINE EVENTS (Codex round-6 MEDIUM). A single SSE event may contain
        // SEVERAL field lines separated by a SINGLE EOL; `eventsource-stream` buffers the
        // WHOLE event and only dispatches at the BLANK line, so the event is ONE frame
        // whose size INCLUDES its internal single-EOL line terminators. A per-LINE counter
        // would UNDER-count these — a multi-line event whose TOTAL exceeds the cap (but
        // each LINE is under it) would be wrongly ACCEPTED. These cases pin the at/over-cap
        // edge for the WHOLE multi-line event and are threaded through every chunking + EOF
        // below, so the guard (which counts the event whole) and the oracle must agree, and
        // the over-cap event must REJECT.
        //
        // `inner` is a SINGLE greedy EOL (NEVER a blank line): only `\n`, `\r`, `\r\n`
        // qualify — `\n\r`/`\r\r` are two EOLs (a blank line) and would split the event.
        for &(inner_label, inner) in &[
            ("lf", b"\n".as_slice()),
            ("cr", b"\r".as_slice()),
            ("crlf", b"\r\n".as_slice()),
        ] {
            // Two field lines `aaa <inner> bbb`, then a closing blank line `\n\n`. Pick the
            // two line lengths so the event's TOTAL byte count (line1 + inner + line2) is
            // EXACTLY cap (ACCEPT) and ONE OVER cap (REJECT). Each line stays ~half the
            // cap — well UNDER it — so a per-LINE counter would accept BOTH; only a
            // per-EVENT counter rejects the over-cap one.
            let line1 = cap / 2;
            for (edge_label, total) in [("at_cap", cap), ("over_cap", cap + 1)] {
                let line2 = total - line1 - inner.len();
                let mut s = Vec::new();
                s.extend_from_slice(&vec![b'a'; line1]);
                s.extend_from_slice(inner);
                s.extend_from_slice(&vec![b'b'; line2]);
                s.extend_from_slice(b"\n\n");
                corpus.push((format!("multiline_2/{inner_label}/{edge_label}"), s));

                // Same multi-line event but UNTERMINATED (no closing blank line); at EOF
                // the whole event is the final frame, charged in full.
                let mut s = Vec::new();
                s.extend_from_slice(&vec![b'a'; line1]);
                s.extend_from_slice(inner);
                s.extend_from_slice(&vec![b'b'; line2]);
                corpus.push((
                    format!("multiline_2_unterminated/{inner_label}/{edge_label}"),
                    s,
                ));
            }
        }

        // A THREE-line event with MIXED internal EOL separators (`\n` then `\r\n`), so the
        // event's size includes BOTH internal terminators. Sized to EXACTLY cap (ACCEPT)
        // and ONE OVER (REJECT). The three lines are each ~cap/3 — far under the cap.
        for (edge_label, total) in [("at_cap", cap), ("over_cap", cap + 1)] {
            let l1 = cap / 3;
            let l2 = cap / 3;
            // total = l1 + 1 (LF) + l2 + 2 (CRLF) + l3  =>  l3 = total - l1 - l2 - 3.
            let l3 = total - l1 - l2 - 3;
            let mut s = Vec::new();
            s.extend_from_slice(&vec![b'a'; l1]);
            s.extend_from_slice(b"\n");
            s.extend_from_slice(&vec![b'b'; l2]);
            s.extend_from_slice(b"\r\n");
            s.extend_from_slice(&vec![b'c'; l3]);
            s.extend_from_slice(b"\n\n");
            corpus.push((format!("multiline_3_mixed_inner/{edge_label}"), s));
        }

        // Multi-line events COMBINED with leading / extra / trailing BLANK lines, so the
        // event's internal single EOLs are counted INTO it while the surrounding blank
        // lines are charged to NO frame. Each event is sized to EXACTLY cap (ACCEPT) and
        // ONE OVER (REJECT). `lead` is a leading empty-event run; `mid` is an extra blank
        // line between the head frame and the multi-line event.
        for &(ctx_label, lead, mid) in &[
            ("leading_blank", b"\n".as_slice(), b"\n\n".as_slice()),
            ("leading_run", b"\n\n\n".as_slice(), b"\n\n".as_slice()),
            ("extra_blank_before", b"".as_slice(), b"\n\n\n".as_slice()),
            (
                "mixed_eol_blanks",
                b"\r\n".as_slice(),
                b"\r\n\r\n".as_slice(),
            ),
        ] {
            let line1 = cap / 2;
            let inner: &[u8] = b"\n";
            for (edge_label, total) in [("at_cap", cap), ("over_cap", cap + 1)] {
                let line2 = total - line1 - inner.len();
                let mut s = Vec::new();
                s.extend_from_slice(lead); // leading empty event(s)
                s.extend_from_slice(b"data: head");
                s.extend_from_slice(mid); // extra blank line(s) before the multi-line event
                s.extend_from_slice(&vec![b'a'; line1]);
                s.extend_from_slice(inner); // internal single-EOL line terminator
                s.extend_from_slice(&vec![b'b'; line2]);
                s.extend_from_slice(b"\n\n"); // closing blank line
                corpus.push((format!("multiline_ctx/{ctx_label}/{edge_label}"), s));
            }
        }

        // CRITICAL leading-EOL positions: a SINGLE leading EOL (or an ODD-length run)
        // is NOT a complete blank-line boundary that the boundary scan absorbs from
        // index 0, so a round-5-style bug would charge it INTO the first frame. With a
        // frame whose content is EXACTLY cap, that off-by-one is the false reject. The
        // even/complete `seps` above hide it (they reset at index 0), so these single /
        // odd leading EOLs are what make the oracle gate catch the bug, not just assert
        // self-consistency. Each frame body is sized to cap and cap+1 to pin the edge.
        for lead in [
            b"\n".as_slice(),
            b"\r".as_slice(),
            b"\r\n".as_slice(),
            b"\n\n\n".as_slice(), // odd run: one extra leading EOL past a boundary
            b"\r\n\r\n\r".as_slice(), // boundary + a dangling leading CR
            b"\n\r".as_slice(),   // mixed single-blank-line worth, two EOLs
        ] {
            for body in [cap, cap + 1] {
                let mut s = Vec::new();
                s.extend_from_slice(lead);
                s.extend_from_slice(&vec![b'L'; body]);
                s.extend_from_slice(b"\n\n");
                corpus.push((format!("single_leading_eol/{lead:?}/body{body}"), s));
                // Also without a closing boundary (trailing unterminated at/over cap).
                let mut s = Vec::new();
                s.extend_from_slice(lead);
                s.extend_from_slice(&vec![b'M'; body]);
                corpus.push((
                    format!("single_leading_eol_unterminated/{lead:?}/body{body}"),
                    s,
                ));
            }
        }

        // A few pure-EOL and degenerate inputs (no frames, or a single frame).
        corpus.push(("only_eols_lf".into(), b"\n\n\n\n".to_vec()));
        corpus.push(("only_eols_mixed".into(), b"\r\n\r\r\n".to_vec()));
        corpus.push(("empty".into(), Vec::new()));
        corpus.push(("single_at_cap_unterminated".into(), vec![b'g'; cap]));
        corpus.push(("single_over_cap_unterminated".into(), vec![b'h'; cap + 1]));

        let mut rng: u64 = 0xcafe_f00d_1234_5678;
        let mut next_rand = move |bound: usize| -> usize {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            if bound == 0 {
                0
            } else {
                ((rng >> 33) as usize) % bound
            }
        };

        for (label, data) in &corpus {
            // THE ABSOLUTE TRUTH: the independent whole-buffer reference oracle.
            let oracle = oracle_accepts(data, cap);

            // The streaming guard, finalized, must MATCH the oracle for every framing.
            // (1) whole + EOF.
            assert_eq!(
                verdict_for_split_eof(data, cap, &[data.len()], true),
                oracle,
                "[{label}] guard whole+EOF verdict disagrees with reference oracle"
            );
            // (2) byte-by-byte + EOF (maximal fragmentation: splits every EOL run).
            let ones = vec![1usize; data.len()];
            assert_eq!(
                verdict_for_split_eof(data, cap, &ones, true),
                oracle,
                "[{label}] guard byte-by-byte+EOF disagrees with reference oracle"
            );
            // (3) EVERY single-byte cut (two chunks) + EOF.
            for cut in 1..data.len() {
                assert_eq!(
                    verdict_for_split_eof(data, cap, &[cut, data.len() - cut], true),
                    oracle,
                    "[{label}] guard two-chunk split at offset {cut} + EOF disagrees with oracle"
                );
            }
            // (4) seeded-random multi-chunk groupings + EOF.
            for trial in 0..16 {
                let mut splits = Vec::new();
                let mut remaining = data.len();
                while remaining > 0 {
                    let size = (1 + next_rand(remaining.min(9))).min(remaining);
                    splits.push(size);
                    remaining -= size;
                }
                assert_eq!(
                    verdict_for_split_eof(data, cap, &splits, true),
                    oracle,
                    "[{label}] guard random grouping #{trial} {splits:?} + EOF disagrees with oracle"
                );
            }
        }
    }
}
