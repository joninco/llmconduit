# 03 — Spine: `attempts[]` trace 🔭⚙️

> Ralph gap spec — implementation-free. FEATURES.md **item 2 (data-contract pass / the spine)**.
> Backend. Sequence: spine seam — before specs 11 (UI) and 12 (per-provider metrics).

## Operator question
"Which provider failed, why, how long did we wait, and what eventually served?"

## Current state (verified by code search)
- Failover loop `src/upstream.rs:1409-1467` (`stream_chat_completion_with_provider_indices`); `mark_provider_failure` `:1338-1378` records `(provider, cooling_until, error)` and `mark_provider_success` `:1320-1336` — but **into `ProviderHealth` counters, NOT into the flow.**
- `FlowRecord` keeps only the final `upstream_target`. **No `attempts[]`.**
- First-upstream-byte stamp point exists: `prefetch_first_chunk` `upstream.rs:1260` / `engine.rs:2010` (`next_upstream_chunk()`).
- **Metrics are recorded evict-safely** at the D3 terminal finalize seam via `ServingToken`/`TelemetryGuard` (`metrics.rs:798`), **not** by reading the evictable `FlowStore`. So attempt data destined for per-provider metrics (spec 12) must ride the terminal payload, not only the flow record. (Codex review.)

## Scope — what to build
- Add a **measured** `attempts[]` to `FlowRecord` **and** `SnapshotFlowSummary`. Each attempt: `provider`, `model`, `start_ms`, `end_ms`, `first_upstream_byte_ms`, `status`, `error_class`, `failover_reason`.
- **`error_class` / `failover_reason` are BOUNDED, sanitized, taxonomic values** (a short enum/code), NOT raw upstream error text — they ride the body-free `SnapshotFlowSummary`, so they must not become a backdoor for raw upstream error bodies (those stay behind spec 05's separate gated seam). (Codex R2.)
- Thread the per-attempt data the failover loop already computes into the flow record.
- **Evict-safe for metrics:** the per-attempt data must ALSO reach the terminal metrics path (carried on the `ServingToken`/terminal finalize payload, the way usage is), so spec 12 can read per-provider metrics without re-reading the evictable `FlowStore`. (Codex review.)
- Single success → **exactly 1** attempt; failover → **≥2** (failed ones + the served one).

## Data quality (bake into acceptance)
- `measured`. `first_upstream_byte_ms = None` when an attempt failed before response headers; `error_class = None` on the served attempt.

## Acceptance criteria
- [ ] `attempts[]` on `FlowRecord` + `SnapshotFlowSummary`; a non-failover flow has `len == 1`.
- [ ] A forced failover (wiremock `503` then `200`) yields `len ≥ 2`: first marked failed with `failover_reason`, last served.
- [ ] `first_upstream_byte_ms` measured at the prefetch point; `None` when the attempt never received response headers (renders `—`).
- [ ] **Mid-stream** provider failure does **not** append a new attempt — it terminates the serving attempt as error (failover-pre-first-chunk invariant, `upstream.rs:407-419`).
- [ ] **Routing mode**: only the selected upstream's nested `fallback_upstreams` appear as attempts — never a sibling routing upstream (AGENTS.md hard rule).
- [ ] **don't-lie-with-zeros**: any unmeasured per-attempt time is `None`, never `0`.
- [ ] Per-attempt data reaches the **evict-safe terminal payload** (not only `FlowRecord`): a test where the flow record is **evicted before finalize** shows the terminal payload still carries ALL attempts. (Per-provider aggregation of those attempts is asserted in spec 12, not here.)
- [ ] **body-free** on the summary; deserialize→serialize round-trip test.
- [ ] `error_class`/`failover_reason` are capped, sanitized taxonomic codes — no raw upstream error text on the body-free summary (raw bodies remain spec-05-gated).

## Constraints / invariants (AGENTS.md)
- Failover only pre-first-chunk; routing providers are **not** failure fallbacks; `parallel_tool_calls: false` unaffected.
- `error_class`/`failover_reason` capped + sanitized (taxonomic, not raw upstream text); the body-free summary stays body-free; the raw upstream error body remains spec-05-gated.

## Out of scope
- Per-provider aggregate percentiles (spec 12); the inspector stepper UI (spec 11).

## Validation gate
- **Backend:** `cargo test` (single + wiremock failover + routing) · `cargo clippy --all-targets` · `cargo fmt`.
- Then **REVIEW_PROTOCOL.md**: Codex-xhigh review before the next gap.
