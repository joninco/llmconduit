# 13 — Per-provider latency + error distribution (UI) 🔭⚙️

> Ralph gap spec — implementation-free. FEATURES.md **item 7 (frontend half)**. Frontend. **Depends on spec 12.**

## Operator question
"Which upstream is degrading?"

## Current state (verified by code search)
- Spec 12 adds per-provider p50/p95/p99 + error rate to the D4 topology/health DTO.
- `CooldownTooltip` (`dashboard_api.rs:380` feeds it) + topology nodes already exist; the tooltip currently shows the **global** p99.

## Scope — what to build
- Surface per-provider p50/p95/p99 + error rate in `CooldownTooltip` (replace the global p99 there).
- Size/color topology nodes by per-provider latency / error rate.

## Data quality (bake into acceptance)
- `derived` (from spec 12). A provider with an `unavailable` window renders `—`.

## Acceptance criteria
- [ ] Tooltip shows **per-provider** percentiles + error rate (no longer the global p99).
- [ ] Node sizing/color reflects per-provider latency/error; an `unavailable` provider → a neutral state + `—`, **not** a `0`-sized or falsely-healthy node.
- [ ] **don't-lie-with-zeros**.

## Constraints / invariants
- Frontend only; consume the spec-12 DTO fields. Keep click-to-filter topology behavior intact.

## Out of scope
- The per-provider ring/DTO (spec 12).

## Validation gate
- **Frontend:** `npm run typecheck` (`tsc -b`) · `npm run lint` (`eslint . --max-warnings 0`) · `npm run test` (`vitest run`) · `npm run e2e` (Playwright): tooltip per-provider values + node states (healthy / degrading / unavailable).
- Then **REVIEW_PROTOCOL.md**: Codex-xhigh review before the next gap.
