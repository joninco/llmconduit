# T6 — Extract SSE guard module + shrink port_streaming.rs

> **Source:** thermo-nuclear review (G6 MEDIUM #9, #23, #24). See `/tmp/thermo-synthesis.md`.

**Priority:** MEDIUM · **Surface:** src/upstream.rs + tests · **Thermo findings:** G6 MEDIUM (SSE guard in wrong module), G6 MEDIUM (SseFrameGuard public for white-box tests), G6 MEDIUM (port_streaming.rs 1,432-line review-history suite)

## Purpose
G6 added a full SSE grammar state machine into the already-monolithic `src/upstream.rs` (4,497
lines). `SseFrameGuard` is public mainly so `tests/port_streaming.rs:29` can white-box internals,
leaking a private DoS mechanism into the crate API. The new 1,432-line `port_streaming.rs` is a
review-history/oracle suite for a four-bullet acceptance spec, duplicating low-level unit coverage
and carrying "Codex round" archaeology in test prose. Extract the guard, tighten visibility, and
shrink the test file.

## Jobs to Be Done
- The SSE guard is a focused module; `upstream.rs` keeps only the call-site wiring.
- `SseFrameGuard` is `pub(crate)`; scanner edge cases are module unit tests, integration tests cover
  real `ReqwestUpstreamClient` acceptance.

## Acceptance criteria
- [ ] SSE grammar state machine + `SseFrameGuard` move to `src/sse_guard.rs` (or `src/upstream/sse.rs`);
      `upstream.rs` keeps the call-site wiring only.
- [ ] `SseFrameGuard` (and its internal scanner) becomes `pub(crate)`; the white-box tests move into
      the guard module's unit tests.
- [ ] `tests/port_streaming.rs` shrinks to the required end-to-end acceptance cases; table-driven
      scanner/property tests move beside the guard module; "Codex round" archaeology removed from
      test prose.
- [ ] No behavior change — G6 acceptance (EOL grammar, EOF finalize, oversized rejection, keep-alive)
      stays green.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Extends:** `src/upstream.rs`, `tests/port_streaming.rs`.
- **New files:** `src/sse_guard.rs` (or `src/upstream/sse.rs`).
- Composes with T5 (Bytes specialization) — coordinate so T5's `Bytes` work lands in the new module
  if both are done.

## Constraints
- Pure structural + visibility + test-relocation change — NO behavior change.
- Preserve `DEFAULT_MAX_SSE_FRAME_BYTES` single-source (commit `07117b2`).

## Out of scope
- Bytes specialization (T5 — but coordinate placement).

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
