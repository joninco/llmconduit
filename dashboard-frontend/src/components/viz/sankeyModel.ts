/**
 * Pure token-flow model for the Sankey (D12), kept out of the .tsx so it is unit-testable and the
 * component file exports only the component (react-refresh). Turns the live flow rows + price
 * table into the 3-column graph d3-sankey lays out: client → gateway → served-model.
 *
 * Band height = tokens over a rolling window (default 30 s): we sum `usage.total` of the flows
 * whose activity falls inside `[now - windowMs, now]`. A flow with no served model or no usage
 * yet contributes nothing (it has no band). Cost per band is derived from the D13 price table
 * (`/topology` `price_table`) — prompt+cached billed at input/cached rates, completion at output —
 * and the color is a low→high cost ramp so the expensive lanes read hot.
 */
import type { FlowSummary, ModelPrice } from '../../api/types';

/** A node in the 3-column graph. `col` fixes its column for the layout + the test. */
export interface SankeyModelNode {
  /** Stable id: `client`, `gateway`, or `model:<served-model>`. */
  id: string;
  label: string;
  col: 0 | 1 | 2;
  /** The served-model id for a column-2 node (drives the click→filter cross-link). */
  model?: string;
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

/** Cost of a flow's usage under the price table (input+cached at their rates, completion at output). */
export function flowWindowCost(flow: FlowSummary, price: ModelPrice | undefined): number {
  const u = flow.usage;
  if (!u || !price) return 0;
  const input = Math.max(0, u.prompt - u.cached);
  return (
    (input / 1000) * price.input_per_1k +
    (u.cached / 1000) * price.cached_per_1k +
    (u.completion / 1000) * price.output_per_1k
  );
}

/** True when the flow had activity inside the rolling window ending at `nowMs`. */
function inWindow(flow: FlowSummary, nowMs: number, windowMs: number): boolean {
  const start = flow.started_ms;
  // An open flow (no finish) is active up to now; a finished flow used the window if it ended
  // inside it. Either way: the flow overlaps `[now - windowMs, now]`.
  const end = flow.finished_ms ?? nowMs;
  return end >= nowMs - windowMs && start <= nowMs;
}

/**
 * Build the client → gateway → served-model graph from the live rows. Flows are bucketed by their
 * SERVED model (the lane that actually consumed tokens); requested-but-rerouted models collapse
 * into the served lane (the topology already tells the routing story). Lanes with zero windowed
 * tokens are dropped so the Sankey shows only what is actually flowing.
 */
export function buildSankeyModel(
  flows: FlowSummary[],
  priceTable: Record<string, ModelPrice>,
  nowMs: number,
  windowMs = 30_000,
): SankeyModel {
  // model → { tokens, cost } over the window.
  const perModel = new Map<string, { tokens: number; cost: number }>();
  for (const f of flows) {
    const model = f.model_served ?? f.model_requested;
    if (!model || !f.usage) continue;
    if (!inWindow(f, nowMs, windowMs)) continue;
    const tokens = f.usage.total;
    if (tokens <= 0) continue;
    const cost = flowWindowCost(f, priceTable[model]);
    const acc = perModel.get(model) ?? { tokens: 0, cost: 0 };
    acc.tokens += tokens;
    acc.cost += cost;
    perModel.set(model, acc);
  }

  const nodes: SankeyModelNode[] = [
    { id: CLIENT, label: 'client', col: 0 },
    { id: GATEWAY, label: 'gateway', col: 1 },
  ];
  const links: SankeyModelLink[] = [];
  let totalTokens = 0;
  let totalCost = 0;
  // Stable model order (alphabetical) so the layout + the test are deterministic.
  for (const model of [...perModel.keys()].sort()) {
    const { tokens, cost } = perModel.get(model)!;
    nodes.push({ id: `model:${model}`, label: model, col: 2, model });
    links.push({ source: CLIENT, target: GATEWAY, value: tokens, cost, model });
    links.push({ source: GATEWAY, target: `model:${model}`, value: tokens, cost, model });
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
