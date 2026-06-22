# 12 — Per-provider latency + error distribution (backend) 🔭⚙️

> Ralph gap spec — implementation-free. FEATURES.md **item 7 (backend half)**. Backend. **Depends on spec 03.**

## Operator question
"Which upstream is degrading?" (the measured backbone for the UI in spec 13)

## Current state (verified by code search)
- Percentiles are **global only** — `metrics.rs:351` (`percentiles()` window-wide, no per-key breakdown).
- `ProviderHealth` is **point-in-time** (`upstream.rs:198`, single swapped `Arc`, no ring).
- The `CooldownTooltip` shows the **global** p99 (`dashboard_api.rs:380`).
- Spec 03 emits the attempt sequence into the **evict-safe terminal payload** (not just the evictable `FlowRecord`) — this is the per-provider metrics source; do NOT read `FlowStore` for metrics. (Codex review.)

## Scope — what to build
- A **per-provider** `MetricsLayer` ring: p50/p95/p99 + error rate **per upstream**, fed from the **evict-safe terminal attempt payload** (spec 03 — carried on the `ServingToken`/terminal finalize, like usage), **NOT** by re-reading the evictable `FlowStore`. A failed primary is counted (final-served latency alone hides unhealthy providers).
- Extend the D4 topology/health DTO with the per-provider percentiles + error rate.

## Data quality (bake into acceptance)
- `derived` percentiles per provider. A provider with **zero samples** in the window → `unavailable` (`—`), **not `0`** (mirrors spec 01).

## Acceptance criteria
- [ ] Per-provider ring keyed by upstream; fed from the **evict-safe terminal payload** (spec 03), not by reading the evictable `FlowStore`; failed attempts counted, not just the served one.
- [ ] A flow evicted before finalize still counts its failed-primary provider (evict-safety test).
- [ ] D4 DTO carries per-provider p50/p95/p99 + error rate.
- [ ] **don't-lie-with-zeros**: a zero-sample provider window → `unavailable`, not `0` (same rule as spec 01).
- [ ] The global metrics choke-point invariant and per-domain `{domain, seq}` cursors are undisturbed.
- [ ] Per-provider aggregation test, including a **failed-primary** counted toward its provider.

## Constraints / invariants (AGENTS.md)
- Per-domain `{domain, seq}` cursors only; don't add unconditional broadcast overhead when `--with-debug-ui` is off.

## Out of scope
- The tooltip/topology rendering (spec 13); provider health-history timeline (later program).

## Validation gate
- **Backend:** `cargo test` (per-provider aggregation + failed-primary) · `cargo clippy --all-targets` · `cargo fmt`.
- Then **REVIEW_PROTOCOL.md**: Codex-xhigh review before the next gap.
