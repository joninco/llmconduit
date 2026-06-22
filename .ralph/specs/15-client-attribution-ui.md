# 15 — Client / key / app attribution (UI) 🔭⚙️⭐

> Ralph gap spec — implementation-free. FEATURES.md **item 9**. Frontend. **Depends on spec 04.**

## Operator question
"Who is generating the cost, errors, latency — or abuse?"

## Current state (verified by code search)
- Spec 04 emits `client_label` + its `source` on the `/flows` summary.
- The CLIENT column already renders `—` honestly.

## Scope — what to build
- CLIENT-column rollup + a per-client filter chip + per-client rollups (cost / errors / latency by client).
- Show the label **source** (principal / key-hash vs the weaker UA-fallback).

## Data quality (bake into acceptance)
- `measured` label, `source`-tagged. `None` → `—`.

## Acceptance criteria
- [ ] CLIENT column shows `client_label`; the UA-fallback is visibly marked weaker than principal/key-hash.
- [ ] Per-client filter + rollup (cost / err / latency by client).
- [ ] **No raw secret** ever rendered (only the hash/label).
- [ ] `None` → `—`, never a fabricated client or `0`.
- [ ] **don't-lie-with-zeros**.

## Constraints / invariants (AGENTS.md)
- Never render raw secrets; rely on the spec-04 hash/label.

## Out of scope
- Backend `client_label` derivation (spec 04); abuse detection (later program).

## Validation gate
- **Frontend:** `npm run typecheck` (`tsc -b`) · `npm run lint` (`eslint . --max-warnings 0`) · `npm run test` (`vitest run`) · `npm run e2e` (Playwright): label sources render + the per-client filter works.
- Then **REVIEW_PROTOCOL.md**: Codex-xhigh review before the next gap.
