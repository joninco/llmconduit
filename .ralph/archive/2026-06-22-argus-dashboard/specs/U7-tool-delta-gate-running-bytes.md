# U7 — ToolDeltaGate per-call cap: O(1) running byte count (kill O(n^2) re-sum)

> **Source:** thermo-nuclear PROJECT review 2026-06-20 (Topic 12). See /tmp/thermo-project-review.md

**Priority:** MEDIUM · **Surface:** `src/tool_delta_gate.rs` · **Thermo finding:** the `Pending`/`None` buffering arm re-sums the entire buffer on every nameless delta, making a single pending call O(n^2) in delta count (bounded DoS).

## Purpose
Inside the accepted Topic-11 `ToolDeltaGate` refactor, the name-still-unknown buffering arm enforces the per-call cap by calling `buffered_len(buffered)` — an O(n) `.iter().map(...).sum()` over EVERY fragment already buffered for this call (`src/tool_delta_gate.rs:229`, helper at `:54-56`) — on EVERY incoming nameless delta. That makes buffering a single pending call O(n^2) in its delta count: with 1-byte fragments the 256 KiB per-call cap (`MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL`, `:33`) permits up to 262,144 buffered deltas, so the cap check alone performs ~34.36 billion fragment-length visits before it trips. This is a bounded DoS (CPU burn, not unbounded memory — the byte caps already hold) gated behind `vision_active` plus an operator-configured backend, hence MEDIUM. The byte total `self.pending_buffer_bytes` (`:113`) is ALREADY maintained as an O(1) running counter for the cross-call total cap (`:230`, `:235`); the per-call cap is the only check still paying for an O(n) re-sum. FIX: carry a running `bytes: usize` inside the `Pending` variant (`:44-47`) so the per-call comparison at `:229` becomes `bytes + delta_bytes > MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL` (O(1)), incrementing `bytes` alongside the existing `buffered.push(...)` (`:234-235`). The three terminal `buffered_len(...)` sites that run exactly once per call when a buffer leaves the pending pool (`:180-182` drop, `:212-214` in-stream client flush, `:256-258` turn-end flush) are O(1)-amortized and stay as-is.

## Jobs to Be Done
- Per-delta work in the name-unknown buffering arm is O(1): no full-buffer re-sum on the hot path.
- The per-call byte cap trips at the EXACT same boundary as today (256 KiB), byte-for-byte.
- The running per-call `bytes` is the single source of truth the per-call cap reads; it stays consistent with what later flush/drop sites subtract from `pending_buffer_bytes`.
- Empty-string deltas remain no-ops (push a zero-byte fragment, increment by 0) — no behavior change.

## Acceptance criteria
- [ ] The `Pending` variant carries a running `bytes: usize` field; it is initialized to `0` at the single construction site (`:162-164`) and equals the sum of its buffered fragment lengths after every push (invariant: `bytes == buffered_len(buffered)`).
- [ ] The per-call cap check in the `(AnalyzeDeltaState::Pending { .. }, None)` arm (`:227-237`) compares the running `bytes + delta_bytes` against `MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL` and performs NO call to `buffered_len` (no O(n) re-sum on the per-delta path).
- [ ] `bytes` is incremented by `delta_bytes` at the same point as the existing `buffered.push((name, delta))` (`:234-235`), AFTER the cap check passes, so a delta that overflows is neither pushed nor counted.
- [ ] The cross-call total cap check still uses the O(1) `self.pending_buffer_bytes` counter and trips at the same `MAX_PENDING_TOOL_DELTA_BYTES_TOTAL` boundary; `pending_buffer_bytes` bookkeeping (increment on push, `saturating_sub` on drop/flush) is unchanged on the wire and in value.
- [ ] The three once-per-call terminal subtractions remain correct: the `Some(true)` drop arm (`:178-186`), the in-stream `Some(false)` client-flush arm (`:200-222`), and `flush_pending_client_tool` (`:253-267`) subtract the buffer's full byte count from `pending_buffer_bytes` exactly as before (read from the running `bytes` field or via `buffered_len`; either is acceptable as it runs once). `flush_pending_client_tool`'s `!buffered.is_empty()` guard is preserved.
- [ ] All ten existing tests stay green UNCHANGED, in particular: `per_call_pending_byte_cap_overflows` (`:474-483`), `total_pending_byte_cap_overflows_across_calls` (`:485-503`), `dropping_analyze_image_reclaims_full_total_budget` (`:505-545`), and `flushing_client_tool_reclaims_its_pending_budget` (`:547-577`) — the cap boundaries and reclamation behavior are byte-identical.
- [ ] ADD a test proving the per-call cap boundary is unchanged with the running counter: buffering exactly `MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL` bytes via MANY small nameless deltas (e.g. 1-byte fragments) on one call_id all return `DeltaDecision::None`, and the very next 1-byte nameless delta on that same call returns `Err(PendingBufferOverflow)` — exercising the running-sum accumulation path rather than the single-huge-delta path the existing `per_call_pending_byte_cap_overflows` covers.
- [ ] ADD a test (or assertion) that empty-string nameless deltas remain no-ops and do not move the per-call cap: an arbitrary number of `""` deltas on a pending call_id all return `DeltaDecision::None` and a subsequent full `MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL`-byte chunk still buffers (not overflows), proving zero-byte fragments add `0` to the running `bytes`.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Extends:** `src/tool_delta_gate.rs` — the `AnalyzeDeltaState::Pending` variant and the `on_delta` Pending/None arm only.
- **New APIs:** none. `ToolDeltaGate::on_delta` / `flush_pending_client_tool` signatures, `DeltaDecision`, `DeltaEmission`, and the public byte-cap behavior are all unchanged.
- **Depends on:** T3 (`ToolDeltaGate` extraction) — FINAL. No engine-side change: `src/engine.rs:1618` and `:1709` callers are untouched (the `Pending` variant is constructed only inside this module at `:162`).

## Constraints
- The per-call cap boundary (`MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL = 256 * 1024`) and the total cap (`MAX_PENDING_TOOL_DELTA_BYTES_TOTAL = 1024 * 1024`) values and trip points stay byte-identical — this is a complexity fix, NOT a cap change.
- `MonitorHub::disabled()` zero-overhead no-op and redaction are not touched (this is a pure decision machine — it never reaches the hub or SSE; preserve that).
- The hot path stays allocation-free: `None`/`One` never heap-allocate and `Flush` MOVES the buffer out (no copy). Adding a `usize` field to `Pending` introduces no allocation.
- `vision_active=false` remains a pure pass-through (fast path at `:141-147` returns `One` unchanged) — this fix only touches the active buffering arm.
- The invariant that `pending_buffer_bytes` equals the sum of all `Pending` buffers' running `bytes` must hold after every operation (so total-cap and reclamation stay exact).
- COMPILE REQUIREMENT: adding `bytes` to the struct-variant `AnalyzeDeltaState::Pending` forces every match/destructure of it (`:179`, `:202`, `:227`, `:255`) to bind or elide the new field (`{ buffered, bytes }` or `{ buffered, .. }`); these edits are mechanical and must not change behavior. Keep the O(1) running-byte design unchanged.

## Out of scope
- Changing either DoS cap value, or the total-cap algorithm (already O(1)).
- The `buffered_len` helper may stay (still used by the once-per-call terminal sites) or be removed if those sites read the running `bytes` field instead — implementer's choice, but do NOT regress the terminal subtraction correctness.
- Any engine-side or wire-format change; any change to drop/flush ordering or `DeltaDecision` shape.
- The G8 `emit_thinking` toggle (absent by design) and G5 `.jsonl` exclusion — untouched.

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
