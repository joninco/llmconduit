# 08 — Token economics: cached · reasoning · cache-hit % 🔭⭐

> Ralph gap spec — implementation-free. FEATURES.md **item 3**. Frontend. **Depends on spec 07.**

## Operator question
"Is prefix caching saving money, and what are reasoning models really costing?"

## Current state (verified by code search)
- `Usage.{cached, reasoning}` are **already measured** on the wire and in `FlowUsage` — but **dropped by the UI**.
- `ModelPrice.cached_per_1k` exists but is `f64` defaulting to `0.0` — **presence is unknowable** without spec-07's cached-price presence seam, which this spec consumes for any `$ saved` claim. (Codex review.)
- Spec 07 supplies the `unreported` (`unavailable`) vs reported-`0` distinction and the price-confidence tag.

## Scope — what to build
- Surface the cached/reasoning **token split**, **cache-hit rate** by model/client, and **"$ saved by cache"** (`derived`, **only** when `cached_per_1k` is set for the model).
- Breakdown popover on the table's tokens cell + an inspector line + an aggregate cache-hit rate.

## Data quality (bake into acceptance)
- Token split `measured`; cache-hit % and `$ saved` `derived`; provider-not-reporting-cached → `unavailable` (distinct from a cache **miss** = reported `0`).

## Acceptance criteria
- [ ] Tokens-cell popover shows the cached/reasoning split; the inspector line mirrors it.
- [ ] `cached = unavailable` (spec 07) renders `—` and is **not** counted as a cache miss; a reported `cached = 0` renders `0` (a real miss).
- [ ] `$ saved` shows **only** when spec-07 reports a **configured** cached price (presence flag) — NOT merely a numeric `cached_per_1k` (which defaults to `0.0`); otherwise the split shows with **no** dollar figure (no fabricated saving).
- [ ] Aggregate cache-hit rate by model/client; rows tagged `estimated` when spec-07 confidence is not `confident`.
- [ ] **don't-lie-with-zeros**: `unavailable ≠ 0` everywhere in this surface.

## Constraints / invariants
- Read backend fields; do not re-derive cost client-side beyond the documented `derived` formulas (backend owns cost).

## Out of scope
- Budgets/SLO (later program); the per-model price table editor (D13).

## Validation gate
- **Frontend:** `npm run typecheck` (`tsc -b`) · `npm run lint` (`eslint . --max-warnings 0`) · `npm run test` (`vitest run` — split/derive/unavailable) · `npm run e2e` (Playwright): popover renders the split and `—` on `unavailable`.
- Then **REVIEW_PROTOCOL.md**: Codex-xhigh review before the next gap.
