/** Pure model for server-authored terminal-flow Sankey lanes. */
import type { OverviewLaneRollup } from '../../api/types';

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
  /** False when the server could not price any terminal flow in this lane. */
  costAvailable?: boolean;
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

/**
 * Build the graph from the server-authored terminal lane ledger. This consumes exactly the
 * selected Overview window/cut and never reprices or differences flows in the browser.
 * `prompt + completion` is the canonical reported volume; cached/reasoning are
 * subsets and therefore never added a second time.
 */
export function buildServerSankeyModel(
  lanes: OverviewLaneRollup[],
  windowSeconds: number,
): SankeyModel {
  const nodes: SankeyModelNode[] = [
    { id: CLIENT, label: 'client', col: 0 },
    { id: GATEWAY, label: 'gateway', col: 1 },
  ];
  const links: SankeyModelLink[] = [];
  let totalTokens = 0;
  let totalCost = 0;

  for (const lane of [...lanes].sort((left, right) =>
    laneId(left.provider, left.model).localeCompare(laneId(right.provider, right.model)))) {
    const prompt = lane.tokens.prompt ?? 0;
    const completion = lane.tokens.completion ?? 0;
    const tokens = prompt + completion;
    if (tokens <= 0) continue;
    const id = laneId(lane.provider, lane.model);
    const costAvailable = lane.cost.samples > 0 && lane.cost.total_usd !== null;
    const cost = costAvailable ? lane.cost.total_usd! : 0;
    nodes.push({
      id,
      label: lane.provider ? `${lane.model} @${lane.provider}` : lane.model,
      col: 2,
      model: lane.model,
      upstream: lane.provider || null,
    });
    const common = {
      value: tokens,
      cost,
      costAvailable,
      model: lane.model,
      upstream: lane.provider || null,
    };
    links.push({ source: CLIENT, target: GATEWAY, ...common });
    links.push({ source: GATEWAY, target: id, ...common });
    totalTokens += tokens;
    totalCost += cost;
  }

  const costPerMin = windowSeconds > 0 ? totalCost * (60 / windowSeconds) : 0;
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
