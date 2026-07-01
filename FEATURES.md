# Argus — Dashboard Features

Argus is the realtime observability dashboard for **llmconduit** (the LLM proxy gateway). It
watches every request flow `client → gateway → upstream provider`, capturing the request
transformation, model routing / failover, tokens, cost, latency, and live output.

This is a catalog of high-value features — shipped and proposed — each with the operator question
it answers and how it fits the existing seams: `DashboardFlowStore` (D1), `MetricsLayer` (D5),
`MonitorHub` transcript ring, `ProviderHealth` (D4), the price table (D13). Design language lives
in `dashboard-frontend/src/design/DESIGN_NOTES.md` ("Night Watch" instrument aesthetic) — it is the
*how it looks*; this doc is the *what it should do and why*.

> v2 — hardened after an adversarial design review. The framing shifted from "pretty flow
> artifacts" to "can Argus answer the incident question?" — *was this slow/expensive/failed because
> of the client, the prompt, routing, failover, provider prefill, decode speed, or our own gateway?*

---

## How to read this

**Status** — ✅ shipped · 🔭 proposed · ⚙️ needs a backend seam · ⭐ high priority · 🐞 correctness

**Data quality** — every metric a feature renders is tagged so the UI never implies more precision
than it has:
- `measured` — recorded directly (a timestamp, a token count on the wire).
- `derived` — computed from measured values (stream tok/s = completion ÷ stream duration).
- `estimated` — an approximation with a known error bound (must be labelled as such in the UI).
- `unavailable` — not captured. **Render it as such — never as `0`.**

**Entry shape** — *Answers* (operator question) · *Data* (quality + required fields/source) ·
*Surface* (where it lands) · *Backend* (seam, if any) · *Risk* (when relevant) · *Effort* (S/M/L).

### Principle: don't lie with zeros
Unknown cost, unreported `cached`/`reasoning` tokens, an un-measured first-token time, a provider
that didn't return usage — all must read `unavailable` / `—`, not a fabricated `0`. A confident
wrong number is worse than an honest gap, and this dashboard is an instrument operators trust during
incidents. The CLIENT column already does this (renders `—`); make it the rule everywhere.

---

## The spine — per-flow execution trace 🔭⚙️⭐

The single highest-value addition, and the backbone the latency / failover / routing / error
features below all hang off. Today a flow is a set of artifacts (bodies, deltas, a lumped
`elapsed`); it is not a *trace*.

- **Answers:** "where did this request spend its time, and what actually happened to it?"
- **Data:** `measured` phases — ingress → normalization → routing decision → upstream attempt(s) →
  first upstream byte → first content token → stream end → finalization. Requires new backend
  timestamps (`first_upstream_byte_ms`, `first_content_delta_ms`) + an `attempts[]` array.
- **Surface:** a horizontal phase waterfall in the inspector header (Night Watch: iris stream
  segment, amber when a phase exceeds threshold), expandable to the attempt/error detail.
- **Backend:** the big one — D2 (identity) + D3 (telemetry guard) emit per-phase timestamps and the
  attempt sequence into the `FlowRecord`. Unblocks honest versions of Latency, Failover, and
  Per-provider latency below.
- **Effort:** M–L. **Do the data-contract part early** (see build order) — many features stay
  dishonest until it exists.

---

## Performance & latency

### Per-flow latency breakdown 🔭⚙️⭐
- **Answers:** "was it slow at the provider (prefill/TTFT) or just a long generation?"
- **Data:** *true* TTFT is `measured` only once the spine adds `first_content_delta_ms`. Until then,
  a frontend-`derived` **"first-visible-activity latency"** (first monitor `output` segment
  `timestamp_ms` − `started_ms`) is the honest fallback — label it as such, it is dashboard-visible
  activity, not upstream first byte. Stream tok/s = `derived` (completion ÷ stream duration).
- **Surface:** a "Timing" line in the inspector header + a slice of the spine waterfall.
- **Backend:** none for the labelled fallback; the real TTFT rides the spine.
- **Effort:** M. *Note:* true per-token cadence ("inter-token latency") is NOT derivable if segments
  are batched — see streaming-stall health for the measured version.

### Streaming stall / inter-token health 🔭⚙️
- **Answers:** "did the model stall mid-generation?"
- **Data:** `measured` `max_inter_chunk_gap_ms`, `stall_count`, `first_chunk_ms` — emitted by the
  backend at chunk arrival (the frontend only sees batched segments, so this can't be faked client-side).
- **Surface:** a stall marker on the theater stream + an inspector timing row.
- **Backend:** D3 records inter-chunk gaps. **Effort:** M.

### Per-provider latency + error distribution 🔭⚙️
- **Answers:** "which upstream is degrading?"
- **Data:** p50/p95/p99 + error rate **per upstream**, from `attempts[]` (the spine) so a failed
  primary is counted — final-served latency alone hides unhealthy providers. `ProviderHealth` today
  is point-in-time and the tooltip shows the *global* p99.
- **Surface:** `CooldownTooltip` + node sizing/color in the topology.
- **Backend:** extend the D4 DTO with per-provider percentiles fed by a per-provider `MetricsLayer`
  ring. **Effort:** M–L.

### Outlier / slow-request spotlight 🔭
- **Answers:** "show me the tail without scrolling."
- **Data:** `derived` — flag flows beyond a percentile for their model.
- **Surface:** a "slowest" filter chip + an amber row marker (reuse the inspector match-marker).
- **Backend:** none. **Effort:** S–M.

---

## Context & tokens

### Context-window utilization 🔭⚙️
- **Answers:** "are we near max context — risking slow prefill, truncation, or 400s?"
- **Data:** `derived` `% = Usage.prompt ÷ max_context` + remaining tokens + overflow risk. Needs the
  served model's `max_context` / `max_model_len` (vLLM reports it on `/v1/models`; e.g. the live
  GLM-5.2 advertised 500k) surfaced via the D4 provider catalog.
- **Surface:** a gauge in the inspector header + an aggregate "context pressure" stat.
- **Backend:** expose per-model max-context in the catalog DTO. **Effort:** M.

### Token economics — cached · reasoning · cache-hit % 🔭⭐
- **Answers:** "is prefix caching saving money, and what are reasoning models really costing?"
- **Data:** `measured` `Usage.{cached, reasoning}` are already on the wire but dropped by the UI.
  **Distinguish `0` from `unavailable`** — a provider that doesn't report cached tokens is not a
  cache miss. `$ saved by cache` is only valid (`derived`) when the price table carries a *separate
  cached-input price* for that model — otherwise show the token split without a dollar claim.
- **Surface:** a breakdown popover on the table's tokens cell + an inspector line + aggregate
  cache-hit rate by model/client.
- **Backend:** none for the split; cached-input pricing is a price-table addition. **Effort:** S.

---

## Reliability & routing

### Failover / attempt trace 🔭⚙️
- **Answers:** "which provider failed, why, how long did we wait, and what served?"
- **Data:** `measured` `attempts[]` (provider, model, start/end, status/error class, first-byte,
  failover reason) — the spine's array. Today only the final `upstream_target` + an `FO` badge show.
- **Surface:** an instrument stepper in the inspector header (`A failed: 503 · 0.8s → B served`).
- **Backend:** capture the attempt sequence (D2/D4). **Effort:** M.

### Failure taxonomy & error deep-dive 🔭⚙️
- **Answers:** "what is failing and why, in aggregate — not one red row at a time?"
- **Data:** `measured` `terminal_reason` + status (already present) grouped + error RATE per
  model/provider. The **upstream response/error body is NOT among the captured bodies** (those are
  the three *request* layers) — surfacing it needs a new, separately-gated response-capture seam.
- **Surface:** enriched inspector `ErrorTab` + an error-rate chip on the stats strip/topology.
- **Backend:** error-body capture is the only new seam; grouping is frontend. **Effort:** M.

### Graceful image degradation 🔭⚙️
- **Answers:** "did this turn's images get placeholdered or rejected because the backend can't see
  images, and why didn't a bad request take the whole provider down with it?"
- **Data:** `measured`, engine-side only today (Topic E / E2 — field incident: a claude-cli
  tool-result image hit a text-only vLLM upstream, which 400'd "not a multimodal model" and cooled
  the provider for 30s, 502-ing every unrelated request for the whole window). Two invariants now
  hold at the gateway: a **request-intrinsic 4xx** (`400`/`413`/`415`/`422`) is `Terminal` — it
  never cools or fails over a healthy provider (`401`/`403`/`404`/`408`/`429`/5xx unchanged); and
  any image still reaching a non-native-vision backend is swept at the engine canonical layer and
  either replaced in place with an instructive text placeholder (the model is told to ask the user
  or request text — never to guess) or the turn is rejected pre-dispatch with an HTTP `400` (never
  `502`; the provider is never contacted) — per `unsupported_image_policy: placeholder|reject`
  (default `placeholder`). A degraded turn emits a `WARN` log + a monitor `ToolPhase` event, but
  neither reaches the dashboard yet.
- **Surface:** a flag/badge on the flow row + inspector detail (`images degraded: {n}, policy: …`).
- **Backend:** needs the same metadata-wrapper seam noted elsewhere in this doc (the engine returns
  a bare `ReceiverStream`, so surfacing this needs a response header or a `FlowRecord` field) —
  documented follow-up, not yet built. **Effort:** S once the wrapper seam exists.

### Provider health history (cooldown timeline) 🔭⚙️
- **Answers:** "did failures line up with a provider cooling/recovering?"
- **Data:** `measured` health transitions (healthy → cooling → down → recovered) over time — needs a
  retained ring; `ProviderHealth` today is point-in-time only, so this is **not** frontend-only.
- **Surface:** a lane per provider under the topology/scrubber, on the scrubber's time axis.
- **Backend:** a small health-transition ring. **Effort:** M.

---

## Cost & reliability targets

### Cost attribution & budgets 🔭⚙️
- **Answers:** "where is the money going, and am I over budget?"
- **Data:** `derived` cost by model/provider/client/window (`MetricsLayer` + price table + grouping).
  Tag rows with `estimated` when any flow lacks a confident price.
- **Surface:** the analytics view (below) + budget thresholds in the D13 config route.
- **Backend:** budget config field. **Effort:** M.

### SLO / error-budget view 🔭⚙️
- **Answers:** "are we inside our reliability/latency target, and how fast are we burning budget?"
- **Data:** `derived` burn-rate against per-model/provider/client targets — more honest than a raw
  red gauge.
- **Surface:** an overview tile + a burn-down sparkline.
- **Backend:** target config. **Effort:** M.

---

## Identity & multi-tenancy

### Client / key / app attribution 🔭⚙️⭐
- **Answers:** "who is generating the cost, errors, latency — or abuse?"
- **Data:** a stable `client_label` from (in priority) auth principal / API-key **hash** / a
  configured header, with user-agent as a *fallback*, not the identity model. **Never expose raw
  secrets** — hash keys. The CLIENT column already renders `—` honestly.
- **Surface:** the CLIENT column + a per-client filter and rollup.
- **Backend:** derive + emit `client_label` on the `/flows` summary (the D1/D13 TODO captures UA;
  key-hash attribution is the stronger seam). **Effort:** M.

---

## Flows, search & comparison

> **Read the Safety & governance section first** — searching and exporting captured bodies is where
> sensitive-data risk concentrates.

### "Effective changes" transform summary 🔭⚙️
- **Answers:** "what did llmconduit actually change before upstream?" — the proxy's whole job.
- **Data:** `measured` a compact semantic diff above the raw 3-pane bodies: profile matched, model
  remap, defaults applied, system-prefix injected, `chat_template_kwargs` merged, tools stripped.
- **Surface:** a summary strip atop the inspector (the raw structural diff stays below).
- **Backend:** emit the applied-transform record (D2/D3 know these decisions). **Effort:** M.

### Full-text flow search 🔭
- **Answers:** "find every request that sent `temperature=0`, used tool X, or hit provider Y."
- **Data:** `measured` over FlowStore's capped+redacted request bodies; reuse the `viz/jsonFold`
  search core across rows.
- **Surface:** a search field in the `FilterBar`; matches filter the table.
- **Risk:** searches captured bodies — gate behind the retention/redaction policy below.
- **Backend:** none. **Effort:** M.

### Flow comparison — diff two flows 🔭
- **Answers:** "why did this one fail / cost more / route differently than that one?"
- **Data:** reuse the path-keyed `diffLayers` engine, pointed at two flows instead of two layers.
- **Surface:** a "compare" affordance from table multi-select. **Backend:** none. **Effort:** M.

---

## Aggregate / overview

### Control-room analytics view 🔭⚙️
- **Answers:** "how is the gateway doing, at a glance?"
- **Data:** top models/providers by volume · cost · latency · error-rate; cost-over-time; token-mix.
  **Gated on data quality** — without per-client, per-attempt, true timing, and price confidence it
  is a pretty summary of incomplete data, so it lands *after* the data-contract pass, not before.
- **Surface:** a 5th hash route (`flows | topology | sankey | theater | overview`) — segmented gauge
  clusters + uPlot trends.
- **Backend:** consumes the spine. **Effort:** M–L.

---

## Streaming

### Theater enhancements 🔭
- **Answers:** "watch a live generation in depth." (Shipped Theater already has tok/s + fullscreen —
  this adds the missing depth, not a re-do.)
- **Data:** expandable tool cards (today NOT expandable — parity with the inspector's `DeltasPanel`),
  a persisted reasoning-visibility toggle, and a stall/inter-token marker (from streaming-stall health).
- **Surface:** `viz/River.tsx`; share the inspector's tool-card component.
- **Backend:** none (stall marker rides streaming-stall health). **Effort:** S–M.

---

## Integration & export

### Prometheus / OpenTelemetry export 🔭⚙️
- **Answers:** "can this feed our real monitoring + alerting, not just a browser tab?"
- **Data:** a `/metrics` scrape endpoint and/or OTel spans/events per flow + attempt + upstream call
  + tool/search call. This is what makes alerting *real* (see below).
- **Surface:** none (it's an egress) — documented endpoints.
- **Backend:** an exporter over `MetricsLayer` + the spine. **Effort:** M–L.

### Export flow as JSON / curl 🔭
- **Answers:** "reproduce or share this exact request."
- **Data:** the flow body is already in the store — pure frontend serialization, redaction-aware.
- **Surface:** an inspector action. **Risk:** strip secrets on export. **Effort:** S.

---

## Operability (gated mutations)

### Replay a request 🔭⚙️
- **Answers:** "re-send this through the gateway to test a fix." Split from export — a *different risk
  class*.
- **Risk:** **dangerous** — spends money, repeats tool/search side effects, can re-expose secrets.
  Requires explicit confirm, header stripping, the D6/D7 `mutations_enabled` + CSRF gate, **and an
  audit log entry** (same posture as Kill, plus audit).
- **Backend:** a guarded replay route. **Effort:** M (security review mandatory).

### Visual thresholds → real alerting 🔭⚙️
- **Answers:** "let the instrument shout instead of being watched."
- **Data:** two tiers — (a) `derived` **visual thresholds** that flip a gauge amber/red in-dashboard
  (frontend + small config), and (b) **real alerting** (error% / p99 / $-spike → webhook/PagerDuty)
  which belongs on the OTel/Prometheus egress, not a banner in a tab.
- **Surface:** stats-strip gauges (already color-capable) + threshold config in D13.
- **Backend:** (b) rides the exporter. **Effort:** M.

---

## Safety & governance

### Retention, sampling & body-capture policy 🔭⚙️
- **Answers:** "what sensitive data are we storing, for how long, and can I turn it down?"
- **Data:** operator knobs — disable/sample body capture, extra header redaction, retention cap,
  export policy. A prerequisite to shipping full-text body search + replay responsibly.
- **Surface:** the D13 config route + a visible "capture: on/sampled/off" indicator.
- **Backend:** D1 capture is already capped+redacting; add the policy knobs. **Effort:** M.

### Abuse / secret-leak detection 🔭⚙️
- **Answers:** "did a request carry credentials, PII, or prompt-injection-looking content?"
- **Data:** deterministic secret/credential patterns over captured bodies+headers first; flag, don't
  block. Be honest that this is a *signal*, not a security product.
- **Surface:** a flag chip on the flow row + an inspector finding.
- **Backend:** a scan pass on capture. **Effort:** M–L.

### Web-search / tool-call observability 🔭⚙️
- **Answers:** "are server-side search/tool loops driving latency or failures?" (llmconduit runs
  bounded web_search + tool handling server-side.)
- **Data:** `measured` search rounds, query count, per-search latency, ceiling-hit/timeout, and
  injected error text; rejected mixed-tool batches.
- **Surface:** an inspector "Tools" tab + an aggregate tool-latency stat.
- **Backend:** emit search/tool spans. **Effort:** M.

---

## UX

### Command palette & keyboard nav 🔭
- **Answers:** operator speed — drive the instrument without the mouse.
- **Data:** none — pure frontend over the hash router + stores.
- **Surface:** ⌘K palette + table/inspector keyboard nav.
- **Effort:** M. *Priority:* nice-to-have; do it after the data model is sound.

---

## Foundations (fix before trusting any insight)

### Stats-strip accuracy 🐞⭐
- **Answers:** the headline gauges read `0.0` with fresh real traffic in the table (observed on the
  live vLLM run). Every number above reads off these — fix first.
- **Suspects (broaden the hunt):** window-decay vs `MetricsLayer`↔FlowStore wiring; server/client
  clock skew; the time-travel seek cursor freezing the strip; a stale WS snapshot; a unit conversion;
  a window-boundary off-by-one; or metrics fed only by *completed* flows (so live ones never count).
- **Surface:** `StatsStrip` ← `MetricsLayer` (D5). **Effort:** S–M (investigation).

---

## Suggested build order

The hard lesson from review: **don't ship UI polish on top of a weak data model** — it produces
attractive summaries of incomplete data. Fix the foundation, harden the data contract, *then* build
the surfaces.

|  # | step | why here | size |
|-|-|-|-|
| 1 | Stats-strip accuracy 🐞 | foundation — everything reads off it | S–M |
| 2 | **Data-contract pass** ⚙️ (spine): per-phase timestamps, `attempts[]`, `client_label`/key-hash, response-body capture flag, price confidence, per-model max-context | unblocks the *honest* version of most features below | M–L |
| 3 | Token economics (with `unavailable` states) ⭐ | already on the wire; cheap honest win | S |
| 4 | Context-window utilization | LLM-specific, high signal | M |
| 5 | Per-flow latency breakdown (real TTFT off the spine) ⭐ | flagship insight, now honest | M |
| 6 | Failover / attempt trace ⚙️ | core gateway value — ahead of analytics | M |
| 7 | Per-provider latency + error distribution ⚙️ | core gateway value | M–L |
| 8 | Failure taxonomy | scattered red rows → a picture | M |
| 9 | Client / key attribution ⭐ | who drives cost/errors | M |
| 10 | Control-room overview ⭐ | now backed by real fields | M–L |
| 11 | Retention/privacy controls ⚙️ → full-text search + flow compare | privacy gate *before* body search | M |
| 12 | Export JSON/curl · effective-changes summary · theater depth | cheap, high-value surfaces | S–M |
| 13 | OTel/Prometheus export → real alerting ⚙️ | feed production monitoring | M–L |
| 14 | Web-search/tool observability · SLO view · abuse scan | deeper ops | M |
| 15 | Replay (gated+audited) · command palette | risk-class + polish, last | M |

---

## Already shipped (for reference)

- ✅ **Transformation inspector** (D10) — 3-pane `inbound → normalized → upstream` *request*
  structural diff, now **per-path collapsible + searchable across all three layers**.
- ✅ **Topology map** (D4/D12) — provider routing graph, click-to-filter, cooldown/health tooltip.
- ✅ **Token Sankey** (D12) — client → gateway → model token flow, cost-ramp bands, click-to-filter.
- ✅ **Theater** (D12) — live per-stream output / reasoning / tool cards, tok/s, fullscreen.
- ✅ **Stats strip** (D11) — req/s · active · err% · p50/p95/p99 · tok/s · $/min gauges + sparklines.
- ✅ **Time-travel scrubber** (D11) — seek to a historical moment; frozen-cut coherence.
- ✅ **Filtering** — status / model / upstream chips, cross-linked from topology + sankey.
- ✅ **Kill** (D6) — abort an in-flight flow (CSRF-gated mutation).
- ✅ **Auth + CSP** (D7), **Night Watch** instrument design system (`src/design/`).
