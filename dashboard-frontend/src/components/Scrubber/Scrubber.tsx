/**
 * Scrubber (D11) — the time-travel timeline under the StatsStrip.
 *
 * Background "hill" = the reqs/s ring buffer (~30 min, 1 s granularity) derived from `metric_tick`
 * history, drawn as an SVG area path. Dragging/clicking the playhead enters SEEK:
 *   1. `socket.seek()` pauses applying live WS frames (they shadow-buffer in the socket);
 *   2. the drag's pixel-x maps to a wall-clock instant in the ring's span;
 *   3. `SnapshotController.requestAt(ts)` fetches `/snapshot?at=<ts>` — rAF-throttled + LRU-cached
 *      by second-bucket, so rapid drags coalesce to ≤1 fetch/frame (NO request storm);
 *   4. on resolve, the frozen body-free cut is broadcast into the store (`applySnapshot`) and the
 *      seek instant re-stamped (`enterSeek`) so D10/D12 render the frozen moment with `connection
 *      === 'seeking'` and a frozen `seekAtMs`.
 * A LIVE toggle resumes: `socket.live()` replays the buffered frames (or applies a reconnect
 * snapshot) and flips back to live. The playhead hover shows the time + reqs/s at that point.
 *
 * `prefers-reduced-motion` cuts the pulsing LIVE indicator + the playhead transition (static).
 *
 * Snapshot bodies are evicted (D5 body-free): stats/summary render as-of; this is surfaced as a
 * small "as of <time> · bodies live" note while seeking (the documented tradeoff).
 */
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import type { DashboardSocket } from '../../api/ws';
import type { MetricsResponse, SnapshotResponse } from '../../api/types';
import { useDashboard } from '../../store/hooks';
import { useMetricStream } from '../../store/useMetricStream';
import { dashboardStore } from '../../store/dashboardStore';
import { getConnection } from '../../api/connection';
import { prefersReducedMotion } from '../../design/tokens';
import { cn } from '../../lib/cn';
import { fmtClock } from '../FlowTable/format';
import {
  appendReqs,
  reqsBounds,
  reqsPeak,
  sampleAt,
  xToTime,
  type ReqsSample,
} from './reqsHistory';
import { SnapshotController } from './snapshotController';

const HILL_H = 40;

export function Scrubber({ socket }: { socket: DashboardSocket }) {
  const { client } = getConnection();
  const connection = useDashboard((s) => s.connection);
  const seeking = connection === 'seeking';
  const seekAtMs = useDashboard((s) => s.seekAtMs);
  const reduced = prefersReducedMotion();

  // reqs/s ring (held in a ref). `useMetricStream` folds EVERY distinct store sample (deduped by
  // seq, no sample lost to render batching) and bumps `version` so the hill re-renders. Each tick
  // is stamped with arrival wall-clock, nudged strictly monotonic so several arriving in the same
  // millisecond still produce distinct hill points (the wire carries no per-tick timestamp).
  const ringRef = useRef<ReqsSample[]>([]);
  const lastStampRef = useRef<number>(0);
  const fold = useCallback((sample: MetricsResponse) => {
    const t = Math.max(Date.now(), lastStampRef.current + 1);
    lastStampRef.current = t;
    ringRef.current = appendReqs(ringRef.current, t, sample.reqs_per_sec);
  }, []);
  const { version } = useMetricStream(fold);
  // `version` is read so this body re-runs after each ring fold; `ringRef.current` is reassigned a
  // FRESH array on every fold (appendReqs is immutable), so `ring` changes reference and the
  // ring-derived memos below recompute without needing `version` in their dep arrays.
  void version;
  const ring = ringRef.current;

  // The snapshot controller: broadcast a fetched cut as a FROZEN seek view. We apply the body-free
  // summaries (replaces rows + clears any prior freeze + bumps epoch) THEN re-stamp the seek
  // instant so `connection === 'seeking'` + `seekAtMs` hold for the D10/D12 seek-listeners.
  const controllerRef = useRef<SnapshotController | null>(null);
  if (controllerRef.current === null) {
    controllerRef.current = new SnapshotController({
      fetchSnapshot: (atMs) => client.snapshot(atMs),
      onSnapshot: (resp: SnapshotResponse) => {
        const store = dashboardStore.getState();
        store.applySnapshot({
          cursors: resp.cursors,
          flows: resp.summaries,
          metrics: resp.metrics,
          topology: resp.topology,
        });
        // Re-enter seek at the cut instant (applySnapshot cleared the freeze): rows now hold the
        // frozen cut AND `connection==='seeking'` + `seekAtMs` are restored for the listeners.
        store.enterSeek(resp.at_ms);
      },
    });
  }
  const controller = controllerRef.current;

  // Cancel any pending frame on unmount so a teardown can't fire a late fetch.
  useEffect(() => () => controller.cancel(), [controller]);

  const trackRef = useRef<HTMLDivElement>(null);
  const [hover, setHover] = useState<{ x: number; t: number; reqs: number } | null>(null);
  const draggingRef = useRef(false);

  /** Pixel clientX → normalized fraction across the track (0 for a missing/degenerate rect). */
  const fracFromClientX = useCallback((clientX: number): number => {
    const el = trackRef.current;
    if (!el || !Number.isFinite(clientX)) return 0;
    const rect = el.getBoundingClientRect();
    if (rect.width <= 0) return 0;
    return Math.min(1, Math.max(0, (clientX - rect.left) / rect.width));
  }, []);

  /** Resolve a pointer clientX to a wall-clock instant in the ring (falls back to now). */
  const timeFromClientX = useCallback((clientX: number): number => {
    const frac = fracFromClientX(clientX);
    const t = xToTime(ringRef.current, frac);
    return t !== null && Number.isFinite(t) ? t : Date.now();
  }, [fracFromClientX]);

  /** Begin/continue a seek at the pointer's time. Enters seek (pause) then requests the cut. */
  const seekToClientX = useCallback(
    (clientX: number) => {
      const t = timeFromClientX(clientX);
      if (!socket.isPaused()) socket.seek(); // pause applying live frames (shadow-buffer)
      // Mark the instant immediately so the playhead tracks the drag even before the fetch lands.
      dashboardStore.getState().enterSeek(t);
      controller.requestAt(t); // rAF-throttled + LRU-cached fetch of the frozen cut
    },
    [controller, socket, timeFromClientX],
  );

  const onPointerDown = useCallback(
    (e: React.PointerEvent) => {
      draggingRef.current = true;
      e.currentTarget.setPointerCapture?.(e.pointerId);
      seekToClientX(e.clientX);
    },
    [seekToClientX],
  );

  const onPointerMove = useCallback(
    (e: React.PointerEvent) => {
      const t = timeFromClientX(e.clientX);
      const s = sampleAt(ringRef.current, t);
      const rect = trackRef.current?.getBoundingClientRect();
      const cx = Number.isFinite(e.clientX) ? e.clientX : 0;
      const x = rect ? Math.min(rect.width, Math.max(0, cx - rect.left)) : 0;
      setHover({ x, t, reqs: s?.reqs ?? 0 });
      if (draggingRef.current) seekToClientX(e.clientX);
    },
    [seekToClientX, timeFromClientX],
  );

  const endDrag = useCallback((e: React.PointerEvent) => {
    draggingRef.current = false;
    e.currentTarget.releasePointerCapture?.(e.pointerId);
  }, []);

  /** Resume LIVE: socket replays buffered frames + flips to live; cancel any pending fetch. */
  const goLive = useCallback(() => {
    controller.cancel();
    socket.live();
  }, [controller, socket]);

  // The playhead position (fraction) while seeking: derived from the frozen `seekAtMs` over the
  // ring span; live → pinned to the right edge (now).
  const playheadFrac = useMemo(() => {
    const b = reqsBounds(ring);
    if (!b || b.tEnd === b.t0) return 1;
    if (seeking && seekAtMs !== null) {
      return Math.min(1, Math.max(0, (seekAtMs - b.t0) / (b.tEnd - b.t0)));
    }
    return 1;
  }, [seeking, seekAtMs, ring]);

  const hillPath = useMemo(() => buildHillPath(ring, HILL_H), [ring]);

  return (
    <div className="mx-4 mt-2 flex items-center gap-3 rounded-md border border-line bg-panel px-3 py-2" data-testid="scrubber">
      {seeking ? (
        <button
          type="button"
          onClick={goLive}
          data-testid="live-toggle"
          className={cn(
            'inline-flex items-center gap-2 rounded-md border border-status-healthy/40 bg-status-healthy/15 px-3 py-1.5 text-sm font-medium text-status-healthy',
            !reduced && 'motion-safe:animate-pulse',
          )}
        >
          <span className="h-2 w-2 rounded-full bg-status-healthy" aria-hidden />
          LIVE
        </button>
      ) : (
        <span className="inline-flex items-center gap-2 px-1 text-xs uppercase tracking-wide text-text-muted" data-testid="live-indicator">
          <span className={cn('h-2 w-2 rounded-full bg-status-healthy', !reduced && 'motion-safe:animate-pulse')} aria-hidden />
          live
        </span>
      )}

      {/* The draggable timeline track + reqs/s hill. */}
      <div
        ref={trackRef}
        data-testid="scrubber-track"
        role="slider"
        aria-label="time-travel scrubber"
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={Math.round(playheadFrac * 100)}
        tabIndex={0}
        className="relative h-10 flex-1 cursor-pointer select-none overflow-hidden rounded bg-panel-raised"
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={endDrag}
        onPointerLeave={(e) => {
          setHover(null);
          if (draggingRef.current) endDrag(e);
        }}
      >
        <svg
          className="absolute inset-0 h-full w-full"
          viewBox={`0 0 100 ${HILL_H}`}
          preserveAspectRatio="none"
          aria-hidden
          data-testid="scrubber-hill"
        >
          <path d={hillPath} className="fill-accent/20 stroke-accent/60" strokeWidth={0.6} vectorEffect="non-scaling-stroke" />
        </svg>

        {/* Playhead */}
        <div
          data-testid="scrubber-playhead"
          className={cn('absolute top-0 h-full w-0.5 bg-accent', !reduced && 'transition-[left] duration-75')}
          style={{ left: `${playheadFrac * 100}%` }}
          aria-hidden
        />

        {/* Hover tooltip — time + reqs/s at the point. */}
        {hover && (
          <div
            data-testid="scrubber-tooltip"
            className="pointer-events-none absolute -top-9 z-10 -translate-x-1/2 whitespace-nowrap rounded border border-line bg-panel px-2 py-1 text-[11px] tabular-nums text-text shadow"
            style={{ left: hover.x }}
          >
            {fmtClock(hover.t)} · {hover.reqs.toFixed(1)} req/s
          </div>
        )}
      </div>

      {/* Seek state readout: shadow-buffer depth + the body-free tradeoff note. */}
      <span className="tabular-nums text-xs text-text-muted" data-testid="scrubber-status">
        {seeking && seekAtMs !== null ? (
          <span title="Snapshot is body-free (D5): stats render as-of; request/response bodies render live.">
            as of {fmtClock(seekAtMs)} · buffered {socket.shadowBufferLength()}
          </span>
        ) : (
          'live'
        )}
      </span>
    </div>
  );
}

/**
 * Build the SVG area path for the reqs/s hill in a `100 × height` viewBox (x normalized 0..100,
 * y inverted so larger reqs rise). Empty/single-sample rings draw a flat baseline.
 */
function buildHillPath(ring: ReqsSample[], height: number): string {
  if (ring.length === 0) return `M0 ${height} L100 ${height} Z`;
  const b = reqsBounds(ring)!;
  const span = b.tEnd - b.t0;
  const peak = reqsPeak(ring);
  const x = (t: number) => (span === 0 ? 0 : ((t - b.t0) / span) * 100);
  const y = (r: number) => height - (r / peak) * height;
  let d = `M0 ${height}`;
  // First point at the left baseline, then the area top following each sample.
  d += ` L${x(ring[0]!.t).toFixed(2)} ${y(ring[0]!.reqs).toFixed(2)}`;
  for (let i = 1; i < ring.length; i++) {
    d += ` L${x(ring[i]!.t).toFixed(2)} ${y(ring[i]!.reqs).toFixed(2)}`;
  }
  // Close down to the baseline at the right edge.
  d += ` L${x(ring[ring.length - 1]!.t).toFixed(2)} ${height} Z`;
  return d;
}
