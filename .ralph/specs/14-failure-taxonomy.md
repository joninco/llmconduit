# 14 — Failure taxonomy & error deep-dive 🔭⚙️

> Ralph gap spec — implementation-free. FEATURES.md **item 8**. Frontend. **Depends on spec 05.**

## Operator question
"What is failing and why, in aggregate — not one red row at a time?"

## Current state (verified by code search)
- `terminal_reason` + `status` are **already present** and `measured`.
- Spec 05 adds the **optional, gated** upstream error body.
- The inspector `ErrorTab` already exists.

## Scope — what to build
- Group `terminal_reason` × `status` by model/provider with an error **rate**.
- Enriched inspector `ErrorTab` that shows the spec-05 error body **when captured**.
- An error-rate chip on the stats strip / topology.

## Data quality (bake into acceptance)
- Grouping `measured`; rate `derived`. Error body `unavailable` when capture is off → `—` (distinct from "no error body").

## Acceptance criteria
- [ ] Aggregate failure groups by reason × model/provider with an error rate.
- [ ] `ErrorTab` shows the captured upstream error body when present; when capture is **OFF**, shows an explicit "capture disabled" state — **not** a blank that implies no error.
- [ ] Error-rate chip; a zero-sample window → `—`, not `0%`.
- [ ] **don't-lie-with-zeros**.

## Constraints / invariants
- Frontend grouping; the only backend dependency is spec 05's gated error body.

## Out of scope
- Backend error-body capture (spec 05); abuse/secret-leak scan (later program).

## Validation gate
- **Frontend:** `npm run typecheck` (`tsc -b`) · `npm run lint` (`eslint . --max-warnings 0`) · `npm run test` (`vitest run` — grouping + rate) · `npm run e2e` (Playwright): grouping renders + capture on/off states.
- Then **REVIEW_PROTOCOL.md**: Codex-xhigh review before the next gap.
