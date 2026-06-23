/**
 * overviewModel (gap 16) — the PURE, DOM-free roll-ups behind the CONTROL-ROOM overview's
 * "top models / providers by volume · cost" and "token-mix" tiles. Sibling of the other surface
 * models (`failureTaxonomy.ts` / `clientAttribution.ts` / `contextUtilization.ts` /
 * `providerLatency.ts`): unit-testable in isolation so the overview tiles can never disagree with
 * the numbers they derive.
 *
 * The control room COMPOSES the gap-01–15 surfaces; it does NOT invent new data. Where an existing
 * pure model already answers a tile (per-provider latency ⇒ gap 13's `buildProviderLatency`,
 * failures ⇒ gap 14's `failureTaxonomy`, clients ⇒ gap 15's `clientRollup`, context ⇒ gap 09's
 * `aggregateContextPressure`), the overview reuses it directly. THIS module adds only the new
 * aggregates the spec asks for that no existing model provides: a flow-row roll-up by MODEL and by
 * PROVIDER (volume + cost), and a token-mix split.
 *
 * SOURCE — all read from the FLOW-LIST population (`FlowRow` via `useFlowRows`, i.e. `/flows` +
 * `/snapshot` + the live store union), the source that actually carries per-flow `cost` /
 * `cost_confidence` / `usage` (the live `flow_status` frame does NOT — gap 15's wire-source trap).
 * The per-provider LATENCY/ERROR tiles are explicitly NOT derived here (they would hide failed
 * primaries — spec 16): they come from the gap-12 per-provider DTO on the REST/snapshot topology
 * node, consumed by the view via gap-13's read pattern.
 *
 * DATA-QUALITY INVARIANTS (the heart of an honest control room — asserted in the sibling tests):
 *  - VOLUME is `measured` (a directly-counted flow count). A group always has `>= 1` flow.
 *  - COST inherits the WEAKEST confidence of its contributing flows (cross-cutting rule 1): a group
 *    that mixes `confident` + `estimated` priced flows is `estimated` (never silently upgraded to a
 *    confident-looking total); a group with NO priced flow is `unavailable` ⇒ `—`, NEVER `$0.00`
 *    (don't-lie-with-zeros). A genuine summed `0` over priced flows stays a measured/derived `$0.00`.
 *  - TOKEN-MIX classes are `measured` from reported usage; an UNREPORTED optional class (cached /
 *    reasoning absent across the whole window) is `unavailable` ⇒ `—`, never a fabricated `0`. A
 *    window with no usage-bearing flow is `available: false` (every figure `—`).
 *  - EMPTY input ⇒ `available: false` everywhere (the empty-state `—` dashboard, not all-`0`).
 */
import type { CostConfidence, FlowSummary } from '../../api/types';

/** Provenance of a figure — mirrors the dashboard's measured/derived/estimated/unavailable tags. */
export type Quality = 'measured' | 'derived' | 'estimated' | 'unavailable';

/** The unavailable / no-data marker (a value that cannot be measured renders this, never `0`). */
export const UNAVAILABLE = '—';

/** How many rows each "top N" tile surfaces (the busiest first; the rest are summarized as a count). */
export const TOP_N = 5;

// ---------------------------------------------------------------------------
// Cost-confidence aggregation (the WEAKEST-tag inheritance rule).
// ---------------------------------------------------------------------------

/**
 * The CONFIDENCE ORDER for the weakest-tag rule (cross-cutting acceptance 1): an aggregate that
 * mixes confident + estimated inputs surfaces the LOWER confidence. `unavailable` is the floor
 * (no priced input at all), then `estimated`, then `confident` is the ceiling. We only ever
 * DOWNGRADE toward the floor as we fold in priced flows — never silently upgrade.
 */
const CONFIDENCE_RANK: Record<CostConfidence, number> = { unavailable: 0, estimated: 1, confident: 2 };

/**
 * Fold a contributing flow's `cost_confidence` into a running aggregate, keeping the WEAKER of the
 * two (the lower rank). A group starts at the strongest (`confident`) and only ever weakens. Called
 * ONLY for flows that actually carried a finite priced cost — an `unavailable`/unpriced flow does
 * not contribute a dollar amount (so it does not drag a real total to `—`); the group's
 * availability is decided separately by whether ANY priced flow was seen.
 */
function weakerConfidence(a: CostConfidence, b: CostConfidence): CostConfidence {
  return CONFIDENCE_RANK[b] < CONFIDENCE_RANK[a] ? b : a;
}

/** Map an aggregate `cost_confidence` to its DQ tag — `confident` ⇒ `derived`, `estimated` ⇒ `estimated`. */
export function costConfidenceQuality(confidence: CostConfidence): Quality {
  if (confidence === 'confident') return 'derived';
  if (confidence === 'estimated') return 'estimated';
  return 'unavailable';
}

/** A non-empty trimmed string, else null (so a blank field is treated as absent, not rendered ""). */
function text(v: string | null | undefined): string | null {
  if (typeof v !== 'string') return null;
  const t = v.trim();
  return t.length > 0 ? t : null;
}

// ---------------------------------------------------------------------------
// Top models / providers by VOLUME + COST.
// ---------------------------------------------------------------------------

/** The grouping DIMENSION for a leaderboard tile. */
export type LeaderboardDimension = 'model' | 'provider';

/** One row of a "top models/providers" leaderboard — volume (measured) + cost (weakest-tag). */
export interface LeaderboardRow {
  /** Stable group key — the model id (served→requested) or the provider/upstream id. */
  key: string;
  /** Display label (same as `key`; the `—` sentinel when the dimension was absent on the row). */
  label: string;
  /** Total flows OBSERVED for this group (the VOLUME; `measured`; always `>= 1`). */
  volume: number;
  /** Summed cost across this group's PRICED flows (USD), or `null` when NONE were priced (→ `—`). */
  cost: number | null;
  /** The AGGREGATE confidence of `cost` — the WEAKEST of the contributing priced flows. */
  costConfidence: CostConfidence;
  /** DQ tag for `cost`: `derived`/`estimated` when priced, `unavailable` when none (→ `—`). */
  costQuality: Quality;
  /** Count of this group's flows that carried a finite priced `cost` (the cost measurability base). */
  pricedFlows: number;
}

/** A "top N" leaderboard the overview renders (volume or cost ordered). */
export interface Leaderboard {
  /** Whether ANY flow was observed (the measurability gate). `false` ⇒ the empty-state `—`. */
  available: boolean;
  /** The grouping dimension (`model`/`provider`) — for the tile heading + test hooks. */
  dimension: LeaderboardDimension;
  /** Total flows observed across ALL groups (the denominator note). */
  totalFlows: number;
  /** Total distinct groups (so the tile can show "+N more" beyond the visible top-N). */
  groupCount: number;
  /** The top-N rows in the requested order (volume desc, or cost desc). */
  rows: LeaderboardRow[];
}

interface GroupAccum {
  key: string;
  volume: number;
  costSum: number;
  pricedFlows: number;
  confidence: CostConfidence; // the running WEAKEST over contributing priced flows
}

/** The grouping key for a flow on a dimension (served identity wins; blank ⇒ the `—` sentinel). */
function dimKey(flow: FlowSummary, dimension: LeaderboardDimension): string {
  if (dimension === 'provider') return text(flow.upstream_target) ?? UNAVAILABLE;
  return text(flow.model_served) ?? text(flow.model_requested) ?? UNAVAILABLE;
}

/** Build the per-group accumulators off the rows (one pass; shared by the volume + cost orderings). */
function accumulate(rows: readonly FlowSummary[], dimension: LeaderboardDimension): Map<string, GroupAccum> {
  const groups = new Map<string, GroupAccum>();
  for (const flow of rows) {
    const key = dimKey(flow, dimension);
    let g = groups.get(key);
    if (!g) {
      // Start cost confidence at the CEILING (`confident`); fold each priced flow to the weaker.
      g = { key, volume: 0, costSum: 0, pricedFlows: 0, confidence: 'confident' };
      groups.set(key, g);
    }
    g.volume += 1;
    // Cost — only a finite priced flow contributes (an unpriced/`null` cost is NOT a measured `0`).
    if (typeof flow.cost === 'number' && Number.isFinite(flow.cost)) {
      g.costSum += flow.cost;
      g.pricedFlows += 1;
      g.confidence = weakerConfidence(g.confidence, flow.cost_confidence);
    }
  }
  return groups;
}

/** Finalize a group accumulator into a render-ready leaderboard row (the cost DQ rules applied). */
function toRow(g: GroupAccum): LeaderboardRow {
  // A group with NO priced flow has UNAVAILABLE cost (→ `—`), never a fabricated `$0.00`. The
  // aggregate confidence then collapses to `unavailable` (it never saw a priced flow to weaken from).
  const priced = g.pricedFlows > 0;
  const cost = priced ? g.costSum : null;
  const costConfidence: CostConfidence = priced ? g.confidence : 'unavailable';
  return {
    key: g.key,
    label: g.key,
    volume: g.volume,
    cost,
    costConfidence,
    costQuality: costConfidenceQuality(costConfidence),
    pricedFlows: g.pricedFlows,
  };
}

/** The empty leaderboard (no flows observed) — the empty-state `—` (never an all-`0` board). */
function emptyLeaderboard(dimension: LeaderboardDimension): Leaderboard {
  return { available: false, dimension, totalFlows: 0, groupCount: 0, rows: [] };
}

/**
 * Build a "top models/providers by VOLUME" leaderboard from the observed flow rows. Volume is the
 * directly-counted flow count (`measured`); cost inherits the WEAKEST confidence of its priced
 * flows. Ordered by volume desc (then cost desc, then key) and capped to {@link TOP_N}. Empty input
 * ⇒ `available: false` (the empty-state `—`).
 */
export function topByVolume(
  flows: readonly FlowSummary[] | null | undefined,
  dimension: LeaderboardDimension,
): Leaderboard {
  const rows = flows ?? [];
  if (rows.length === 0) return emptyLeaderboard(dimension);
  const groups = accumulate(rows, dimension);
  const built = [...groups.values()].map(toRow);
  built.sort((a, b) => b.volume - a.volume || (b.cost ?? -1) - (a.cost ?? -1) || a.key.localeCompare(b.key));
  return {
    available: true,
    dimension,
    totalFlows: rows.length,
    groupCount: built.length,
    rows: built.slice(0, TOP_N),
  };
}

/**
 * Build a "top models/providers by COST" leaderboard from the observed flow rows. Only groups with
 * a PRICED cost are ranked (an unpriced group has no dollar figure to rank by — it is excluded from
 * the cost board entirely rather than ranked as a fabricated `$0`); the cost inherits the WEAKEST
 * confidence of its priced flows. Ordered by cost desc (then volume desc, then key), capped to
 * {@link TOP_N}. A window with flows but NONE priced ⇒ `available: false` (no cost board to show —
 * the empty-state `—`, distinct from a `$0.00` board).
 */
export function topByCost(
  flows: readonly FlowSummary[] | null | undefined,
  dimension: LeaderboardDimension,
): Leaderboard {
  const rows = flows ?? [];
  if (rows.length === 0) return emptyLeaderboard(dimension);
  const groups = accumulate(rows, dimension);
  // Only PRICED groups can be ranked by cost (don't fabricate a `$0` rank for an unpriced group).
  const priced = [...groups.values()].filter((g) => g.pricedFlows > 0).map(toRow);
  priced.sort((a, b) => (b.cost ?? 0) - (a.cost ?? 0) || b.volume - a.volume || a.key.localeCompare(b.key));
  return {
    available: priced.length > 0,
    dimension,
    totalFlows: rows.length,
    groupCount: priced.length,
    rows: priced.slice(0, TOP_N),
  };
}

// ---------------------------------------------------------------------------
// Token-mix (prompt / completion / cached / reasoning split across the window).
// ---------------------------------------------------------------------------

/** One token class in the mix — its summed count + provenance (the per-class READOUT, not the bar). */
export interface TokenMixClass {
  /** Class id (`prompt`/`completion`/`cached`/`reasoning`). */
  key: 'prompt' | 'completion' | 'cached' | 'reasoning';
  /** Display label. */
  label: string;
  /** Summed tokens of this class across usage-bearing flows, or `null` when the class is UNREPORTED. */
  tokens: number | null;
  /**
   * Share of this class relative to the `prompt + completion` total, in [0,1], or `null` when
   * unavailable. NOTE: `cached` is a SUBSET of `prompt` and `reasoning` is a SUBCATEGORY of
   * `completion`, so these are OVERLAPPING shares (informational annotations) — they are NOT
   * additive and must NOT be stacked. The stacked bar uses the EXCLUSIVE {@link TokenMix.barSegments}
   * instead (which sum to ≤100%).
   */
  fraction: number | null;
  /** DQ tag — `measured` when the class is reported (incl. a real `0`), `unavailable` when not (→ `—`). */
  quality: Quality;
}

/**
 * One EXCLUSIVE segment of the stacked token-mix bar. Because `cached ⊆ prompt` and
 * `reasoning ⊆ completion`, stacking the raw four classes can exceed 100%. These segments instead
 * partition the `prompt + completion` total into NON-OVERLAPPING buckets — `prompt - cached` (the
 * non-cached input), `cached`, `completion - reasoning` (the non-reasoning output), `reasoning` —
 * so their fractions sum to ≤1 (== 1 when total > 0). An unreported optional class carves nothing
 * out (its carve-out segment is omitted and the full prompt/completion segment stands).
 */
export interface TokenMixSegment {
  key: 'prompt_uncached' | 'cached' | 'completion_unreasoned' | 'reasoning';
  /** The token-class color key the bar/legend maps to (`cached`/`reasoning` carve-outs reuse theirs). */
  colorKey: 'prompt' | 'completion' | 'cached' | 'reasoning';
  /** Share of the `prompt + completion` total in [0,1] (always finite; `0` segments may be dropped by the view). */
  fraction: number;
}

/** The token-mix model the overview renders (the prompt/completion/cached/reasoning split). */
export interface TokenMix {
  /** Whether ANY usage-bearing flow was observed. `false` ⇒ every class `—` (don't-lie-with-zeros). */
  available: boolean;
  /** Count of flows that reported a usage block (the measurability base). */
  usageFlows: number;
  /** The summed total of the REQUIRED classes (`prompt + completion`) — the mix denominator. */
  totalTokens: number;
  /** The four token classes (the per-class READOUT; an unreported optional class is `unavailable`/`—`). */
  classes: TokenMixClass[];
  /** The EXCLUSIVE stacked-bar segments (sum ≤1; cached/reasoning carved out of prompt/completion). */
  barSegments: TokenMixSegment[];
}

/** Sum an OPTIONAL usage class across flows — `null` (UNAVAILABLE) when NO flow reported it. A
 *  reported `0` is a MEASURED zero and makes the class available (sum stays a real number). */
function sumOptional(rows: readonly FlowSummary[], pick: (u: NonNullable<FlowSummary['usage']>) => number | null | undefined): number | null {
  let sum = 0;
  let reported = false;
  for (const f of rows) {
    if (!f.usage) continue;
    const v = pick(f.usage);
    if (typeof v === 'number' && Number.isFinite(v)) {
      sum += v;
      reported = true;
    }
  }
  return reported ? sum : null;
}

/** Sum a REQUIRED usage class (`prompt`/`completion`) across usage-bearing flows (always a number). */
function sumRequired(rows: readonly FlowSummary[], pick: (u: NonNullable<FlowSummary['usage']>) => number): number {
  let sum = 0;
  for (const f of rows) {
    if (!f.usage) continue;
    const v = pick(f.usage);
    if (Number.isFinite(v)) sum += v;
  }
  return sum;
}

/**
 * Build the token-mix split across the observed flow rows. The REQUIRED `prompt`/`completion`
 * classes are summed across usage-bearing flows (a real `0` is measured); the OPTIONAL
 * `cached`/`reasoning` classes are `unavailable` (`—`) when NO flow in the window reported them
 * (don't-lie-with-zeros — never a fabricated `0`), and `measured` (incl. a real `0`) otherwise. The
 * per-class `fraction` (over `prompt + completion`) is an OVERLAPPING annotation (cached ⊆ prompt,
 * reasoning ⊆ completion), so the stacked BAR uses `barSegments` — EXCLUSIVE buckets that sum to ≤1.
 * A window with NO usage-bearing flow ⇒ `available: false` (every class `—`, no bar segments).
 */
export function tokenMix(flows: readonly FlowSummary[] | null | undefined): TokenMix {
  const rows = flows ?? [];
  const usageRows = rows.filter((f) => !!f.usage);
  if (usageRows.length === 0) {
    const none: TokenMixClass[] = TOKEN_CLASS_SPECS.map((s) => ({
      key: s.key,
      label: s.label,
      tokens: null,
      fraction: null,
      quality: 'unavailable',
    }));
    return { available: false, usageFlows: 0, totalTokens: 0, classes: none, barSegments: [] };
  }

  const prompt = sumRequired(usageRows, (u) => u.prompt);
  const completion = sumRequired(usageRows, (u) => u.completion);
  const cached = sumOptional(usageRows, (u) => u.cached);
  const reasoning = sumOptional(usageRows, (u) => u.reasoning);
  const totalTokens = prompt + completion;

  // PARENT-CLAMPED subset amounts — the SAME values the bar segments use (review MEDIUM): `cached` is a
  // SUBSET of `prompt`, `reasoning` of `completion`, so a malformed `cached > prompt` (or `reasoning >
  // completion`) is clamped to its parent. The readout SHARE is derived from these clamped amounts so a
  // per-class share can never read OVER 100% even when the raw total is malformed (consistent with the
  // clamped bar). The displayed token COUNT stays the raw measured value (honest data, not silently
  // altered) — only the share is bounded. An unreported (`null`) optional class stays UNAVAILABLE.
  const cachedIn = cached === null ? null : Math.min(Math.max(0, cached), prompt);
  const reasoningIn = reasoning === null ? null : Math.min(Math.max(0, reasoning), completion);
  const tokensByKey: Record<TokenMixClass['key'], number | null> = { prompt, completion, cached, reasoning };
  // The fraction NUMERATOR per class: required classes use their own total; optional classes use the
  // parent-clamped amount so the share is bounded by ≤ the parent's share.
  const shareNumByKey: Record<TokenMixClass['key'], number | null> = {
    prompt,
    completion,
    cached: cachedIn,
    reasoning: reasoningIn,
  };
  const classes: TokenMixClass[] = TOKEN_CLASS_SPECS.map((s) => {
    const tokens = tokensByKey[s.key];
    if (tokens === null) {
      // An unreported optional class — UNAVAILABLE (`—`), no share. Never a fabricated `0`.
      return { key: s.key, label: s.label, tokens: null, fraction: null, quality: 'unavailable' };
    }
    const shareNum = shareNumByKey[s.key];
    const fraction = totalTokens > 0 && shareNum !== null ? shareNum / totalTokens : null;
    return { key: s.key, label: s.label, tokens, fraction, quality: 'measured' };
  });

  // EXCLUSIVE bar segments (cached carved out of prompt, reasoning out of completion) so the stack
  // sums to ≤1 (== 1 when total > 0) — never the >100% an additive cached+reasoning stack would give.
  // Uses the SAME parent-clamped subset amounts as the readout shares above (`0` when unreported).
  const barSegments: TokenMixSegment[] = [];
  if (totalTokens > 0) {
    const cachedSeg = cachedIn ?? 0;
    const reasoningSeg = reasoningIn ?? 0;
    const pushSeg = (key: TokenMixSegment['key'], colorKey: TokenMixSegment['colorKey'], tokens: number) => {
      if (tokens > 0) barSegments.push({ key, colorKey, fraction: tokens / totalTokens });
    };
    pushSeg('prompt_uncached', 'prompt', prompt - cachedSeg);
    pushSeg('cached', 'cached', cachedSeg);
    pushSeg('completion_unreasoned', 'completion', completion - reasoningSeg);
    pushSeg('reasoning', 'reasoning', reasoningSeg);
  }

  return { available: true, usageFlows: usageRows.length, totalTokens, classes, barSegments };
}

interface TokenClassSpec {
  key: TokenMixClass['key'];
  label: string;
}

/** The token classes in display order (prompt/completion are the always-reported base). */
const TOKEN_CLASS_SPECS: readonly TokenClassSpec[] = [
  { key: 'prompt', label: 'prompt' },
  { key: 'completion', label: 'completion' },
  { key: 'cached', label: 'cached' },
  { key: 'reasoning', label: 'reasoning' },
];

/** Format a token-mix share fraction as a percent (`62%`), or `—` when unavailable. */
export function fmtMixShare(fraction: number | null): string {
  if (fraction === null || !Number.isFinite(fraction)) return UNAVAILABLE;
  return `${Math.round(fraction * 100)}%`;
}
