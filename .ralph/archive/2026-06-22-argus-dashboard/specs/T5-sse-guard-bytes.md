# T5 — Bytes-specialized SSE guard (cap before copy)

> **Source:** thermo-nuclear review (G6 HIGH #8). See `/tmp/thermo-synthesis.md`.

**Priority:** HIGH · **Surface:** src/upstream.rs (SSE guard) · **Thermo findings:** G6 HIGH (unbounded chunk allocation inside DoS guard)

## Purpose
The G6 SSE guard copies the full upstream chunk before enforcing the per-frame cap, then
`scan_frames_since_boundary` copies it again (`upstream.rs:2474`, `upstream.rs:2636`). An oversized
frame delivered as one large body chunk still creates O(chunk) memory outside `eventsource()` before
rejection, weakening the G6 DoS bound. Specialize the adapter to `Bytes`, scan borrowed bytes
before yielding the same chunk, and retain only the ≤3 byte carry.

## Jobs to Be Done
- An oversized SSE frame is rejected without first materializing its full body in a guard-owned buffer.

## Acceptance criteria
- [ ] The bounded stream adapter specializes to `Bytes` (not `Vec<u8>` / `BytesMut` copies).
- [ ] `scan_frames_since_boundary` scans borrowed bytes and retains only the ≤3-byte carry across
      chunks; it does not clone the full chunk.
- [ ] A single oversized body chunk larger than `max_sse_frame_bytes` is rejected with the G6 error
      WITHOUT allocating a full-copy buffer of that size (verified by a test asserting the guard's
      peak buffered bytes stay bounded, e.g. via a tracing/counter seam or a property test).
- [ ] Existing G6 acceptance tests (EOL grammar, EOF finalize, oversized rejection) stay green.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Extends:** `src/upstream.rs` `bounded_sse_byte_stream` / `SseFrameGuard` / `scan_frames_since_boundary`.
- May compose with T6 (module extraction) — if T6 lands first, apply this in the new module.

## Constraints
- Do not weaken the EOL-grammar correctness or EOF-finalize behavior.
- `DEFAULT_MAX_SSE_FRAME_BYTES` (added in commit `07117b2`) stays the single source of the default.

## Out of scope
- Module extraction (T6). Test shrink (T6).

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
