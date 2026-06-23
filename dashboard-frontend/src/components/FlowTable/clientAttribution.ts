/**
 * clientAttribution (gap 15) — the PURE, DOM-free model behind the CLIENT column cell + the
 * AGGREGATE "by client" roll-up panel + the per-client filter. Sibling of `failureTaxonomy.ts` /
 * `tokenEconomics.ts` / `providerLatency.ts`: unit-testable in isolation so the column, the panel,
 * and the filter can never disagree on which client a flow belongs to or how strong that
 * attribution is.
 *
 * Operator question (spec 15): "WHO is generating the cost, errors, latency — or abuse?"
 * Answered by attributing each flow to its non-secret client identity (`client_label` +
 * `client_source`, gap 04) and rolling cost / errors / latency up BY client.
 *
 * SOURCES — all ALREADY on the wire (gate F; verified by code search — the Rust `FlowRow` projects
 * `client_label`/`client_source` from the record/summary in `from_record`/`from_summary`):
 *  - `FlowSummary.client_label` — the stable, non-secret display id (a `key-<hex>` digest, a
 *    configured caller-id, or a UA fallback). NEVER a raw key — only the one-way hash prefix.
 *  - `FlowSummary.client_source` — the `ClientSource` provenance (`key_hash`/`configured_header`/
 *    `user_agent`), the source-STRENGTH signal.
 *  - `FlowSummary.status` (`failed`) + `cost` + `elapsed_ms` — the per-client roll-up dimensions.
 *
 * DATA-QUALITY INVARIANTS (the heart of an honest attribution surface — asserted in the tests):
 *  - SOURCE STRENGTH: a `key_hash` / `configured_header` attribution is a STRONG identity →
 *    `measured`. A `user_agent` attribution is a WEAK, spoofable fallback → `derived` (rendered
 *    visibly weaker + labelled), NEVER presented as a confirmed identity.
 *  - DON'T-LIE-WITH-ZEROS: a flow with NO `client_label` ⇒ `unavailable` (`—`), NEVER a fabricated
 *    client id. The roll-up over an empty population is `available: false` (every rate `—`).
 *  - NEVER a raw secret: the model only ever reads the already-hashed `client_label` (gap 04
 *    guarantees only the hash prefix exists); it never derives or surfaces a raw key.
 */
import type { ClientSource, CostConfidence, FlowSummary } from '../../api/types';

/** Provenance of a figure — mirrors the dashboard's measured/derived/estimated/unavailable tags. */
export type Quality = 'measured' | 'derived' | 'estimated' | 'unavailable';

/** The unavailable / no-data marker (a value that cannot be measured renders this, never `0`). */
export const UNAVAILABLE = '—';

/**
 * Source STRENGTH of an attribution:
 *  - `strong`      — a `key_hash` or `configured_header` source (a real, stable identity → `measured`).
 *  - `weak`        — a `user_agent` source (a spoofable fallback → `derived`; rendered weaker + labelled).
 *  - `unavailable` — no attribution at all (`—`; don't-lie-with-zeros).
 */
export type ClientStrength = 'strong' | 'weak' | 'unavailable';

/**
 * The CONFIDENCE ORDER for the weakest-tag rule (cross-cutting acceptance 1): an aggregate that
 * mixes confident + estimated priced inputs must surface the LOWER confidence (the floor is
 * `unavailable` — no priced input). We only ever DOWNGRADE toward the floor as priced flows fold in.
 */
const CONFIDENCE_RANK: Record<CostConfidence, number> = { unavailable: 0, estimated: 1, confident: 2 };

/** Fold a contributing priced flow's `cost_confidence` into a running aggregate, keeping the WEAKER. */
function weakerConfidence(a: CostConfidence, b: CostConfidence): CostConfidence {
  return CONFIDENCE_RANK[b] < CONFIDENCE_RANK[a] ? b : a;
}

/**
 * Map an aggregate `cost_confidence` to its DQ tag (the SAME mapping the stats strip + the overview
 * use): a `confident` summed cost is a real DERIVED figure; an `estimated` one (a contributing flow
 * is priced via an unconfigured cache rate / unreported tokens) is a LABELLED estimate — NEVER
 * silently shown as `measured`; `unavailable` ⇒ no priced flow (→ `—`).
 */
export function costConfidenceQuality(confidence: CostConfidence): Quality {
  if (confidence === 'confident') return 'derived';
  if (confidence === 'estimated') return 'estimated';
  return 'unavailable';
}

/** Map a `ClientSource` to its display strength (the source-strength DQ tag). */
export function sourceStrength(source: ClientSource | null | undefined): ClientStrength {
  if (source === 'key_hash' || source === 'configured_header') return 'strong';
  if (source === 'user_agent') return 'weak';
  return 'unavailable';
}

/** The DQ quality tag for an attribution: strong ⇒ `measured`, weak ⇒ `derived`, none ⇒ `unavailable`. */
export function strengthQuality(strength: ClientStrength): Quality {
  if (strength === 'strong') return 'measured';
  if (strength === 'weak') return 'derived';
  return 'unavailable';
}

/** Human label for a `ClientSource` — e.g. `key_hash` → "key hash", `user_agent` → "user-agent". */
const SOURCE_LABEL: Record<ClientSource, string> = {
  key_hash: 'key hash',
  configured_header: 'configured id',
  user_agent: 'user-agent',
};

/** Short SOURCE label for the inline column badge (the weak-UA fallback marker). */
const SOURCE_BADGE: Record<ClientSource, string> = {
  key_hash: 'key',
  configured_header: 'id',
  user_agent: 'ua',
};

/** A non-empty trimmed string, else null (so a blank field is treated as absent, not rendered ""). */
function text(v: string | null | undefined): string | null {
  if (typeof v !== 'string') return null;
  const t = v.trim();
  return t.length > 0 ? t : null;
}

// ---------------------------------------------------------------------------
// The CLIENT-column cell (one flow).
// ---------------------------------------------------------------------------

/** The render-ready CLIENT cell for ONE flow row. */
export interface ClientCell {
  /** The display label — the non-secret `client_label`, or `—` when unattributed. NEVER a raw key. */
  label: string;
  /** Whether a real attribution exists (`true` ⇒ `label` is the client id; `false` ⇒ `label` is `—`). */
  attributed: boolean;
  /** The bounded source, or null when unattributed. */
  source: ClientSource | null;
  /** Display strength — `strong`/`weak`/`unavailable` (drives the visibly-weaker UA rendering). */
  strength: ClientStrength;
  /** DQ tag: `measured` (strong) / `derived` (weak UA) / `unavailable` (absent). */
  quality: Quality;
  /** Whether this is the WEAK User-Agent fallback (rendered visibly different + labelled, not an identity). */
  weak: boolean;
  /** Short source badge for the inline marker (`key`/`id`/`ua`), or null when unattributed. */
  badge: string | null;
  /** Long source label for the title/tooltip (`key hash`/`configured id`/`user-agent`), or null. */
  sourceLabel: string | null;
  /** A human note for the cell title — explains the source strength + the don't-lie-with-zeros `—`. */
  detail: string;
}

/** The unattributed cell — a flow with no key / configured-id / UA. Renders `—`, never a fake id. */
const UNATTRIBUTED_CELL: ClientCell = {
  label: UNAVAILABLE,
  attributed: false,
  source: null,
  strength: 'unavailable',
  quality: 'unavailable',
  weak: false,
  badge: null,
  sourceLabel: null,
  detail:
    'no client attribution — the request carried no API key, no configured caller-id header, and ' +
    'no User-Agent. Unavailable (—), NOT a fabricated client id (don\'t-lie-with-zeros).',
};

/**
 * Resolve the CLIENT cell for one flow. ABSENT `client_label` ⇒ the unattributed `—` cell (never a
 * fabricated id). A present label is tagged by its source STRENGTH: a key-hash / configured-id is a
 * STRONG `measured` identity; a `user_agent` is a WEAK `derived` fallback (rendered visibly weaker +
 * labelled, NOT an identity claim). The label is the already-hashed `client_label` — a raw key never
 * reaches here (gap 04 hashes it in place pre-redaction).
 */
export function clientCell(flow: FlowSummary): ClientCell {
  const label = text(flow.client_label);
  if (!label) return UNATTRIBUTED_CELL;
  // A label with no/garbage source is still a real label, but with no strength signal: treat as the
  // weakest tagged tier so it can NEVER read as a confirmed identity (the source guard already
  // rejected a bogus enum at the wire boundary, so `source` is a valid `ClientSource | null` here).
  const source = flow.client_source ?? null;
  const strength = sourceStrength(source);
  const weak = strength === 'weak';
  return {
    label,
    attributed: true,
    source,
    strength,
    quality: strengthQuality(strength),
    weak,
    badge: source ? SOURCE_BADGE[source] : null,
    sourceLabel: source ? SOURCE_LABEL[source] : null,
    detail: detailFor(strength, source),
  };
}

function detailFor(strength: ClientStrength, source: ClientSource | null): string {
  if (strength === 'strong') {
    return source === 'key_hash'
      ? 'strong attribution: a non-reversible key-hash digest (key-<hex>) — the same caller key groups stably. Not a raw key.'
      : 'strong attribution: an operator-configured caller-id header (explicit, caller-asserted identity).';
  }
  if (strength === 'weak') {
    return 'WEAK attribution: a User-Agent fallback only (spoofable — NOT a confirmed identity). Shown weaker + labelled.';
  }
  return UNATTRIBUTED_CELL.detail;
}

/** The stable group key for a flow's client (its `client_label`), or null when unattributed. */
export function clientKey(flow: FlowSummary): string | null {
  return text(flow.client_label);
}

// ---------------------------------------------------------------------------
// The AGGREGATE "by client" roll-up (the panel + the filter chips).
// ---------------------------------------------------------------------------

/** One client's roll-up: its identity + source strength + cost / errors / latency over its flows. */
export interface ClientRollupRow {
  /** Stable key = the `client_label` (the filter value). NEVER a raw key. */
  key: string;
  /** Display label (the `client_label`). */
  label: string;
  /** The source the label was derived from (the STRONGEST source seen across this client's flows). */
  source: ClientSource | null;
  /** Source strength (`strong`/`weak`) — drives the visibly-weaker UA rendering in the panel. */
  strength: ClientStrength;
  /** DQ tag for the attribution itself: `measured` (strong) / `derived` (weak UA). */
  attributionQuality: Quality;
  /** Whether this client's strongest attribution is the WEAK UA fallback. */
  weak: boolean;
  /** Total flows OBSERVED for this client (the error-rate denominator; always `>= 1`; measured). */
  total: number;
  /** Of `total`, the count that FAILED (`status === 'failed'`); a measured count. */
  failed: number;
  /** Error rate `failed/total × 100` (`derived`). A client with 0 failures is a real measured-base `0%`. */
  errorRatePct: number;
  /** Pre-formatted error-rate string (`33%` / `0%`) — always available for an observed client. */
  errorRateText: string;
  /** Summed cost across this client's PRICED flows (USD), or `null` when NONE were priced (→ `—`). */
  cost: number | null;
  /** The AGGREGATE confidence of `cost` — the WEAKEST `cost_confidence` of the contributing priced flows. */
  costConfidence: CostConfidence;
  /**
   * DQ tag for `cost`, from {@link costConfidence}: `derived` (all-confident priced), `estimated`
   * (a contributing priced flow is an estimate — surfaced, never silently upgraded to `measured`),
   * `unavailable` when none priced (→ `—`).
   */
  costQuality: Quality;
  /** Count of this client's flows that carried a finite `cost` (the cost measurability denominator). */
  pricedFlows: number;
  /** Mean elapsed-ms across this client's TIMED flows, or `null` when NONE had a duration (→ `—`). */
  avgLatencyMs: number | null;
  /** DQ tag for `avgLatencyMs`: `derived` (a mean of measured durations) or `unavailable` (none → `—`). */
  latencyQuality: Quality;
  /** Count of this client's flows that carried a finite `elapsed_ms` (the latency denominator). */
  timedFlows: number;
}

/** The aggregate "by client" model the panel renders + the filter-chip source. */
export interface ClientRollup {
  /** Whether ANY ATTRIBUTED flow was observed (the measurability gate). `false` ⇒ every figure `—`. */
  available: boolean;
  /** Total flows observed across ALL rows (attributed + unattributed) — the panel's denominator note. */
  totalFlows: number;
  /** Of `totalFlows`, the count with NO attribution (rendered `—`; surfaced as an explicit count). */
  unattributedFlows: number;
  /** The per-client rows, ordered by total flows desc (then label) — the heaviest clients first. */
  rows: ClientRollupRow[];
}

interface ClientAccum {
  label: string;
  source: ClientSource | null;
  total: number;
  failed: number;
  costSum: number;
  pricedFlows: number;
  /** The running WEAKEST `cost_confidence` over this client's PRICED flows (starts at the ceiling). */
  costConfidence: CostConfidence;
  latencySum: number;
  timedFlows: number;
}

/** Strength ranking so the STRONGEST source a client ever presented wins the row's source tag. */
const STRENGTH_RANK: Record<ClientStrength, number> = { strong: 2, weak: 1, unavailable: 0 };

/**
 * Build the aggregate "by client" roll-up from the observed flow rows. Each ATTRIBUTED flow counts
 * toward its client's roll-up (cost / errors / latency); an UNATTRIBUTED flow bumps only the
 * `unattributedFlows` count (never a fabricated client). Empty / all-unattributed input ⇒
 * `available: false` (every figure `—`; don't-lie-with-zeros). The strongest source a client ever
 * presented wins its row's source tag (so a client seen once via key-hash and once via UA reads as a
 * strong identity, not the weak fallback).
 */
export function clientRollup(flows: readonly FlowSummary[] | null | undefined): ClientRollup {
  const rows = flows ?? [];
  const clients = new Map<string, ClientAccum>();
  let totalFlows = 0;
  let unattributedFlows = 0;

  for (const flow of rows) {
    totalFlows += 1;
    const key = clientKey(flow);
    if (!key) {
      unattributedFlows += 1;
      continue;
    }
    let c = clients.get(key);
    if (!c) {
      // Cost confidence starts at the CEILING (`confident`); each priced flow folds it to the weaker.
      c = { label: key, source: null, total: 0, failed: 0, costSum: 0, pricedFlows: 0, costConfidence: 'confident', latencySum: 0, timedFlows: 0 };
      clients.set(key, c);
    }
    c.total += 1;
    if (flow.status === 'failed') c.failed += 1;
    // The STRONGEST source this client ever presented wins (a key-hash beats a later UA sighting).
    const src = flow.client_source ?? null;
    if (STRENGTH_RANK[sourceStrength(src)] > STRENGTH_RANK[sourceStrength(c.source)]) c.source = src;
    // Cost — only a finite priced flow contributes (an unpriced/`null` cost is NOT a measured `0`).
    // The aggregate inherits the WEAKEST `cost_confidence` of those priced flows (don't silently
    // upgrade an estimated client spend to a confident-looking total — cross-cutting rule 1).
    if (typeof flow.cost === 'number' && Number.isFinite(flow.cost)) {
      c.costSum += flow.cost;
      c.pricedFlows += 1;
      c.costConfidence = weakerConfidence(c.costConfidence, flow.cost_confidence);
    }
    // Latency — only a finite duration contributes to the mean.
    if (typeof flow.elapsed_ms === 'number' && Number.isFinite(flow.elapsed_ms)) {
      c.latencySum += flow.elapsed_ms;
      c.timedFlows += 1;
    }
  }

  const built: ClientRollupRow[] = [];
  for (const c of clients.values()) {
    const strength = sourceStrength(c.source);
    const errorRatePct = c.total > 0 ? (c.failed / c.total) * 100 : 0;
    const priced = c.pricedFlows > 0;
    const cost = priced ? c.costSum : null;
    // No priced flow ⇒ the aggregate confidence collapses to `unavailable` (it never weakened from
    // a priced flow); else the running WEAKEST priced-flow confidence drives the DQ tag.
    const costConfidence: CostConfidence = priced ? c.costConfidence : 'unavailable';
    const avgLatencyMs = c.timedFlows > 0 ? c.latencySum / c.timedFlows : null;
    built.push({
      key: c.label,
      label: c.label,
      source: c.source,
      strength,
      attributionQuality: strengthQuality(strength),
      weak: strength === 'weak',
      total: c.total,
      failed: c.failed,
      errorRatePct,
      errorRateText: fmtRate(errorRatePct),
      cost,
      costConfidence,
      costQuality: costConfidenceQuality(costConfidence),
      pricedFlows: c.pricedFlows,
      avgLatencyMs,
      latencyQuality: avgLatencyMs === null ? 'unavailable' : 'derived',
      timedFlows: c.timedFlows,
    });
  }
  // Heaviest clients first (by observed flow count), then by label for a stable, readable order.
  built.sort((a, b) => b.total - a.total || a.label.localeCompare(b.label));

  return {
    available: built.length > 0,
    totalFlows,
    unattributedFlows,
    rows: built,
  };
}

/**
 * Format a `derived` error rate (a percentage in [0,100]) for a client row, or `—` when unavailable.
 * A MEASURED-base `0` reads `0%` (an observed client with no failures) — distinct from the
 * unavailable `—`. One decimal under 10% (so 4.2% is visible), whole percent otherwise.
 */
export function fmtRate(pct: number | null): string {
  if (pct === null || !Number.isFinite(pct)) return UNAVAILABLE;
  if (pct === 0) return '0%';
  if (pct < 10) return `${pct.toFixed(1)}%`;
  return `${Math.round(pct)}%`;
}

/** Format a mean latency (ms) for a client row, or `—` when unavailable. Sub-second in ms, else s. */
export function fmtLatency(ms: number | null): string {
  if (ms === null || !Number.isFinite(ms)) return UNAVAILABLE;
  if (ms < 1000) return `${Math.round(ms)}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}
