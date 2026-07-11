/**
 * Token-economics derivations (gap 08) — the cached / reasoning token split, the cache-hit
 * rate, and the "$ saved by prefix caching" figure. Pure + DOM-free so the popover, the
 * inspector line, and the aggregate panel share ONE honest computation (and it is unit-testable).
 *
 * The operator question (spec 08): "Is prefix caching saving money, and what are reasoning
 * models really costing?" This module makes that legible WITHOUT lying:
 *
 * Data-quality contract (baked into every value here — Codex checks this):
 *  - Token SPLIT (cached / reasoning) is `measured`: it is the upstream-reported count. An
 *    UNREPORTED class (`usage.cached`/`reasoning` is `null`/absent — the gap-07 `Option`) is
 *    `unavailable` ⇒ renders `—`, NEVER `0`. A provider-reported `0` is a MEASURED zero (a real
 *    cache miss) ⇒ renders `0`, distinct from `—`.
 *  - Cache-hit RATE is `derived` (`cached / prompt`). It is `unavailable` when `cached` is
 *    unreported (you cannot claim a hit rate you did not measure) — NOT counted as a 0% miss.
 *    A reported `cached === 0` is a genuine `0%` (a real miss), distinct from unavailable.
 *  - "$ SAVED" is `derived` from the signed cache-price impact persisted by the backend at the
 *    terminal seam. An absent impact is unavailable; the browser never consults the current price
 *    table and therefore cannot reprice a historical flow.
 *
 * Cost itself is owned by the backend (`flowModel.flowCost` / the `cost_confidence` tag); this
 * module never re-derives the flow cost. The only `derived` dollar formula here is the documented
 * cache SAVING, which is the negated persisted cost impact.
 */
import type { FlowSummary, Usage } from '../../api/types';
import { fmtCost, fmtTokens } from './format';

/** The data-quality tier of a single token-economics figure (mirrors the cross-cutting rule). */
export type EconQuality = 'measured' | 'derived' | 'unavailable';

/** A rendered token-economics figure: the display string + its provenance tier. */
export interface EconValue {
  /** Formatted display string (`128`, `12.4%`, `$0.0003`, `—`). `—` ⇒ unavailable. */
  value: string;
  /** Provenance tier — drives the `data-quality` attribute + any label. */
  quality: EconQuality;
}

/** A raw cached/reasoning count rendered as an `EconValue`: `measured` when reported, else `—`. */
function tokenClass(n: number | null | undefined): EconValue {
  // gap-07 contract: `null`/absent ⇒ UNREPORTED (unavailable, `—`); a present finite `0` is a
  // MEASURED zero (renders `0`). `fmtTokens` already maps null/undefined→`—` and `0`→`"0"`.
  if (n === null || n === undefined || !Number.isFinite(n)) {
    return { value: '—', quality: 'unavailable' };
  }
  return { value: fmtTokens(n), quality: 'measured' };
}

/**
 * The cache-hit rate `cached / prompt` as an `EconValue`. `derived`.
 *  - `cached` UNREPORTED (null/absent) ⇒ `unavailable` (`—`): a hit rate cannot be claimed for a
 *    class the provider never reported (NOT a 0% miss).
 *  - `prompt <= 0` (no prompt tokens to hit against) ⇒ `unavailable` (`—`): the ratio is undefined.
 *  - a reported `cached === 0` over a positive prompt ⇒ a genuine `0.0%` (a real miss), MEASURED-distinct.
 */
function cacheHitRate(usage: Usage): EconValue {
  const cached = usage.cached;
  if (cached === null || cached === undefined || !Number.isFinite(cached)) {
    return { value: '—', quality: 'unavailable' };
  }
  if (!Number.isFinite(usage.prompt) || usage.prompt <= 0) {
    return { value: '—', quality: 'unavailable' };
  }
  const pct = (Math.max(0, cached) / usage.prompt) * 100;
  return { value: `${pct.toFixed(1)}%`, quality: 'derived' };
}

/**
 * The dollars SAVED by serving `cached` prompt tokens at the cached rate instead of the full
 * input rate. The backend stores cost impact as `(cache rate - input rate) * cached tokens`; this
 * presentation negates it so positive means saved and negative means caching cost more.
 */
function cacheSaving(cachePriceImpactUsd: number | null | undefined): EconValue {
  if (cachePriceImpactUsd === null || cachePriceImpactUsd === undefined || !Number.isFinite(cachePriceImpactUsd)) {
    return { value: '—', quality: 'unavailable' };
  }
  // The backend persists signed COST impact (cache rate - input rate). Negate it for
  // the existing "$ saved" presentation; a cache rate above input becomes negative saving.
  return { value: fmtCost(-cachePriceImpactUsd), quality: 'derived' };
}

/**
 * The full token-economics breakdown for ONE flow — the shape the tokens-cell popover and the
 * inspector line both render. Every field is an `EconValue` carrying its own provenance, so the
 * UI tags each line and a `derived`/`unavailable` line can never read as a fabricated `0`.
 */
export interface TokenEconomics {
  /** Prompt (input) tokens — always-reported core count; `measured` (or `—` if non-finite). */
  prompt: EconValue;
  /** Completion (output) tokens — always-reported core count; `measured`. */
  completion: EconValue;
  /** Cache-read prompt tokens; `measured` when reported, `—` (unavailable) when not. */
  cached: EconValue;
  /** Reasoning tokens; `measured` when reported, `—` (unavailable) when not. */
  reasoning: EconValue;
  /** Cache-hit rate `cached/prompt`; `derived`, or `—` when cached is unreported / no prompt. */
  cacheHit: EconValue;
  /** $ saved by prefix caching; `derived`, shown only with a CONFIGURED cached price + reported cached. */
  saved: EconValue;
  /** Whether the served model has a configured cached price (drives the "$ saved" row's presence). */
  cachedPriceConfigured: boolean;
}

/**
 * Build the token-economics breakdown for a flow. `usage === null` (no usage reported at all)
 * ⇒ every figure is `unavailable` (`—`), never a fabricated `0`. The price for the SERVED model
 * (the model actually billed) gates the `$ saved` figure via its `cached_price_configured` flag.
 */
export function tokenEconomics(flow: FlowSummary): TokenEconomics {
  const usage = flow.usage ?? null;
  const calculationUsage = flow.normalized_usage ?? usage;
  const cachedPriceConfigured = Number.isFinite(flow.cache_price_impact_usd);

  if (!usage || !calculationUsage) {
    const na: EconValue = { value: '—', quality: 'unavailable' };
    return {
      prompt: na,
      completion: na,
      cached: na,
      reasoning: na,
      cacheHit: na,
      saved: na,
      cachedPriceConfigured,
    };
  }

  return {
    prompt: tokenClass(usage.prompt),
    completion: tokenClass(usage.completion),
    cached: tokenClass(usage.cached),
    reasoning: tokenClass(usage.reasoning),
    cacheHit: cacheHitRate(calculationUsage),
    saved: cacheSaving(flow.cache_price_impact_usd),
    cachedPriceConfigured,
  };
}

/**
 * An AGGREGATE cache-hit rate over a group of flows (spec 08: "aggregate cache-hit rate by
 * model/client"). It is `derived`. Honest aggregation rules:
 *  - Only flows that REPORTED `cached` (a finite number) contribute to BOTH the numerator (sum of
 *    cached) and the denominator (sum of prompt). A flow whose `cached` is unreported is EXCLUDED
 *    entirely (it cannot push the rate toward 0% — that would lie). `reportedSamples` counts the
 *    contributing flows so the UI can render `—` (unavailable) for a group with NONE.
 *  - `saved` sums the per-flow `derived` savings, counting ONLY flows with a configured cached
 *    price (presence) AND a reported cached count; `savedConfigured` is true iff at least one flow
 *    contributed — so a group with no configured cache price shows the hit rate but NO dollar total.
 *  - `confident` is true iff EVERY grouped flow's `cost_confidence === 'confident'` — the taint runs
 *    for ALL members, NOT only the cached-reporting ones, so a group with a confident cached sample
 *    plus a non-confident UNREPORTED sample is still `estimated`. Otherwise the group MUST be
 *    labelled (the cross-cutting rule) — a single non-confident member taints the aggregate.
 */
export interface CacheAggregate {
  /** Group key (a model id or a client label). */
  key: string;
  /** Sum of reported cached prompt tokens across contributing flows. */
  cachedTokens: number;
  /** Sum of prompt tokens across contributing flows (the hit-rate denominator). */
  promptTokens: number;
  /** Count of flows that reported `cached` (contributed to the rate). `0` ⇒ rate unavailable. */
  reportedSamples: number;
  /** Total flows in the group (reported + unreported) — context for the readout. */
  totalSamples: number;
  /** Summed `$ saved` across flows with a configured cached price + reported cached. */
  savedDollars: number;
  /** True iff ≥1 flow contributed a configured-price saving (gates the `$ saved` column). */
  savedConfigured: boolean;
  /** False iff ANY grouped flow (reported or not) is not `confident` ⇒ the aggregate is `estimated`. */
  confident: boolean;
}

/** The display-ready aggregate row: formatted hit-rate + saving + the labelling flags. */
export interface CacheAggregateRow {
  key: string;
  /** Cache-hit rate string (`12.4%`) or `—` when no flow reported cached (unavailable). */
  hitRate: EconValue;
  /** $ saved string (`$0.0123`) or `—` when no configured-price saving in the group. */
  saved: EconValue;
  /** Reported / total sample counts (e.g. "2 / 3 reported"). */
  reportedSamples: number;
  totalSamples: number;
  /** True ⇒ the row is an ESTIMATE (some member not `confident`) and MUST be badged `est`. */
  estimated: boolean;
}

/**
 * Group flows by a key (model or client) and compute each group's aggregate cache economics,
 * then format them into display rows sorted by reported cache volume (busiest cache first).
 * Groups with NO key (null/blank) are dropped. A group with reported flows but a zero prompt sum
 * renders `—` for the rate (undefined ratio) — never a fabricated `0%`.
 */
export function aggregateCacheByKey(
  flows: FlowSummary[],
  keyOf: (flow: FlowSummary) => string | null | undefined,
): CacheAggregateRow[] {
  const groups = new Map<string, CacheAggregate>();

  for (const flow of flows) {
    const key = keyOf(flow);
    if (!key) continue;
    let agg = groups.get(key);
    if (!agg) {
      agg = {
        key,
        cachedTokens: 0,
        promptTokens: 0,
        reportedSamples: 0,
        totalSamples: 0,
        savedDollars: 0,
        savedConfigured: false,
        confident: true,
      };
      groups.set(key, agg);
    }
    agg.totalSamples += 1;

    // Confidence taint runs for EVERY grouped flow — including flows that did NOT report cached
    // (unreported / `usage === null`). A single non-confident member (estimated/unavailable) makes
    // the whole group an estimate; gating this on the cached-reported branch (as gap-08 round 0 did)
    // would let a group with [confident cached + non-confident unreported] render as fully confident.
    if (flow.cost_confidence !== 'confident') agg.confident = false;

    const usage = flow.normalized_usage ?? flow.usage ?? null;
    const cached = usage?.cached;
    // A flow contributes to the hit rate ONLY when it reported a finite cached count.
    if (usage && cached !== null && cached !== undefined && Number.isFinite(cached)) {
      agg.reportedSamples += 1;
      agg.cachedTokens += Math.max(0, cached);
      if (Number.isFinite(usage.prompt)) agg.promptTokens += Math.max(0, usage.prompt);

      // Terminal-time signed impact is authoritative; never reprice this flow in the browser.
      if (Number.isFinite(flow.cache_price_impact_usd)) {
        agg.savedDollars += -(flow.cache_price_impact_usd ?? 0);
        agg.savedConfigured = true;
      }
    }
  }

  const rows: CacheAggregateRow[] = [...groups.values()].map((agg): CacheAggregateRow => {
    // Rate unavailable when no flow reported cached OR the prompt denominator is 0 (undefined ratio).
    const hitRate: EconValue =
      agg.reportedSamples === 0 || agg.promptTokens <= 0
        ? { value: '—', quality: 'unavailable' }
        : { value: `${((agg.cachedTokens / agg.promptTokens) * 100).toFixed(1)}%`, quality: 'derived' };
    const saved: EconValue = agg.savedConfigured
      ? { value: fmtCost(agg.savedDollars), quality: 'derived' }
      : { value: '—', quality: 'unavailable' };
    return {
      key: agg.key,
      hitRate,
      saved,
      reportedSamples: agg.reportedSamples,
      totalSamples: agg.totalSamples,
      // A group is an estimate when ANY member is non-confident (taint runs for every grouped flow,
      // reported or not). Independent of whether a hit rate is shown — a non-confident unreported
      // member still makes the group an estimate, and the `est` badge must surface it even when the
      // hit rate is `—` (unavailable). The badge's own render must not re-gate on hit-rate quality.
      estimated: !agg.confident,
    };
  });

  // Busiest cache first (most cached tokens). Stable tiebreak on key for determinism.
  rows.sort((a, b) => {
    const av = groups.get(a.key)!.cachedTokens;
    const bv = groups.get(b.key)!.cachedTokens;
    if (bv !== av) return bv - av;
    return a.key.localeCompare(b.key);
  });
  return rows;
}
