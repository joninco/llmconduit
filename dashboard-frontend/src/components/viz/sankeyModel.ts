/**
 * Pure token-flow model for the Sankey (D12), kept out of the .tsx so it is unit-testable and the
 * component file exports only the component (react-refresh). Turns TIMESTAMPED usage deltas + the
 * price table into the 3-column graph d3-sankey lays out: client → gateway → (upstream, served-model).
 *
 * Band height = tokens over a rolling window (default 30 s). The input is a stream of `SankeyUsage
 * Delta`s — each the INCREMENT a flow's usage grew by at a wall-clock `ts` (finding 2). The band =
 * the SUM of deltas whose `ts` falls inside `[now - windowMs, now]`, NOT a flow's cumulative
 * lifetime total. (A long-running flow that streamed 1 M tokens an hour ago but is idle now
 * contributes 0 to the 30 s band; only its recent deltas count.) The LIVE accumulator
 * (`useSankeyWindow`) derives these deltas by diffing successive cumulative usage snapshots; a SEEK
 * cut, having no delta history, contributes one delta per frozen flow at the cut instant.
 *
 * Lanes are bucketed by `(upstream_target, served-model)` — the SAME model served by two upstreams
 * is two distinct lanes (finding 9) — and cost is derived from the D13 price table (`/topology`
 * `price_table`): prompt+cached billed at input/cached rates, completion at output. The color is a
 * low→high cost ramp so the expensive lanes read hot.
 */
import type { ModelPrice } from '../../api/types';

/**
 * One increment of a flow's usage at a wall-clock `ts` (finding 2). The token fields are the DELTA
 * amounts (the increase since the prior snapshot), bucketed by the lane the flow was served on.
 * `upstream` is null when the flow has no attributed upstream target yet.
 */
export interface SankeyUsageDelta {
  ts: number;
  upstream: string | null;
  model: string;
  /** Delta token sub-counts (the amounts this increment added). */
  prompt: number;
  cached: number;
  completion: number;
  total: number;
}

/** A node in the 3-column graph. `col` fixes its column for the layout + the test. */
export interface SankeyModelNode {
  /** Stable id: `client`, `gateway`, or `served:<upstream>|<served-model>`. */
  id: string;
  label: string;
  col: 0 | 1 | 2;
  /** The served-model id for a column-2 node (drives the click→filter cross-link). */
  model?: string;
  /** The upstream target for a column-2 node (the OTHER facet the click filters — finding 9). */
  upstream?: string | null;
}

/** A link carrying token volume + the derived cost (for the band color + `$`/min readout). */
export interface SankeyModelLink {
  source: string;
  target: string;
  /** Tokens over the window (the band height input — d3-sankey calls this `value`). */
  value: number;
  /** Total cost of this lane's tokens over the window (USD). */
  cost: number;
  /** The served-model id (column-2 lane), for the click→filter cross-link. */
  model?: string;
  /** The upstream target (column-2 lane), filtered ATOMICALLY with the model on click (finding 9). */
  upstream?: string | null;
}

export interface SankeyModel {
  nodes: SankeyModelNode[];
  links: SankeyModelLink[];
  /** Sum of all lane costs over the window, projected to USD/min (the `$`/min readout). */
  costPerMin: number;
  /** Total tokens over the window (for an empty-state check / readout). */
  totalTokens: number;
}

const GATEWAY = 'gateway';
const CLIENT = 'client';

/** The lane id for a column-2 node, keyed by (upstream, model) so a model split across upstreams
 * resolves to distinct lanes (finding 9). A null upstream collapses to a `?` segment. */
function laneId(upstream: string | null, model: string): string {
  return `served:${upstream ?? '?'}|${model}`;
}

/** Cost of a usage delta under the price table (input+cached at their rates, completion at output). */
export function deltaCost(d: SankeyUsageDelta, price: ModelPrice | undefined): number {
  if (!price) return 0;
  const input = Math.max(0, d.prompt - d.cached);
  return (
    (input / 1000) * price.input_per_1k +
    (d.cached / 1000) * price.cached_per_1k +
    (d.completion / 1000) * price.output_per_1k
  );
}

/** A per-lane accumulator over the window. */
interface LaneAcc {
  upstream: string | null;
  model: string;
  tokens: number;
  cost: number;
}

/**
 * Build the client → gateway → (upstream, served-model) graph from the WINDOWED usage deltas. Only
 * deltas whose `ts` lands inside `[now - windowMs, now]` count toward a band (finding 2): a flow's
 * lifetime total never inflates the rolling 30 s window. Lanes are keyed by `(upstream, model)` so
 * the same model on two upstreams is two lanes (finding 9). Empty lanes are dropped so the Sankey
 * shows only what is actually flowing.
 */
export function buildSankeyModel(
  deltas: SankeyUsageDelta[],
  priceTable: Record<string, ModelPrice>,
  nowMs: number,
  windowMs = 30_000,
): SankeyModel {
  const cutoff = nowMs - windowMs;
  // (upstream, model) → accumulated windowed tokens + cost.
  const perLane = new Map<string, LaneAcc>();
  for (const d of deltas) {
    // Lower bound only: a delta is in-window once its `ts` is within `windowMs` of `now`. We do NOT
    // upper-bound on `ts > now` — a LIVE delta folded microseconds after the view's `nowMs` snapshot
    // (a benign clock race between the accumulator's fold clock and the render clock) is still
    // current and must count; a seek stamps every delta at `atMs` with `nowMs === atMs`, so the
    // lower bound alone is correct there too.
    if (d.ts < cutoff) continue; // aged out of the rolling window — excluded.
    if (!d.model || d.total <= 0) continue;
    const id = laneId(d.upstream, d.model);
    const acc = perLane.get(id) ?? { upstream: d.upstream, model: d.model, tokens: 0, cost: 0 };
    acc.tokens += d.total;
    acc.cost += deltaCost(d, priceTable[d.model]);
    perLane.set(id, acc);
  }

  const nodes: SankeyModelNode[] = [
    { id: CLIENT, label: 'client', col: 0 },
    { id: GATEWAY, label: 'gateway', col: 1 },
  ];
  const links: SankeyModelLink[] = [];
  let totalTokens = 0;
  let totalCost = 0;
  // Stable lane order (by id, alphabetical) so the layout + the test are deterministic.
  for (const id of [...perLane.keys()].sort()) {
    const { upstream, model, tokens, cost } = perLane.get(id)!;
    // Label disambiguates the upstream when one is attributed ("model @upstream"), bare model else.
    const label = upstream ? `${model} @${upstream}` : model;
    nodes.push({ id, label, col: 2, model, upstream });
    links.push({ source: CLIENT, target: GATEWAY, value: tokens, cost, model, upstream });
    links.push({ source: GATEWAY, target: id, value: tokens, cost, model, upstream });
    totalTokens += tokens;
    totalCost += cost;
  }

  // Project the windowed cost to USD/min (the strip shows $/min; the Sankey echoes it).
  const costPerMin = windowMs > 0 ? totalCost * (60_000 / windowMs) : 0;
  return { nodes, links, costPerMin, totalTokens };
}

/**
 * Low→high cost RAMP for a band: maps a lane's cost (relative to the max lane cost in view) onto a
 * cool→hot color so the expensive lanes read hot. Returns a hex string for the SVG fill (d3/SVG
 * can't use CSS vars). `maxCost <= 0` (all-free) → the cool end.
 */
export function costColor(cost: number, maxCost: number): string {
  const t = maxCost > 0 ? Math.max(0, Math.min(1, cost / maxCost)) : 0;
  // Cool accent-blue (107,182,255) → hot down-red (255,107,107), linear in t.
  const lerp = (a: number, b: number) => Math.round(a + (b - a) * t);
  const r = lerp(107, 255);
  const g = lerp(182, 107);
  const b = lerp(255, 107);
  const hx = (n: number) => n.toString(16).padStart(2, '0');
  return `#${hx(r)}${hx(g)}${hx(b)}`;
}
