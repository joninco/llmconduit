/**
 * Pure derivations shared by the FlowTable + FlowDetail. Kept dependency-free and DOM-free so
 * they are unit-testable and reused by both the virtualized rows and the inspector header.
 *
 * The wire contract (frozen, D9): a flow is keyed by `api_call_id`; `model_served`,
 * `upstream_target`, `usage`, `cost`, and the timing fields are optional (the Rust `Option<_>`
 * fields) and may be absent until D2/D3 attach them. Every derivation here tolerates the
 * partially-populated row a live `flow_status` produces before usage/target arrive.
 */
import type { CostConfidence, FlowStatus, FlowSummary, ModelPrice, Usage } from '../../api/types';
import { fmtCost } from './format';

/** Coarse status bucket used for the colored chip (running / 2xx / 4xx / 5xx). */
export type StatusClass = 'running' | 'ok' | 'client-error' | 'server-error';

/**
 * The render-ready cost display for ANY `$` surface (gap 07 — the never-lie-with-zero +
 * label-estimated contract applied uniformly). Mirrors the `StatsStrip` chip provenance: the
 * dollar STRING and an `estimated` flag are derived TOGETHER from the cost + its `cost_confidence`,
 * so no cost render site can drop the confidence or paint an absent/unpriced cost as `$0.00`.
 *  - `value`     — the formatted string (`$0.0061`, `—`). `—` ⇒ UNAVAILABLE (never a fabricated `$0.00`).
 *  - `estimated` — the figure is a labelled best-effort estimate (a billed token class bills at the
 *                  default `0.0` rate) — the surface MUST badge/title it `est`.
 *  - `confidence`— the raw tier (handy for `data-confidence` attributes / tests).
 */
export interface CostDisplay {
  value: string;
  estimated: boolean;
  confidence: CostConfidence;
}

/**
 * Derive the cost-cell display from a (possibly null) dollar figure + its backend confidence tag.
 *
 * Don't-lie-with-zeros (gap 07): an `unavailable` confidence ALWAYS renders `—`, even if some stale
 * dollar figure rode alongside it — an unpriced/absent cost must NEVER read as a measured `$0.00`.
 * A genuine measured `$0` (a priced flow with `confident`/`estimated` tag and `cost === 0`) stays
 * distinct: it renders `$0.00`. `estimated` is surfaced as a labelled best-effort figure.
 */
export function costDisplay(cost: number | null, confidence: CostConfidence): CostDisplay {
  // `unavailable` ⇒ no trustworthy cost: render `—` regardless of any accompanying number, so an
  // unpriced row can never masquerade as a measured `$0.00` (the cost should already be null here).
  const value = confidence === 'unavailable' ? '—' : fmtCost(cost);
  return { value, estimated: confidence === 'estimated', confidence };
}

/**
 * The `$`/min readout display for a metrics window (the Sankey + any rate surface). The cost is the
 * window's `cost_per_min`; it is UNAVAILABLE — `—/min`, never `$0.00/min` — when the window is
 * absent OR nothing was priced (`priced_samples === 0`) OR the aggregate tag is `unavailable`. An
 * `estimated` aggregate is badged. Mirrors the `$/min` chip's denominator + `cost_confidence` logic
 * (the chip's `metricUnavailable` gate is `priced_samples === 0`). `metrics === null` (no tick yet)
 * ⇒ unavailable.
 */
export function costPerMinDisplay(
  metrics: { cost_per_min: number; priced_samples: number; cost_confidence: CostConfidence } | null,
): CostDisplay {
  // No window, nothing priced, or an explicitly-unavailable aggregate ⇒ unavailable (`—/min`),
  // distinguishing an unpriced/idle window from a genuine measured `$0.00/min`.
  if (!metrics || metrics.priced_samples === 0 || metrics.cost_confidence === 'unavailable') {
    return { value: '—', estimated: false, confidence: 'unavailable' };
  }
  return {
    value: `$${metrics.cost_per_min.toFixed(2)}`,
    estimated: metrics.cost_confidence === 'estimated',
    confidence: metrics.cost_confidence,
  };
}

/**
 * Maps a `FlowStatus` (+ optional terminal reason) to the chip class. `open` is "running"
 * (pulse). `completed` is 2xx. `failed`/`cancelled` map to an error class; when the terminal
 * reason names an upstream HTTP status (e.g. "upstream 503", "429"), the 4xx/5xx split is read
 * from it, else `failed`→5xx and `cancelled`→4xx (a client-initiated kill is a 4xx-shaped end).
 */
export function statusClass(status: FlowStatus, terminalReason?: string | null): StatusClass {
  if (status === 'open') return 'running';
  if (status === 'completed') return 'ok';
  const code = extractHttpStatus(terminalReason);
  if (code !== null) {
    if (code >= 500) return 'server-error';
    if (code >= 400) return 'client-error';
    if (code >= 200 && code < 300) return 'ok';
  }
  return status === 'cancelled' ? 'client-error' : 'server-error';
}

/** Pulls the first 3-digit HTTP-ish status out of a terminal reason string, if any. */
function extractHttpStatus(reason?: string | null): number | null {
  if (!reason) return null;
  const m = reason.match(/\b([1-5]\d\d)\b/);
  return m ? Number(m[1]) : null;
}

/** Short, copy-pasteable form of an `api_call_id` for the dense table column. */
export function shortId(apiCallId: string): string {
  // ids look like `api_001` or a uuid; show the trailing 8 chars (or all, if shorter).
  return apiCallId.length <= 10 ? apiCallId : `…${apiCallId.slice(-8)}`;
}

/**
 * Cost in dollars for a flow. Prefers the server roll-up (`flow.cost`, D5/D13). When that is
 * absent (live row before the roll-up, or mock without a precomputed cost) it is computed from
 * `usage` × the price table for the SERVED model (the model actually billed). Cached prompt
 * tokens are priced at the cached rate and subtracted from the prompt rate. Returns `null` when
 * neither a roll-up nor a usable (usage + price) pair exists, so the column can render "—".
 */
export function flowCost(flow: FlowSummary, priceTable: Record<string, ModelPrice>): number | null {
  if (typeof flow.cost === 'number' && Number.isFinite(flow.cost)) return flow.cost;
  if (!flow.usage) return null;
  const model = flow.model_served ?? flow.model_requested;
  if (!model) return null;
  const price = priceTable[model];
  if (!price) return null;
  return computeCost(flow.usage, price);
}

/** usage × price (per-1k rates). Cached prompt tokens billed at the cached rate.
 * Gap 07: an UNREPORTED (`null`/absent) cached count bills as 0 cached tokens — the whole
 * prompt then bills at the input rate (matching the Rust `cost_for_usage`). The honest
 * confidence of that figure rides `cost_confidence`, not this dollar number. */
export function computeCost(usage: Usage, price: ModelPrice): number {
  const cached = Math.max(0, usage.cached ?? 0);
  const billablePrompt = Math.max(0, usage.prompt - cached);
  return (
    (billablePrompt / 1000) * price.input_per_1k +
    (cached / 1000) * price.cached_per_1k +
    (usage.completion / 1000) * price.output_per_1k
  );
}

/** Elapsed ms for the row: explicit `elapsed_ms`, else `finished-started`, else live `now-started`. */
export function elapsedMs(flow: FlowSummary, now: number): number | null {
  if (typeof flow.elapsed_ms === 'number') return flow.elapsed_ms;
  if (typeof flow.finished_ms === 'number') return flow.finished_ms - flow.started_ms;
  if (flow.status === 'open') return Math.max(0, now - flow.started_ms);
  return null;
}

/** A flow that failed OVER to another upstream is tagged (failover_count surfaced via target). */
export function isFailover(flow: FlowSummary): boolean {
  // A served model different from the requested one, OR a terminal reason mentioning failover,
  // marks a row that was re-routed. (The authoritative failover_count lives on ProviderHealth;
  // at the flow row we infer the tag from the requested→served divergence + reason text.)
  const reason = flow.terminal_reason?.toLowerCase() ?? '';
  if (reason.includes('failover') || reason.includes('failed over')) return true;
  return (
    !!flow.model_requested &&
    !!flow.model_served &&
    flow.model_requested !== flow.model_served &&
    !!flow.upstream_target
  );
}
