# IMPLEMENTATION_PLAN.md — Argus dashboard phase 2 (FEATURES.md items 1–10)

> **Source:** `/ralph-guide` on 2026-06-22, scoped to `FEATURES.md` PROPOSED (🔭) features, build-order
> **items 1–10**. Items 11–15 (retention/search/compare, export, OTel/Prometheus, alerts, replay,
> command palette) are deferred to a later `/ralph-guide-update`.
> **Specs:** `.ralph/specs/01..16`. **Review gate:** `.ralph/REVIEW_PROTOCOL.md` (Codex-xhigh, per-gap).
> **Branch:** `worktree-dashboard`. **Inherits:** `AGENTS.md` (commands + hard rules) + REVIEW_PROTOCOL.
> **Run:** `/ralph-orchestrate --no-review --agents 1` (serial; per-gap Codex review is the gate).

## Status: NOT STARTED — 16 gaps (01–16), serial, per-gap Codex-xhigh reviewed.

## The discipline (cross-cutting acceptance — applies to EVERY gap)
FEATURES.md hardened the framing from "pretty flow artifacts" to "can Argus answer the incident
question?" Two rules are acceptance criteria on every gap, not nice-to-haves:
1. **Data-quality tags** — every rendered metric is tagged `measured` / `derived` / `estimated` /
   `unavailable`. `estimated` must be labelled as such in the UI.
2. **Don't lie with zeros** — a value that can't be measured renders `unavailable` / `—`, NEVER `0`.
   A confident wrong number is worse than an honest gap; this is an instrument operators trust during
   incidents. The CLIENT column already does this — make it the rule everywhere. Distinguish a genuine
   measured `0` from `unavailable`.

## Why this order
Don't ship UI polish on a weak data model. **Foundation → data-contract pass (the spine) → surfaces.**
The spine's backend seams (02–07) must land BEFORE the UI surfaces (08–16) that read them, or the
surfaces stay dishonest.

## Sequencing
- **Phase 0 — Foundation:** 01 (stats-strip 🐞). FIRST — every gauge reads off it.
- **Phase 1 — Data-contract pass / the spine (backend, before ANY UI):** 02, 03, 04, 05, 06, 07.
  Mutually independent; any order within the phase; ALL before Phase 2.
- **Phase 2 — Insight surfaces (ride the spine):** 08→16, each gated on its backend dep below.

## Verified current state (code-searched 2026-06-22 — do NOT re-derive; confirm, then extend)
Several "missing" seams already partly exist. Builders: confirm against current code, then extend.
| seam | verified state | implication |
|-|-|-|
| FlowRecord timing | has `started_ms`/`elapsed_ms`/`finished_ms` only (`dashboard_flow.rs`) | 02 ADDS phase ts + `first_content_delta_ms`; 03 ADDS `attempts[]` + `first_upstream_byte_ms` |
| metrics feed | fed ONLY at the D3 terminal finalize CAS (`metrics.rs:798`) — live flows never count; ALSO the live WS path `dashboard_ws.rs:665 window_tile()` hard-codes `active_streams`/`cost_per_min` to `0.0` | 01 must fix BOTH (REST feed + live WS tile), not just REST |
| per-model max-context | parse exists (`context_limit_by_id`, incl. `max_model_len`), BUT the dashboard DTO `CatalogEntry.context_limit: i64` collapses missing→`0` (`dashboard_api.rs:778 unwrap_or(0)`) — lies-with-zero | 06 makes it **nullable end-to-end (B+F)**; reuse the parser, don't add one |
| cached-input price | `ModelPrice.cached_per_1k` exists but is `f64` default `0.0` (`config.rs:104`) — **presence unknowable** | 07 adds a cached-price **presence** seam (`Option`/flag); 08 consumes presence, not the numeric |
| price null-not-zero | `price_for()→Option`, cost null when unpriced ("never a fabricated zero", `dashboard_api.rs:99`) | 07 adds the `estimated` tier + unreported-token distinction |
| attempts | failover loop → `ProviderHealth` counters, NOT the flow (`upstream.rs:1409`); metrics are evict-safe terminal (`ServingToken`/`metrics.rs:798`), NOT `FlowStore` reads | 03 threads attempts into the flow AND the evict-safe terminal payload (12's source) |
| per-provider percentiles | GLOBAL only (`metrics.rs:351`); `ProviderHealth` point-in-time (`upstream.rs:198`) | 12 adds a per-provider ring |
| upstream response body | NOT captured — only the 3 REQUEST layers | 05 is a genuine new gated seam |
| client_label / UA | UA logged to trace + present in redacted `FlowRecord.headers`; NO `client_label` field; raw key hashable pre-redaction (`http.rs:386–449`) | 04 derives + emits; confirms the archived D1/D13 client TODO (key-hash = stronger seam) |

## Backend↔frontend contract rule (Codex review — the biggest risk to the run)
A backend-only gap that changes a dashboard JSON contract passes `cargo` + per-gap review but leaves the
React app stale/dishonest. Spine specs are therefore two kinds:
- **Additive** (02 phase-ts · 03 attempts · 04 client_label · 05 response-body) — new OPTIONAL fields the
  app ignores until a surface consumes them → **backend-only** gate is fine.
- **Contract migration** (06 `context_limit` i64→nullable · 07 `FlowUsage` i64→`Option` + cached-price
  presence) — these CHANGE existing JSON the frontend already reads → the gap is **B+F atomic** (Rust +
  TS types/guards/mocks/WS in one commit), never backend-only.

## Gaps
Checklist; `[ ]` = not started. Gate: **B** = backend (`cargo test`/`clippy`/`fmt`), **F** = frontend
(`npm run typecheck`/`lint`/`test`/`e2e`). All → Codex-xhigh per REVIEW_PROTOCOL.

### Phase 0 — Foundation
- [ ] **01** stats-strip accuracy 🐞⭐ · gate **B+F** · `01-stats-strip-accuracy.md` · investigation-first.

### Phase 1 — Data-contract pass (the spine; backend; before any UI)
- [ ] **02** spine: per-phase timestamps + `first_content_delta_ms` · gate **B** · feeds 10, 16.
- [ ] **03** spine: `attempts[]` + `first_upstream_byte_ms` · gate **B** · feeds 11, 12.
- [ ] **04** spine: `client_label` / key-hash · gate **B** · feeds 15.
- [ ] **05** spine: gated upstream response/error-body capture · gate **B** · feeds 14.
- [ ] **06** spine: surface per-model max-context (nullable `context_limit`) · gate **B+F** (contract migration) · feeds 09.
- [ ] **07** spine: cost + usage confidence (`FlowUsage` Option + cached-price presence) · gate **B+F** (contract migration) · feeds 08, 16.

### Phase 2 — Insight surfaces
- [ ] **08** token economics ⭐ · gate **F** · dep 07.
- [ ] **09** context-window utilization · gate **F** · dep 06.
- [ ] **10** per-flow latency breakdown ⭐ · gate **F** · dep 02 (03 enriches).
- [ ] **11** failover / attempt trace UI · gate **F** · dep 03.
- [ ] **12** per-provider latency + error distribution (backend) · gate **B** · dep 03.
- [ ] **13** per-provider latency UI · gate **F** · dep 12.
- [ ] **14** failure taxonomy · gate **F** · dep 05.
- [ ] **15** client / key attribution UI ⭐ · gate **F** · dep 04.
- [ ] **16** control-room overview ⭐ · gate **F** · dep 02, 03, 04, 07, **12** (12 REQUIRED for provider tiles). **LAST.**

## Per-gap Definition of Done
1. Read the gap's `.ralph/specs/<NN>-*.md` — acceptance criteria are the oracle.
2. Confirm with code search before assuming anything is missing (see verified-state table).
3. Obey AGENTS.md "Hard rules in the engine" + the dashboard Don'ts.
4. Gate green (B or F per the gap).
5. Data-quality tags + don't-lie-with-zeros satisfied (every gap).
6. Commit → **Codex-xhigh review** of that commit (REVIEW_PROTOCOL.md) before the next gap. ≤3 rounds;
   unresolved findings recorded here + halt.

## Live-verify (recommended; mirrors the prior program's T1 live-verify)
01 + the spine touch the live data path. After 01, and after the spine (02–07) lands, verify against the
live vLLM run (release binary on :5022, `--with-debug-ui`, `/dashboard`): the strip is honest under real
streaming traffic; the inspector shows real phase/attempt data; nothing renders a fabricated `0`.

## Out of scope (later /ralph-guide-update — FEATURES.md items 11–15)
Retention/privacy → full-text search + flow compare; export JSON/curl + effective-changes summary +
theater depth; OTel/Prometheus + real alerting; web-search/tool observability + SLO + abuse scan; replay
(gated+audited) + command palette. Also deferred: streaming-stall / inter-token health; provider
health-history (cooldown timeline); outlier / slow-request spotlight.
