# D3 — TelemetryGuard (L0/L1 CAS) + cumulative-aware usage emission

> **Source:** DASHBOARD_PLAN.md rev 8 §4.3, §4.4. Topic 13.

**Priority:** HIGH · **Surface:** `src/engine.rs` (`stream_responses`, usage loop), `src/monitor.rs`
(`MonitorEventKind::Usage`, wire, redact, snapshot), `src/http.rs` (L0 middleware guard)

## Purpose
Guarantee every flow is finalized exactly once with correct partial usage, surviving extractor
failures, pre-spawn validation, and midstream cancels. Fixes Codex blockers: only `ApiCallId` crosses
`next.run` (so a separate engine guard cannot "disarm" the middleware one by reference — use a CAS
state machine in the record); pre-spawn failures (engine.rs:809) and midstream `next_upstream_chunk…?`
skipped the post-loop `into_response_usage` (engine.rs:1805), losing partial usage; OpenAI usage is
CUMULATIVE so `add`-every-chunk double-counts and `snapshot()` before `add()` misses the current turn.

## Jobs to Be Done
- Record carries `claim: Arc<AtomicU8>` with states `OpenL0 → ClaimedL1 → Finalized` (D1 allocates it).
- **L0 (middleware, RAII):** `flow_store.open` returns a `MiddlewareGuard` holding `api_call_id`. Its
  `Drop` runs `compare_exchange(OpenL0 → Finalized-Failed("unhandled"))` — finalizes ONLY if still
  `OpenL0` (extractor/conversion failure, `next.run` errored before handler). Race-free via CAS.
- **L1 (engine):** at the top of `stream_responses` the `TelemetryGuard` does
  `compare_exchange(OpenL0 → ClaimedL1)`; on success it owns finalization on every exit path
  (pre-spawn engine.rs:809/:1383, spawned engine.rs:817, Completed/Failed) → `ClaimedL1 → Finalized`
  with `status, serving provider (from D2 token), elapsed = started.elapsed()` (private `Instant` for
  monotonic latency, NOT epoch-ms). RAII `Drop` fallback finalizes only if still `ClaimedL1`.
- `/v1/completions` is NOT whitelisted (D1) → never opens a record → no orphan, no instrumentation.
- **Cumulative-aware usage:** `turn_base = accumulated_usage.snapshot()` at turn start; on each
  usage-bearing chunk (engine.rs:1513 region, `chunk.usage.is_some()`) upsert
  `total = turn_base + chunk.usage` (turn-local cumulative — no double-count); after the loop
  (engine.rs:1676) the single authoritative `accumulated_usage.add(turn_usage)` advances `turn_base`
  for the next turn. A midstream cancel keeps the last upserted total. No usage chunk → `usage = None`.
- `MonitorEventKind::Usage { prompt, completion, total, cached, reasoning }` emitted via `emit_with`
  at engine.rs:1805 + a new `DebugWsMessage::Usage` wire variant + `apply_event` arm (stores on record)
  + `snapshot()` replay + no-op redact arm (monitor.rs:295). Usage also written to FlowStore + MetricsLayer.

## Acceptance criteria
- [ ] `claim: Arc<AtomicU8>` `OpenL0/ClaimedL1/Finalized` state machine; L0 `Drop`
      `compare_exchange(OpenL0→Finalized)`; L1 `compare_exchange(OpenL0→ClaimedL1)` before finalizing.
- [ ] Extractor-failure test (axum rejects `Json`): record left `OpenL0`, L0 `Drop` finalizes
      `Failed("unhandled")` — no orphan; no `ClaimedL1` transition occurs.
- [ ] Pre-spawn budget-failure (engine.rs:809) + midstream-cancel test: L1 finalizes with the last
      upserted cumulative usage (NOT zero/previous-round); latency monotonic (`Instant`).
- [ ] Cumulative-usage test: a stream emitting usage chunk(s) upserts `turn_base + chunk.usage` (no
      double-count), the single post-loop `add` advances `turn_base`; midstream cancel after a usage
      chunk retains the last total; "no usage chunk" path leaves `usage = None`.
- [ ] `MonitorEventKind::Usage` added (monitor.rs:15); `DebugWsMessage::Usage` (monitor.rs:98) with
      `apply_event`/`snapshot()` replay/no-op redact-match arm; a test asserts usage reaches
      `snapshot()` + `subscribe()`.
- [ ] `/v1/completions` opens no record (cross-check with D1 whitelist test).
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Depends on:** D1 (`claim` field, `flow_store.open/finalize/record_usage`), D2 (serving token read
  at finalize).
- **Extends:** `src/engine.rs::stream_responses` (guard at top, usage in loop), `src/monitor.rs` enum +
  wire + snapshot + redact, `src/http.rs` L0 guard.
- **New APIs:** `TelemetryGuard`, `MiddlewareGuard`, `MonitorEventKind::Usage`, `DebugWsMessage::Usage`.
- **Send across spawn:** the L1 guard/handle moves into the `tokio::spawn` at engine.rs:817 — holds only
  `Arc`s + `Instant` (all `Send`); verified by the midstream-cancel test compiling.

## Constraints
- The CAS is the ONLY ownership-transfer mechanism (no object disarm across `next.run`).
- `into_response_usage()` at engine.rs:1805 stays the final total for the CLIENT response (unchanged);
  the incremental upsert is purely additive for the dashboard/MetricsLayer.
- Don't double-count: store the running cumulative total per upsert, never an increment.
- Preserve all existing `tx.is_closed()` cancellation points (AGENTS.md "Don'ts"); the cancel token
  check (D6) composes with these, not replaces.

## Out of scope
- MetricsLayer ring math (D5); D3 calls `record_usage`/`record_response`, D5 implements.
- AbortHub kill cancellation wiring (D6).
- Frontend rendering of usage (Sankey/theater).

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
