# COMPLETED_TASKS — Argus dashboard phase 2

Archive of completed Ralph gaps (full detail). Newest first.

---

## Gap 05 — REVIEW ROUND 2 (Codex-xhigh re-review). Gate B (backend-only). ✅

Follow-up to gap 05 round 1 (commit `bcc7891`). Codex-xhigh round-2 re-review found **1 HIGH
edge-case** in the final-outcome body semantics. Gap 05 stays `- [x]`; this is a CORRECTNESS
follow-up commit (not an amend).

### Finding (HIGH) — `src/upstream.rs` failover loop
A later failover attempt that fails **without an HTTP error body** did not overwrite/clear an
EARLIER attempt's staged body, so `A = 500(body) → B = connect/timeout/prefetch-stream-error`
committed A's STALE body even though the turn's FINAL failure carried none. Root cause: the staged
body was cleared ONLY on a served attempt (the failover serve-success seam) — a body-less failure
never re-stages and never reaches that clear, so A's body survived to the finalize commit.

### Fix — clear the staged body at the START of each attempt (final-outcome wins)
- **`src/upstream.rs` failover loop** (`stream_chat_completion_with_provider_indices`): call
  `serving.clear_pending_response_body()` at the START of each provider attempt, right beside the
  existing `serving.arm_attempt_header_byte()` (the SAME per-attempt scratch-reset seam). An
  HTTP-status failure then RE-STAGES its own body; a served attempt clears (as it already did at the
  serve-success seam); a body-less final failure leaves the slot cleared. Net: the FINAL attempt's
  outcome determines the committed body.
- **`src/upstream.rs` bare-leaf path** (`ReqwestUpstreamClient::stream_chat_completion`, the
  `tag_primary_provider` single-attempt branch): the SAME per-attempt clear beside its
  `arm_attempt_header_byte()`, so the single-attempt (no-failover) path holds the invariant
  identically (a fresh token is already empty, so it is normally a no-op — kept symmetric for
  defense-in-depth so a body can never commit from stale staging on any path).
- **Routing path**: unchanged code — routing delegates to the selected provider's failover loop
  (`stream_chat_completion_with_timeout[_from_provider]` → `…_with_provider_indices`), so the
  loop-level clear covers it. Siblings remain NOT fallbacks (AGENTS.md hard rule untouched).
- Doc comments on `pending_response_body` / `set_pending_response_body` /
  `clear_pending_response_body` / `capture_upstream_response_body` updated from the round-1
  "last-writer-wins survives" framing to the round-2 "FINAL attempt's outcome wins (per-attempt
  clear)" semantics.

### Tests added (`src/upstream.rs`, wiremock failover)
- `gap05_failover_a500_then_b_transport_failure_commits_no_stale_body` — A 500(body) → B connect
  refused (transport, no body) ⇒ token pending body is `None` AND committed
  `record.upstream_response == None` (the finding's exact case).
- `gap05_failover_a500_then_b_no_first_chunk_commits_no_stale_body` — A 500(body) → B 200 with an
  EMPTY SSE body (prefetch `Stream`-class failure, body-less, 2xx never stages) ⇒ committed
  `upstream_response == None`.

### Prior gap-05 guarantees preserved (kept green)
- `gap05_failover_a500_then_b200_commits_no_stale_error_body` (A 500 → B 200 ⇒ None) and
  `gap05_failover_all_fail_commits_final_error_body` (A 500 → B 503 ⇒ B's last body) both still pass
  under the per-attempt clear (B stages AFTER the start-of-attempt clear, so B's body still wins on
  an all-fail turn). Gate on/off, single-failing-HTTP-attempt-keeps-its-body, capped-copy/no
  256 MiB `Bytes`-slice retention, no body on snapshots, env-gated OFF by default, byte
  budget/eviction, tri-state honesty, failover-pre-first-chunk-only, cancellation — all unchanged.

### Gate results
- **B:** `cargo test` — 927 passed / 0 failed (all suites) · `cargo clippy --all-targets` exit 0
  (zero warnings) · `cargo fmt --check` clean.

### Notes
- AGENTS.md gap-05 operational note updated: the body is now cleared at the START of each attempt
  (not only on serve), and the final-outcome enumeration adds A 500 → B body-less-failure ⇒ `None`.
  Operational-only; no changelog narrative.
- No regressions; no deferrals.

---

## Gap 04 — spine: `client_label` / key-hash. Gate B (backend-only). ✅

ADDITIVE spine gap: new OPTIONAL attribution fields the React app ignores until spec 15. Answers the
operator question "who is generating the cost/errors/latency/abuse?" by deriving a STABLE, NON-SECRET
client label from the inbound request — with the raw API key hashed (one-way) and never stored.

### What was built
- **`ClientSource` enum** (`src/dashboard_flow.rs`) — `KeyHash | ConfiguredHeader | UserAgent`,
  `#[serde(rename_all = "snake_case")]`, derives `Serialize + Deserialize` (round-trips on the
  WS/snapshot wire). Tags the PROVENANCE so spec 15 can render the weak UA fallback differently from
  the stronger key-hash / configured-id sources. No proxy auth-principal source exists today (the
  proxy forwards keys, it does not authenticate a principal) — deliberately omitted per the spec.
- **`ClientAttribution{label: Option<String>, source: Option<ClientSource>}`** with `none()` and
  `derive(headers, configured_header)`. `derive` honors priority `KeyHash → ConfiguredHeader →
  UserAgent → None`:
  - **Key-hash**: the `Authorization` bearer token (case-insensitive `Bearer ` prefix stripped, else
    the whole value) OR the `x-api-key` value → `key_hash_label` = `key-<first 12 hex of
    SHA-256(key)>`. The raw key is read, hashed, and dropped IN-PLACE — never stored/returned/logged.
    A blank/whitespace key is ignored.
  - **Configured header**: when an operator-named non-secret header (e.g. `x-client-id`) is present +
    non-empty, its value is the label.
  - **User-Agent**: a present, non-empty UA is the (weak, labelled) fallback.
  - else `None`/`None`.
- **`FlowRecord` + `SnapshotFlowSummary`** each gained `client_label: Option<String>` +
  `client_source: Option<ClientSource>` (both `skip_serializing_if = "Option::is_none"` on the
  summary ⇒ absent JSON when unattributed). `SnapshotFlowSummary::from_record` projects them
  (body-free scalar metadata; `ClientSource` is `Copy`). `FlowRecord::summary_bytes` now counts
  `client_label` against the live byte quota; `open()` `cap_scalar`-bounds the label.
- **`DashboardFlowStore::open()`** gained a trailing `client: ClientAttribution` param. All ~29
  non-production call sites (tests across `dashboard_flow.rs`, `dashboard_ws.rs`, `upstream.rs`,
  `metrics.rs`, `tests/gateway.rs`, `benches/metrics.rs`) pass `ClientAttribution::none()`.
- **Production seam (`src/http.rs` `log_api_call`)**: `ClientAttribution::derive(&headers, …)` is
  called on the RAW headers BEFORE `redact_headers` (the only point the raw key is readable), then
  threaded into `open()`. The configured caller-id header NAME is read ENV-ONLY via
  `dashboard_client_header()` (`LLMCONDUIT_DASHBOARD_CLIENT_HEADER`) — never on the persisted
  `Config` (which is `#[derive(Debug, Clone)]`), mirroring the dashboard-auth env-only posture.

### Security (the gap Codex scrutinizes hardest)
- The key-hash is a one-way SHA-256 digest; only a 12-hex (48-bit) PREFIX becomes the label, so it is
  non-invertible and reveals nothing about the key. The SAME key → SAME label (stable grouping); a
  DIFFERENT key → DIFFERENT label.
- The raw key is NEVER stored on `ClientAttribution`, `FlowRecord`, `SnapshotFlowSummary`, the
  persisted `Config`, or any log/WS surface. `redact_payload_secrets` / the header redactor are not
  bypassed — `derive` reads the raw header, hashes, and drops it; redaction still runs afterward.
- The configured header NAME is non-secret config (only the api-key VALUE is sensitive); reading it
  env-only keeps all attribution config out of the `Debug`/`Clone` `Config`.

### Data quality (don't-lie-with-zeros)
- No key + no configured id + no UA (including blank/whitespace-only values) ⇒ `None`/`None` ⇒ both
  keys ABSENT on the wire ⇒ renders `—` (unavailable) downstream — never a fabricated id, `0`, or
  empty-string-as-id. "No client info" is distinguishable from a real value.

### Tests added (`src/dashboard_flow.rs`, 7 new)
- `client_attribution_key_hash_wins_over_user_agent` — key (`Authorization` AND `x-api-key`) beats a
  co-present UA; label is `key-` + 12 hex; raw key never embedded.
- `client_attribution_key_hash_is_stable_and_distinct` — same key → same label, different key →
  different label; bearer-stripped key hashes identically to the bare key.
- `client_attribution_configured_header_beats_user_agent` — configured header wins over UA; with no
  configured name the same request falls through to UA.
- `client_attribution_user_agent_is_the_weak_fallback` — UA only when no key + no configured id.
- `client_attribution_none_when_no_signal` — empty headers AND blank key + blank UA ⇒ `none()`.
- `client_attribution_flows_to_record_and_summary_without_raw_key` — end-to-end through the store;
  asserts label+source on the record AND the summary, and the raw key appears in NEITHER the
  `{record:?}` dump NOR the serialized summary JSON (the redaction assertion).
- `client_attribution_absent_on_wire_and_source_round_trips` — unattributed flow omits both keys;
  `ClientSource` serialize→deserialize round-trips for all 3 variants; snake_case spellings asserted.

### Gate results
- **B:** `cargo test` — all binaries green (0 failed; lib unit 598 incl. the 7 new) ·
  `cargo clippy --all-targets` exit 0 (clean, zero warnings) · `cargo fmt --check` clean.

### Notes / discoveries
- Threading a new `open()` param touched ~29 test/bench call sites; a single `ClientAttribution`
  arg (vs. two positional fields) kept each edit to one line. `cargo test` does not compile benches —
  `--all-targets` clippy caught the `benches/metrics.rs` site that `cargo test` missed.
- Frontend untouched (Gate B); no `FlowRow`/`FlowDetailBody` change (the strict frozen `types.ts`
  contract is left for spec 15 to extend). The new fields live on `FlowRecord` + `SnapshotFlowSummary`
  per the spec's acceptance criteria.

---

## Gap 01 — REVIEW ROUND 1 fixes (Codex-xhigh on `bbc89d50`). Gate B+F. ✅

A Codex-xhigh review of the gap-01 commit found 4 issues; all 4 fixed in a FOLLOW-UP commit
(not an amend). Gates re-run green.

### Finding 1 (HIGH) — live metrics frames gated only on terminal `metrics_seq`
`active_streams` changes while a request is IN FLIGHT, but the metrics ring `metrics_seq` only
advances at the D3 terminal finalize, so the WS `metric_ticker` arm (gated solely on
`seq != last_metrics_seq`) never re-emitted for an in-flight count change — the strip's live
count froze at the snapshot value until the next finalize. Also the client drops same-seq frames.
- **Fix (`src/dashboard_ws.rs`):** the per-second metric tick now emits when EITHER the view seq
  advanced OR the live `active_streams` count changed (sampled at tick time). The frame `seq` is a
  STRICTLY-MONOTONIC per-domain cursor via `next_metrics_cursor(view_seq, last_emitted) =
  max(view_seq, last_emitted + 1)`: a genuine ring advance carries the true seq, an active-only
  change nudges the cursor by one — always `> last_emitted`, so the client's `seq <= cursor`
  whole-frame dedup (and the `metrics_seq` sample dedup) accept it. NO global watermark (still a
  per-domain `{domain, seq}` cursor); the batched `DashboardFrame` envelope, `/debug/ws` bare
  contract, cancellation/expiry selects, and body-free snapshot are all untouched.
- Unit test: `next_metrics_cursor_is_strictly_monotonic_for_active_only_changes`.

### Finding 2 (HIGH) — sparklines plotted raw `0` for unavailable sample-derived points
`seriesFor` mapped the raw field, so an unavailable p50/tok-s/$/min (sample == 0) drew a
misleading zero trend.
- **Fix (`metricHistory.ts`):** `seriesFor` now emits `NaN` (a uPlot GAP, not a `0`) for a point
  that was UNMEASURABLE in its own sample, gated by the SAME `metricUnavailable` predicate the chip
  value uses (moved into `metricHistory.ts` with `METRIC_AVAILABILITY` to avoid a chips↔history
  import cycle). `req/s`/`active_streams` are never gapped.
- Unit tests: 3 in `metricHistory.test.ts` (tok/s gap on `usage_samples==0`; req/s never gapped;
  cost gap on `priced_samples==0` while tok/s plots).

### Finding 3 (MAJOR) — one `samples` count for all derived metrics → fake `0` tok/s & $/min
Token and cost availability are SEPARATE from latency: a missing-usage flow or an unpriced model
yielded `tokens_per_sec`/`cost_per_min == 0` with `samples > 0` (fabricated zeros).
- **Fix (end-to-end wire field threading, B+F atomic):**
  - `metrics.rs`: added `BucketCounts.usage_samples` (incremented only when a terminal flow
    reported usage), summed in `WindowRing::aggregate`; new `WindowReport::usage_sample_count()` +
    `priced_sample_count(has_price)` (the cost denominator = usage-bearing samples whose served
    model has a price — derived where the price table lives, so `metrics.rs` stays price-agnostic).
  - `dashboard_ws.rs` / `dashboard_api.rs`: `MetricWindow` + the headline `MetricTick`/
    `MetricsSnapshot` carry `usage_samples` + `priced_samples`; `rest_window_tile`/`metrics_body`
    compute them.
  - `types.ts`: added both fields to `MetricWindow`/`MetricTickPayload`/`MetricsResponse` + ALL
    validators (`isMetricWindow`, `metric_tick` arm, `isMetricsResponse`); `ws.ts` threads them;
    `mock.ts` + `ws.fixtures.ts` updated.
  - `chips.ts`: `tokens_per_sec` keys off `usage_samples`, `cost_per_min` off `priced_samples`,
    latency/err% off `samples` → each renders `—` independently when its OWN denominator is 0.
- Round-trip test (AGENTS.md rule): `metrics_body_new_sample_fields_round_trip_through_json`
  (serialize → JSON → re-parse at headline + per-window). Plus `metrics.rs`
  `usage_samples_count_only_usage_bearing_terminals`, `dashboard_api.rs`
  `metrics_body_per_metric_denominators_diverge`, golden-shape + frontend validator-rejection tests.

### Finding 4 (MEDIUM) — chips exposed no measured/derived/estimated/unavailable provenance
- **Fix:** `ChipDescriptor` gains a `quality: 'measured'|'derived'|'estimated'|'unavailable'` field
  (`chips.ts`): req/s + active = `measured`, err%/p50/p95/p99/tok-s = `derived`, $/min = `estimated`
  (priced → labelled, per IMPLEMENTATION_PLAN), and `unavailable` whenever the value is `—`.
  `StatsStrip.tsx` renders it via `data-quality` + a `title` hover hint + an `aria-label` on the
  value (visible-text/tooltip/ARIA/`data-*`, all four).
- Tests: 4 chips.test.ts provenance cases (all chip states + closed-set guard) + 2 StatsStrip
  component tests (`data-quality` rendered; flips to `unavailable`) + 1 e2e provenance assertion.

### Gate results (round 1)
- **B:** `cargo test` 566 lib + integration green · `cargo clippy --all-targets` exit 0 ·
  `cargo fmt --check` clean.
- **F:** `npm run typecheck` exit 0 · `npm run lint` exit 0 · `npm run test` 342 pass ·
  `npm run e2e` 6/6 pass (visual baselines unaffected — provenance is non-visual `data-*`/`title`/
  `aria`; the mock window is fully measured so no sparkline gaps appear).

### Notes
- Finding 1's monotonic-cursor approach keeps the metrics domain self-contained: on reconnect the
  snapshot re-seeds `lastSeq.metrics` from `metrics_seq`, so any cursor drift from active-only
  bumps resets cleanly; within a connection the cursor is strictly increasing.
- The old split `record_response`+`record_usage` path also counts a usage sample (in `add_tokens`)
  so its callers/tests stay consistent; the production terminal path uses the atomic
  `record_terminal`, counting usage exactly once.

---

## Gap 01 — stats-strip accuracy (bug, foundation). Gate B+F. ✅

**Operator question answered:** the headline gauges read `0.0` with fresh real traffic
because the LIVE `/dashboard/ws` metrics path collapsed the metrics view with a SEPARATE,
crippled tile builder (`window_tile`) that hard-coded `active_streams`/`tokens_per_sec`/
`cost_per_min` to `0.0` and shipped RAW window counts as `reqs_per_sec`. The REST
`/dashboard/api/metrics` read was already correct (`rest_window_tile`/`metrics_body`), but
the strip folds the WS tick after the first frame — so once a WS `metric_tick` (or the WS
initial snapshot) landed, those fields read `0` even while traffic streamed and finalized.

### Root cause (both surfaces)
1. **Live WS tick + WS initial snapshot** (`dashboard_ws.rs`) used `window_tile()`:
   `active_streams: 0`, `tokens_per_sec: 0.0`, `cost_per_min: 0.0`, `reqs_per_sec = raw count`.
   This is the dominant bug — the strip's live source was dishonest.
2. **Don't-lie-with-zeros gap (both REST + WS):** an empty/zero-sample window rendered
   `p50/p95/p99 = 0`, `tok/s = 0`, `$/min = 0`, `err% = 0` — indistinguishable from a
   genuine measured `0`. Nothing carried the "no finalized sample" signal to the client.

### Fix
- **Unified the tile computation:** `metric_tick_frame` + `metrics_snapshot` (WS) now build
  via the SAME `crate::dashboard_api::metrics_body` the REST read uses — ONE honest
  computation for both surfaces. The broken `window_tile` is deleted. Both WS call sites
  (the periodic tick loop and the initial-snapshot setup in `dashboard_socket`) now thread
  the live open-flow count (`active_stream_count`, made `pub(crate)` — single source, no
  drift) + the gateway price table, so `active_streams`/`tokens_per_sec`/`cost_per_min` and
  the TRUE per-second `reqs_per_sec` carry real values live (or `unavailable` semantics, never
  hard-coded `0.0`). The single-CAS terminal feed (`metrics.rs:798`) is untouched and stays
  idempotent — only the wire collapse of the already-recorded view changed.
- **Data-quality signal (`samples`):** added `samples: u64` (= the window's TERMINAL-flow
  `total_count`) to `MetricWindow` + `MetricTick` + `MetricsSnapshot` (Rust) and to
  `MetricWindow`/`MetricTickPayload`/`MetricsResponse` + the three runtime validators (TS,
  `isUint`). It's a finite `u64`, so the FROZEN finite-number wire contract is preserved (no
  `null`/`NaN` over the wire, which would break every validator/fixture). This is a B+F
  CONTRACT-MIGRATION commit (Rust + TS types/guards/mocks/WS/fixtures atomic), per the plan's
  contract-migration rule.
- **Frontend honesty (`deriveChips`, `chips.ts`):** when `cur.samples === 0`, the
  sample-derived metrics (`err%`, `p50/p95/p99`, `tok/s`, `$/min`) render `unavailable`
  (`—`), NEVER a fabricated `0`, with a flat delta and no err% threshold accent. `reqs_per_sec`
  (a genuine measured `0` for an idle window) and `active_streams` (the live open-flow count)
  are NOT sample-derived → they always show their real numeric value. A real `0` and an
  `unavailable` are thus distinguishable on the strip (the CLIENT-column rule, generalized).

### Files changed (16)
- `src/dashboard_ws.rs` — `samples` on the 3 tile structs; `metric_tick_frame`/`metrics_snapshot`
  delegate to `metrics_body` (+ `active_streams`/prices params); both `dashboard_socket` call
  sites pass live open-flow count + price table; deleted `window_tile`; updated golden-shape test.
- `src/dashboard_api.rs` — `rest_window_tile` sets `samples = total`; `metrics_body` mirrors
  `m1.samples`; `active_stream_count` → `pub(crate)`; 2 new gap-01 tests.
- `dashboard-frontend/src/api/types.ts` — `samples` on 3 interfaces + 3 validators.
- `dashboard-frontend/src/api/ws.ts` — thread `payload.samples` → `setMetrics`.
- `dashboard-frontend/src/components/StatsStrip/chips.ts` — `sampleDerived` flag + unavailable
  rendering + `UNAVAILABLE` export.
- Test/mock/fixture updates carrying `samples`: `mock.ts`, `ws.fixtures.ts`, `ws.test.ts`,
  `chips.test.ts` (+3 new don't-lie tests), `StatsStrip.test.tsx` (+1 new e2e-of-component test),
  `metricHistory.test.ts`, `Scrubber.test.tsx`, `useMetricStream.test.tsx`,
  `TopologySankeyViews.test.tsx`, `e2e/dashboard.spec.ts` (+1 new gap-01 live-strip assertion).
- `dashboard-frontend/src/components/viz/jsonFold.test.ts` — fixed 4 PRE-EXISTING
  `noUncheckedIndexedAccess` `tsc` errors (non-null assertions on test-guaranteed indices) that
  were blocking the shared `npm run typecheck` gate. Unrelated to gap 01; minimal + safe.

### Tests added (proving the new behavior)
- Backend: `metrics_body_counts_a_terminal_flow_with_real_rates` (a finalized flow IS counted:
  `samples == 1`, real `active_streams`/`tok/s`/`$/min`/true `req/s`, not the old zeros) +
  `metrics_body_empty_window_reports_zero_samples_not_a_fake_zero` (empty window → `samples == 0`
  with `req/s == 0` distinguishable from unavailable latency/tok-s/cost).
- Frontend unit: 3 chips tests (zero-sample window → `—`; genuine `samples>0` zero stays numeric;
  unavailable → flat delta + no threshold accent) + 1 StatsStrip component test (end-to-end
  through the store) + 1 Playwright test (strip reads real numbers under live mock traffic).

### Gate results
- **B:** `cargo test` 562 lib + all integration green · `cargo clippy --all-targets` exit 0
  (clean) · `cargo fmt --check` clean.
- **F:** `npm run typecheck` exit 0 · `npm run lint` exit 0 · `npm run test` 328 pass ·
  `npm run e2e` 6/6 pass (screenshot baselines unaffected — mock seeds non-zero `samples`).

### Notes / discoveries
- The frozen TS wire contract types every `MetricWindow` field as `number` and validators
  require finite numbers — so "don't lie with zeros" could NOT be carried by `null`/`NaN` on the
  wire. The `samples` u64 companion field is the contract-safe signal; the frontend derives
  `unavailable` from it. This is the durable design choice for every later gap's DQ tags.
- `data-testid="stats-strip"` does NOT render in the real DOM (the `Panel` primitive drops
  unknown props) — the chip-level testids (`chip-<key>`, `chip-value`) do. The gap-01 e2e queries
  chips directly. Left `Panel` untouched (out of scope).

## Gap 02 — spine: per-phase timestamps + true TTFT (`first_content_delta_ms`). Gate B. ✅

FEATURES.md item 2 (the data-contract spine). ADDITIVE backend-only: new OPTIONAL phase
timestamps the React app ignores until specs 10 (per-flow latency waterfall) and 16
(control-room) consume them. No frontend touched.

### What was added
A `PhaseTimings` bundle of six OPTIONAL measured wall-clock epoch-ms phases on `FlowRecord`,
projected (flattened) onto the body-free `SnapshotFlowSummary` (so it reaches the WS/snapshot
wire automatically — `dashboard_ws::SnapshotMessage.flows: Vec<SnapshotFlowSummary>` serializes
it):
- `ingress_ms` — record open (≈ `started_ms`; the waterfall left anchor).
- `normalization_done_ms` — inbound→canonical settled (`set_normalized`).
- `routing_decision_ms` — served model resolved + candidate plan fetched + canonical lowered to
  the upstream chat payload (engine pre-spawn, after all `?` paths pass).
- `first_content_delta_ms` — TRUE TTFT: first canonical CONTENT SSE delta to the client.
- `stream_end_ms` — terminal `response.completed`/`response.incomplete` emitted (clean end only).
- `finalize_ms` — flow reached a terminal state (EVERY terminal: completed/failed/cancelled).

### Design decisions
- **First-write-wins + monotonic clamp** (`PhaseTimings::stamp`): the field stamps only if
  currently `None`, and the value is clamped UP to the latest already-recorded phase. This (a)
  makes `first_content_delta_ms` mark the FIRST content token (the `OutputTextDelta` arm fires
  per-delta, but only the first stamps), (b) stops the outer replay/tool `loop` in `run_turn` from
  re-stamping on later rounds, and (c) guarantees the spec's monotonicity
  (`ingress ≤ normalization ≤ routing ≤ first_content_delta ≤ stream_end ≤ finalize`) even across a
  backwards wall-clock step.
- **`routing_decision_ms` at the ENGINE seam, not the leaf.** Initial attempt stamped it inside
  `set_upstream` (the leaf's `capture_upstream_body`), but `MockUpstream` bypasses the production
  leaf, so it stayed `None` in mock-driven tests. Moved to a dedicated engine seam
  (`stamp_routing_decision`, keyed by `api_call_id`, called pre-spawn after lowering succeeds) so
  it fires for EVERY upstream client and is semantically "the engine committed to a route" —
  distinct from spec 03's leaf-level `first_upstream_byte_ms` (the on-the-wire byte timing).
- **don't-lie-with-zeros.** A phase that did not occur is `None` → `#[serde(skip_serializing_if =
  "Option::is_none")]` → ABSENT from JSON (renders `—` later), never `0` and never `null`. A
  wall-clock epoch stamp is never legitimately `0`, so `Some(_)` unambiguously means "this phase
  ran". (Distinct from the pre-existing `elapsed_ms`, a monotonic delta that CAN legitimately be
  `0` on a sub-ms turn — kept separate.)
- **body-free preserved.** `PhaseTimings` is `Copy` scalar metadata; flattening it onto the
  summary adds no `Arc<[u8]>` body, so the snapshots-are-body-free invariant (135 GiB worst case)
  holds. `FlowRecord` stays non-`Serialize` (the summary is the wire projection).
- **`record_seq` respected.** The new phase-only mutators go through `state.update`, which bumps
  the per-record mutation cursor exactly like every other mutator (no double-stamp on replay; the
  later flow frame carries the freshly-stamped phase).

### Files changed
- `src/dashboard_flow.rs` — new `PhaseTimings` struct (`Serialize` + `Deserialize`, for the
  round-trip), six `stamp_*` helpers; `phases` field on `FlowRecord` (ingress stamped in `open`)
  and flattened on `SnapshotFlowSummary` (`from_record` copies it); `set_normalized` stamps
  normalization, `finalize` stamps finalize; new public `stamp_routing_decision`,
  `stamp_first_content_delta`, `stamp_stream_end` (all gated on `enabled`, no-op when disabled,
  join by api_call_id OR response_id via the link index where relevant). 9 new unit tests.
- `src/engine.rs` — three seams: pre-spawn `stamp_routing_decision` (after lowering/budget pass);
  `stamp_first_content_delta` at the top of the `StreamEmission::OutputTextDelta` arm (content-only
  — reasoning/tool-arg/signature arms untouched); `stamp_stream_end` after the terminal SSE emit at
  the end of `run_turn`. All three gated on `api_call_id.is_some()` so the production hot path
  (debug UI off) skips even the disabled-store early-return call overhead.
- `tests/gateway.rs` — 3 streaming integration tests.

### Tests added (proving the new behavior)
- Unit (`dashboard_flow.rs`): ingress-at-open/others-None; full-path stamp order + monotonicity;
  first-content first-write-wins; error-before-content → TTFT None; backwards-clock monotonic
  clamp; never-zero-for-missing; absent phases serialize ABSENT (not 0/null);
  deserialize→serialize round-trip of `PhaseTimings` (present + absent mix survives);
  phases-survive-summary-`Value`-round-trip.
- Integration (`gateway.rs`, real engine + `stream_responses_with_api_call_id`):
  `gap02_phases_populate_on_real_streamed_turn` (all six phases present + monotonic on a real
  streamed turn; summary mirrors record; every measured phase > 0);
  `gap02_reasoning_deltas_do_not_stamp_ttft_content_does` (BOTH directions: a reasoning-ONLY
  stream completes with TTFT `None` AND a clean `stream_end`, proving the gate is content-specific
  not a missing seam; reasoning-THEN-content stamps TTFT, ordered after routing, before
  stream_end); `gap02_error_before_content_leaves_ttft_and_stream_end_none` (upstream `Err` as the
  first stream item → Failed, TTFT `None`, stream_end `None`, finalize `Some`; the absent phases
  ABSENT on the serialized summary).

### Gate results
- **B:** `cargo test` — 575 lib + 134 gateway integration + all other binaries green (0 failed) ·
  `cargo clippy --all-targets` exit 0 (clean, zero warnings) · `cargo fmt --check` clean.

### Notes / discoveries
- `MockUpstream` does not call the production leaf's `capture_upstream_body`/`set_upstream`, which
  drove the routing-seam relocation above. This is the right place anyway — a routing DECISION
  exists the moment lowering succeeds, even if a later pre-dispatch error occurs.
- The first cut of the integration assertion used `!json.contains("_ms\":0")` which false-matched
  the legitimate pre-existing `"elapsed_ms":0` on a sub-ms mock turn. Replaced with structured
  per-phase `value > 0` assertions — `elapsed_ms` is a monotonic delta and CAN be 0; the phase
  epoch stamps cannot. Worth remembering for later DQ gaps: assert on typed values, not JSON
  substrings, when a 0 sentinel is in play.
- `SnapshotFlowSummary` is `Serialize`-only; the AGENTS.md "no new wire field without a round-trip
  test" rule is satisfied by deriving `Deserialize` on `PhaseTimings` (the new bundle) and
  round-tripping it directly + via the summary's flattened `Value`.
- The REST cost-projection DTOs (`dashboard_api::FlowRow`/`FlowDetailBody`) were intentionally NOT
  threaded — out of scope; specs 10/16 will surface the phases through them when they build the
  waterfall/control-room. The spine already reaches the live wire via the WS snapshot path.

---

## Gap 02 — review round 1 (Codex-xhigh) — 1 HIGH fixed

**Reviewed commit:** `16132c4d857d8af168b32202e9d40b6dd602b09c` (the gap 02 feature commit).
**Finding (HIGH):** `src/engine.rs` stamped `first_content_delta_ms` BEFORE the content delta's
`send_event(...).await?`, so a closed/cancelled stream could record TTFT for a first content token
the client never actually saw.

### Fix
- `src/engine.rs` (`OutputTextDelta` arm in `run_turn`): MOVED the
  `flow_store().stamp_first_content_delta(api_call_id)` call to AFTER `send_event(...).await?`
  returns `Ok`. `send_event` maps a closed receiver OR a fired kill token to
  `AppError::cancelled()`, so the `?` short-circuits and the stamp is skipped when the first
  content delta is NOT delivered — TTFT stays `None`. Still content-only (reasoning / tool-arg /
  signature deltas have their own arms) and first-write-wins in the store, so only the first
  DELIVERED content delta stamps. The monitor `emit_with` stays before the send (it is a
  fire-and-forget debug broadcast, not a client-delivery signal; pre-send keeps debug-UI event
  ordering unchanged). Hot path unchanged (still gated on `api_call_id.is_some()`).

### Tests
- NEW `gap02_cancel_before_first_content_delta_leaves_ttft_none` (`tests/gateway.rs`): a
  `PendingChunkUpstream` parks before yielding any content; the test drains the preamble
  (`response.created`/`in_progress` — not content), waits for the upstream to park, then drops the
  SSE receiver. The engine is suspended in `next_upstream_chunk`'s `tx.closed()` select, so the
  content arm is never reached and the flow finalizes Cancelled with `first_content_delta_ms ==
  None`, `routing_decision_ms == Some` (request reached the wire), `stream_end_ms == None`,
  `finalize_ms == Some`; and the undelivered TTFT serializes ABSENT on the body-free summary.
- STRENGTHENED `d3_midstream_cancel_finalizes_cancelled_with_last_usage`: now ALSO asserts
  `first_content_delta_ms.is_some()` — the content delta `"Hel"` WAS drained (delivered) before the
  hang-up, so a cancel AFTER delivery KEEPS the true TTFT. Together the two tests bracket the
  contract: TTFT is stamped iff the first content delta's `send_event` succeeded.
- Happy-path coverage retained: `gap02_phases_populate_on_real_streamed_turn` still asserts TTFT IS
  stamped once on a delivered streamed turn.

### Gate results
- **B:** `cargo test` — all binaries green (0 failed; gateway integration now 135 tests incl. the
  new cancellation test) · `cargo clippy --all-targets` exit 0 (clean, zero warnings) · `cargo fmt`
  applied (clean).

### Notes / discoveries
- The bug's exact mechanical trigger (engine ENTERS the content arm AND that delta's `send_event`
  returns `Err`) is not fully deterministic to provoke for the FIRST content delta: with the 128-
  slot SSE channel the first content delta (event ~5) always sends into an empty channel before any
  full-channel block, and the biased `tx.closed()` in `next_upstream_chunk` catches a pre-content
  hang-up before the arm is reached. The fix is therefore enforced structurally by the `?` on
  `send_event`; the deterministic test asserts the END-TO-END contract (hang-up before delivery →
  TTFT `None`) per the finding's wording, and the delivered-then-cancelled test asserts the
  positive direction. No regression to other phase timestamps or any cancellation/failover/replay
  invariant.

## Gap 03 — spine: `attempts[]` + `first_upstream_byte_ms`. Gate B. ✅

> FEATURES.md item 2 (the spine). ADDITIVE backend-only — new OPTIONAL fields the React app
> ignores until surfaces 11 (attempt-trace UI) + 12 (per-provider metrics) consume them. Operator
> question: "Which provider failed, why, how long did we wait, and what eventually served?"

### What was added
- `src/dashboard_flow.rs`: new types `AttemptStatus{served,failed}`, `AttemptErrorClass{connect,
  http_status,timeout,stream,terminal,other}`, `AttemptFailoverReason{provider_failed,
  terminal_no_failover}` (all snake_case, Serialize+Deserialize, BOUNDED taxonomic — NOT raw
  upstream text), and `Attempt{provider,model,start_ms,end_ms,first_upstream_byte_ms,status,
  error_class,failover_reason}`. Added `attempts: Vec<Attempt>` + `first_upstream_byte_ms:
  Option<u128>` to `FlowRecord` AND `SnapshotFlowSummary` (body-free), `attempts` to
  `TerminalMetricsInputs` (the evict-safe terminal payload spec 12 reads). New store mutator
  `record_attempts(api_call_id, attempts, first_byte)` — REPLACES attempts (the token holds the
  complete ordered trace), first-write-wins on the byte time. `summary_bytes()` now counts each
  attempt's two scalar strings (quota safety). `open()` inits both fields; `from_record` projects
  them.
- `src/upstream.rs` (`ServingToken` — the evict-safe carrier, same as usage/model): added
  `attempts: Vec<Attempt>` + `first_upstream_byte_ms` to `ServingInfo`, methods `record_attempt`
  (pushes in order; a SERVED attempt sets the flow-level wire-first-byte first-write-wins; a FAILED
  attempt never sets it) and `attempts_snapshot()`. New free fn `classify_attempt_error(&AppError)`
  → bounded `AttemptErrorClass` (matches llmconduit's OWN fixed error strings + HTTP status +
  Terminal disposition, never raw upstream bodies). New `now_epoch_ms_u128()`.
- Failover loop `stream_chat_completion_with_provider_indices`: records ONE `Attempt` per provider
  it tries (connect-error / prefetch-failure → FAILED with taxonomic class + failover_reason;
  prefetch-success → SERVED with `first_upstream_byte_ms = now` measured AT THE PREFETCH POINT). A
  Terminal-disposition connect error records a `terminal_no_failover` attempt then returns.
- Bare-leaf path (`ReqwestUpstreamClient`, `tag_primary_provider` only): `stream_chat_completion`
  was split — `dispatch_chat_stream` holds the POST + G1 shrink-retry; the bare path wraps a
  successful stream with `record_served_attempt_on_first_byte` (records ONE served attempt stamped
  when the FIRST chunk arrives on the wire — the bare analogue of the prefetch point; a stream that
  yields zero items → FAILED `stream` class, `first_byte=None`) and records a FAILED attempt on a
  dispatch error (`failed_bare_attempt`). Nested leaves are NOT bare-marked, so only the failover
  loop records there — no double-count.
- L1 `TelemetryGuard::finalize`: reads `serving.attempts_snapshot()` (BEFORE store.finalize) and
  threads it into BOTH `TerminalMetricsInputs.attempts` (evict-safe payload) AND the FlowStore
  record via `record_attempts` — so the record and the metrics path carry identical data, and an
  evicted record still leaves the attempts on the terminal payload.

### Design decisions
- **Why the `ServingToken` carries attempts:** it is the SAME evict-safe seam D5 already uses for
  usage/model/route/provider; the guard reads it at finalize independent of FlowStore retention, so
  spec 12 aggregates per-provider metrics from the terminal payload without re-reading the evictable
  record (spec acceptance: attempts reach the evict-safe terminal payload).
- **`first_upstream_byte_ms` is wire TTFB, distinct from gap-02 `first_content_delta_ms`** (the
  client-facing content TTFT). Per-attempt (on each `Attempt`) AND flow-level (the served attempt's,
  first-write-wins). Measured at the prefetch point (failover) / bare-leaf first-chunk seam.
- **Mid-stream failure appends NO attempt** (`stream_after_prefetch` left untouched): the
  failover-pre-first-chunk invariant holds — a mid-stream provider error terminates the
  already-recorded served attempt's stream as an error, it does not start a new attempt.
- **Routing mode**: routing delegates to the selected provider's failover client, so attempts come
  from THAT loop only — never a sibling routing upstream (AGENTS.md hard rule), satisfied
  structurally with no routing-layer attempt recording.
- **don't-lie-with-zeros**: every unmeasured per-attempt time + the flow-level byte time is
  `Option`/`None` → absent JSON (`skip_serializing_if`), never `0`. `error_class`/`failover_reason`
  are `None` on the served attempt.

### Files changed
- `src/dashboard_flow.rs` — types + record/summary/terminal-payload fields + `record_attempts` +
  guard finalize threading + 7 tests.
- `src/upstream.rs` — `ServingToken` attempts seam + `classify_attempt_error` + failover-loop +
  bare-leaf wrapper/split + 4 tests.

### Tests added (proving the new behavior)
- `dashboard_flow`: `record_attempts_threads_onto_record_and_summary` (single success → len==1);
  `record_attempts_failover_trace_and_first_byte_first_write_wins` (failed+served → ≥2, byte
  first-write-wins); `record_attempts_no_upstream_byte_is_none_and_absent` (don't-lie-with-zeros —
  absent on the wire); `snapshot_summary_attempts_round_trip_and_bounded_codes` (deserialize→
  serialize round-trip of the new `attempts[]` wire payload; bounded snake_case codes, no raw text);
  `guard_finalize_threads_attempts_into_record_and_terminal_payload` (BOTH sinks);
  `attempts_survive_record_eviction_before_finalize` (record evicted before finalize via TTL AND cap
  → terminal payload still carries ALL attempts).
- `upstream`: `serving_token_records_attempts_and_first_byte_first_write_wins`;
  `classify_attempt_error_is_bounded_taxonomy`;
  `bare_leaf_single_success_records_one_served_attempt_with_first_byte` (real wiremock 200 → 1
  served attempt, measured wire first-byte); `bare_leaf_dispatch_failure_records_failed_attempt_
  with_no_first_byte` (wiremock 503 → 1 failed, first_byte None);
  `failover_503_then_200_records_failed_then_served_attempts` (real wiremock failover → first FAILED
  with failover_reason + no first-byte, last SERVED with measured first-byte; also covers routing's
  attempt source).

### Gate results
- **B:** `cargo test` — all binaries green (0 failed; lib unit 586, +11 new) · `cargo clippy
  --all-targets` exit 0 (clean, zero warnings) · `cargo fmt --check` clean.

### Notes / discoveries
- The bare-leaf served-attempt first-byte is measured by wrapping the returned `UpstreamStream` in
  an `async_stream` that stamps on the first yielded item (mirrors `stream_after_prefetch`'s
  pattern) — pure pass-through, never buffers/duplicates tokens, so the failover-pre-first-chunk
  invariant is untouched.
- `SnapshotFlowSummary` stays Serialize-only (gap-02 precedent); the round-trip test exercises the
  new `attempts[]` payload via `Attempt`'s Serialize+Deserialize, as gap-02 did for `PhaseTimings`.
- No frontend touched (additive spine gap; surfaces 11/12 consume these later).

## Gap 03 — review round 1 (Codex-xhigh on `b574ab60`) — 4 findings fixed (1 HIGH, 2 MEDIUM, 1 LOW). Gate B. ✅

**Reviewed commit:** `b574ab60dd8e92a3456e441a74e6bbb07bcfae5f` (the gap 03 feature commit).
All four findings fixed completely; gap 03 stays `- [x]`.

### F1 (HIGH) — `first_upstream_byte_ms` was not wire TTFB
`src/upstream.rs`. Previously a SERVED attempt was stamped only when the parsed SSE chunk was
yielded, and an HTTP-status failure recorded `None` even though `send().await` had already received
response headers.
- Added a PER-ATTEMPT scratch slot `attempt_header_byte_ms` to `ServingInfo`, with
  `arm_attempt_header_byte` (reset to `None` before each dispatch), `stamp_attempt_header_byte`
  (first-write-wins, set by the leaf), and `take_attempt_header_byte` (read by the caller) on
  `ServingToken`.
- The leaf (`dispatch_chat_stream`, now threaded `serving`) stamps the slot the instant
  `logged_send_chat_request` returns — BEFORE inspecting the status — so BOTH a 2xx and a non-2xx
  capture the TRUE wire byte time. A connect/timeout-BEFORE-response never reaches the stamp (the
  `?` propagated the transport error), so the slot stays `None`.
- The failover loop (`stream_chat_completion_with_provider_indices`) arms the slot before each
  `stream_chat_completion`, then reads it after the dispatch resolves and passes it to
  `record_attempt` for the served arm, the prefetch-failure arm, the generic `Err` arm, AND the
  terminal arm — replacing the old hardcoded `Some(now_at_chunk)`/`None`. Attempts dispatch strictly
  sequentially within one flow, so a single slot is race-free.
- The bare-leaf path mirrors this: `record_served_attempt_on_first_byte` + `failed_bare_attempt`
  now take `header_byte_ms` and use it as the attempt's first-byte (the served attempt is still
  RECORDED at first-chunk yield to distinguish a zero-chunk stream, but its measured byte is the
  header time). `None` is left ONLY for connect/timeout-before-response (don't-lie-with-zeros
  preserved); the failover-pre-first-chunk invariant is untouched (this only records).

### F2 (MEDIUM) — `classify_attempt_error` substring-matched raw `err.to_string()`
`src/upstream.rs:classify_attempt_error`. The old code `contains()`-scanned the FULL display text,
which interpolates the redacted-but-attacker-influenced upstream body
(`"upstream chat failed with {status}: {body}"`), so a body containing "timed out"/"request failed"
could flip the bounded code.
- Rewrote to classify from STRUCTURED metadata + FIXED gateway-emitted PREFIXES only: `Terminal`
  disposition → `Terminal`; `starts_with("upstream chat request/models/completions failed:")` →
  `Connect`; exact body-free markers `== "upstream stream timed out"` → `Timeout` and
  `== "upstream stream ended before the first chunk"` → `Stream`;
  `starts_with("upstream chat failed with ")` → `HttpStatus`; everything else → `Other`.
- The `{body}` is always interpolated strictly AFTER a fixed prefix, so `starts_with` on that prefix
  is immune to body content. REMOVED the blanket `(400..600)` status fallback: every
  `AppError::upstream` collapses to 502, so it had mislabeled generic gateway errors (cooldown /
  "no models" / "all providers failed") as `HttpStatus` — they are now honestly `Other`.

### F3 (MEDIUM) — attempt scalars bypassed the store's `SCALAR_CAP`
`src/dashboard_flow.rs` + `src/upstream.rs`. Uncapped `provider`/`model` strings on `ServingToken`
were cloned into `TerminalMetricsInputs` and `FlowRecord`, bypassing the store invariant.
- Added `Attempt::capped()` (`dashboard_flow.rs`) that `cap_scalar`-bounds `provider`/`model`
  (REUSING the existing helper/const — no second cap; bounded enums + `u128` need none).
- Called it at the single retention choke point `ServingToken::record_attempt` (`upstream.rs`), so
  the bounded copy is what rides BOTH the record AND the evict-safe terminal payload (the store's
  `record_attempts` replaces the vector wholesale and does not re-cap).

### F4 (LOW) — round-trip test only covered nested `attempts[]`, not the top-level field
`src/dashboard_flow.rs`. The claimed wire round-trip only deserialized the `attempts` sub-array; the
new top-level `SnapshotFlowSummary.first_upstream_byte_ms` was never round-tripped.
- Added `snapshot_summary_full_dto_round_trip_covers_both_new_wire_fields`: serializes the WHOLE
  production `SnapshotFlowSummary`, deserializes it into an EQUIVALENT DTO covering BOTH new wire
  fields (top-level `first_upstream_byte_ms` + nested `attempts[]`) in one pass, re-serializes, and
  asserts both are byte-identical to the production wire. Also asserts the unmeasured case (absent
  top-level key on the wire → deserializes back to `None`, never `0`). `SnapshotFlowSummary` stays
  `Serialize`-only (gap-02 precedent); the DTO carries `status` as a `String` (FlowStatus is
  Serialize-only and emits a snake_case string).

### Tests added / changed (proving each fix)
- `upstream`: `classify_attempt_error_ignores_upstream_body_text` (F2 — hostile bodies containing
  "timed out"/"request failed"/"failed to parse" stay `HttpStatus`; a generic 502 is `Other`);
  RENAMED `bare_leaf_dispatch_failure_…` → `bare_leaf_http_status_failure_records_failed_attempt_
  with_measured_first_byte` (F1 — a 503 failure NOW carries a measured byte time);
  NEW `bare_leaf_connect_failure_records_failed_attempt_with_no_first_byte` (F1 — connect-refused →
  `Connect` class, byte `None`); NEW `failover_connect_refused_then_200_leaves_failed_first_byte_
  none` (F1 — failover connect refusal stays `None`, served carries a byte); NEW
  `serving_token_caps_attempt_scalar_strings` (F3 — 8 KiB provider/model truncated to ≤4 KiB);
  UPDATED `failover_503_then_200_…` (the 503 failed provider now asserts `first_byte.is_some()`).
- `dashboard_flow`: NEW `snapshot_summary_full_dto_round_trip_covers_both_new_wire_fields` (F4).

### Gate results
- **B:** `cargo test` — all binaries green (0 failed; lib unit 591) · `cargo clippy --all-targets`
  exit 0 (clean, zero warnings) · `cargo fmt` applied (clean).

### Notes / discoveries
- F1's flow-level `first_upstream_byte_ms` still seeds ONLY from the SERVED attempt's byte time
  (first-write-wins) — a flow where every attempt fails (even with headers received) keeps the
  flow-level value `None`, matching the field's "wire TTFB of the SERVING attempt" semantics.
- A failed attempt that DID receive headers (503, or a stream-ended/timeout AFTER headers) now
  carries a per-attempt `first_upstream_byte_ms` — only the per-attempt field; it never seeds the
  flow-level value (that stays the served attempt's).
- No frontend touched; no public API/wire shape change beyond the already-additive gap-03 fields.

## Gap 03 — review round 2 (Codex-xhigh on `11860aec`) — 1 MEDIUM regression fixed. Gate B. ✅

**Reviewed commit:** `11860aec4f357a9318d775b592b18f686b9eff0b` (the round-1 fix commit).
The round-1 F2 fix (replacing the unsafe `contains()` body scan with fixed-prefix `starts_with`)
inadvertently dropped the two first-chunk failure prefixes, so they fell through to `Other`.
Fixed completely; gap 03 stays `- [x]`.

### F-R2 (MEDIUM) — first-chunk parse/read failures regressed from `Stream` to `Other`
`src/upstream.rs` (`classify_attempt_error`, before the `Other` fallback). `stream_success_response`
emits two FIXED gateway-owned prefixes for first-chunk failures:
- `"failed to parse upstream chat chunk: {err}; payload={redacted}"` (line ~3792), and
- `"failed to read upstream SSE: {err}"` (line ~3802).
After round-1's F2 fix removed the substring scan, neither matched any branch and both fell to
`Other`, regressing the gap-03 `Stream` taxonomy (the `AttemptErrorClass::Stream` docstring
explicitly covers "the first chunk could not be read/parsed").
- Added two `starts_with(...)` branches mapping `"failed to parse upstream chat chunk:"` and
  `"failed to read upstream SSE:"` → `AttemptErrorClass::Stream`, placed BEFORE the `Other`
  fallback. Both fixed prefixes precede ALL interpolation (`{err}` = serde/transport error, plus a
  redacted `{body}` strictly AFTER the prefix), so `starts_with` on them is immune to
  payload/body content — it does NOT reintroduce the F2 substring scan. Confirmed the exact
  literals against `stream_success_response` (not paraphrased).
- Updated the function's leading doc comment to record the two Stream prefixes.

### Tests added / changed
- `upstream::classify_attempt_error_is_bounded_taxonomy`: added two assertions — a first-chunk
  parse failure (`"failed to parse upstream chat chunk: …; payload=<redacted>"`) and an SSE read
  failure (`"failed to read upstream SSE: connection reset by peer"`) each classify as `Stream`
  (not `Other`).
- `upstream::classify_attempt_error_ignores_upstream_body_text`: extended the hostile-body loop with
  two bodies echoing the new Stream prefixes (`"failed to parse upstream chat chunk: injected"`,
  `"failed to read upstream SSE: injected"`) inside an `"upstream chat failed with 500: {body}"`
  envelope — proving a genuine HTTP-status failure stays `HttpStatus` and the body can never flip it
  to `Stream` (the Stream prefixes sit AFTER the fixed HTTP-status prefix).

### Gate results
- **B:** `cargo test` — all binaries green (0 failed; lib unit 591) · `cargo clippy --all-targets`
  exit 0 (clean, zero warnings) · `cargo fmt` applied (clean).

### Notes / discoveries
- All prior gap-03 fixes intact: true wire TTFB (F1), capped scalars (F3), full round-trip (F4),
  and structured classification for HTTP-status / timeout / connect / terminal cases (F2). This
  round only restores the `Stream` leg for the two first-chunk-failure prefixes.
- No frontend touched; no wire shape change (the taxonomy enum is unchanged — `Stream` already
  existed; this only routes the two prefixes to it).

---

## gap 04 review round 1 — suppress sensitive configured headers, normalize key candidates, thread attribution to FlowRow

**Review:** Codex-xhigh on commit `dd9c8b71330d8735bfb9d48ec1aae61ec09a28b0` (4 findings: 1 HIGH key-leak, 2 MEDIUM, 1 LOW). All fixed.

### F1 (HIGH, security) — sensitive configured header leaked verbatim (`dashboard_flow.rs` `ClientAttribution::derive`)
- Before: a configured caller-id header (`LLMCONDUIT_DASHBOARD_CLIENT_HEADER=api-key` / `authorization`)
  emitted its RAW value verbatim into `client_label` because only `Authorization`-bearer + `x-api-key`
  were hashed. A sensitive header configured as the caller-id therefore leaked the raw key.
- Fix: rewrote `derive` around the SINGLE sensitivity authority `redaction::is_sensitive_payload_key`
  (same normalization the capture seam uses — strips `-`/`_`, lowercases; covers `api-key`/`apikey`/
  `authorization`/etc.). The configured header is classified ONCE: if its NAME is sensitive it joins the
  ordered KEY-CANDIDATE list `[bearer, x-api-key, configured-if-sensitive]` and is HASHED (one-way
  SHA-256 prefix), never verbatim. The verbatim `ConfiguredHeader` branch is now gated
  `!configured_is_sensitive`, so a sensitive configured header can NEVER take the verbatim path — if it
  carried no usable key it is SUPPRESSED (derivation falls through to UA), not leaked. The non-sensitive
  `x-client-id` happy path is unchanged.

### F2 (MEDIUM) — empty/`Bearer`-only Authorization suppressed a valid `x-api-key` / fabricated a hash
- Before: `bearer_token(headers).or_else(|| api_key_header(headers))` chose `Authorization` first and
  `bearer_token` returned `Some` even for `Authorization: Bearer` (no token) or blank, so the `or_else`
  never fell through; `Authorization: Bearer` also hashed the literal word `"Bearer"`.
- Fix: each key candidate is `.trim()`-normalized and a BLANK one is SKIPPED in the candidate loop
  BEFORE the next is tried, so an empty bearer falls through to `x-api-key`. Hardened `bearer_token` to
  strip the `Bearer` scheme word whether followed by whitespace OR end-of-string (previously required a
  trailing space, so the exact value `"Bearer"` slipped through as a raw credential and was hashed) —
  it now yields an empty token (skipped). UTF-8-safe (`get(..6)` + ASCII-boundary slice; no panic).

### F3 (MEDIUM) — `FlowRow` dropped the attribution (`dashboard_api.rs`)
- Added OPTIONAL `client_label`/`client_source` (`#[serde(skip_serializing_if = "Option::is_none")]`,
  imported `ClientSource`); populated in BOTH `from_record` and `from_summary`. So `GET /dashboard/api/flows`
  and the REST `/snapshot` summaries now carry the attribution. Additive/optional — frontend ignores
  until gap 15. Absent stays absent (no fabricated value).

### F4 (LOW) — full round-trip didn't cover the gap-04 wire fields (`dashboard_flow.rs`)
- Extended `snapshot_summary_full_dto_round_trip_covers_both_new_wire_fields`: the equivalent DTO now
  also deserializes `client_label` + `client_source`; opened a record WITH a key-hash attribution and
  asserted BOTH survive serialize→deserialize→re-serialize losslessly (PRESENT), and that an
  unattributed flow OMITS both keys and deserializes them back to `None` (ABSENT). Also asserts the raw
  key is absent from the summary wire.

### Tests added / changed
- `dashboard_flow::client_attribution_sensitive_configured_header_is_hashed_not_leaked` (F1): `api-key` /
  `authorization` configured ⇒ `KeyHash` label (`key-<hex>`), raw value absent from label AND Debug dump;
  hashes identically to the canonical `x-api-key` path (one audited mapping).
- `dashboard_flow::client_attribution_sensitive_configured_header_blank_is_suppressed_not_verbatim` (F1):
  blank sensitive configured header ⇒ falls through to UA; absent ⇒ `None`/`None` (never verbatim).
- `dashboard_flow::client_attribution_empty_bearer_falls_through_to_x_api_key` (F2): `Authorization: Bearer`
  (+ valid `x-api-key`) ⇒ hashes the x-api-key, never the literal; `Bearer    ` + UA ⇒ UA fallback.
  (This test caught the no-trailing-space `"Bearer"` bug before commit.)
- `dashboard_api::flow_row_serializes_optional_client_attribution_present_and_absent` (F3): PRESENT emits
  both snake_case keys; ABSENT omits both. Updated the `apply_paging` test's `FlowRow` literal.
- Extended the full-summary round-trip test (F4) as above.

### Gate results
- **B:** `cargo test` — all binaries green (0 failed; lib unit 602) · `cargo clippy --all-targets`
  exit 0 (clean, zero warnings) · `cargo fmt` applied (clean).

### Notes / discoveries
- All prior gap-04 guarantees preserved: raw key NEVER stored/logged/emitted (re-asserted absent from
  Debug dump AND serialized JSON, including the new configured-header path); not on the persisted
  `Config` (header NAME still env-only); `redact_payload_secrets` / header redaction not bypassed;
  `None`/`None` when no client info. The fix is contained to `derive` + `bearer_token` + the `FlowRow`
  projection + tests; no wire-enum change, no frontend touched.

---

## gap 04 review round 2 — normalize configured authorization header, no scheme-literal hash. Gate B. ✅

**Review:** Codex-xhigh on commit `aa37eda28c68a3b283f0d2c564b2b4abac3f7396` (the round-1 fix commit) —
1 MEDIUM edge-case fixed completely. Gap 04 stays `- [x]`.

### F-R2 (MEDIUM) — configured `authorization` re-read raw, fabricating a hash of the literal `"Bearer"`
`src/dashboard_flow.rs` (`ClientAttribution::derive`, the configured-sensitive key candidate `[2]`).
When `LLMCONDUIT_DASHBOARD_CLIENT_HEADER=authorization` AND the request carried a token-less
`Authorization: Bearer` / `Bearer   ` (whitespace-only), the canonical bearer candidate `[0]`
(`bearer_token`) correctly normalized the scheme away → an empty token → skipped. But the
configured-sensitive fallback `[2]` re-read the RAW header via `header_value` and saw the literal
`"Bearer"` (non-empty after trim), so it HASHED the scheme word — fabricating a `client_label`
(`source = KeyHash`) instead of falling through to `x-api-key → UA → None`.

- Root cause: candidate `[2]` is only reached when `[0]`/`[1]` both yield blank, and for a configured
  `authorization` header it read the raw value rather than the scheme-normalized one. For a configured
  `authorization` name, `[2]` is fully redundant with `[0]` (both target the `Authorization` header),
  but `[2]` lacked the `Bearer` normalization.
- Fix (finding's option **a**, applied surgically): when the configured header name is the
  `Authorization` carrier (matched case-insensitively against `axum::http::header::AUTHORIZATION`),
  candidate `[2]` now sources the value through the SAME `bearer_token` normalization as `[0]` — so a
  token-less `Bearer`/`Bearer   ` yields an empty token (skipped), never the scheme literal hashed. For
  any OTHER sensitive alias (`api-key`, `bearer-token`, …) — which the canonical `[0]`/`[1]` candidates
  do NOT read (`[1]` reads only the literal `x-api-key` header, not the `api-key` alias) — the raw
  `header_value` is still the key to hash, preserving the round-1 F1 behavior. This also removes a latent
  inconsistency where a configured `authorization: Bearer <tok>` reaching `[2]` would have hashed
  `"Bearer <tok>"` instead of the bare token (now scheme-stripped, identical to the canonical path).
- Updated the `derive` doc comment + the inline candidate-`[2]` comment to record the normalization.

### Tests added / changed
- `dashboard_flow::client_attribution_configured_authorization_tokenless_bearer_falls_through` (new
  regression): with `configured = authorization` — (a) `Authorization: Bearer` (no token) + a valid
  `x-api-key` ⇒ falls through to the x-api-key hash (label == hashing the x-api-key alone; literal never
  hashed); (b) `Bearer   ` (whitespace-only) + only a UA ⇒ UA fallback; (c) `Bearer` (no token) + no
  other signal ⇒ `None`/`None`; (d) HAPPY PATH `Bearer sk-…REALTOKEN` ⇒ `KeyHash` hashing the
  scheme-stripped token (label == canonical bearer path), with neither the scheme word nor the raw token
  present in the label. Every leg asserts `"Bearer"` is absent from the label and Debug dump.
- All round-1 gap-04 tests kept passing unchanged (sensitive configured header hashed-not-leaked,
  blank-suppressed-to-UA, empty-bearer-to-x-api-key, FlowRow optional attribution, full round-trip).

### Gate results
- **B:** `cargo test` — all binaries green (0 failed; lib unit 603) · `cargo clippy --all-targets`
  exit 0 (clean, zero warnings) · `cargo fmt` applied (clean).

### Notes / discoveries
- All prior gap-04 guarantees + round-1 fixes preserved: raw key/scheme literal NEVER hashed or emitted;
  sensitive configured headers hashed-or-suppressed (never verbatim, F1); non-empty bearer required (F2);
  `FlowRow` optional attribution (F3); full round-trip (F4). The fix is contained to `derive`'s candidate
  `[2]` + its doc comments + one new regression test; no wire-enum change, no `bearer_token` change, no
  frontend touched.

## Gap 05 — spine: gated upstream RESPONSE/ERROR-body capture. Gate B. ✅

### Operator question answered
"When a request failed, what did the upstream actually say back?" — the upstream response/error
body is now captured (opt-in, bounded) onto the live flow record so the dashboard (gap 14) can show
WHY a turn failed. Verified starting state by code search: ONLY the 3 REQUEST layers were captured
(`inbound_body`, `normalized`, `upstream_body` = the upstream *request*, post-sanitize); there was NO
upstream response/error-body field — this is a genuinely new capture seam.

### What was built (backend only; frontend untouched — additive optional field, ignored until gap 14)
- **New field on the LIVE record:** `FlowRecord.upstream_response: Option<UpstreamResponseBody>` where
  `UpstreamResponseBody { bytes: Arc<[u8]>, truncated: bool }` (`src/dashboard_flow.rs`). NOT added to
  `SnapshotFlowSummary` — bodies live ONLY on the live record (the 135 GiB worst-case, body-free-snapshot
  invariant; AGENTS.md). The body is part of the eviction target: counted in `summary_bytes` +
  `body_bytes`, shed in `enforce_summary_quota` phase 1 (record survives body-free), nulled in the COW
  body-shed path.
- **Capped/redacting capture (copy, NEVER slice):** new `capture_response_body(raw: &[u8]) ->
  CapturedResponseBody` runs the SAME shared O(CAP) `redaction::capture_capped_redacted` primitive the
  request layers use (BODY_CAP = 128 KiB, SCALAR_CAP = 4 KiB), wrapping the result in a fresh
  `Arc<[u8]>`. So no `Bytes` slice of the 256 MiB middleware body buffer is retained (a slice would keep
  the whole allocation alive — AGENTS.md anti-pattern). `truncated = raw.len() > BODY_CAP` (reflects the
  RAW length, independent of whether the redacted output is the bounded marker). `CapturedResponseBody`
  is the ONLY way to mint a stored response body (newtype seam, mirrors `CapturedBody`), so a caller
  cannot hand the store an unredacted/over-cap/slice-retaining body.
- **Separate gate, OFF by default:** `DashboardFlowStore` gained `response_capture_enabled: bool` read
  env-only from `LLMCONDUIT_DASHBOARD_CAPTURE_UPSTREAM_RESPONSE` (`1`/`true`/`yes`) in `new()` — DISTINCT
  from the debug-UI gate that arms request capture. `is_response_capture_enabled() = enabled &&
  response_capture_enabled`. Never persisted on the `Debug`/`Clone` `Config` (mirrors dashboard-auth
  env-only posture). New mutator `set_upstream_response(id, Option<CapturedResponseBody>)` no-ops unless
  the gate is on; joins by `api_call_id` OR `response_id` via the link index (mirrors `set_upstream`).
- **Leaf wiring (`src/upstream.rs`):** new `ReqwestUpstreamClient::capture_upstream_response_body(
  response_id, body: &str)` (no-op unless the gate is on AND a `response_id` is threaded) copies the
  already-read upstream error TEXT (a `String`, not the inbound buffer) through `capture_response_body`.
  Called at the THREE terminal-error sites in `dispatch_chat_stream`: the first-attempt non-2xx, and the
  shrink-and-retry's final body (both the Terminal context-overflow and the normal retry-failed paths) —
  last-writer-wins, so the body that ACTUALLY ended the turn is the one shown. The success path captures
  nothing here (no error body); `/v1/completions` stays NOT instrumented (whitelist unchanged).

### Data quality (don't-lie-with-zeros tri-state)
- `None` = capture disabled OR no upstream error body captured.
- `Some(_)` = an error body WAS captured. A genuinely empty upstream body is recorded HONESTLY as the
  fixed `[redacted: unparseable body 0 bytes]` marker (the redactor's non-JSON fallback) — it records the
  body existed and was empty, never a fabricated body and never masquerading as "no error". The
  capture-disabled `None` and the captured-empty `Some(marker)` are never conflated.
- `truncated: bool` flags a cap-truncated (partial) body so the dashboard never presents a partial body
  as complete. Secrets in the error body are redacted by the shared serializer (no trace-log contract
  change — same redaction path as the request layers).

### Tests added (`src/dashboard_flow.rs` unit + `src/upstream.rs` leaf wiremock)
- `upstream_response_captured_when_gate_on` — gate ON: error body lands on the live record, keyed by
  `response_id`, not truncated, content retained.
- `upstream_response_capture_disabled_store_retains_none` + `upstream_response_capture_off_by_default` —
  gate OFF ⇒ `None`, never an empty/fabricated body; request-capture does NOT imply response-capture.
- `upstream_response_empty_body_is_distinct_from_absent` — captured-empty `Some(0-bytes marker)` vs
  capture-disabled `None`.
- `upstream_response_over_cap_is_truncated_and_flagged` — a body > BODY_CAP is capped (bytes ≤ cap) AND
  `truncated == true`.
- `upstream_response_body_is_redacted` — an `api_key` echoed in the error body is redacted.
- `upstream_response_body_not_on_snapshot_summary` — serialized `SnapshotFlowSummary` carries neither the
  field nor the content (body-free invariant).
- `upstream_response_body_counts_toward_quota_and_evicts` — body is quota-visible and shed under quota
  pressure (record survives body-free). D5 evict-safety.
- `gap05_leaf_captures_upstream_error_body_when_gate_on` / `..._when_gate_off` — REAL `ReqwestUpstreamClient`
  leaf (bare-primary) against a wiremock 500: gate-on lands the body end-to-end; gate-off retains `None`.

### Gate results
- **B:** `cargo test` — all binaries green (0 failed; lib unit 613) · `cargo clippy --all-targets`
  exit 0 (clean, zero warnings) · `cargo fmt` applied (clean).

### Notes / discoveries
- An empty/non-JSON upstream error body becomes the fixed `[redacted: unparseable body N bytes]` marker
  via the redactor's existing non-JSON fallback (not literal empty bytes). This is the honest
  representation — `Some(marker)` distinguishes "captured, body empty/unparseable" from `None` (no
  capture). The tri-state distinction the spec wants lives at the `Some`-vs-`None` boundary, plus the
  `truncated` flag.
- `with_flow_store`/`into_bare_primary` are `pub(crate)`, so the end-to-end leaf-wiring tests live inside
  `src/upstream.rs` (same crate) rather than `tests/gateway.rs`; a `#[cfg(test) pub(crate)]
  new_with_response_capture(bool)` constructor sets the gate deterministically (the production `new()`
  reads a process-global env var, which would be racy across parallel tests).

---

## Gap 05 — review round 1 (Codex-xhigh) — FOLLOW-UP FIX

Codex-xhigh review of gap 05 (commit d579a00) found 2 HIGH issues. Both fixed. Follow-up commit (NOT amend).

### F1 (HIGH, correctness) — `upstream_response` must reflect the TURN's FINAL outcome
**Problem:** the leaf wrote `record.upstream_response` for EVERY non-2xx attempt before `FailoverUpstreamClient`
knew the turn's overall outcome. Provider A 500 → provider B 200 left provider A's error body on a SUCCESSFUL
turn (gap 14 would misclassify it as a turn with a failure body).

**Fix (option b — clear-on-later-success, aligned with the gap-03 served-attempt-is-authoritative model):**
the captured body is STAGED on the shared `ServingToken`, NOT committed straight onto the record.
- New `ServingInfo.pending_response_body: Option<CapturedResponseBody>` + token methods
  `set_pending_response_body` (last-writer-wins per attempt), `clear_pending_response_body`,
  `take_pending_response_body` (`src/upstream.rs`).
- The leaf's `capture_upstream_response_body(serving, body)` now STAGES onto the token (was: wrote the store
  directly via `set_upstream_response(response_id, …)`), still gated on `is_response_capture_enabled()`. Both
  `dispatch_chat_stream` terminal-error sites (first-attempt non-2xx + shrink-and-retry final body) pass `serving`.
- The failover serve-success seam (right after `serving.set_provider(...)`) CLEARS the pending body — a served
  turn has no failure body.
- `TelemetryGuard::finalize` (`src/dashboard_flow.rs`) COMMITS the token's pending body via
  `store.set_upstream_response(&api_call_id, serving.take_pending_response_body())`, right after `record_attempts`
  — the SAME evict-safe token source the attempt trace/usage already use, AFTER the failover layer decided the
  final outcome. `set_upstream_response` is itself gated (no-op when capture off / record evicted).
- Net: record carries a body IFF the turn ultimately FAILED. A 500 → B 200 ⇒ `None`; A 500 → B 500 ⇒ B's (last)
  body. Failover-pre-first-chunk-only + which-provider-serves UNCHANGED. The store method
  `set_upstream_response` contract is unchanged (still `Option<CapturedResponseBody>`); only the production CALLER
  moved from the leaf to the guard.

**Tests added (`src/upstream.rs`, wiremock failover):**
- `gap05_failover_a500_then_b200_commits_no_stale_error_body` — provider A 500 → B 200 ⇒ token's pending body
  cleared by B's serve ⇒ final committed `upstream_response == None` (no stale error body on a successful turn).
- `gap05_failover_all_fail_commits_final_error_body` — A 500 → B 503 (all fail) ⇒ the FINAL (last-tried, B's)
  error body is committed, NOT the first provider's.
- Updated `gap05_leaf_captures_..._when_gate_on` / `..._when_gate_off` to drive the guard's commit
  (`store.set_upstream_response(&api_call_id, token.take_pending_response_body())`) — proving the staged body
  lands on the record on a genuinely-failed bare-leaf turn, and `None` when the gate is off.

### F2 (HIGH, completeness) — project `FlowRecord.upstream_response` into `FlowDetailBody`
**Problem:** the captured body was unreachable through `/dashboard/api/flows/:id` — never projected into
`FlowDetailBody`, and no wire round-trip coverage for the dashboard / gap 14 to consume.

**Fix (`src/dashboard_api.rs`):**
- New `FlowUpstreamResponse { body: serde_json::Value, truncated: bool }` (derives `Serialize + Deserialize`).
- New OPTIONAL `FlowDetailBody.upstream_response: Option<FlowUpstreamResponse>` with
  `skip_serializing_if = "Option::is_none"` (absent stays absent). Populated in `dashboard_flow_detail` from
  `record.upstream_response` (bytes parsed via the existing `parse_captured_body`, `truncated` alongside).
- DELIBERATELY kept OFF `FlowRow` (list rows) and `SnapshotFlowSummary` (body-free invariant — both untouched).
- `FlowDetailBody` itself stays SERIALIZE-ONLY (it is only ever a response; deriving `Deserialize` on it would
  cascade onto `FlowDelta`/`FlowUsage`/`FlowStatus`). The round-trip is pinned on the self-contained
  `FlowUpstreamResponse` sub-DTO.

**Test added:** `flow_detail_body_upstream_response_round_trips_present_and_absent` — serialize the whole
`FlowDetailBody`, assert the `upstream_response` key+sub-fields, then DESERIALIZE the sub-object back into
`FlowUpstreamResponse`. Covers PRESENT `truncated=false`, PRESENT `truncated=true`, and ABSENT (key OMITTED).

### Prior gap-05 guarantees preserved
Bounded capped-copy via the shared redacting serializer (no 256 MiB `Bytes`-slice retention) · no body on
snapshots · env-gated OFF by default · byte budget/eviction accounting (`summary_bytes`/`body_bytes` + eviction
phase 1 still shed `upstream_response`) · tri-state `None`/empty-marker/`truncated`. All 9 prior gap-05 store
unit tests + the 2 updated leaf tests + the 2 new failover tests + the new detail round-trip test pass.

### Gate results
- **B:** `cargo test` — all suites green (0 failed) · `cargo clippy --all-targets` exit 0 (zero warnings) ·
  `cargo fmt --check` clean.

### Notes
- AGENTS.md gap-05 operational note updated to the final-outcome semantics (staged on `ServingToken`, cleared on
  serve, committed at finalize; A 500→B 200 ⇒ `None`, all-fail ⇒ last body) + the new `FlowDetailBody.upstream_response`
  detail-only wire field. Operational-only; no changelog narrative.
- No regressions; no deferrals.
