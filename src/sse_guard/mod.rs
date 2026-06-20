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
    /// that began at a completed frame boundary. After a blank line (boundary =
    /// two EOLs) any ADDITIONAL consecutive EOLs are extra empty lines that
    /// `eventsource-stream` dispatches as empty events / skips, so they belong to
    /// NO data frame and must be charged to neither. When this is true, a leading
    /// EOL on the next chunk continues that empty-line run (consumed, uncharged)
    /// rather than being charged into the next frame; the `carry` then holds the
    /// run's trailing partial EOL (a lone `\r` whose CR/CRLF nature is still
    /// ambiguous) instead of a boundary-prefix. Cleared the moment a non-EOL byte
    /// (real frame content) ends the run.
    in_eol_run: bool,
}

impl SseFrameGuard {
    /// Build a guard with the given per-frame ceiling (floored at 1 KiB so a
    /// misconfigured tiny cap cannot reject every normal frame).
    ///
    /// `in_eol_run` starts TRUE: the stream begins AS IF immediately after a frame
    /// boundary, so any LEADING EOLs (an empty/blank-line SSE event, or stray
    /// separators before the first `data:`) are an empty-line run charged to NO
    /// frame — exactly like extra blank lines BETWEEN frames. (Starting it false
    /// would charge a leading EOL into the first real frame, falsely rejecting a
    /// frame otherwise exactly at cap.) A stream that opens directly on real
    /// content ends the (zero-length) leading run at byte 0, so this is a no-op for
    /// the common case.
    pub fn new(max_frame_bytes: usize) -> Self {
        Self {
            max_frame_bytes: max_frame_bytes.max(1024),
            since_boundary: 0,
            carry: Vec::new(),
            in_eol_run: true,
        }
    }

    /// The effective (floored) per-frame cap this guard enforces. Test-only today
    /// (the production path threads the cap in via `new`); the `#[cfg(test)]` gate
    /// drops once production code reads the floor directly.
    #[cfg(test)]
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
    /// part of the still-open, unterminated frame and a final cap check is emitted:
    /// an unterminated frame must not slip past the cap by a trailing
    /// `\n`/`\r`/`\r\n`/`\r\n\r` just because EOF arrived before the carry was
    /// disambiguated. A trailing carry that is itself a complete boundary (`\n\r`,
    /// `\r\r`, `\r\n\r`, resolving the final CR at EOF) resets instead of being
    /// charged. Idempotent: after a successful call the carry is empty, so a second
    /// call is a no-op.
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
/// trailing separator byte could be disambiguated. This is why a plain `.map` is
/// insufficient — the adapter must be able to act on end-of-stream — so it is a
/// stateful `async_stream` that drives the guard and emits one trailing error
/// item when finalization trips the cap.
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
/// the caller charges none of them.
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
/// `\r\r` (2); `\n\r\n`, `\r\n\n`, `\r\n\r`, `\r\r\n` (3); `\r\n\r\n` (4) — every
/// mixed combo must be recognized, not just `\n\n`/`\r\r`/`\r\n\r\n`.
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
///     consecutive EOLs: those are extra empty lines that the parser dispatches as
///     empty events / skips, so they belong to no frame and resume scanning from
///     the end of the run with `since_boundary` still 0. A run that straddles the
///     chunk edge is finished on the next chunk via `in_eol_run` (a leading EOL
///     there continues it, uncharged) so it is never charged.
///   * After the last boundary/run, the trailing segment is split: when `!at_eof`,
///     its longest suffix that is a PROPER PREFIX of a boundary is deferred
///     uncharged into the new carry and the remainder is charged; when `at_eof`
///     there is no future byte to disambiguate, so the entire trailing segment is
///     charged (a dangling single EOL is part of the still-open, unterminated
///     frame) — UNLESS we are still inside an EOL run, in which case a trailing EOL
///     is one more empty line and stays uncharged. (An unterminated *frame* must be
///     charged at EOF, but an inter-frame empty line must not.)
///
/// Correctness properties:
///   * A trailing byte that merely STARTS a boundary is never charged until the
///     next chunk disambiguates it, so an ambiguous tail cannot trip the cap (and
///     the verdict does not depend on the chunk split).
///   * A deferred boundary-prefix carry that never completes is charged to the
///     unterminated frame at EOF.
///   * Extra/empty blank-line EOLs are charged to no frame, with the run consumed
///     even when split across a chunk edge (carry = run tail, `in_eol_run = true`).
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
        // are charged to no frame.
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
mod tests;
