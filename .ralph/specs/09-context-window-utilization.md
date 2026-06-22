# 09 — Context-window utilization 🔭⚙️

> Ralph gap spec — implementation-free. FEATURES.md **item 4**. Frontend. **Depends on spec 06.**

## Operator question
"Are we near max context — risking slow prefill, truncation, or 400s?"

## Current state (verified by code search)
- Spec 06 surfaces per-model max-context (`context_limit`, resolvable for `model_served`).
- `Usage.prompt` is already `measured`.

## Scope — what to build
- `derived` utilization: `% = prompt ÷ max_context`, plus remaining tokens and an overflow-risk flag.
- A gauge in the inspector header + an aggregate "context pressure" stat.

## Data quality (bake into acceptance)
- `derived`. When `max_context` is `unavailable` (spec 06 `None`), the `%` is `unavailable` (`—`) — **not** `0%` or `100%`.

## Acceptance criteria
- [ ] Inspector gauge shows `% util` + remaining tokens; an aggregate context-pressure stat exists.
- [ ] `max_context` unavailable → gauge renders `—` (no number, no division-by-zero, no fake `0%`/`100%`).
- [ ] Overflow risk flagged when `%` approaches/exceeds 100 **with a real** `max_context`.
- [ ] **don't-lie-with-zeros**: missing `max_context` ⇒ `unavailable` utilization.

## Constraints / invariants
- Pure frontend over the flow + catalog DTOs; no backend change here.

## Out of scope
- Backend max-context wiring (spec 06).

## Validation gate
- **Frontend:** `npm run typecheck` (`tsc -b`) · `npm run lint` (`eslint . --max-warnings 0`) · `npm run test` (`vitest run` — %/remaining/unavailable) · `npm run e2e` (Playwright): gauge **with** and **without** `max_context`.
- Then **REVIEW_PROTOCOL.md**: Codex-xhigh review before the next gap.
