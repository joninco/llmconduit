/**
 * Chip descriptors — the pure mapping from a `MetricWindow` sample (+ its predecessor) to each
 * strip chip's display value, sparkline stroke, threshold accent, and delta direction.
 *
 * DOM-free so it is unit-testable and the component stays a thin renderer. Formatting reuses the
 * flow-table formatters where they fit (tokens), and adds small local ones for rates/latency/%.
 */
import type { CostConfidence, EngineThroughputSample, InstantMetricSample } from '../../api/types';
import { fmtCost, fmtCostRate, fmtLatency, fmtPercent, fmtRate, fmtTokensPerSec } from '../FlowTable/format';
import { colors } from '../../design/tokens';
import { metricUnavailable, type MetricKey, type MetricSource } from './metricHistory';

/** Error-% threshold above which the err chip turns red (spec: "red above threshold"). */
export const ERROR_PCT_THRESHOLD = 5;

/**
 * Below this many latency samples the p50/p95/p99 trio collapses into ONE "gateway E2E latency"
 * chip (U7): percentiles over a near-empty window are machinery pretending to be statistics.
 * Zero samples keeps the trio (each honestly `—`) so the unavailable semantics stay intact.
 */
export const LATENCY_PERCENTILE_MIN_SAMPLES = 5;

/**
 * Below this many priced samples the $/min chip stops extrapolating a rate and shows the
 * interval's actual cost total instead (U2): one priced request annualized into "$9.35/min"
 * reads as an alarm, not a measurement. Idle (retained) windows always show the total.
 */
export const COST_RATE_MIN_PRICED_SAMPLES = 3;

export type DeltaDir = 'up' | 'down' | 'flat';

/**
 * The data-quality provenance of a chip's value (IMPLEMENTATION_PLAN cross-cutting rule:
 * EVERY rendered metric is tagged measured / derived / estimated / unavailable). Rendered
 * via `data-quality` + an ARIA/title hint on the chip so operators can tell a directly
 * counted value from a derived/estimated one from an honest gap:
 *  - `measured`     — directly counted off the live gateway (`req/s`, `active_streams`).
 *  - `derived`      — computed from samples/counter deltas (err%, p50/p95/p99, tok/s).
 *  - `estimated`    — priced via the configured price table, i.e. a modelled estimate
 *                     ($/min). MUST be surfaced as such (the plan calls this out).
 *  - `unavailable`  — not measurable in this window; the value renders `—`, never `0`.
 */
export type MetricQuality = 'measured' | 'derived' | 'partial' | 'estimated' | 'unavailable';

export interface ChipDescriptor {
  key: MetricKey;
  label: string;
  /** Preformatted value string (tabular-nums applied by the chip component). */
  value: string;
  /** Sparkline stroke (hex token). */
  stroke: string;
  /** Token accent for the value text (threshold-driven for err%). */
  accent: 'accent' | 'healthy' | 'meta' | 'down' | 'text';
  /** Direction of change vs. the previous sample (drives the delta arrow). */
  delta: DeltaDir;
  /**
   * Data-quality provenance of the rendered value (finding 4). `unavailable` whenever
   * the value is `—`; otherwise the metric's intrinsic tier (measured/derived/estimated).
   */
  quality: MetricQuality;
  /**
   * Optional muted inline suffix rendered right of the value (U2/U7): a qualifier that must be
   * VISIBLE, not hover-only — "· window total" on a non-extrapolated cost, "· 1 request" on a
   * collapsed low-n latency chip, "· last active" on a retained engine tok/s.
   */
  valueSuffix?: string;
  /**
   * When false the chip renders NO sparkline (R2): the history series is a per-minute RATE, so
   * drawing it beside a displayed interval TOTAL pairs a rising spark with a falling number.
   */
  spark?: boolean;
  /** The telemetry seam supplying this value; exposed in the DOM and tooltip. */
  source: MetricSource;
  /** uPlot stroke as hex for the sparkline (mirrors `stroke`, kept explicit for clarity). */
  sparkStroke: string;
  /** Accessible formula, coverage, sample count, and approximation summary. */
  details: string;
}

function engineThroughputDetails(sample: EngineThroughputSample): string {
  const sources = `${sample.measured_sources}/${sample.total_sources} physically distinct metrics sources`;
  return `inverse mean per-request time-per-output-token (TPOT) from the latest backend Prometheus interval; ${sources}; ${sample.coverage} coverage; output tokens only; speculative accepted tokens are already reflected in TPOT; may include traffic sent directly to the engine; sampled at ${sample.sampled_at_ms}`;
}

function metricDetails(window: InstantMetricSample | null, key: MetricKey): string {
  if (!window) return 'No metrics interval has been published.';
  const coverage = window.interval_duration_ms === null ? 'publisher interval unavailable' : `${window.interval_duration_ms}ms publisher interval`;
  switch (key) {
    case 'accepted_per_sec': return `accepted starts / observed seconds; ${window.accepted_requests} starts; ${coverage}`;
    case 'terminal_per_sec': return `terminal flows / observed seconds; ${window.terminal_requests} terminals; ${coverage}`;
    case 'active_streams_now': return `open flows at the published cut; ${window.active_streams_now} active; global scope`;
    case 'failure_pct': return `failures / terminal flows × 100; ${window.failures}/${window.terminal_requests} terminals; cancellations excluded`;
    case 'cancellation_pct': return `cancellations / terminal flows × 100; ${window.cancellations}/${window.terminal_requests} terminals`;
    case 'p50_ms':
    case 'p95_ms':
    case 'p99_ms': return `${key.slice(0, 3)} nearest-rank logarithmic histogram; ${window.latency_samples} latency samples; max relative error ${(window.max_relative_error * 100).toFixed(1)}%; ${window[`${key.slice(0, 3)}_quality` as 'p50_quality' | 'p95_quality' | 'p99_quality']}`;
    case 'reported_tokens_per_sec': return `normalized prompt + completion / observed seconds; ${window.usage_samples} usage samples, ${window.usage_anomaly_count} anomalies; subsets counted once; ${coverage}`;
    case 'cost_per_min': return `persisted terminal cost / observed minutes; ${window.priced_samples}/${window.terminal_requests} priced terminals; ${window.cost_confidence}; ${coverage}`;
  }
}

/** Round-trip-safe compact rate (`4.2`, `142`, `1.2k`). */
const fmtBareRate = (n: number) => fmtRate(n, '');

/** Compare a metric field across two samples → delta direction (with a small epsilon). */
function deltaDir(cur: number, prev: number | undefined): DeltaDir {
  if (prev === undefined || !Number.isFinite(prev) || !Number.isFinite(cur)) return 'flat';
  const d = cur - prev;
  const eps = Math.max(1e-6, Math.abs(prev) * 1e-4);
  if (d > eps) return 'up';
  if (d < -eps) return 'down';
  return 'flat';
}

/**
 * The $/min chip's provenance tier from the backend's AGGREGATE `cost_confidence` (gap 07
 * review round 1, finding 5). Called ONLY when the cost is available (the `unavailable`
 * denominator branch wins otherwise):
 *  - `confident`   → `derived`   (a real cost computed from finalized-flow samples — every
 *                                 priced bucket's billed classes have known rates).
 *  - `estimated`   → `estimated` (some priced bucket bills cached at the default 0.0, or an
 *                                 unpriced bucket bears usage — a labelled best-effort estimate).
 *  - `unavailable` → `unavailable` (defensive; this normally coincides with `priced_samples ===
 *                                 0`, which the denominator branch already caught).
 */
function costQuality(confidence: CostConfidence): MetricQuality {
  switch (confidence) {
    case 'confident':
      return 'derived';
    case 'estimated':
      return 'estimated';
    case 'unavailable':
      return 'unavailable';
  }
}

/** Per-metric display config: label, formatter, stroke. Order = strip display order. */
interface MetricSpec {
  key: MetricKey;
  label: string;
  fmt: (n: number) => string;
  stroke: string;
  accent: ChipDescriptor['accent'];
  /**
   * Intrinsic data-quality tier when the value IS available (finding 4). `measured` for
   * directly-counted metrics, `derived` for sample-computed ones, `estimated` for the
   * price-modelled cost. Collapses to `unavailable` when the metric's denominator is `0`
   * (the denominator itself lives in `METRIC_AVAILABILITY` in metricHistory).
   */
  quality: Exclude<MetricQuality, 'partial' | 'unavailable'>;
}

const METRIC_SPECS: readonly MetricSpec[] = [
  { key: 'accepted_per_sec', label: 'inbound req/s', fmt: fmtBareRate, stroke: colors.accent, accent: 'accent', quality: 'measured' },
  { key: 'terminal_per_sec', label: 'done req/s', fmt: fmtBareRate, stroke: colors.accent, accent: 'text', quality: 'measured' },
  { key: 'active_streams_now', label: 'active now', fmt: fmtBareRate, stroke: colors.accent, accent: 'text', quality: 'measured' },
  { key: 'failure_pct', label: 'failures', fmt: fmtPercent, stroke: colors.statusDown, accent: 'text', quality: 'derived' },
  { key: 'cancellation_pct', label: 'cancellations', fmt: fmtPercent, stroke: colors.statusCooling, accent: 'text', quality: 'derived' },
  { key: 'p50_ms', label: 'gateway E2E p50', fmt: fmtLatency, stroke: colors.statusHealthy, accent: 'text', quality: 'derived' },
  { key: 'p95_ms', label: 'gateway E2E p95', fmt: fmtLatency, stroke: colors.statusCooling, accent: 'text', quality: 'derived' },
  { key: 'p99_ms', label: 'gateway E2E p99', fmt: fmtLatency, stroke: colors.statusDown, accent: 'text', quality: 'derived' },
  { key: 'reported_tokens_per_sec', label: 'reported throughput', fmt: fmtTokensPerSec, stroke: colors.statusHealthy, accent: 'healthy', quality: 'derived' },
  // $/min: the static tier here is a FALLBACK only — its real quality is derived per-sample from
  // the backend `cost_confidence` (gap 07 finding 5, see `costQuality`), so a confident aggregate
  // reads `derived` and an estimated one reads `estimated` (no longer always `estimated`).
  { key: 'cost_per_min', label: 'cost rate', fmt: fmtCostRate, stroke: colors.meta, accent: 'meta', quality: 'estimated' },
];

/** The unavailable / no-data marker (a value that cannot be measured renders this, never `0`). */
export const UNAVAILABLE = '—';

/** The metric keys, in strip order (handy for tests / iteration). */
export const CHIP_METRICS: readonly MetricKey[] = METRIC_SPECS.map((s) => s.key);

/**
 * Derive every chip descriptor for the current + previous window samples. `cur === null`
 * (no tick yet) renders every value as the unavailable marker with a flat delta.
 *
 * Don't-lie-with-zeros + per-metric availability (gap 01 findings 3/4): each metric has its
 * OWN measurability denominator — latency/err% need a finalized flow (`samples`), tok/s needs
 * a flow that REPORTED usage (`usage_samples`), $/min needs a usage-bearing flow on a PRICED
 * model (`priced_samples`). A window can have `samples > 0` yet `usage_samples === 0` (no
 * tokens reported) or `priced_samples === 0` (only unpriced models): those metrics render
 * `unavailable` (`—`), NEVER a fabricated `0`, with a flat delta and no threshold accent.
 * `req/s` (a genuine idle `0`) and `active_streams` (the live open count) are never gated.
 * Every chip also carries a `quality` provenance tag (measured/derived/estimated/unavailable).
 */
export function deriveChips(
  cur: InstantMetricSample | null,
  prev: InstantMetricSample | null,
  engineThroughput: EngineThroughputSample | null = null,
  previousEngineThroughput: EngineThroughputSample | null = null,
  opts: { retained?: boolean } = {},
): ChipDescriptor[] {
  // U7: with 1–4 latency samples the tail percentiles are noise — collapse the trio into one
  // "gateway E2E latency" chip. Zero samples keeps the trio so `—`/unavailable semantics hold.
  const collapseLatency = cur !== null
    && cur.latency_samples > 0
    && cur.latency_samples < LATENCY_PERCENTILE_MIN_SAMPLES;
  const specs = collapseLatency
    ? METRIC_SPECS.filter((spec) => spec.key !== 'p95_ms' && spec.key !== 'p99_ms')
    : METRIC_SPECS;
  return specs.map((spec): ChipDescriptor => {
    const useEngine = spec.key === 'reported_tokens_per_sec' && engineThroughput !== null;
    // Unmeasurable when there is no window, or this metric's own denominator is 0.
    const unavailable = useEngine ? false : metricUnavailable(cur, spec.key);
    const currentValue = useEngine
      ? engineThroughput.generated_tokens_per_sec
      : cur?.[spec.key] ?? null;
    const previousValue = useEngine
      ? previousEngineThroughput?.generated_tokens_per_sec
      : prev?.[spec.key] ?? undefined;
    // U2: too few priced samples (or an idle/retained window) → the $/min extrapolation is
    // suppressed in favor of the interval's ACTUAL persisted cost (rate × observed minutes,
    // which round-trips the backend's `persisted terminal cost / observed minutes` exactly).
    const costAsTotal = spec.key === 'cost_per_min'
      && !unavailable
      && cur !== null
      && cur.cost_per_min !== null
      && cur.interval_duration_ms !== null
      && (cur.priced_samples < COST_RATE_MIN_PRICED_SAMPLES || opts.retained === true);
    const value = unavailable || currentValue === null
      ? UNAVAILABLE
      : costAsTotal
        ? fmtCost((currentValue * cur.interval_duration_ms!) / 60_000)
        : spec.fmt(currentValue);
    const valueSuffix = costAsTotal
      ? '· window total'
      : collapseLatency && spec.key === 'p50_ms'
        ? `· ${cur!.latency_samples} request${cur!.latency_samples === 1 ? '' : 's'}`
        : useEngine && opts.retained === true
          ? '· last active'
          : undefined;
    // The err% chip turns red ABOVE the threshold — but only when it is actually MEASURED
    // (an unavailable err% carries no threshold accent); others keep their static accent.
    const accent: ChipDescriptor['accent'] =
      !unavailable && cur && spec.key === 'failure_pct' && cur.failure_pct !== null && cur.failure_pct > ERROR_PCT_THRESHOLD ? 'down' : spec.accent;
    // No trend direction for an unavailable value, nor across the genuine→unavailable boundary
    // (the previous sample being unavailable for THIS metric makes the delta meaningless).
    const prevUnavailable = useEngine
      ? previousEngineThroughput === null
      : metricUnavailable(prev, spec.key);
    // Total mode shows an interval TOTAL while the underlying series is a RATE — a trend arrow
    // comparing rates against a displayed total misleads (review MED), so it stays flat.
    const delta = !unavailable && currentValue !== null && !prevUnavailable && !costAsTotal
      ? deltaDir(currentValue, previousValue ?? undefined)
      : 'flat';
    // Provenance (finding 4): `unavailable` when `—`, else the metric's intrinsic tier — EXCEPT
    // the cost chip, whose tier is the AGGREGATE `cost_confidence` the backend reports (gap 07
    // review round 1, finding 5), not a hard-coded `estimated`. A `confident` aggregate (every
    // priced bucket's billed classes have known rates) is a real DERIVED figure; an `estimated`
    // one (some priced bucket bills cached at the default 0.0, or an unpriced bucket bears usage)
    // is a labelled estimate. `unavailable` is already handled by the denominator branch above
    // (it coincides with `priced_samples === 0`), so operators can finally tell a confident
    // aggregate cost from an estimated one instead of every `$/min` always reading `estimated`.
    const percentileQuality = cur && (spec.key === 'p50_ms' || spec.key === 'p95_ms' || spec.key === 'p99_ms')
      ? cur[`${spec.key.slice(0, 3)}_quality` as 'p50_quality' | 'p95_quality' | 'p99_quality']
      : null;
    const quality: MetricQuality = unavailable
      ? 'unavailable'
      : useEngine && engineThroughput.coverage === 'partial'
        ? 'partial'
      : percentileQuality === 'partial'
        ? 'partial'
      : spec.key === 'cost_per_min' && cur
        ? costQuality(cur.cost_confidence)
        : spec.quality;
    return {
      key: spec.key,
      label: useEngine
        ? 'engine gen tok/s'
        : collapseLatency && spec.key === 'p50_ms'
          ? 'gateway E2E latency'
          : costAsTotal
            ? 'cost'
            : spec.label,
      value,
      stroke: spec.stroke,
      accent,
      delta,
      quality,
      valueSuffix,
      spark: !costAsTotal,
      source: useEngine ? 'engine' : spec.key === 'reported_tokens_per_sec' ? 'reported' : 'gateway',
      sparkStroke: spec.stroke,
      details: useEngine ? engineThroughputDetails(engineThroughput) : metricDetails(cur, spec.key),
    };
  });
}

/** Delta arrow glyph for a direction (UI affordance). */
export function deltaGlyph(dir: DeltaDir): string {
  return dir === 'up' ? '▲' : dir === 'down' ? '▼' : '·';
}
