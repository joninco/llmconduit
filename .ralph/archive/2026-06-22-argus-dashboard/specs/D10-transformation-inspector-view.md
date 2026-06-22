# D10 — Transformation inspector view (FlowTable + 3-pane + deltas)

> **Source:** DASHBOARD_PLAN.md rev 8 §1.1, §3.3. Topic 13. The flagship view ("defines the product").

**Priority:** HIGH · **Surface:** `dashboard-frontend/src/views/FlowsView.tsx`,
`components/FlowTable/`, `components/FlowDetail/`, `components/viz/JsonPane.tsx`

## Purpose
The mitmweb-style flow list + a 3-layer side-by-side inspector: raw inbound → normalized internal
Responses → upstream chat-completions payload actually sent (D2's on-wire body), scroll-synced,
JSON-highlighted, with per-path diff tints + a streamed-deltas sub-panel. This is the screen that
shows exactly what the gateway transformed.

## Jobs to Be Done
- **FlowTable:** virtualized rows (`react-virtual`), newest-on-top, columns: timestamp, short
  `api_call_id`, client (user-agent), endpoint, requested→served model, upstream target, status chip
  (running/2xx/4xx/5xx), tokens in/out, cost (D5/D13), elapsed; error rows red, streaming pulse,
  failover-tagged. Filter bar (status/model/upstream quick chips). Click row → `FlowDetail`.
  Driven by WS `FlowStatus` + `RequestUpsert`-equivalent monitor frames; invalidated detail on
  `flow_status` per-domain seq.
- **FlowDetail:** 3 scroll-synced side-by-side panels — (A) raw inbound body+headers (D1 inbound),
  (B) normalized Responses (D1 normalized), (C) upstream chat-completions body (D2 on-wire). Each a
  `JsonPane` (highlight.js) with a structural-diff overlay tinting added/changed/removed between
  layers. Tabs: Headers / Timeline / Error. A **deltas sub-panel** renders streamed deltas (output
  bright, reasoning dim, tool-call cards) from monitor `segment_append` frames.
- **Time-travel interaction:** when scrubber (D11) is in `seek` mode, the FlowTable/`FlowDetail`
  render the selected snapshot's summaries; the body panel reads live and shows "body evicted" if the
  flow's body is gone (D5 body-free-snapshot tradeoff).
- **Kill action** on an active flow: POSTs `/flows/:id/kill` with the CSRF header (D7); optimistic
  state update.

## Acceptance criteria
- [ ] Virtualized FlowTable renders 10k mock rows smoothly; filters work; WS-driven live updates.
- [ ] `FlowDetail` shows all 3 bodies scroll-synced; JsonPane highlights JSON; the diff overlay tints
      changed/added/removed fields between layers (tested on a known 3-layer fixture).
- [ ] Deltas sub-panel renders output/reasoning/tool deltas live from monitor frames; tool calls as
      expandable cards.
- [ ] Headers/Timeline/Error tabs populate (timeline from monitor `event_append`).
- [ ] Time-travel seek: table + detail render the snapshot summary; body panel shows "body evicted"
      when the live body is evicted (and the real body when retained).
- [ ] Kill button POSTs with CSRF; optimistic UI; 403 handled (mutations off).
- [ ] `tsc`/`eslint` clean; StrictMode-safe viz; Codex-xhigh APPROVED.

## Integration points
- **Depends on:** D9 (scaffold/plumbing), D1 (inbound+normalized bodies), D2 (upstream body), D3
  (usage), D7 (auth/CSRF), D13 (`/flows`, `/flows/:id`, kill routes).
- **Reuses:** `DashboardSocket` (D9), `JsonPane` viz, design tokens.

## Constraints
- Scroll-sync across the 3 panes must not fight React virtualization.
- Diff overlay is structural (per-JSON-path), hand-rolled (no extra heavy dep) per §3.3.
- StrictMode-safe imperative rendering (highlight.js keyed to a ref, cleaned up).

## Out of scope
- Stats strip + scrubber (D11); topology/sankey/theater (D12).
- The Rust body-capture correctness (D1/D2) — this view consumes it.

## Definition of done
- [ ] Acceptance criteria green (mock + real when Rust lands); Codex-xhigh APPROVED.
