# 11 — Failover / attempt trace (UI) 🔭⚙️

> Ralph gap spec — implementation-free. FEATURES.md **item 6**. Frontend. **Depends on spec 03.**

## Operator question
"Which provider failed, why, how long did we wait, and what served?"

## Current state (verified by code search)
- Spec 03 adds `attempts[]` to the flow + summary.
- Today only the final `upstream_target` + an `FO` badge are shown.

## Scope — what to build
- An instrument **stepper** in the inspector header from `attempts[]` — e.g. `A failed: 503 · 0.8s → B served`.
- Expandable to per-attempt detail (provider, model, status/`error_class`, duration, `first_upstream_byte_ms`, `failover_reason`).

## Data quality (bake into acceptance)
- `measured` from `attempts[]`; a `None` per-attempt first-byte renders `—`.

## Acceptance criteria
- [ ] Stepper renders one node per attempt with provider, status/`error_class`, duration, `failover_reason`; the served node is visually distinct.
- [ ] A single-attempt flow → a single node (**no fake failover**); ≥2 attempts → the chain.
- [ ] A per-attempt unmeasured time renders `—`, not `0`.
- [ ] **don't-lie-with-zeros** across the stepper.

## Constraints / invariants
- Frontend only; consume `attempts[]`. Reuse Night Watch instrument styling (see `src/design/DESIGN_NOTES.md`).

## Out of scope
- Backend attempt capture (spec 03); per-provider aggregates (specs 12/13).

## Validation gate
- **Frontend:** `npm run typecheck` (`tsc -b`) · `npm run lint` (`eslint . --max-warnings 0`) · `npm run test` (`vitest run`) · `npm run e2e` (Playwright): single-attempt **and** failover fixtures render correctly.
- Then **REVIEW_PROTOCOL.md**: Codex-xhigh review before the next gap.
