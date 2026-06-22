# Argus — Dashboard Features

Argus is the realtime observability dashboard for **llmconduit** (the LLM proxy gateway). It
watches every request flow `client → gateway → upstream provider`, capturing the request
transformation, model routing / failover, tokens, cost, latency, and live output.

This is a catalog of high-value features — what is shipped and what is proposed — each with the
insight it delivers and how it fits the existing architecture: the authoritative
`DashboardFlowStore` (D1), `MetricsLayer` (D5), `MonitorHub` transcript ring, `ProviderHealth`
(D4), the price table (D13), and the "Night Watch" instrument design system (`src/design/`).

**Legend** — ✅ shipped · 🔭 proposed · ⚙️ needs a backend seam · ⭐ high priority · 🐞 correctness

**Design ethos.** Every feature reads as an *instrument readout*, not a generic SaaS widget:
mono telemetry numerals (IBM Plex Mono), the iris "Eye" accent for interaction, amber as the
attention-signal, semantic status traffic-lights (mint / amber / red). Honest empty + loading
states ("unavailable", never a fake `0`). DoD matches the project bar: executable test green ·
`tsc` · `eslint --max-warnings 0` · `vitest` · Playwright e2e · keyboard focus · reduced-motion.

---

## Performance & latency

### Per-flow latency breakdown — TTFT · stream · tok/s 🔭⭐
- **What:** replace the single lumped `elapsed` ("3.1s") with a timing waterfall — `queue →
  time-to-first-token → stream duration` — plus per-flow tokens/sec and inter-token latency.
- **Insight:** *why was THIS request slow?* Distinguishes a slow provider (high TTFT) from a
  long generation (many tokens) — the defining LLM-serving metric, today invisible.
- **Fits:** derive from the timestamped monitor timeline events (`DebugTimelineEvent.timestamp_ms`)
  + `started_ms` / `finished_ms` + `Usage.completion`. **Frontend-only, no backend change.** Lands
  as a "Timing" section in the inspector header (beside cost/elapsed) + a thin waterfall bar (iris
  stream segment, amber when TTFT crosses a threshold).
- **Effort:** M. *Caveat:* TTFT precision is bounded by the first `output` segment's timestamp
  granularity — surface "first-activity latency" honestly if events are coarsely batched.

### Per-provider latency distribution ⚙️🔭
- **What:** p50/p95/p99 + error rate *per upstream*, in the topology node tooltip and a per-provider
  stat row.
- **Insight:** which provider is degrading — today `CooldownTooltip` shows the GLOBAL window p99,
  not the node's own.
- **Fits:** extend the D4 `ProviderHealth` DTO (status / cooling_until_ms / served_count /
  failover_count / consecutive_failures / catalog) with per-provider latency percentiles, fed by a
  per-provider ring in `MetricsLayer`. Drives node sizing/color + the tooltip. **Backend seam** (Rust)
  + frontend.
- **Effort:** M–L.

### Outlier / slow-request spotlight 🔭
- **What:** auto-flag flows whose latency or tok/s is a statistical outlier for their model; a
  "slowest" quick-filter chip.
- **Insight:** surface the tail without scrolling the table.
- **Fits:** compute from FlowStore rows + `MetricsLayer` percentiles client-side; a filter chip +
  a subtle amber row marker (reuse the inspector's match-marker treatment). Frontend.
- **Effort:** S–M.

---

## Cost & tokens

### Token economics — cached · reasoning · cache-hit % 🔭⭐
- **What:** surface the `cached` and `reasoning` token counts **already captured** in `Usage` but
  dropped by the UI; show cache-hit % and reasoning-token share per flow and in aggregate.
- **Insight:** cache hits = direct cost savings; reasoning tokens = hidden spend on reasoning
  models. Today the table shows only `prompt / completion` (e.g. `812 / 512`).
- **Fits:** `Usage` already carries `prompt / completion / total / cached / reasoning` (D3) — it is
  on the wire. Add a tokens breakdown popover on the table's tokens cell + an inspector line, and
  combine with the price table for "$ saved by cache". **Frontend-only — a pure win.**
- **Effort:** S.

### Cost attribution & budgets ⚙️🔭
- **What:** cost rolled up by model / provider / client / window, with optional budget thresholds
  that flip a gauge amber → red.
- **Insight:** *where is the money going, and am I over budget?*
- **Fits:** `MetricsLayer` cost windows + the price table + FlowStore grouping. Rollups land in the
  Analytics view (below); budget thresholds go in the D13 config route. Mostly frontend; budgets
  want a small config field.
- **Effort:** M.

---

## Reliability & errors

### Failure deep-dive & error taxonomy 🔭⭐
- **What:** an enriched Error surface — group failures by `terminal_reason` + HTTP class, show the
  error RATE per model/provider, and the upstream error body when captured.
- **Insight:** *what is failing and why*, at a glance, instead of one red row at a time.
- **Fits:** `terminal_reason` + `status` are on every `FlowSummary`; the MonitorHub join already
  carries a monitor error. Enrich the inspector `ErrorTab` + add an error-rate chip to the stats
  strip / topology node. Frontend; retaining the upstream error body is a small D3 seam if not
  already kept.
- **Effort:** M.

### Failover / routing chain 🔭⚙️
- **What:** for a re-routed request (the `FO` badge), show the ordered attempt chain — `provider A
  (failed: reason) → provider B (served)` — as a compact instrument stepper.
- **Insight:** *why* a request failed over and what it cost in latency.
- **Fits:** D2 request identity + `ProviderHealth.failover_count`; needs the per-attempt sequence
  retained on the `FlowRecord` (a D2/D4 seam) — today only the final served upstream shows. Renders
  in the inspector header.
- **Effort:** M–L.

### Cooldown / circuit-breaker timeline 🔭
- **What:** a lane per provider showing health transitions (healthy → cooling → down → recovered)
  across the scrubber window.
- **Insight:** correlate failure bursts with provider cooldowns.
- **Fits:** D4 health + `cooling_until_ms` + the scrubber's time axis (`Scrubber.tsx`). A lane under
  the topology or scrubber. Frontend if health history is retained (else a small ring).
- **Effort:** M.

---

## Flows, search & identity

### Client identity & per-client breakdown 🔭⭐
- **What:** wire the user-agent (D1 already captures it) into a `client` label; fill the table's
  CLIENT column (today `—`) and enable per-client filter + rollup.
- **Insight:** which app/agent drives traffic, cost, and errors.
- **Fits:** the **documented D1/D13 TODO** — D1 captures the UA header; add a derived `client` to the
  `/flows` summary shape (D13). The column already renders honestly; this is pure wiring.
- **Effort:** S.

### Full-text flow search 🔭
- **What:** extend the per-flow JSON search (✅ just shipped) to a TABLE-level search — match across
  captured bodies / headers / model / id, not only the open flow.
- **Insight:** *find every request that sent `temperature=0`, used tool X, or hit provider Y.*
- **Fits:** FlowStore retains capped + redacted bodies (D1); reuse the `viz/jsonFold` search core
  across rows. A search field in the `FilterBar`; matches filter the table. Frontend over retained
  bodies.
- **Effort:** M.

### Flow comparison — diff two flows 🔭
- **What:** select two flows → side-by-side structural diff of their request / normalized / upstream
  bodies + timing + cost.
- **Insight:** *why did this one fail / cost more / route differently than that one?*
- **Fits:** the path-keyed `diffLayers` engine already powers the 3-pane inspector — point it at two
  FLOWS instead of two layers. A "compare" affordance from table multi-select. Frontend.
- **Effort:** M.

### Aggregate analytics view 🔭⭐
- **What:** a 5th nav tab — the "control room" summary: top models/providers by volume · cost ·
  latency · error-rate over the window; cost-over-time; error breakdown; token-mix.
- **Insight:** the operator's at-a-glance *how is the gateway doing* without reading individual flows.
- **Fits:** `MetricsLayer` windows + FlowStore aggregation + price table; a new route in the hash
  router (`flows | topology | sankey | theater | analytics`). Pure frontend over existing data — the
  segmented gauge + uPlot-trend instrument design shines here.
- **Effort:** M–L.

---

## Streaming & content

### Theater enhancements 🔭
- **What:** expandable tool cards (parity with the inspector), a persisted reasoning-visibility
  toggle, a per-stream tok/s sparkline, and pin/fullscreen a single stream.
- **Insight:** watch a live generation in depth — tools + reasoning + rate — not just output text.
- **Fits:** `viz/River.tsx` already renders output/reasoning/tools; the inspector's `DeltasPanel`
  already has expandable tool cards — share the component. Frontend.
- **Effort:** S–M.

---

## Operability

### Export & replay 🔭⚙️
- **What:** export a captured flow as JSON or a ready-to-run `curl`; (gated) REPLAY a request back
  through the gateway.
- **Insight:** reproduce + share a request; replay to validate a fix.
- **Fits:** the flow body is already in the store (export = frontend). Replay rides the D6/D7
  mutation policy + CSRF double-submit (`mutations_enabled` gate) — security-sensitive,
  operator-authorized only, same posture as the Kill button.
- **Effort:** export S; replay M (security review).

### Alerts & thresholds 🔭
- **What:** operator-set thresholds (error% > X, p99 > Y, $/min > Z) that flip the relevant gauge
  amber/red and optionally raise a banner.
- **Insight:** don't stare at the dashboard — let it shout.
- **Fits:** `MetricsLayer` values + the stats-strip gauges (already color-capable — ERR% goes red).
  Threshold config in the D13 config route. Frontend + small config.
- **Effort:** M.

### Command palette & keyboard nav 🔭
- **What:** ⌘K palette — jump to a flow by id, switch views, run a search, toggle live/seek; full
  keyboard nav of the table + inspector.
- **Insight:** operator speed — an instrument you drive without the mouse.
- **Fits:** pure frontend over the existing hash router + stores; squarely on the instrument ethos.
- **Effort:** M.

---

## Foundations (fix before trusting insight)

### Stats-strip accuracy 🐞⭐
- **What:** the headline gauges read `0.0` with fresh real traffic sitting in the table (observed on
  the live vLLM run). Confirm whether it is window-decay or a `MetricsLayer` ↔ FlowStore wiring gap,
  and fix.
- **Why first:** every metric and insight above sits on the headline numbers — if they are wrong,
  trust in the whole instrument erodes.
- **Fits:** `MetricsLayer` (D5) feeding `StatsStrip`; verify the rolling window folds recent flows.
- **Effort:** S–M (investigation).

---

## Suggested build order

A pragmatic sequence — fix the foundation, then ship the cheap pure-frontend wins that use
already-captured data, then the flagship insight, then the bigger views and backend seams.

|  # | feature | why here | size |
|-|-|-|-|
| 1 | Stats-strip accuracy 🐞 | foundation — everything reads off it | S–M |
| 2 | Token economics ⭐ | data already on the wire; pure win | S |
| 3 | Client identity ⭐ | documented TODO; column already renders | S |
| 4 | Per-flow latency breakdown ⭐ | flagship LLM insight; frontend-only | M |
| 5 | Failure deep-dive | turns scattered red rows into a picture | M |
| 6 | Aggregate analytics view ⭐ | operator's at-a-glance control room | M–L |
| 7 | Full-text flow search | extends the inspector search just shipped | M |
| 8 | Theater enhancements | shares inspector components | S–M |
| 9 | Per-provider latency ⚙️ | needs ProviderHealth DTO + ring | M–L |
| 10 | Failover chain ⚙️ | needs per-attempt capture | M–L |
| 11 | Export / replay, Alerts, Command palette | operability polish | M |

---

## Already shipped (for reference)

- ✅ **Transformation inspector** (D10) — 3-pane `inbound → normalized → upstream` structural diff,
  now **per-path collapsible + searchable across all three layers**.
- ✅ **Topology map** (D4/D12) — provider routing graph, click-to-filter, cooldown/health tooltip.
- ✅ **Token Sankey** (D12) — client → gateway → model token flow, cost-ramp bands, click-to-filter.
- ✅ **Theater** (D12) — live per-stream output / reasoning / tool cards, tok/s, fullscreen.
- ✅ **Stats strip** (D11) — req/s · active · err% · p50/p95/p99 · tok/s · $/min gauges + sparklines.
- ✅ **Time-travel scrubber** (D11) — seek to a historical moment; frozen-cut coherence.
- ✅ **Filtering** — status / model / upstream chips, cross-linked from topology + sankey.
- ✅ **Kill** (D6) — abort an in-flight flow (CSRF-gated mutation).
- ✅ **Auth + CSP** (D7), **Night Watch** instrument design system (`src/design/`).
