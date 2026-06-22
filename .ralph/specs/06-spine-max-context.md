# 06 — Spine: per-model max-context surfaced 🔭⚙️

> Ralph gap spec — implementation-free. FEATURES.md **item 2 (data-contract pass / the spine)**.
> **Backend + Frontend (JSON-contract migration).** Sequence: spine seam — before spec 09 (context-window utilization UI).

## Operator question
"What is the served model's context ceiling, so the UI can tell us how close we are to it?"

## Current state (verified by code search)
- **`context_limit` already exists** per-model: `CatalogEntry { id, context_limit }` (`dashboard_api.rs:236`); `UpstreamModelCatalog.context_limit_by_id` (`engine.rs:144`).
- **The parse already exists.** G3 budgeting parses max-context from upstream `/v1/models` into `context_limit_by_id` — 5 keys incl. `max_input_tokens`, `context_length`, `context_window`, `max_context_length`, **`max_model_len`** (per the archived `IMPLEMENTATION_PLAN` Task 4, GLM-5.2 advertised 500k). So this gap **surfaces** it — it does NOT add a parser.
- **Gap:** confirm `CatalogEntry.context_limit` is populated for the dashboard's served models and is **resolvable for a flow's `model_served`** (so spec 09 can compute `%`). It exists on the catalog DTO today but may not be wired to each flow.
- **The DTO lies-with-zero TODAY (Codex review):** `CatalogEntry.context_limit: i64` (non-null) and `dashboard_api.rs:778` does `entry.context_limit.unwrap_or(0)` — a missing limit collapses to `0` (the internal value IS `Option`, flattened at the DTO boundary). The frontend reads it as a plain number; a `0` ceiling reads as garbage/infinite utilization downstream.

## Scope — what to do
- Surface the **already-parsed** per-model max-context to the dashboard: ensure `CatalogEntry.context_limit` is populated for served models AND resolvable for a flow's `model_served`.
- **Reuse `context_limit_by_id`** — do NOT add a second max-context parser (the thermo review already flagged catalog-parser duplication; T11 deduped it — don't reintroduce it).
- **Make it nullable end-to-end (B+F contract migration):** `CatalogEntry.context_limit` becomes nullable; **drop the `.unwrap_or(0)`** (`dashboard_api.rs:778`); migrate the frontend TS type + mocks to `number | null` and render `—` on null. This CHANGES an existing dashboard JSON field → the gap is **B+F**, not backend-only.
- A model that doesn't advertise it → **unavailable** (`None`/`null`).

## Data quality (bake into acceptance)
- `measured` (from the upstream catalog) when advertised; `unavailable`/`None` when the upstream omits it.

## Acceptance criteria
- [ ] **Reuses** the existing `context_limit_by_id` parse (no second max-context parser); a model lacking an advertised limit → `None`.
- [ ] Surfaced on the catalog DTO and resolvable for `model_served`.
- [ ] `CatalogEntry.context_limit` is **nullable end-to-end**; the `.unwrap_or(0)` collapse is removed; a model without a limit serializes `null`, not `0`.
- [ ] Frontend TS type + mocks updated to `number | null`; the UI renders `—` on null (no `0` ceiling).
- [ ] **don't-lie-with-zeros**: missing max-context = `unavailable`, **never `0`** (a `0` ceiling would imply infinite/garbage utilization downstream).
- [ ] Catalog cache semantics preserved (`UpstreamModelCatalog` 300s `engine.rs:56`; routing union 300s `upstream.rs:32`) — tests construct a fresh `Gateway`/router (AGENTS.md).
- [ ] Parse + DTO test: one model **with** the field, one **without** (→ `None`).

## Constraints / invariants (AGENTS.md)
- Respect the 300s catalog caches; routing-mode union catalog also 300s.

## Out of scope
- The utilization gauge UI (spec 09).

## Validation gate (B+F contract migration)
- **Backend:** `cargo test` (parse with/without + nullable DTO) · `cargo clippy --all-targets` · `cargo fmt`.
- **Frontend** (`dashboard-frontend/`): `npm run typecheck` (`tsc -b`) · `npm run lint` (`eslint . --max-warnings 0`) · `npm run test` (`vitest run` — `number | null`) · `npm run e2e` (Playwright): renders `—` on a null limit.
- Then **REVIEW_PROTOCOL.md**: Codex-xhigh review before the next gap.
