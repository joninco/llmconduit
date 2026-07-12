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
import { updateHashScope, useHashScope } from '../../router/useHashRoute';

export function StatsStrip() {
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
    queryFn: () => client.history({ limit: 10_000 }),
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
  const chips = deriveChips(cur, prev, engineThroughput, previousEngineThroughput);

  return (
    <Panel
      className="m-2 mb-0 flex snap-x snap-mandatory items-center gap-1 overflow-x-auto px-2 py-1 sm:m-4 sm:mb-0"
      data-testid="stats-strip"
      data-metrics-state={showingRetained ? 'stale' : idle ? 'empty' : currentInstant ? 'instant' : 'empty'}
    >
      <ActivityState
        activeStreams={currentInstant?.active_streams_now ?? 0}
        lastActivityAtMs={retainedActivity?.at_ms ?? null}
        seeking={seeking}
        seekAtMs={seekAtMs}
      />
      {chips.map((chip) => (
        <ChipCell key={chip.key} chip={chip} series={seriesFor(visibleHistory, chip.key, chip.source)} stale={showingRetained} />
      ))}
      <div className="ml-auto flex items-center gap-2 pr-1">
        <span
          className="rounded border border-border px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-wide text-muted"
          title="Gateway-wide metrics; URL flow filters do not change these values."
          aria-label="Global gateway metrics, unaffected by flow filters"
          data-testid="stats-global-badge"
        >
          Global
        </span>
        <WindowSelector value={window} onChange={(next) => updateHashScope({ window: next })} />
        <ConnectionDot state={connection} hasData={hasDashboardData} />
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
  stale,
}: {
  chip: ChipDescriptor;
  series: { times: number[]; values: number[] };
  stale: boolean;
}) {
  const qualityText = QUALITY_LABEL[chip.quality];
  return (
    <div
      className="flex shrink-0 snap-start flex-col gap-1 border-l border-line/50 px-3 py-1 first:border-l-0"
      data-testid={`chip-${chip.key}`}
      data-stale={stale ? 'true' : undefined}
      // Provenance exposed to the DOM (finding 4): tests + tooling can assert the tag, and
      // the `title` gives operators a hover hint. EVERY chip carries one of
      // measured/derived/estimated/unavailable.
      data-quality={chip.quality}
      data-source={chip.source}
      title={`${stale ? 'Stale retained sample. ' : ''}${chip.label}: ${qualityText}. ${chip.details}`}
      aria-description={chip.details}
    >
      <span className="text-[10px] uppercase tracking-[0.14em] text-text-muted">{chip.label}</span>
      <div className="flex items-baseline gap-1">
        <span
          className={cn('font-mono text-xl font-semibold tabular-nums tracking-tight', ACCENT_TEXT[chip.accent])}
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
      </div>
      <Sparkline data={series.values} timestamps={series.times} stroke={chip.sparkStroke} label={`${chip.label} trend`} />
    </div>
  );
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

/** Fixed-width stale clock (`MM:SS`, then `HH:MM:SS`, with a day prefix when needed). */
function formatStaleAge(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1_000));
  const seconds = total % 60;
  const minutes = Math.floor(total / 60) % 60;
  const hours = Math.floor(total / 3_600) % 24;
  const days = Math.floor(total / 86_400);
  const pad = (value: number) => String(value).padStart(2, '0');
  const clock = total < 3_600
    ? `${pad(Math.floor(total / 60))}:${pad(seconds)}`
    : `${pad(hours)}:${pad(minutes)}:${pad(seconds)}`;
  return days > 0 ? `${days}d ${clock}` : clock;
}

/** Live/idle mode indicator. The clock owns its timer so chip sparklines do not rerender each second. */
function ActivityState({
  activeStreams,
  lastActivityAtMs,
  seeking,
  seekAtMs,
}: {
  activeStreams: number;
  lastActivityAtMs: number | null;
  seeking: boolean;
  seekAtMs: number | null;
}) {
  const [nowMs, setNowMs] = useState(() => Date.now());
  useEffect(() => {
    if (activeStreams > 0 || lastActivityAtMs === null || seeking) return;
    setNowMs(Date.now());
    const id = globalThis.setInterval(() => setNowMs(Date.now()), 1_000);
    return () => globalThis.clearInterval(id);
  }, [activeStreams, lastActivityAtMs, seeking]);

  if (activeStreams > 0) {
    return null;
  }
  if (lastActivityAtMs === null) {
    return (
      <span
        className="sticky left-0 z-10 shrink-0 rounded border border-line bg-panel px-2 py-1 text-[11px] font-semibold uppercase tracking-wide text-text-muted"
        data-testid="stats-activity-state"
        data-state="empty"
      >
        idle · no request yet
      </span>
    );
  }

  const clockAtMs = seeking ? (seekAtMs ?? lastActivityAtMs) : nowMs;
  const age = formatStaleAge(clockAtMs - lastActivityAtMs);
  return (
    <span
      className="sticky left-0 z-10 flex shrink-0 items-baseline gap-2 rounded border-2 border-status-cooling/70 bg-panel px-2.5 py-1 text-status-cooling shadow-lg"
      role="status"
      aria-label={`Statistics stale for ${age} since the last request`}
      data-testid="stats-activity-state"
      data-state={seeking ? 'historical' : 'stale'}
      data-stale="true"
    >
      <span className="text-[10px] font-bold uppercase tracking-[0.14em]">{seeking ? 'historical' : 'stats stale'}</span>
      <span className="font-mono text-lg font-bold tabular-nums leading-none" data-testid="stats-stale-age">{age}</span>
      <span className="text-[10px] font-semibold uppercase tracking-wide">since last request</span>
    </span>
  );
}

function ConnectionDot({ state, hasData }: { state: string; hasData: boolean }) {
  const color =
    state === 'live' ? 'bg-status-healthy'
    : state === 'connecting' || state === 'seeking' ? 'bg-status-cooling'
    : state === 'error' ? 'bg-status-down'
    : 'bg-text-muted';
  const label = state === 'connecting' && hasData ? 'reconnecting · stale' : state;
  return (
    <span
      className="flex items-center gap-2 text-[10px] uppercase tracking-[0.14em] text-text-muted"
      data-testid="connection-state"
      data-stale={state === 'connecting' && hasData ? 'true' : undefined}
    >
      <span className={`h-2 w-2 rounded-full ${color}`} aria-hidden />
      {label}
    </span>
  );
}
