# D12 — Topology + token-flow Sankey + live stream theater views

> **Source:** DASHBOARD_PLAN.md rev 8 §1.2-1.4. Topic 13. The routing/cost/wow story.

**Priority:** MEDIUM · **Surface:** `dashboard-frontend/src/views/{TopologyView,SankeyView,TheaterView}.tsx`,
`components/viz/{RadialTopology,TokenSankey,River}.tsx`

## Purpose
The three remaining signature views: the upstream topology map (routing story), the token-flow Sankey
(cost story), and the live stream theater (the "wow"). All consume the D3/D5 data over D7's WS frames
+ D13's REST endpoints.

## Jobs to Be Done
- **TopologyView** (`RadialTopology`): radial SVG, gateway center, upstream nodes on a ring. Node
  color = health (healthy `#58d68d` / cooling `#f6c453` / down `#ff6b6b`) from D4 `ProviderHealth`.
  Edges gateway→upstream pulse, width ∝ live reqs/s, animated `offset-path` particles flowing outward
  (`prefers-reduced-motion` cuts particles). Click node → sets the FlowTable (D10) upstream filter.
  Tooltip: cooldown countdown (`cooling_until_ms`), `last_error`, `failover_count`, p99, catalog_size.
  Driven by `TopologyUpdate` frames (D7) + `GET /topology` (D13) on load.
- **TokenSankeyView** (`TokenSankey`): 3-column d3-sankey — client → gateway → upstream-model. Band
  height = tokens/time (rolling 30 s, derived client-side from flows/usage), color gradient by cost
  (D13 `/topology` price table). Pulsing dash. Click band → filters FlowTable to that client/model.
  `$`/min from `MetricTick`. Recompute ~1 s.
- **TheaterView** (fullscreen dark cinematic): active streams as "rivers" — output text bright mono,
  reasoning dim + collapsible, tool calls as cards; per-river tokens/sec meter + blinking cursor.
  Auto-grid 1→big / 2→split / 3-6 (watch N models at once). Driven by monitor `segment_append` deltas
  (D3/theater reuse the transcript feed); tiles linger-then-fade. **`framer-motion` may be added here
  ONLY if the fade/river motion can't be done in CSS** (per §3.3 deferral).

## Acceptance criteria
- [ ] Topology: radial layout, health-colored nodes, animated edges (particles), click→filter wiring,
      cooldown-countdown tooltip; `TopologyUpdate` drives live updates; when `prefers-reduced-motion:
      reduce` is set, edge particles are disabled (animation off).
- [ ] Sankey: 3-column d3-sankey, band height = tokens/30 s, cost-colored, click→filter, `$`/min; no
      perf cliff at reasonable flow counts.
- [ ] Theater: rivers of output/reasoning/tool deltas from live streams; per-river tok/s + cursor;
      multi-grid 1/2/3-6; tiles linger-then-fade; fullscreen toggle.
- [ ] All three StrictMode-safe (d3 via `useLayoutEffect`+cleanup, no leaked sims/duplicate SVG).
- [ ] Seek mode (D11): topology/sankey/theater render the frozen snapshot's state; theater shows an
      explicit **"historical — deltas not replayed"** affordance and renders only the snapshot's
      terminal summary per flow (body-free snapshots carry no delta stream — see D5/D10), NOT a live
      river. (This is the approved body-free-snapshot tradeoff surfaced in the UI.)
- [ ] `tsc`/`eslint` clean; Codex-xhigh APPROVED.

## Integration points
- **Depends on:** D9 (scaffold/viz wrappers), D4 (topology data), D5 (metrics/cost), D3 (usage/deltas),
  D7 (`TopologyUpdate`/`Usage`/deltas frames), D13 (`/topology` + price table).
- **Reuses:** d3-force (radial hub-and-spoke), d3-sankey, the River viz, design tokens.

## Constraints
- d3 owns the SVG via refs; React owns lifecycle (§3.3); force-layout charge/collision tuned for
  stability (empirical; iterate aesthetics).
- No FLIP in any virtualized list; particles behind `prefers-reduced-motion`.
- `framer-motion` addition requires justification — default to CSS motion.

## Out of scope
- D4/D5/D13 data production; this view consumes it.
- Stats strip + scrubber (D11).

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
