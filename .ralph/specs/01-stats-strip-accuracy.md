# 01 — Stats-strip accuracy 🐞⭐

> Ralph gap spec — Argus dashboard phase 2. Implementation-free: **what + acceptance**, not code.
> FEATURES.md build-order **item 1 (foundation)**. Sequence: **FIRST** — everything reads off the strip.
> Cross-cutting bug: may resolve on the backend (`metrics.rs`) or frontend (`StatsStrip`) — both gates apply.

## Operator question
"The headline gauges read `0.0` with fresh real traffic in the table (observed on the live vLLM run) — why, and how do we make them honest?"

## Current state (verified by code search)
- `src/metrics.rs:798` — `record_terminal()`/`record_response()` is fed **only at the D3 terminal finalize CAS choke point** ("NOT the middleware, NOT per-chunk"). **Live/in-flight flows never contribute.**
- `src/metrics.rs:856-858` — `view_with_seq()` returns `(MetricsView::default(), 0)` before any terminal flow.
- `src/dashboard_api.rs:374-408` — `rest_window_tile()` returns `0.0` when `total_count == 0`.
- `src/metrics.rs:317` — window floor `now.saturating_sub(len-1)`; `:321` aggregate filters slots in `[floor, now]` (boundary off-by-one suspect).
- **REST vs WS mismatch (Codex review):** `active_streams` is the live open count via REST (`dashboard_api.rs:751`), BUT the live **WS** path `dashboard_ws.rs:665 window_tile()` **hard-codes `active_streams: 0` + `cost_per_min: 0.0`** (comment `:662`: these D5/D13 roll-ups are "not yet wired (0.0)"). The strip folds WS samples after the first tick → these read `0` live. A REST-only fix leaves the live WS zeros intact.
- Frontend trusts backend verbatim — `StatsStrip.tsx:46`, `chips.ts:75-84` (no client recompute); seek-freeze `useMetricStream.ts:51-58`; `deriveChips()` `chips.ts:93` → `"—"` when `cur` is null.

## Scope — what to do (investigation-first; do NOT hard-code the remedy)
1. **Reproduce** the `0.0` condition against live, still-streaming traffic; record which chips are `0` vs `—` vs populated.
2. **Root-cause** it. Verified suspects, broaden the hunt: completed-only feed (prime); **WS `window_tile()` hard-coded `active_streams`/`cost_per_min` zeros (`dashboard_ws.rs:665`)**; window-floor off-by-one (`metrics.rs:317`); frozen time-travel seek staleness; stale-WS / REST-seed `metrics_seq` dedup race; server/client clock skew; unit conversion.
3. **Fix** so the strip reflects reality during a live vLLM run, and document the root cause in the commit message + IMPLEMENTATION_PLAN.

## Data quality (bake into acceptance)
- A window with **zero completed-flow samples** must render `p50/p95/p99`, `tok/s`, `$/min` as **`unavailable` (`—`)**, NOT `0.0`.
- **Distinguish** genuine zero-traffic (legit `req/s = 0`) from "traffic in flight, none finalized" (samples `unavailable`).

## Acceptance criteria
- [ ] Repro captured: which chips read `0` vs `—` vs real, under live streaming traffic.
- [ ] Root cause identified and written into the commit msg / IMPLEMENTATION_PLAN (one of the verified suspects, or a new one).
- [ ] Strip is honest during a live vLLM run (no all-`0.0` while real traffic streams + finalizes).
- [ ] **Both** metric paths fixed: the REST seed AND the live **WS `window_tile()`** tick (`dashboard_ws.rs:665`) — `active_streams` + `cost_per_min` carry real values (or render `unavailable`), never hard-coded `0.0`. A REST-only fix that leaves the WS tile zeroed does NOT satisfy this gap.
- [ ] **don't-lie-with-zeros**: zero-*sample* latency/tok-s/cost windows render `—`; zero-*traffic* `req/s` renders `0` — and they are distinguishable.
- [ ] No regression to time-travel seek coherence (frozen-cut) or the per-domain `{domain, seq}` cursors (AGENTS.md — no global watermark).
- [ ] If the fix counts live flows: the single-CAS-choke-point finalize must stay **idempotent** (a flow never double-counts).

## Constraints / invariants (AGENTS.md)
- Per-domain `{domain, seq}` cursors only — never a global `seq` watermark.
- `MonitorHub::disabled()` = zero overhead when `--with-debug-ui` is off — don't add unconditional broadcast sends.

## Out of scope
- New gauges; the **per-provider** breakdown (specs 12/13).

## Validation gate
- **Backend:** `cargo test` (metrics) · `cargo clippy --all-targets` · `cargo fmt`.
- **Frontend:** `npm run typecheck` (`tsc -b`) · `npm run lint` (`eslint . --max-warnings 0`) · `npm run test` (`vitest run`, chips/derive) · `npm run e2e` (Playwright): strip non-zero after seeded completed flows **and** renders `—` on an empty window.
- Then **REVIEW_PROTOCOL.md**: Codex-xhigh review of the commit before the next gap.
