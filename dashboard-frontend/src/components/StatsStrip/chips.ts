/**
 * Chip descriptors — the pure mapping from a `MetricWindow` sample (+ its predecessor) to each
 * strip chip's display value, sparkline stroke, threshold accent, and delta direction.
 *
 * DOM-free so it is unit-testable and the component stays a thin renderer. Formatting reuses the
 * flow-table formatters where they fit (tokens), and adds small local ones for rates/latency/%.
 */
import type { MetricWindow } from '../../api/types';
import { colors } from '../../design/tokens';
import { fmtTokens } from '../FlowTable/format';
import type { MetricKey } from './metricHistory';

/** Error-% threshold above which the err chip turns red (spec: "red above threshold"). */
export const ERROR_PCT_THRESHOLD = 5;

export type DeltaDir = 'up' | 'down' | 'flat';

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
  /** uPlot stroke as hex for the sparkline (mirrors `stroke`, kept explicit for clarity). */
  sparkStroke: string;
}

/** Round-trip-safe compact rate (`4.2`, `142`, `1.2k`). */
function fmtRate(n: number): string {
  if (!Number.isFinite(n)) return '—';
  if (n >= 1000) return `${(n / 1000).toFixed(1)}k`;
  if (n >= 100) return String(Math.round(n));
  return n.toFixed(1);
}

/** Latency ms → integer ms (`920`). */
function fmtMs(n: number): string {
  return Number.isFinite(n) ? String(Math.round(n)) : '—';
}

/** Percent → 1dp (`1.1`). */
function fmtPct(n: number): string {
  return Number.isFinite(n) ? n.toFixed(1) : '—';
}

/** Dollars/min → 2dp (`0.21`). */
function fmtMoney(n: number): string {
  return Number.isFinite(n) ? n.toFixed(2) : '—';
}

/** Compare a metric field across two samples → delta direction (with a small epsilon). */
function deltaDir(cur: number, prev: number | undefined): DeltaDir {
  if (prev === undefined || !Number.isFinite(prev) || !Number.isFinite(cur)) return 'flat';
  const d = cur - prev;
  const eps = Math.max(1e-6, Math.abs(prev) * 1e-4);
  if (d > eps) return 'up';
  if (d < -eps) return 'down';
  return 'flat';
}

/** Per-metric display config: label, formatter, stroke. Order = strip display order. */
interface MetricSpec {
  key: MetricKey;
  label: string;
  fmt: (n: number) => string;
  stroke: string;
  accent: ChipDescriptor['accent'];
}

const METRIC_SPECS: readonly MetricSpec[] = [
  { key: 'reqs_per_sec', label: 'req/s', fmt: fmtRate, stroke: colors.accent, accent: 'accent' },
  { key: 'active_streams', label: 'active', fmt: fmtRate, stroke: colors.accent, accent: 'text' },
  { key: 'error_pct', label: 'err %', fmt: fmtPct, stroke: colors.statusDown, accent: 'text' },
  { key: 'p50', label: 'p50 ms', fmt: fmtMs, stroke: colors.statusHealthy, accent: 'text' },
  { key: 'p95', label: 'p95 ms', fmt: fmtMs, stroke: colors.statusCooling, accent: 'text' },
  { key: 'p99', label: 'p99 ms', fmt: fmtMs, stroke: colors.statusDown, accent: 'text' },
  { key: 'tokens_per_sec', label: 'tok/s', fmt: fmtTokens, stroke: colors.statusHealthy, accent: 'healthy' },
  { key: 'cost_per_min', label: '$/min', fmt: fmtMoney, stroke: colors.meta, accent: 'meta' },
];

/** The metric keys, in strip order (handy for tests / iteration). */
export const CHIP_METRICS: readonly MetricKey[] = METRIC_SPECS.map((s) => s.key);

/**
 * Derive every chip descriptor for the current + previous window samples. `cur === null`
 * (no tick yet) renders every value as the unavailable marker with a flat delta.
 */
export function deriveChips(cur: MetricWindow | null, prev: MetricWindow | null): ChipDescriptor[] {
  return METRIC_SPECS.map((spec): ChipDescriptor => {
    const value = cur ? spec.fmt(cur[spec.key]) : '—';
    // The err% chip turns red ABOVE the threshold; others keep their static accent.
    const accent: ChipDescriptor['accent'] =
      spec.key === 'error_pct' && cur && cur.error_pct > ERROR_PCT_THRESHOLD ? 'down' : spec.accent;
    const delta = cur ? deltaDir(cur[spec.key], prev?.[spec.key]) : 'flat';
    return {
      key: spec.key,
      label: spec.label,
      value,
      stroke: spec.stroke,
      accent,
      delta,
      sparkStroke: spec.stroke,
    };
  });
}

/** Delta arrow glyph for a direction (UI affordance). */
export function deltaGlyph(dir: DeltaDir): string {
  return dir === 'up' ? '▲' : dir === 'down' ? '▼' : '·';
}
