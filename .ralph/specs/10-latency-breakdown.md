# 10 — Per-flow latency breakdown 🔭⚙️⭐

> Ralph gap spec — implementation-free. FEATURES.md **item 5**. Frontend. **Depends on spec 02** (03 enriches it).

## Operator question
"Was it slow at the provider (prefill/TTFT) or just a long generation?"

## Current state (verified by code search)
- Spec 02 adds `first_content_delta_ms` (true TTFT to client) + the phase timestamps; spec 03 adds per-attempt `first_upstream_byte_ms`.
- Monitor `output` segments (`timestamp_ms`) give the honest **derived fallback** until the spine fields are populated for a flow.

## Scope — what to build
- A "Timing" line in the inspector header + a slice of the spine phase waterfall.
- **True TTFT** from `first_content_delta_ms` (`measured`). Until present for a flow, a `derived` **"first-visible-activity latency"** (first monitor `output` segment `timestamp_ms` − `started_ms`), **labelled as such** — dashboard-visible activity, not upstream first byte.
- Stream `tok/s` = `derived` (completion ÷ stream duration).

## Data quality (bake into acceptance)
- TTFT `measured` (spec 02) or `derived`-fallback (explicitly labelled); `tok/s` `derived`.

## Acceptance criteria
- [ ] Inspector Timing line + a phase-waterfall slice from the spine timestamps.
- [ ] `first_content_delta_ms` present → label **"measured"**; absent → the **derived first-visible-activity** fallback, explicitly labelled (never presented as upstream first byte).
- [ ] Stream `tok/s` `derived`; when stream duration is unavailable → `—`.
- [ ] **don't-lie-with-zeros**: no TTFT/`tok/s` rendered as `0` when unmeasured — render `—`.
- [ ] Inter-token cadence is **not** claimed here (segments are batched) — deferred to streaming-stall health (later program).

## Constraints / invariants
- Frontend only; consume spine + monitor segments. Label `measured` vs `derived` distinctly in the UI.

## Out of scope
- Backend timestamp emission (spec 02/03); streaming-stall / inter-token health (later program).

## Validation gate
- **Frontend:** `npm run typecheck` (`tsc -b`) · `npm run lint` (`eslint . --max-warnings 0`) · `npm run test` (`vitest run`) · `npm run e2e` (Playwright): waterfall renders + the measured/derived TTFT label switches correctly.
- Then **REVIEW_PROTOCOL.md**: Codex-xhigh review before the next gap.
