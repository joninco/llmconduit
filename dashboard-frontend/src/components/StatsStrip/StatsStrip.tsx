/**
 * StatsStrip (D11) — the always-on top bar. Most chips show the latest one-second gateway sample;
 * tok/s prefers the latest physically de-duplicated backend Prometheus inverse-TPOT interval
 * and explicitly falls back to finalized-response usage. Each also has a uPlot sparkline + delta
 * arrow; the 1m/5m/1h selector changes only that history horizon, never the instantaneous values.
 *
 * Data flow:
 *  - The live store `metrics` (a `MetricsResponse`) is the authoritative latest sample — the
 *    socket writes it from `metric_tick` frames (and the initial snapshot). We fold EVERY distinct
 *    LIVE sample (deduped by `metrics_seq`) into a per-window ring via `useMetricStream`, which
 *    subscribes to the store directly so no sample is lost to React's render batching.
 *  - The `/metrics` TanStack query seeds the strip before the first WS tick AND is the production
 *    REST source; `metric` frames invalidate `queryKeys.metrics` (connection.ts) so it refetches.
 *    It primes the same ring (deduped by `metrics_seq`), so seed + live share one history.
 *  - Sparklines are uPlot via `Sparkline` (StrictMode-safe dispose; reduced-motion static).
 *  - Idle retention: the backend carries the most recent request-bearing interval beside each
 *    truthful empty interval. While `active_streams_now === 0`, chips retain that interval (with
 *    the active-count field overlaid to the current `0`) and a prominent clock makes its age
 *    explicit. Active requests always render the current reset-on-publish interval directly.
 *  - Seek (D11 R5): the sparkline/delta history is LIVE-only (`useMetricStream` skips the frozen
 *    seek cut), so the trends never absorb historical data. The chip CURRENT VALUE, however, must
 *    reflect the seeked moment like the rest of the dashboard — so while `seeking` we read the
 *    chips' `cur` from the FROZEN store `metrics` (the snapshot cut `applySeekCut` installed) for
 *    the selected window, leaving the sparkline + delta on the (unpolluted) live history ring.
 *
 * Always rendered at the top of `App.tsx` (above the Scrubber).
 */
import { useCallback, useEffect, useRef, useState } from 'react';
import { useQuery } from '@tanstack/react-query';
import type { MetricsResponse } from '../../api/types';
import { useDashboard } from '../../store/hooks';
import { useMetricStream } from '../../store/useMetricStream';
import { getConnection, queryKeys } from '../../api/connection';
import { Panel } from '../ui/Panel';
import { cn } from '../../lib/cn';
import { Sparkline } from '../../viz/Sparkline';
import {
  appendTick,
  emptyHistory,
  horizon,
  latest,
  latestActivity,
  latestEngineThroughput,
  mergeRetained,
  previous,
  seriesFor,
  WINDOW_KEYS,
  WINDOW_LABELS,
  type MetricHistory,
  type WindowKey,
} from './metricHistory';
import { deriveChips, deltaGlyph, type ChipDescriptor } from './chips';
import { fmtLatency, fmtTokensPerSec } from '../FlowTable/format';
import { updateHashScope, useHashScope } from '../../router/useHashRoute';
import { deriveDashboardStatus } from '../../lib/dashboardStatus';
import { OperationalStatus } from '../ui/OperationalStatus';

const PRIMARY_METRICS = new Set([
  'active_streams_now',
  'failure_pct',
  'p50_ms',
  'p95_ms',
  'reported_tokens_per_sec',
]);

/**
 * CompactStatsStrip (U4) — the one-line variant for non-Overview tabs: the full strip costs
 * ~30% of every viewport, but Flows/Topology/Sankey/Theater only need the operational pulse.
 * Shows Connection/Metrics/Traffic (the same `OperationalStatus` model) + E2E p50 + engine
 * tok/s, and an expand control that pins the full strip.
 */
export function CompactStatsStrip({ onExpand }: { onExpand: () => void }) {
  const connection = useDashboard((s) => s.connection);
  const seeking = connection === 'seeking';
  const seekAtMs = useDashboard((s) => s.seekAtMs);
  const hasDashboardData = useDashboard((s) =>
    s.metrics !== null || s.flows.size > 0 || s.topologyNodes.length > 0,
  );
  const storeMetrics = useDashboard((s) => s.metrics);
  const { client } = getConnection();
  const query = useQuery({ queryKey: queryKeys.metrics, queryFn: () => client.metrics() });
  // SEEK gating (review HIGH): while seeking, the store metrics ARE the frozen snapshot cut —
  // never fall back to the live `/metrics` REST read, or a historical view shows CURRENT
  // numbers labeled as the seeked moment. Null frozen metrics render honest dashes.
  const currentMetrics = seeking ? storeMetrics : (storeMetrics ?? query.data ?? null);
  const instant = currentMetrics?.instant ?? null;
  const retainedActivity = currentMetrics?.last_activity ?? null;
  const idle = instant !== null && instant.active_streams_now === 0;
  const showingRetained = idle && retainedActivity !== null;
  const cur = showingRetained && instant && retainedActivity
    ? { ...retainedActivity.instant, active_streams_now: instant.active_streams_now }
    : instant;
  const [nowMs, setNowMs] = useState(() => Date.now());
  useEffect(() => {
    const id = globalThis.setInterval(() => setNowMs(Date.now()), 1_000);
    return () => globalThis.clearInterval(id);
  }, []);
  const operationalStatus = deriveDashboardStatus({
    connection,
    hasDashboardData,
    generatedAtMs: currentMetrics?.generated_at_ms ?? null,
    activeStreams: instant?.active_streams_now ?? 0,
    lastActivityAtMs: retainedActivity?.at_ms ?? null,
    // While seeking the strip reads as-of the frozen instant, never the wall clock.
    nowMs: seeking ? (seekAtMs ?? nowMs) : nowMs,
  });
  const p50 = cur !== null && cur.latency_samples > 0 && cur.p50_ms !== null ? fmtLatency(cur.p50_ms) : '—';
  const engine = currentMetrics?.engine_throughput ?? null;
  const tokPerSec = engine !== null
    ? fmtTokensPerSec(engine.generated_tokens_per_sec)
    : cur !== null && cur.usage_samples > 0 && cur.reported_tokens_per_sec !== null
      ? fmtTokensPerSec(cur.reported_tokens_per_sec)
      : '—';
  const interval = showingRetained ? 'last active interval' : 'latest interval';
  return (
    <Panel
      className="m-2 mb-0 flex min-w-0 flex-wrap items-center gap-x-4 gap-y-1 px-3 py-1.5 sm:m-4 sm:mb-0"
      data-testid="stats-strip-compact"
      data-metrics-state={showingRetained ? 'retained' : cur ? 'instant' : 'empty'}
    >
      <OperationalStatus model={operationalStatus} />
      <span className="text-[11px] text-text-muted" title={`Gateway E2E p50 · ${interval}`}>
        E2E p50{' '}
        <span className="font-mono font-semibold tabular-nums text-text" data-testid="compact-p50">{p50}</span>
        {/* The retained qualifier belongs to the GATEWAY value (it comes from the retained
            request-bearing interval); the engine figure below is its own generation-associated
            interval and must not inherit this label (review MED). */}
        {showingRetained && <span className="ml-1 text-[10px]">· last active</span>}
      </span>
      <span className="text-[11px] text-text-muted" title={`${engine !== null ? 'Engine generation throughput · latest generation-associated interval' : `Reported throughput · ${interval}`}`}>
        {engine !== null ? 'gen' : 'reported'}{' '}
        <span className="font-mono font-semibold tabular-nums text-status-healthy" data-testid="compact-toks">{tokPerSec}</span>
      </span>
      <button
        type="button"
        className="ml-auto inline-flex min-h-7 items-center rounded px-1.5 text-[11px] font-medium text-text-muted hover:text-text focus-visible:ring-2 focus-visible:ring-accent"
        aria-expanded={false}
        onClick={onExpand}
        data-testid="stats-strip-expand"
      >
        Expand metrics
      </button>
    </Panel>
  );
}

export function StatsStrip({ onCompact }: { onCompact?: () => void } = {}) {
  const scope = useHashScope();
  const window = scope.window as WindowKey;
  const connection = useDashboard((s) => s.connection);
  const hasDashboardData = useDashboard((s) =>
    s.metrics !== null || s.flows.size > 0 || s.topologyNodes.length > 0,
  );
  const seeking = connection === 'seeking';
  const storeMetrics = useDashboard((s) => s.metrics);
  // The FROZEN store metrics while seeking — the snapshot cut `applySeekCut` installed. Read as the
  // chip CURRENT VALUE (per window) so the strip reads as-of the seeked moment, while the sparkline
  // history stays the LIVE ring (the seek cut is never folded into it — D11 R5).
  const seekMetrics = useDashboard((s) => (s.connection === 'seeking' ? s.metrics : null));
  const seekAtMs = useDashboard((s) => s.seekAtMs);
  const { client } = getConnection();

  // The `/metrics` REST read: seeds the strip pre-WS and is the production data source. `metric`
  // frames invalidate this key so it refetches the authoritative shape. The mock answers it; the
  // store metrics (live) supersedes it as soon as a tick lands.
  const query = useQuery({
    queryKey: queryKeys.metrics,
    queryFn: () => client.metrics(),
  });
  const historyQuery = useQuery({
    queryKey: ['history'],
    queryFn: () => client.history({ limit: 2_000 }),
    staleTime: 5_000,
    refetchInterval: seeking ? false : 5_000,
  });

  // History ring (per window), held in a ref so streaming ticks don't recreate it. `useMetricStream`
  // folds every distinct store sample (deduped by seq) and bumps `version` so the chips re-render.
  const historyRef = useRef<MetricHistory>(emptyHistory());
  const fold = useCallback((sample: MetricsResponse) => {
    historyRef.current = appendTick(historyRef.current, sample);
  }, []);
  const { version } = useMetricStream(fold, query.data);

  // `version` is read so this body re-runs after each ring fold (the ref mutation is otherwise
  // invisible to React); `window` switches the source window — so the memo below recomputes.
  void version;
  const history = mergeRetained(historyRef.current, historyQuery.data?.points ?? []);
  const visibleHistory = horizon(history, window, seeking ? seekAtMs : null);
  // While seeking, the chip CURRENT value is the FROZEN snapshot window (as-of the seeked moment),
  // NOT the live ring's latest — but the sparkline (`seriesFor` below) stays the live history. Its
  // delta compares with the preceding retained cut so a historical point still has local context;
  // live mode compares the current interval with the preceding live one.
  const currentMetrics = seeking ? seekMetrics : (storeMetrics ?? query.data ?? null);
  const currentInstant = currentMetrics?.instant ?? latest(history);
  // The wire field survives reloads and arbitrarily long idle periods. The history fallback keeps
  // the behavior compatible with a rolling upgrade from a backend that predates `last_activity`.
  const historyActivity = latestActivity(history);
  const retainedActivity = currentMetrics?.last_activity ?? (historyActivity ? {
    at_ms: historyActivity.t,
    instant: historyActivity.instant,
  } : null);
  const idle = currentInstant !== null && currentInstant.active_streams_now === 0;
  const showingRetained = idle && retainedActivity !== null;
  // `active now` remains truthful even if the retained request-bearing interval happened to be an
  // in-flight-only cut. Every other chip is deliberately frozen to that last instantaneous sample.
  const cur = showingRetained && currentInstant && retainedActivity
    ? { ...retainedActivity.instant, active_streams_now: currentInstant.active_streams_now }
    : currentInstant;
  const prev = showingRetained && retainedActivity
    ? latestActivity(history, retainedActivity.at_ms)?.instant ?? null
    : previous(history, seeking ? seekAtMs : null);
  // The backend associates a physically de-duplicated Prometheus inverse-TPOT interval with the
  // latest request generation. Its absence is intentional (unsupported/warming/stale), in which
  // case the chip falls back to finalized-response usage and labels that source explicitly.
  const engineThroughput = currentMetrics?.engine_throughput
    ?? (currentMetrics === null ? latestEngineThroughput(history) : null);
  const previousEngineThroughput = engineThroughput
    ? latestEngineThroughput(history, engineThroughput.sampled_at_ms)
    : null;
  const chips = deriveChips(cur, prev, engineThroughput, previousEngineThroughput, { retained: showingRetained });
  const primaryChips = chips.filter((chip) => PRIMARY_METRICS.has(chip.key));
  const secondaryChips = chips.filter((chip) => !PRIMARY_METRICS.has(chip.key));
  const [moreMetricsOpen, setMoreMetricsOpen] = useState(false);
  const [nowMs, setNowMs] = useState(() => Date.now());
  useEffect(() => {
    const id = globalThis.setInterval(() => setNowMs(Date.now()), 1_000);
    return () => globalThis.clearInterval(id);
  }, []);
  const operationalStatus = deriveDashboardStatus({
    connection,
    hasDashboardData,
    generatedAtMs: currentMetrics?.generated_at_ms ?? null,
    activeStreams: currentInstant?.active_streams_now ?? 0,
    lastActivityAtMs: retainedActivity?.at_ms ?? null,
    nowMs: seeking ? (seekAtMs ?? nowMs) : nowMs,
  });

  return (
    <Panel
      className="m-2 mb-0 min-w-0 p-2 sm:m-4 sm:mb-0 sm:p-3"
      data-testid="stats-strip"
      data-metrics-state={showingRetained ? 'retained' : idle ? 'empty' : currentInstant ? 'instant' : 'empty'}
    >
      <div className="mb-2 flex min-w-0 flex-wrap items-center gap-x-4 gap-y-2">
        <div className="mr-auto min-w-0">
          <div className="flex items-center gap-2">
            <h2 className="text-xs font-semibold text-text">Gateway metrics</h2>
            <span
              className="rounded border border-border px-1.5 py-0.5 text-[10px] font-medium text-muted"
              title="Gateway-wide metrics; URL flow filters do not change these values."
              aria-label="Global gateway metrics, unaffected by flow filters"
              data-testid="stats-global-badge"
            >
              Global
            </span>
          </div>
          <OperationalStatus model={operationalStatus} />
        </div>
        <div className="flex shrink-0 items-center gap-2">
          <span className="hidden text-[11px] text-text-muted sm:inline">Trend window</span>
          <WindowSelector value={window} onChange={(next) => updateHashScope({ window: next })} />
          {onCompact && (
            <button
              type="button"
              className="inline-flex min-h-7 items-center rounded px-1.5 text-[11px] font-medium text-text-muted hover:text-text focus-visible:ring-2 focus-visible:ring-accent"
              aria-expanded
              onClick={onCompact}
              data-testid="stats-strip-compact-toggle"
            >
              Compact
            </button>
          )}
        </div>
      </div>

      <div className="grid min-w-0 grid-cols-2 gap-px rounded border border-line/60 bg-line/60 sm:grid-cols-3 lg:grid-cols-5" data-testid="primary-metrics">
        {primaryChips.map((chip) => (
          <ChipCell
            key={chip.key}
            chip={chip}
            series={seriesFor(visibleHistory, chip.key, chip.source)}
            retained={showingRetained}
            scope={metricScope(chip, cur, showingRetained)}
            primary
          />
        ))}
      </div>

      <div className="mt-2" data-testid="more-metrics">
        <button
          type="button"
          className="inline-flex min-h-8 items-center rounded px-1 text-[11px] font-medium text-text-muted hover:text-text focus-visible:ring-2 focus-visible:ring-accent"
          aria-expanded={moreMetricsOpen}
          aria-controls="secondary-metrics"
          onClick={() => setMoreMetricsOpen((open) => !open)}
        >
          {moreMetricsOpen ? 'Fewer metrics' : 'More metrics'} <span className="ml-1 text-[10px]">({secondaryChips.length})</span>
        </button>
        <div id="secondary-metrics" hidden={!moreMetricsOpen} className="mt-1 grid min-w-0 grid-cols-2 gap-px rounded border border-line/60 bg-line/60 sm:grid-cols-3 lg:grid-cols-5">
          {secondaryChips.map((chip) => (
            <ChipCell
              key={chip.key}
              chip={chip}
              series={seriesFor(visibleHistory, chip.key, chip.source)}
              retained={showingRetained}
              scope={metricScope(chip, cur, showingRetained)}
            />
          ))}
        </div>
      </div>
    </Panel>
  );
}

const ACCENT_TEXT: Record<ChipDescriptor['accent'], string> = {
  accent: 'text-accent',
  healthy: 'text-status-healthy',
  meta: 'text-meta',
  down: 'text-status-down',
  text: 'text-text',
};

const DELTA_CLASS = { up: 'text-status-healthy', down: 'text-status-down', flat: 'text-text-muted' } as const;

/**
 * Human-readable provenance label per quality tier (finding 4) — surfaced in the chip's
 * `title` + screen-reader text so an operator can tell a directly-counted value from a
 * derived/estimated one from an honest gap, exactly as the IMPLEMENTATION_PLAN requires.
 */
const QUALITY_LABEL: Record<ChipDescriptor['quality'], string> = {
  measured: 'measured',
  derived: 'derived from observed samples or counter deltas',
  estimated: 'estimated (priced via the configured price table)',
  partial: 'partial — incomplete source coverage or sparse interval sample',
  unavailable: 'unavailable — not measurable in this window',
};

/** One chip: label, tabular-nums value + delta arrow, a provenance tag, and the sparkline. */
function ChipCell({
  chip,
  series,
  retained,
  scope,
  primary = false,
}: {
  chip: ChipDescriptor;
  series: { times: number[]; values: number[] };
  retained: boolean;
  scope: string;
  primary?: boolean;
}) {
  const qualityText = QUALITY_LABEL[chip.quality];
  return (
    <div
      className="flex min-w-0 flex-col gap-1 overflow-visible bg-panel px-2.5 py-2"
      data-testid={`chip-${chip.key}`}
      data-retained={retained ? 'true' : undefined}
      // Provenance exposed to the DOM (finding 4): tests + tooling can assert the tag, and
      // the `title` gives operators a hover hint. EVERY chip carries one of
      // measured/derived/estimated/unavailable.
      data-quality={chip.quality}
      data-source={chip.source}
      title={`${retained ? 'Retained last-activity sample. ' : ''}${chip.label}: ${qualityText}. ${chip.details}`}
      aria-description={chip.details}
    >
      <span className="text-[11px] font-medium text-text-muted">{chip.label}</span>
      <div className="flex items-baseline gap-1">
        <span
          className={cn(
            'font-mono font-semibold tabular-nums tracking-tight',
            primary ? 'text-xl sm:text-2xl' : 'text-lg',
            ACCENT_TEXT[chip.accent],
          )}
          data-testid="chip-value"
          // Make the provenance available to assistive tech without cluttering the visual
          // (the value reads e.g. "142 (derived from observed samples or counter deltas)").
          aria-label={`${chip.label} ${chip.value}, ${qualityText}. ${chip.details}`}
        >
          {chip.value}
        </span>
        <span className={cn('text-[10px]', DELTA_CLASS[chip.delta])} aria-hidden data-testid="chip-delta">
          {deltaGlyph(chip.delta)}
        </span>
        {chip.valueSuffix && (
          <span className="truncate text-[10px] text-text-muted" data-testid="chip-value-suffix">
            {chip.valueSuffix}
          </span>
        )}
      </div>
      <div className="flex min-w-0 items-end gap-2">
        <span className="min-w-0 flex-1 text-[10px] leading-tight text-text-muted" data-testid="metric-scope">{scope}</span>
        <span className="w-12 min-w-0 shrink sm:w-16">
          <Sparkline width={48} data={series.values} timestamps={series.times} stroke={chip.sparkStroke} label={`${chip.label} trend`} />
        </span>
      </div>
    </div>
  );
}

function metricScope(chip: ChipDescriptor, sample: MetricsResponse['instant'] | null, retained: boolean): string {
  if (chip.key === 'active_streams_now') return 'Global · current';
  if (!sample) return 'Global · no interval';
  const interval = retained ? 'last active interval' : 'latest interval';
  if (chip.key === 'failure_pct' || chip.key === 'cancellation_pct') {
    return `${sample.terminal_requests} terminal · ${interval}`;
  }
  if (chip.key === 'p50_ms' || chip.key === 'p95_ms' || chip.key === 'p99_ms') {
    return sample.latency_samples < 2 && chip.key !== 'p50_ms'
      ? `Gateway E2E · ${interval} · ${sample.latency_samples} sample · insufficient`
      : `Gateway E2E · ${interval} · ${sample.latency_samples} terminal ${sample.latency_samples === 1 ? 'request' : 'requests'} · ${chip.quality}`;
  }
  if (chip.key === 'reported_tokens_per_sec') return `${sample.usage_samples} usage samples · ${interval}`;
  if (chip.key === 'cost_per_min') return `${sample.priced_samples} priced · ${interval}`;
  return `Global · ${interval}`;
}

/** 1m/5m/1h history selector — changes sparkline depth, never the instantaneous chip value. */
function WindowSelector({ value, onChange }: { value: WindowKey; onChange: (w: WindowKey) => void }) {
  return (
    <div className="flex overflow-hidden rounded-md border border-line" role="group" aria-label="history and analytics horizon" data-testid="window-selector">
      {WINDOW_KEYS.map((w) => (
        <button
          key={w}
          type="button"
          onClick={() => onChange(w)}
          aria-pressed={value === w}
          className={cn(
            'px-2 py-1 text-xs tabular-nums transition-colors',
            value === w ? 'bg-accent/20 text-text' : 'bg-transparent text-text-muted hover:text-text',
          )}
        >
          {WINDOW_LABELS[w]}
        </button>
      ))}
    </div>
  );
}
