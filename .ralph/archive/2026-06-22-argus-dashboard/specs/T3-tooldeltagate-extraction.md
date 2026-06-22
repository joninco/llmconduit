# T3 — Extract ToolDeltaGate from run_turn

> **Source:** thermo-nuclear review (G4 HIGH #7). See `/tmp/thermo-synthesis.md`.

**Priority:** HIGH · **Surface:** engine stream loop · **Thermo findings:** G4 HIGH (run_turn bloat)

## Purpose
The core stream loop in `run_turn` (`engine.rs:1277`) now contains a bespoke `analyzeImage`
delta-buffer state machine plus duplicated monitor/SSE emission paths. This makes `run_turn`
harder to reason about and easier to regress. Extract the server-tool delta handling into a small
`ToolDeltaGate` / server-tool stream filter with unit tests, so the loop only dispatches filtered
emissions.

## Jobs to Be Done
- `run_turn`'s loop body dispatches pre-filtered emissions; the `analyzeImage` delta-buffering +
  monitor emission live behind the gate.

## Acceptance criteria
- [ ] A `ToolDeltaGate` (or equivalent) owns the `analyzeImage` delta-buffer state machine + the
      duplicated monitor/SSE emission paths, with focused unit tests.
- [ ] `run_turn`'s loop body calls the gate; the loop no longer inlines the delta buffering.
- [ ] G4 image-agent suite (47 behaviors) stays green unchanged.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Extends:** `engine.rs::run_turn`, G4 server-tool dispatcher.
- **New APIs:** `ToolDeltaGate` (or `ServerToolStreamFilter`).

## Constraints
- `image_analysis_rounds` hard ceiling stays SEPARATE from `WEB_SEARCH_ROUNDS_HARD_CEILING`
  (AGENTS.md: do not change the latter).
- Preserve streaming cancellation + failover-pre-first-chunk semantics.

## Out of scope
- Vision module split (T4). Routing-candidate plan (T2).

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
