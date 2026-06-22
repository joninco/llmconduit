/**
 * Context-window utilization derivations (gap 09) — how much of a model's context window a flow
 * consumed. Pure + DOM-free so the inspector gauge AND the aggregate context-pressure stat share
 * ONE honest computation (and it is unit-testable, the sibling of `tokenEconomics.ts`).
 *
 * The operator question (spec 09): "Are we near max context — risking slow prefill, truncation,
 * or 400s?" This makes that legible WITHOUT lying.
 *
 * Data-quality contract (baked into every value here — Codex checks this):
 *  - Utilization `%` is `derived` (`used ÷ context_limit`). It is `derived` ONLY when BOTH inputs
 *    are known finite integers; otherwise it is `unavailable` (`—`), NEVER a fabricated `0%`/`100%`:
 *      • `context_limit` is `null`/absent (gap-06 UNKNOWN capacity) ⇒ unavailable — you cannot
 *        divide by an unknown denominator, and a `0` ceiling would read as garbage/infinite use.
 *      • the used-token count is `null`/unreported (the gap-07 `Option`) ⇒ unavailable — you cannot
 *        divide by an unknown numerator either. This is distinct from a measured `0` used.
 *  - A genuine `0%` (a real `0` used tokens against a KNOWN limit) is a DERIVED zero (renders
 *    `0.0%`), distinct from `—` (unavailable). "Unknown capacity" is never shown as "0% used".
 *  - The OVERFLOW-risk flag is raised ONLY with a real `context_limit` + a real used count: it is a
 *    derived signal, never inferred from missing data.
 *
 * The "used" tokens charged against the window = prompt (the context the model had to ingest) PLUS
 * completion (the tokens it emitted into the same window). `total` is preferred when the upstream
 * reports it (it is the authoritative sum and may include classes the split omits); otherwise it
 * falls back to `prompt + completion`. `cached`/`reasoning` are NOT separately added — they are
 * already accounted inside prompt/completion/total (cached is a subset of prompt; reasoning is a
 * subset of completion/total), so adding them would double-count the window.
 */
import type { Usage } from '../../api/types';
import { fmtTokens } from './format';

/** The data-quality tier of a single utilization figure (mirrors the cross-cutting rule). */
export type UtilQuality = 'derived' | 'unavailable';

/**
 * The near-limit / overflow risk band for a DERIVED utilization (only meaningful when the `%` is
 * derived; an unavailable utilization carries `none`). Thresholds are deliberate, operator-facing:
 *  - `over`    — `pct >= 100`: at/over the model's advertised window → truncation / 400 risk NOW.
 *  - `near`    — `pct >= WARN`: approaching the ceiling → slow prefill / imminent overflow.
 *  - `ok`      — comfortably under the warning threshold.
 *  - `none`    — utilization unavailable (no band to show).
 */
export type UtilRisk = 'ok' | 'near' | 'over' | 'none';

/**
 * The near-limit WARNING threshold (percent): at or above this a flow is flagged `near` (amber).
 * Below 100 so an operator gets a heads-up BEFORE the window overflows.
 */
export const CONTEXT_WARN_PCT = 85;

/** A rendered context-utilization figure: the display string + its provenance tier + risk band. */
export interface ContextUtilization {
  /** `derived` only when both used + context_limit are known; else `unavailable`. */
  quality: UtilQuality;
  /**
   * Utilization fraction `used / context_limit`, clamped to `>= 0`. `null` when unavailable.
   * NOT clamped at the top: an over-budget flow can read `> 1` (e.g. `1.04`) so the overflow is
   * honestly visible rather than silently pinned at 100%.
   */
  fraction: number | null;
  /** Display percent string (`72.4%`, `0.0%`, `103.2%`) or `—` when unavailable. */
  percentLabel: string;
  /** Tokens used (prompt+completion / total). `null` when usage is unreported. */
  usedTokens: number | null;
  /** The model's context capacity (gap-06). `null` when unknown. */
  contextLimit: number | null;
  /**
   * Remaining headroom tokens = `context_limit - used` (floored at 0). `null` when unavailable.
   * A negative raw headroom (over budget) floors to `0` — there is no NEGATIVE headroom to show.
   */
  remainingTokens: number | null;
  /** Formatted headroom (`118.0k`, `0`) or `—` when unavailable. */
  remainingLabel: string;
  /** Risk band (drives the gauge color + a near/over badge). `none` when unavailable. */
  risk: UtilRisk;
}

/** The unavailable utilization singleton-shape (all `—`/`null`, quality unavailable, risk none). */
function unavailable(usedTokens: number | null, contextLimit: number | null): ContextUtilization {
  return {
    quality: 'unavailable',
    fraction: null,
    percentLabel: '—',
    usedTokens,
    contextLimit,
    remainingTokens: null,
    remainingLabel: '—',
    risk: 'none',
  };
}

/**
 * The tokens charged against the context window for a usage block: prefer the upstream-reported
 * `total` (authoritative sum), else `prompt + completion`. Returns `null` when neither yields a
 * finite count (usage unreported) — the don't-lie-with-zeros numerator gate.
 */
export function contextUsedTokens(usage: Usage | null | undefined): number | null {
  if (!usage) return null;
  if (Number.isFinite(usage.total)) return Math.max(0, usage.total);
  const { prompt, completion } = usage;
  if (Number.isFinite(prompt) && Number.isFinite(completion)) {
    return Math.max(0, prompt + completion);
  }
  // A partial usage block (e.g. only prompt) still yields a usable lower-bound numerator: the
  // prompt alone is real context the model ingested. Completion-only is likewise usable.
  if (Number.isFinite(prompt)) return Math.max(0, prompt);
  if (Number.isFinite(completion)) return Math.max(0, completion);
  return null;
}

/** Map a derived utilization fraction to its risk band (only called when quality is derived). */
function riskFor(fraction: number): UtilRisk {
  const pct = fraction * 100;
  if (pct >= 100) return 'over';
  if (pct >= CONTEXT_WARN_PCT) return 'near';
  return 'ok';
}

/**
 * Compute the context-window utilization for ONE flow's usage against a model's `context_limit`.
 *
 * `contextLimit` is the gap-06 nullable per-model capacity: a finite POSITIVE integer when known,
 * `null`/non-finite/`<= 0` when unknown (a `0` or negative ceiling is treated as UNKNOWN — never a
 * divide-by-zero or a fabricated 100%). `usage` is the gap-07 optional usage block.
 *
 * Returns a fully-formed display object; both the gauge and the aggregate consume it so neither can
 * paint an unavailable utilization as a real bar.
 */
export function contextUtilization(
  usage: Usage | null | undefined,
  contextLimit: number | null | undefined,
): ContextUtilization {
  const used = contextUsedTokens(usage);
  // Normalize the capacity: only a finite, strictly-positive limit is a usable denominator. A
  // `null`, NaN, or `<= 0` limit is UNKNOWN capacity ⇒ unavailable (NOT a fabricated 0%/100%).
  const limit =
    typeof contextLimit === 'number' && Number.isFinite(contextLimit) && contextLimit > 0
      ? contextLimit
      : null;

  // Both inputs must be known for a real utilization. Either missing ⇒ unavailable (`—`).
  if (used === null || limit === null) {
    return unavailable(used, limit);
  }

  const fraction = used / limit; // may exceed 1 when over budget (kept honest, not clamped)
  const remainingTokens = Math.max(0, limit - used);
  const risk = riskFor(fraction);
  return {
    quality: 'derived',
    fraction,
    percentLabel: `${(fraction * 100).toFixed(1)}%`,
    usedTokens: used,
    contextLimit: limit,
    remainingTokens,
    remainingLabel: fmtTokens(remainingTokens),
    risk,
  };
}

/**
 * A model-id → `context_limit` lookup (the gap-06 nullable catalog), the shape the gauge + the
 * aggregate read. Built from the `/catalog` array. A `null` window stays `null` (unknown), distinct
 * from absent — both render `—`.
 */
export type ContextLimitMap = Record<string, number | null>;

/** Resolve a flow's served (then requested) model's context limit from the catalog map. */
export function contextLimitFor(
  modelServed: string | null | undefined,
  modelRequested: string | null | undefined,
  limits: ContextLimitMap,
): number | null {
  const model = modelServed ?? modelRequested ?? null;
  if (!model) return null;
  // `?? null` collapses an absent key (undefined) to the same `null` an explicit unknown uses.
  return limits[model] ?? null;
}

/**
 * An AGGREGATE context-pressure summary over a group of flows (spec 09: "an aggregate context-
 * pressure stat exists"). `derived`. Honest aggregation rules:
 *  - Only flows whose utilization is DERIVED (both used + a known limit) contribute. A flow with an
 *    unknown limit OR unreported usage is EXCLUDED from the pressure figures entirely — it cannot
 *    push the peak/near counts (that would lie). `measuredFlows` counts the contributors so the UI
 *    can render `—` (unavailable) when NONE are measurable.
 *  - `peakFraction` is the MAX utilization across contributing flows (the worst-case window) — the
 *    pressure signal an operator watches. `null` when no flow is measurable.
 *  - `nearCount`/`overCount` count contributing flows in the `near`/`over` risk bands.
 */
export interface ContextPressureAggregate {
  /** Count of flows whose utilization is derived (contributed to the pressure figures). */
  measuredFlows: number;
  /** Total flows considered (measurable + not) — context for the readout. */
  totalFlows: number;
  /** The peak (max) utilization fraction across contributing flows; `null` ⇒ none measurable. */
  peakFraction: number | null;
  /** Display percent of the peak (`92.1%`) or `—` when no flow is measurable. */
  peakLabel: string;
  /** The risk band of the PEAK flow (drives the stat accent). `none` ⇒ none measurable. */
  peakRisk: UtilRisk;
  /** Count of contributing flows at/over the near-limit warning threshold. */
  nearCount: number;
  /** Count of contributing flows at/over 100% (overflow risk). */
  overCount: number;
}

/**
 * Roll up the context pressure across a set of flows against the catalog limits. Pure: the
 * component renders the returned figures. A set with NO measurable flow yields `peakFraction: null`
 * / `peakLabel: '—'` (unavailable), never a fabricated `0%` peak.
 */
export function aggregateContextPressure(
  flows: { model_served?: string | null; model_requested?: string | null; usage?: Usage | null }[],
  limits: ContextLimitMap,
): ContextPressureAggregate {
  let measuredFlows = 0;
  let peakFraction: number | null = null;
  let peakRisk: UtilRisk = 'none';
  let nearCount = 0;
  let overCount = 0;

  for (const flow of flows) {
    const limit = contextLimitFor(flow.model_served, flow.model_requested, limits);
    const util = contextUtilization(flow.usage ?? null, limit);
    if (util.quality !== 'derived' || util.fraction === null) continue; // exclude the unmeasurable
    measuredFlows += 1;
    if (peakFraction === null || util.fraction > peakFraction) {
      peakFraction = util.fraction;
      peakRisk = util.risk;
    }
    if (util.risk === 'near' || util.risk === 'over') nearCount += 1;
    if (util.risk === 'over') overCount += 1;
  }

  return {
    measuredFlows,
    totalFlows: flows.length,
    peakFraction,
    peakLabel: peakFraction === null ? '—' : `${(peakFraction * 100).toFixed(1)}%`,
    peakRisk,
    nearCount,
    overCount,
  };
}
