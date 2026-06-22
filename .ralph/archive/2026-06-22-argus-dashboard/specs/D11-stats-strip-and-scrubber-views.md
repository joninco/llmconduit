# D11 — Stats strip + time-travel scrubber views

> **Source:** DASHBOARD_PLAN.md rev 8 §1.5, §1.6. Topic 13. The connective tissue.

**Priority:** MEDIUM · **Surface:** `dashboard-frontend/src/components/StatsStrip/`,
`components/Scrubber/`

## Purpose
The always-on top bar (req/s, active streams, error%, p50/p95/p99, tokens/s, $/min) with sparkline
trends + a 1m/5m/1h window selector, and the timeline scrubber that rewinds the whole dashboard to a
past moment via the D5 body-free snapshots.

## Jobs to Be Done
- **StatsStrip:** chips — req/s, active streams, error% (red above threshold), p50/p95/p99, tokens/s,
  $/min — each value + a uPlot sparkline (60 samples) + delta arrow. 1m/5m/1h selector switches
  `metrics.windows.{m1,m5,h1}` + sparkline depth. Driven by the `MetricTick` WS frame (D7 envelope,
  metrics domain) + seeded by `GET /metrics` (D13). Always visible at the top of `App.tsx`.
- **Scrubber:** a horizontal timeline under the strip; background hill = `reqs_per_second` ring buffer
  (1 s granularity, ~30 min) derived from `MetricTick` history. A draggable playhead → `seek` mode:
  the `DashboardSocket` pauses applying live frames (shadow-buffer), the app calls
  `GET /dashboard/api/snapshot?at=<ts>` (rAF-throttled, LRU-keyed by second bucket) and broadcasts the
  frozen cut to all views. A pulsing **LIVE** toggle resumes — replays shadowed frames (or
  reconnect for a clean snapshot). Hover tooltip shows reqs/s at the point.

## Acceptance criteria
- [ ] StatsStrip renders all chips with `tabular-nums`; sparklines update from `MetricTick`; window
      selector switches the source `metrics.windows.*`.
- [ ] Scrubber seek pauses live WS, fetches `/snapshot?at=`, all views (D10/D12) render the frozen cut;
      LIVE toggle resumes. rAF-throttled + LRU-cached (no request storm on drag).
- [ ] Background hill renders the reqs/s history; hover tooltip shows the local rate.
- [ ] Sparkline (uPlot) is StrictMode-safe (dispose on cleanup); `prefers-reduced-motion` cuts the
      pulsing LIVE indicator + any animation.
- [ ] `tsc`/`eslint` clean; Codex-xhigh APPROVED.

## Integration points
- **Depends on:** D9 (scaffold), D5 (`/metrics` `windows` + `/snapshot` body-free cuts), D7
  (`MetricTick` frame), D13 (routes). The scrubber broadcasts seek-state consumed by D10 + D12.
- **Reuses:** uPlot viz wrapper (D9).

## Constraints
- `/snapshot` calls throttled + cached; drag must not flood REST.
- Snapshot is body-free (D5): stats/summary render as-of; bodies render live ("evicted" if gone) —
  this is the documented tradeoff, surface it cleanly in the UI.
- No FLIP/`framer-motion` here.

## Out of scope
- The 5 s snapshot mechanism itself (D5); this view consumes `/snapshot`.
- Other views' seek behavior (each view implements its own seek-listener).

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
