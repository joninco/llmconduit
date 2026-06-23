/**
 * providerLatency (gap 13) — the PURE, DOM-free model that turns a provider's `ProviderLatency`
 * (the spec-12 per-provider DTO on the REST/snapshot topology node) into render-ready figures.
 * Sibling of `tokenEconomics.ts`/`contextUtilization.ts`/`latencyBreakdown.ts`: unit-testable in
 * isolation so the tooltip tile + the node sizing can never disagree with the numbers they derive.
 *
 * Operator question (spec 13): "Which upstream is degrading?" — the per-provider p50/p95/p99 +
 * error rate + per-class failure distribution, replacing the topology tooltip's GLOBAL p99.
 *
 * SOURCE (spec 12 + the gap-12 discovery): `ProviderLatency` is present ONLY on the REST
 * `/topology` + `/snapshot` node — the LIVE WS `topology_update` frame carries it ABSENT (it does
 * not join the metrics window). The caller therefore reads it from the REST/snapshot path, never
 * the live WS topology frame; this module is agnostic to the source (it takes the DTO or absence).
 *
 * DATA-QUALITY INVARIANTS (the heart of an honest per-provider tile — asserted in tests):
 *  - ABSENT `ProviderLatency` (no in-window samples) ⇒ the whole tile is `unavailable`: every
 *    figure reads `—`, NEVER a fabricated `0ms`/`0%`. A node with no per-provider data is sized/
 *    colored NEUTRAL (not a `0`-sized or falsely-healthy node).
 *  - A PRESENT entry is always `derived` with `samples >= 1`: percentiles are real DERIVED ms; the
 *    DQ tag travels onto every figure so the UI labels them.
 *  - `error_rate` is a MEASURED percentage: an all-served provider reads a genuine `0%`
 *    (`measured`-zero), DISTINCT from the `unavailable`/absent `—`.
 *  - the `__other__` overflow bucket + the `unknown` provider sentinel are rendered HONESTLY
 *    (labelled "other providers (overflow)" / "unknown provider"), never hidden.
 */
import type { AttemptErrorClass, ProviderErrorDistribution, ProviderLatency } from '../../api/types';
import { ATTEMPT_ERROR_CLASSES } from '../../api/types';

/** Provenance of a per-provider figure — mirrors the dashboard's measured/derived/unavailable tags. */
export type Quality = 'measured' | 'derived' | 'estimated' | 'unavailable';

/** The bounded overflow/sentinel provider keys the metrics layer can emit (spec 12). */
export const OVERFLOW_PROVIDER_KEY = '__other__';
export const UNKNOWN_PROVIDER_KEY = 'unknown';

/** One headline latency/error figure: a formatted value + its provenance. */
export interface ProviderFigure {
  /** Pre-formatted display string (`82ms` / `4.0%` / `—`). */
  text: string;
  /** Provenance — `derived` for a real percentile, `measured` for the error rate, else `unavailable`. */
  quality: Quality;
}

/** One per-class failure row (only classes that actually occurred appear). */
export interface ProviderErrorRow {
  /** The bounded taxonomic class (gap 03), e.g. `http_status`. */
  class: AttemptErrorClass;
  /** Human label for the class, e.g. "http status". */
  label: string;
  /** The failure count for this class (always `>= 1` — a `0` class is omitted upstream). */
  count: number;
}

/**
 * The render model for one provider's per-provider tile. When `available` is false EVERY figure is
 * `unavailable` (`—`) — the caller renders the tile in its neutral/absent state; the node is sized
 * NEUTRAL. When true the figures carry real `derived`/`measured` values.
 */
export interface ProviderLatencyModel {
  /** Whether this provider has in-window per-provider data (a present `ProviderLatency`). */
  available: boolean;
  /** Display label for the provider this tile is for (overflow/sentinel keys are labelled). */
  providerLabel: string;
  /** True when this tile is for the `__other__` overflow bucket (caller may add a hint). */
  isOverflow: boolean;
  /** True when this tile is for the `unknown` provider sentinel. */
  isUnknown: boolean;
  /** p50 / p95 / p99 attempt latency (`derived`, ms) — `—`/`unavailable` when absent. */
  p50: ProviderFigure;
  p95: ProviderFigure;
  p99: ProviderFigure;
  /** Failure rate (`measured` %); a real `0%` for an all-served provider, `—` when absent. */
  errorRate: ProviderFigure;
  /** Served / total attempts in the window — context for the percentiles (`—` when absent). */
  samplesText: string;
  /** The per-class failure distribution (only occurring classes; empty when none / absent). */
  errors: ProviderErrorRow[];
}

/** Human label for a bounded error class (the gap-03 taxonomy), e.g. `http_status` → "http status". */
const ERROR_CLASS_LABEL: Record<AttemptErrorClass, string> = {
  connect: 'connect',
  http_status: 'http status',
  timeout: 'timeout',
  stream: 'stream',
  terminal: 'terminal',
  other: 'other',
};

/** Format a `derived` latency (ms) for a tile, or `—` when unavailable. Rounds (sub-ms is noise). */
export function fmtProviderLatencyMs(ms: number | null): string {
  if (ms === null || !Number.isFinite(ms)) return '—';
  return `${Math.round(ms)}ms`;
}

/**
 * Format a `measured` error rate (a percentage in [0,100]) for a tile, or `—` when unavailable. A
 * MEASURED `0` reads `0%` (an all-served provider) — distinct from the `unavailable` `—`. One
 * decimal under 10% (so 4.2% is visible), whole percent otherwise.
 */
export function fmtErrorRate(pct: number | null): string {
  if (pct === null || !Number.isFinite(pct)) return '—';
  if (pct === 0) return '0%';
  if (pct < 10) return `${pct.toFixed(1)}%`;
  return `${Math.round(pct)}%`;
}

/** Resolve a provider key to its display label (overflow/sentinel keys labelled honestly, spec 13). */
export function providerLabelFor(provider: string): string {
  if (provider === OVERFLOW_PROVIDER_KEY) return 'other providers (overflow)';
  if (provider === UNKNOWN_PROVIDER_KEY) return 'unknown provider';
  return provider;
}

/** The unavailable figure (no in-window data) — `—`, tagged `unavailable` (never a `0`). */
const UNAVAILABLE_FIGURE: ProviderFigure = { text: '—', quality: 'unavailable' };

/** Build the ordered per-class failure rows from a distribution — only classes with a `>0` count. */
function errorRows(errors: ProviderErrorDistribution): ProviderErrorRow[] {
  const rows: ProviderErrorRow[] = [];
  for (const cls of ATTEMPT_ERROR_CLASSES) {
    const count = errors[cls] ?? 0;
    if (count > 0) rows.push({ class: cls, label: ERROR_CLASS_LABEL[cls], count });
  }
  return rows;
}

/**
 * Build the per-provider tile model from a provider's `ProviderLatency` (or its ABSENCE).
 *
 * `per` ABSENT/null (a zero-sample provider — don't-lie-with-zeros) ⇒ `available: false` and every
 * figure `unavailable` (`—`); the optional `providerHint` (the node id we were resolving) supplies
 * the label so the tile still names the provider it has no data for. A PRESENT `per` ⇒ real
 * `derived` percentiles + a `measured` error rate (a real `0%` for an all-served provider).
 */
export function buildProviderLatency(
  per: ProviderLatency | null | undefined,
  providerHint?: string,
): ProviderLatencyModel {
  const rawKey = per?.provider ?? providerHint ?? '';
  const providerLabel = rawKey ? providerLabelFor(rawKey) : '—';
  const isOverflow = rawKey === OVERFLOW_PROVIDER_KEY;
  const isUnknown = rawKey === UNKNOWN_PROVIDER_KEY;

  // Absent ⇒ unavailable everywhere (the no-sample provider; renders `—`, never `0`).
  if (!per) {
    return {
      available: false,
      providerLabel,
      isOverflow,
      isUnknown,
      p50: UNAVAILABLE_FIGURE,
      p95: UNAVAILABLE_FIGURE,
      p99: UNAVAILABLE_FIGURE,
      errorRate: UNAVAILABLE_FIGURE,
      samplesText: '—',
      errors: [],
    };
  }

  // Present ⇒ a real `derived` measurement (`samples >= 1`). Percentiles are `derived`; the error
  // rate is `measured` (a directly-counted failed/total ratio — a real `0%` when all-served).
  const lat = (ms: number): ProviderFigure => ({ text: fmtProviderLatencyMs(ms), quality: 'derived' });
  return {
    available: true,
    providerLabel,
    isOverflow,
    isUnknown,
    p50: lat(per.p50),
    p95: lat(per.p95),
    p99: lat(per.p99),
    errorRate: { text: fmtErrorRate(per.error_rate), quality: 'measured' },
    samplesText: `${per.served}/${per.samples}`,
    errors: errorRows(per.errors),
  };
}

// ---------------------------------------------------------------------------
// Node sizing/color (spec 13: "Node sizing/color reflects per-provider latency/error;
// an `unavailable` provider → a neutral state + `—`, NOT a 0-sized or falsely-healthy node").
// Pure so the topology map's per-provider emphasis can never disagree with the tile.
// ---------------------------------------------------------------------------

/**
 * The per-provider EMPHASIS state for a topology node — derived from its per-provider metrics, NOT
 * the point-in-time `ProviderHealth.status`. It is overlaid on top of the existing health color (a
 * cooling/down node keeps its status color); this drives a SIZE multiplier + an error-rate ring so
 * a degrading provider stands out.
 *  - `unavailable` — no in-window per-provider data ⇒ NEUTRAL: the base size, no error ring. Never
 *    a `0`-sized node and never recolored to "healthy" (don't-lie-with-zeros at the node level).
 *  - `nominal`     — has data, low error rate ⇒ base size, no ring.
 *  - `degrading`   — has data, an ELEVATED error rate ⇒ enlarged + an error ring (visually hot).
 */
export type ProviderEmphasis = 'unavailable' | 'nominal' | 'degrading';

/** Error rate (%) at/above which a provider is emphasized as `degrading`. */
export const DEGRADING_ERROR_RATE_PCT = 10;
/**
 * p99 attempt latency (ms) at/above which a provider is emphasized as `degrading` on LATENCY alone
 * (a slow-but-no-errors upstream — the spec requires node sizing to reflect per-provider LATENCY,
 * not just errors). p99 (not p50) so a provider is flagged on its TAIL behavior, the thing that
 * actually hurts callers.
 */
export const DEGRADING_P99_MS = 2000;
/** p99 latency (ms) at which the latency-driven size scale saturates (bounded so layout stays sane). */
const P99_SATURATION_MS = 8000;
/** The upper bound on the node radius multiplier (one hot provider can't dominate the layout). */
const MAX_SIZE_SCALE = 1.5;

export interface ProviderNodeEmphasis {
  state: ProviderEmphasis;
  /** Radius MULTIPLIER applied to the base node radius (1 = unchanged). Bounded by `MAX_SIZE_SCALE`. */
  sizeScale: number;
  /** Whether to draw the red ERROR ring around the node (error-driven only — a latency-only
   * degradation enlarges the node but does NOT imply failures). */
  showErrorRing: boolean;
  /** The measured error rate (%) when available, else null (never a fabricated `0` when absent). */
  errorRatePct: number | null;
  /** The measured p99 attempt latency (ms) when available, else null (never fabricated when absent). */
  p99Ms: number | null;
  /** Whether this provider is degrading on LATENCY (p99 ≥ threshold) — drives the latency emphasis. */
  latencyDegraded: boolean;
}

/** The error-rate → size-scale contribution (1 below the threshold, 1.15→`MAX` as it climbs to 100%). */
function errorSizeScale(errorRatePct: number): number {
  if (errorRatePct < DEGRADING_ERROR_RATE_PCT) return 1;
  const t = Math.min(1, (errorRatePct - DEGRADING_ERROR_RATE_PCT) / (100 - DEGRADING_ERROR_RATE_PCT));
  return 1.15 + t * (MAX_SIZE_SCALE - 1.15);
}

/** The p99-latency → size-scale contribution (1 below the threshold, 1.15→`MAX` as p99 climbs). */
function latencySizeScale(p99Ms: number): number {
  if (p99Ms < DEGRADING_P99_MS) return 1;
  const t = Math.min(1, (p99Ms - DEGRADING_P99_MS) / (P99_SATURATION_MS - DEGRADING_P99_MS));
  return 1.15 + t * (MAX_SIZE_SCALE - 1.15);
}

/**
 * Map a provider's `ProviderLatency` (or absence) to its node emphasis. A provider is `degrading`
 * when EITHER its error rate OR its p99 latency is elevated (the spec: node sizing/color reflects
 * per-provider LATENCY *and* error). The size scale is the MAX of the error-driven and
 * latency-driven contributions, bounded so one hot provider can't dominate the layout; the red error
 * ring is shown only for ERROR-driven degradation (a slow-but-no-errors provider is enlarged but
 * carries no error ring — it has no failures to signal). An `unavailable` provider (no in-window
 * samples) is NEUTRAL — never `0`-sized, never forced healthy, and NEVER given a fabricated latency
 * emphasis (don't-lie-with-zeros); a MEASURED 0% error rate with a MEASURED high p99 IS flagged.
 */
export function providerNodeEmphasis(per: ProviderLatency | null | undefined): ProviderNodeEmphasis {
  if (!per) {
    // No samples ⇒ neutral. The node renders at full base size with its health color; we add nothing.
    return { state: 'unavailable', sizeScale: 1, showErrorRing: false, errorRatePct: null, p99Ms: null, latencyDegraded: false };
  }
  const errorRatePct = per.error_rate;
  const p99Ms = per.p99;
  const errorDegraded = errorRatePct >= DEGRADING_ERROR_RATE_PCT;
  const latencyDegraded = p99Ms >= DEGRADING_P99_MS;
  const sizeScale = Math.max(errorSizeScale(errorRatePct), latencySizeScale(p99Ms));
  const state: ProviderEmphasis = errorDegraded || latencyDegraded ? 'degrading' : 'nominal';
  return { state, sizeScale, showErrorRing: errorDegraded, errorRatePct, p99Ms, latencyDegraded };
}
