/**
 * StatsStrip (D11) — the always-on top bar. Chips for req/s, active streams, error% (red above
 * threshold), p50/p95/p99, tokens/s, $/min — each a `tabular-nums` value + a uPlot sparkline +
 * a delta arrow — plus a 1m/5m/1h window selector that switches `metrics.windows.{m1,m5,h1}` and
 * the sparkline source.
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
 *  - Seek (D11 R5): the sparkline/delta history is LIVE-only (`useMetricStream` skips the frozen
 *    seek cut), so the trends never absorb historical data. The chip CURRENT VALUE, however, must
 *    reflect the seeked moment like the rest of the dashboard — so while `seeking` we read the
 *    chips' `cur` from the FROZEN store `metrics` (the snapshot cut `applySeekCut` installed) for
 *    the selected window, leaving the sparkline + delta on the (unpolluted) live history ring.
 *
 * Always rendered at the top of `App.tsx` (above the Scrubber).
 */
import { useCallback, useMemo, useRef, useState } from 'react';
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
  latest,
  previous,
  seriesFor,
  WINDOW_KEYS,
  WINDOW_LABELS,
  type MetricHistory,
  type WindowKey,
} from './metricHistory';
import { deriveChips, deltaGlyph, type ChipDescriptor } from './chips';

export function StatsStrip() {
  const [window, setWindow] = useState<WindowKey>('m1');
  const connection = useDashboard((s) => s.connection);
  const seeking = connection === 'seeking';
  // The FROZEN store metrics while seeking — the snapshot cut `applySeekCut` installed. Read as the
  // chip CURRENT VALUE (per window) so the strip reads as-of the seeked moment, while the sparkline
  // history stays the LIVE ring (the seek cut is never folded into it — D11 R5).
  const seekMetrics = useDashboard((s) => (s.connection === 'seeking' ? s.metrics : null));
  const { client } = getConnection();

  // The `/metrics` REST read: seeds the strip pre-WS and is the production data source. `metric`
  // frames invalidate this key so it refetches the authoritative shape. The mock answers it; the
  // store metrics (live) supersedes it as soon as a tick lands.
  const query = useQuery({
    queryKey: queryKeys.metrics,
    queryFn: () => client.metrics(),
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
  const history = historyRef.current;
  // While seeking, the chip CURRENT value is the FROZEN snapshot window (as-of the seeked moment),
  // NOT the live ring's latest — but the sparkline (`seriesFor` below) stays the live history. The
  // delta is FLAT while seeking (`prev = null`): a point-in-time snapshot is not a live trend, so a
  // direction arrow would be misleading. Live → the ring's latest/previous drive value + delta.
  const liveCur = latest(history, window);
  const cur = seeking ? (seekMetrics?.windows[window] ?? liveCur) : liveCur;
  const prev = seeking ? null : previous(history, window);
  const chips = useMemo(() => deriveChips(cur, prev), [cur, prev]);

  return (
    <Panel className="m-4 mb-0 flex items-center gap-1 px-2 py-1" data-testid="stats-strip">
      {chips.map((chip) => (
        <ChipCell key={chip.key} chip={chip} series={seriesFor(history, window, chip.key)} />
      ))}
      <div className="ml-auto flex items-center gap-2 pr-1">
        <WindowSelector value={window} onChange={setWindow} />
        <ConnectionDot state={connection} />
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

/** One chip: label, tabular-nums value + delta arrow, and the metric's sparkline. */
function ChipCell({ chip, series }: { chip: ChipDescriptor; series: number[] }) {
  return (
    <div
      className="flex flex-col gap-1 border-l border-line/50 px-3 py-1 first:border-l-0"
      data-testid={`chip-${chip.key}`}
    >
      <span className="text-[10px] uppercase tracking-[0.14em] text-text-muted">{chip.label}</span>
      <div className="flex items-baseline gap-1">
        <span
          className={cn('font-mono text-xl font-semibold tabular-nums tracking-tight', ACCENT_TEXT[chip.accent])}
          data-testid="chip-value"
        >
          {chip.value}
        </span>
        <span className={cn('text-[10px]', DELTA_CLASS[chip.delta])} aria-hidden data-testid="chip-delta">
          {deltaGlyph(chip.delta)}
        </span>
      </div>
      <Sparkline data={series} stroke={chip.sparkStroke} label={`${chip.label} trend`} />
    </div>
  );
}

/** 1m/5m/1h window selector — switches the source `metrics.windows.*` + sparkline depth. */
function WindowSelector({ value, onChange }: { value: WindowKey; onChange: (w: WindowKey) => void }) {
  return (
    <div className="flex overflow-hidden rounded-md border border-line" role="group" aria-label="metrics window" data-testid="window-selector">
      {WINDOW_KEYS.map((w) => (
        <button
          key={w}
          type="button"
          onClick={() => onChange(w)}
          aria-pressed={value === w}
          className={cn(
            'px-2 py-1 text-xs tabular-nums transition-colors',
            value === w ? 'bg-accent/20 text-accent' : 'bg-transparent text-text-muted hover:text-text',
          )}
        >
          {WINDOW_LABELS[w]}
        </button>
      ))}
    </div>
  );
}

function ConnectionDot({ state }: { state: string }) {
  const color =
    state === 'live' ? 'bg-status-healthy'
    : state === 'connecting' || state === 'seeking' ? 'bg-status-cooling'
    : state === 'error' ? 'bg-status-down'
    : 'bg-text-muted';
  return (
    <span className="flex items-center gap-2 text-[10px] uppercase tracking-[0.14em] text-text-muted">
      <span className={`h-2 w-2 rounded-full ${color}`} aria-hidden />
      {state}
    </span>
  );
}
