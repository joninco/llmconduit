# 07 — Spine: cost + usage confidence 🔭⚙️

> Ralph gap spec — implementation-free. FEATURES.md **item 2 (data-contract pass / the spine)** — the
> "price confidence" seam. **Backend + Frontend (JSON-contract migration).** Sequence: spine seam — before specs 08 (token economics) and 16 (overview).

## Operator question
"Can I trust this cost/usage number — is it confident, an estimate, or unknown?"

## Current state (verified by code search)
- `ModelPrice { input_per_1k, output_per_1k, cached_per_1k }` exists (`config.rs:96`), but `cached_per_1k` is **`f64`** with `#[serde(default)] → 0.0` when a config entry omits it (`config.rs:92,104`). **Presence is NOT knowable** — "configured cached price" is indistinguishable from "omitted", so "$ saved" (spec 08) can't be honest without a presence seam. (Codex review.)
- `price_for()` → `Option<ModelPrice>` (`config.rs:886`); cost is **null when unpriced**, "never a fabricated zero" (`dashboard_api.rs:99`); `cost_for_usage()` `dashboard_api.rs:281` bills cached as a subset of prompt.
- **Gap:** there is **no explicit confidence tier** — today it's binary (priced `Some` vs `None`). And `FlowUsage` tokens are **`i64`** (`cached`/`reasoning`) — a `0` **cannot** be distinguished from "provider didn't report".

## Scope — what to build
1. Add an explicit **price-confidence tag** per flow: `confident` / `estimated` (fallback/partial) / `unavailable` (unpriced). **`confident` requires every billed token class to have a known rate** — the model is priced AND (reported `cached = 0`, OR `cached_price_configured = true`). If `cached > 0` OR `cached` is `unavailable` while `cached_price_configured = false`, the cached tokens would bill at the default `0.0` (an undercount) → tag **`estimated`, NOT `confident`** (a reported `cached = 0` stays `confident`). An aggregate over **any** non-confident flow is itself tagged `estimated`. (Codex R3.)
2. Represent **unreported** `cached`/`reasoning` usage as **`unavailable`** (e.g. `Option`/sentinel), distinct from a provider-reported `0`, end-to-end — so token economics (spec 08) can honor cached-vs-unavailable.
3. Add a **cached-price PRESENCE** seam as an **ADDITIVE** field — `cached_price_configured: bool` (or `cached_per_1k_present`) on the price/topology DTO — **preserving `cached_per_1k`'s existing numeric type**. Do NOT make `ModelPrice.cached_per_1k` nullable: that would be a second contract migration to the topology/Sankey price table. Config round-trip tested. Spec 08's "$ saved" consumes this presence flag, not the numeric `0.0`. (Codex R1/R2.)
4. **This gap is a B+F contract migration:** changing `FlowUsage.{cached,reasoning}` from `i64` to unavailable-aware (`Option`) changes the dashboard JSON the React app reads — migrate the TS types/guards/mocks/WS payload handling in the SAME gap, or the app goes stale/dishonest while passing `cargo`. (The B+F contract change here is specifically `FlowUsage`; the cached-price presence flag in (3) is additive.)

## Data quality (bake into acceptance)
- Cost tag ∈ {`confident`, `estimated`, `unavailable`}; `cached`/`reasoning` ∈ {`measured` value, `unavailable`}.

## Acceptance criteria
- [ ] Per-flow price-confidence tag emitted; unpriced flow → `unavailable` (null cost preserved, **never `0`**).
- [ ] Cost is `confident` ONLY when the model is priced AND (`cached = 0` reported OR `cached_price_configured`); `cached > 0`/`unavailable` without a configured cached rate ⇒ `estimated` — no confident total that silently bills cached at `0.0`. Reported `cached = 0` stays `confident`.
- [ ] Unreported `cached`/`reasoning` tokens represented as `unavailable`, **distinct** from a provider-reported `0` (wire absence vs `0` preserved end-to-end).
- [ ] An aggregate touching any `unavailable`/`estimated` flow is tagged `estimated` — no silently-confident totals.
- [ ] **don't-lie-with-zeros**: unpriced cost = `unavailable`; unreported tokens = `unavailable`; never `0`.
- [ ] **Cached-price presence** is an ADDITIVE flag (e.g. `cached_price_configured`) preserving `cached_per_1k: number` (no `ModelPrice` nullable migration); config round-trip tested; "configured" distinct from "omitted" (the latter ⇒ no `$ saved`).
- [ ] **B+F:** the `FlowUsage` JSON change is mirrored in the frontend TS types/guards/mocks/WS handling — the React app compiles and renders `—` for `unavailable` cached/reasoning.
- [ ] Round-trip test for the tag + the usage `Option` (no new wire field without a round-trip test — AGENTS.md).

## Constraints / invariants (AGENTS.md)
- Keep cached-as-subset-of-prompt billing (`dashboard_api.rs:282`) unless explicitly changed; no new wire fields without round-trip tests.

## Out of scope
- Budgets / SLO burn-rate (later program); the token-economics UI (spec 08).

## Validation gate (B+F contract migration)
- **Backend:** `cargo test` (tag + usage option round-trip + cached-price presence + aggregate tagging) · `cargo clippy --all-targets` · `cargo fmt`.
- **Frontend** (`dashboard-frontend/`): `npm run typecheck` (`tsc -b`) · `npm run lint` · `npm run test` (`vitest run` — unavailable vs `0`) · `npm run e2e` (Playwright): renders `—` for unavailable cached/reasoning.
- Then **REVIEW_PROTOCOL.md**: Codex-xhigh review before the next gap.
