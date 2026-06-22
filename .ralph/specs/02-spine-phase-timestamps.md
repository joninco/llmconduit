# 02 — Spine: per-phase timestamps + true TTFT 🔭⚙️⭐

> Ralph gap spec — implementation-free. FEATURES.md **item 2 (data-contract pass / the spine)**.
> Backend. Sequence: spine seam — **before any UI surface that depends on it** (specs 10, 16).

## Operator question
"Where did this request spend its time — and when did the client actually see the first content token?"

## Current state (verified by code search)
- `src/dashboard_flow.rs` `FlowRecord` has **only** `started_at: Instant`, `started_ms: u128`, `finished_ms`, `elapsed_ms` — **no intermediate phases**, no TTFT.
- Stamp points already exist: normalization at `engine.rs:1128` (`set_normalized()`); **first canonical content delta** at `engine.rs:2088` (`output_text_delta_event()` in `send_event()`); finalize at `dashboard_flow.rs:1053` (`TelemetryGuard::finalize()`).
- `SnapshotFlowSummary` is body-free and currently carries the same 4 timing fields only.

## Scope — what to build
- Add **measured** per-phase timestamps to `FlowRecord` **and** `SnapshotFlowSummary`: `ingress` (≈`started_ms`), `normalization_done_ms`, `routing_decision_ms`, `first_content_delta_ms`, `stream_end_ms`, `finalize_ms`. Each optional.
- `first_content_delta_ms` = first canonical **content** SSE delta to the client — **not** reasoning, tool-argument, or refusal deltas.

## Data quality (bake into acceptance)
- All `measured`. A phase that didn't happen (e.g. errored before content) → `None` → renders `—`.

## Acceptance criteria
- [ ] `FlowRecord` + `SnapshotFlowSummary` carry the phase timestamps as optional measured fields.
- [ ] `first_content_delta_ms` stamps on the **first content delta only** — a stream with reasoning/tool deltas before content does **not** stamp it early (assert via a streaming test).
- [ ] A flow that errors before any content delta → `first_content_delta_ms = None`.
- [ ] Where present, phases are monotonic: `ingress ≤ normalization ≤ routing ≤ first_content_delta ≤ stream_end ≤ finalize`.
- [ ] **don't-lie-with-zeros**: an unmeasured phase is `None`, never `0`.
- [ ] **body-free**: phase fields are scalar metadata on the summary — no body retention (AGENTS.md snapshots-are-body-free invariant).
- [ ] Deserialize→serialize round-trip test proves the new fields survive.

## Constraints / invariants (AGENTS.md)
- Don't break streaming cancellation — `run_turn` selects on `tx.closed()`; preserve it.
- Don't double-stamp on replay; respect the per-record `record_seq` mutation cursor.

## Out of scope
- `first_upstream_byte_ms` + `attempts[]` (spec 03); the waterfall UI (spec 10).

## Validation gate
- **Backend:** `cargo test` (round-trip + streaming order) · `cargo clippy --all-targets` · `cargo fmt`.
- Then **REVIEW_PROTOCOL.md**: Codex-xhigh review before the next gap.
