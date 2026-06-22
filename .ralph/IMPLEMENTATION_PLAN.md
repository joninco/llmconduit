# IMPLEMENTATION_PLAN.md — Argus dashboard phase 2 (FEATURES.md items 1–10)

> **Source:** `/ralph-guide` on 2026-06-22, scoped to `FEATURES.md` PROPOSED (🔭) features, build-order
> **items 1–10**. Items 11–15 (retention/search/compare, export, OTel/Prometheus, alerts, replay,
> command palette) are deferred to a later `/ralph-guide-update`.
> **Specs:** `.ralph/specs/01..16`. **Review gate:** `.ralph/REVIEW_PROTOCOL.md` (Codex-xhigh, per-gap).
> **Branch:** `worktree-dashboard`. **Inherits:** `AGENTS.md` (commands + hard rules) + REVIEW_PROTOCOL.
> **Run:** `/ralph-orchestrate --no-review --agents 1` (serial; per-gap Codex review is the gate).

## Status: IN PROGRESS — 8/16 done (01 ✅, 02 ✅, 03 ✅, 04 ✅, 05 ✅, 06 ✅, 07 ✅, 08 ✅). 8 gaps remain (09–16), serial, per-gap Codex-xhigh reviewed.

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
- [x] **01** stats-strip accuracy 🐞⭐ · gate **B+F** · `01-stats-strip-accuracy.md` · investigation-first. Root cause: live WS `window_tile` hard-coded `active_streams`/`tok-s`/`cost` to `0.0` (+ raw counts as `req/s`); REST-only fix would have left it. Fix: unified WS tick + snapshot onto the REST `metrics_body`; added `samples` u64 (terminal-flow count) end-to-end so zero-sample windows render `—` not `0`.

### Phase 1 — Data-contract pass (the spine; backend; before any UI)
- [x] **02** spine: per-phase timestamps + `first_content_delta_ms` 🔭⚙️⭐ · gate **B** · feeds 10, 16. Added `PhaseTimings{ingress_ms, normalization_done_ms, routing_decision_ms, first_content_delta_ms, stream_end_ms, finalize_ms}` (all `Option<u128>`, first-write-wins + monotonic-clamp) on `FlowRecord` + flattened onto body-free `SnapshotFlowSummary` (so it reaches the WS/snapshot wire). Seams: open→ingress, `set_normalized`→normalization, engine pre-spawn (post-lower)→routing, `OutputTextDelta` arm→TTFT (content-only), end of `run_turn`→stream_end, `finalize`→finalize. Missing phase = `None` ⇒ absent JSON (don't-lie-with-zeros). `routing` lives at the engine seam (not the leaf) so it fires for mock + real upstreams.
- [x] **03** spine: `attempts[]` + `first_upstream_byte_ms` · gate **B** · feeds 11, 12. Added `Attempt{provider,model,start_ms,end_ms,first_upstream_byte_ms,status,error_class,failover_reason}` (bounded snake_case taxonomic codes, NOT raw upstream text) on `FlowRecord` + body-free `SnapshotFlowSummary` + the evict-safe `TerminalMetricsInputs` (spec-12 source). Attempts ride the shared `ServingToken` (same evict-safe seam as usage): the failover loop records one per provider (failed+served), the bare leaf records exactly one (served via a first-byte stream wrap / failed via dispatch error). The L1 guard threads them into BOTH the record AND the terminal payload at finalize. `first_upstream_byte_ms` = wire TTFB (distinct from gap-02's content TTFT), measured at the prefetch point. Mid-stream failure appends NO attempt (failover-pre-first-chunk untouched); routing-mode attempts come only from the selected provider's failover loop. don't-lie-with-zeros: unmeasured times are `None`→absent, never `0`.
- [x] **04** spine: `client_label` / key-hash · gate **B** · feeds 15. Added `ClientSource{key_hash, configured_header, user_agent}` (snake_case wire enum, Serialize+Deserialize) + `ClientAttribution{label, source}` on `FlowRecord` + body-free `SnapshotFlowSummary` (so it rides the WS/snapshot wire). `ClientAttribution::derive(headers, configured_header)` runs ONCE in `log_api_call` at the PRE-redaction seam (`http.rs` — the only point the raw key is readable): priority `KeyHash → ConfiguredHeader → UserAgent → None`. Key-hash = `key-<first 12 hex of SHA-256(key)>` from the `Authorization` bearer (scheme-stripped) or `x-api-key`; the raw key is hashed in-place + dropped, NEVER stored/logged/emitted. The configured caller-id header NAME is read env-only (`LLMCONDUIT_DASHBOARD_CLIENT_HEADER`) — never on the `Debug`/`Clone` persisted `Config` (mirrors dashboard-auth env-only posture); the name is non-secret, only the key VALUE is. UA is a labelled WEAK fallback (tagged so spec 15 renders it differently), not an identity claim. don't-lie-with-zeros: no key + no configured id + no UA (incl. blank/whitespace) ⇒ `None`/`None` ⇒ absent JSON ⇒ renders `—`, never a fabricated id. `open()` gained a `ClientAttribution` param; label is `cap_scalar`-bounded + counted in `summary_bytes`. Frontend untouched (ignores the new optional fields until spec 15).
- [x] **05** spine: gated upstream response/error-body capture · gate **B** · feeds 14. Added `upstream_response: Option<UpstreamResponseBody{bytes: Arc<[u8]>, truncated: bool}>` on the LIVE `FlowRecord` only (NOT on body-free `SnapshotFlowSummary` — the 135 GiB guard holds; verified by `upstream_response_body_not_on_snapshot_summary`). Captured at the leaf's `dispatch_chat_stream` terminal-error sites (first-attempt non-2xx + the shrink-and-retry's final body, last-writer-wins) via new `set_upstream_response` + `capture_response_body` — COPIED through the SAME capped/redacting serializer as the request layers (`capture_capped_redacted`, BODY_CAP 128 KiB), so NO `Bytes` slice of the 256 MiB middleware buffer is retained; `truncated = raw.len() > BODY_CAP`. SEPARATELY gated: OFF by default, armed only when `LLMCONDUIT_DASHBOARD_CAPTURE_UPSTREAM_RESPONSE=1/true/yes` (env-only, never on persisted `Config`; distinct from the debug-UI gate that arms request capture) — `is_response_capture_enabled() = enabled && response_capture_enabled`. don't-lie-with-zeros tri-state: `None` = capture disabled OR no body; `Some(_)` = captured (an empty upstream body is honestly recorded as the `[redacted: unparseable body 0 bytes]` marker, never a fabricated body); `truncated` flags a partial body. Counted in `summary_bytes`/`body_bytes` + shed in eviction phase 1 (record survives body-free). `/v1/completions` still NOT instrumented (whitelist unchanged). Frontend untouched (ignored until spec 14). Secrets in the error body redacted (`upstream_response_body_is_redacted`).
- [x] **06** spine: surface per-model max-context (nullable `context_limit`) · gate **B+F** (contract migration) · feeds 09. Made `CatalogEntry.context_limit` NULLABLE end-to-end (`Option<i64>`, `skip_serializing_if`) and DELETED the `.unwrap_or(0)` lie-with-zeros collapse at the `/dashboard/api/catalog` handler — a model WITH an advertised window serializes the integer, a model WITHOUT serializes ABSENT/null (NEVER `0`). REUSED the existing `context_limit_by_id` parse (`UpstreamModelEntry.context_limit: Option<i64>` already threaded from upstream `/v1/models` incl. `max_model_len`) — NO second max-context parser added. Backend round-trip test (`d13_catalog_entry_context_limit_round_trips_nullable`, AGENTS.md changed-wire-field rule) + updated `d13_catalog_is_a_bare_array_no_cursor` (model-a 32k integer / model-b absent, asserts `!= 0`); gave `tests/gateway.rs`'s local `MockUpstream` a `set_context_limits` seam. Frontend: TS `CatalogEntry.context_limit?: number | null`; mock gained a `null`-window entry (`mystery-model`); `fmtTokens` (existing token formatter — already renders `—` on null/undefined, never `0`) is the catalog renderer, pinned by new `format.test.ts` (null⇒`—`, real⇒number, measured `0`⇒`"0"` distinct from unavailable); `client.test.ts` asserts the nullable round-trip. The utilization gauge UI is spec 09 (consumes this honest field). Gate: B (cargo test/clippy/fmt) + F (typecheck/lint/test 348/e2e 6) all green.
- [x] **07** spine: cost + usage confidence (`FlowUsage` Option + cached-price presence) · gate **B+F** (contract migration) · feeds 08, 16. `FlowUsage.{cached,reasoning}` i64→`Option<i64>` (`skip_serializing_if` ⇒ unreported is ABSENT, distinct from a reported `0`; provenance via `AccumulatedUsage` Option + `accumulate_optional`); ADDITIVE `ModelPrice.cached_price_configured: bool` via custom `Deserialize` (presence distinct from a configured `0.0`, `cached_per_1k` stays `number` — no second migration); `CostConfidence{confident,estimated,unavailable}` on `FlowRow`/`FlowDetailBody` (priced + `cached==Some(0)`/configured ⇒ confident; `cached>0`/unreported + no cache rate ⇒ estimated; unpriced ⇒ unavailable, never `0`) + AGGREGATE on `MetricWindow`/headline (`window_cost_confidence` + new `BucketCounts.unreported_cached_samples` so a summed-`cached==0` window is still estimated — no silently-confident totals). Frontend mirrored in-commit: TS `Usage.{cached,reasoning}: number|null`, `CostConfidence`, `ModelPrice.cached_price_configured`, all guards/mocks/WS; `FlowDetail` renders `—` for unreported cached/reasoning + labels `est`. B: cargo test 932/clippy 0/fmt. F: typecheck/lint/test 355/e2e 6.

### Phase 2 — Insight surfaces
- [x] **08** token economics ⭐ · gate **F** · dep 07. Built the token-economics surface that CONSUMES the gap-07 honest cost/usage contract. Pure `tokenEconomics.ts` (DOM-free, sibling of `flowModel.ts`): per-flow cached/reasoning split (`measured`; unreported→`—`, reported `0`→`0`), cache-hit rate `cached/prompt` (`derived`; `—` when cached unreported — NOT a 0% miss), and "$ saved by cache" (`derived` = `(cached/1000)·(input−cached)/1k`, shown ONLY when gap-07 `cached_price_configured` PRESENCE is true + cached reported — never the numeric `0.0` default), plus `aggregateCacheByKey` (by-model roll-up: excludes unreported flows from the rate, sums savings only for configured-price flows, marks a group `estimated` when ANY contributing flow's `cost_confidence !== 'confident'`). Surfaces: `TokensCell` popover on the table tokens cell (fixed overlay like `CooldownTooltip`, no FLIP), the FlowDetail inspector "cache hit / $ saved" line MIRRORING it (derived saving labelled), and a collapsed `CacheEconomics` per-model aggregate strip under the table (est-labelled rows). Gate F: typecheck/lint/test 409 (+25)/e2e 7 (+1, hover popover asserts split + `—` on unreported). Backend untouched (no `.rs` changed; cargo test/clippy/fmt confirmed green).
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
