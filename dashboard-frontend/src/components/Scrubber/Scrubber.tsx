/**
 * Scrubber (D11) — the time-travel timeline under the StatsStrip.
 *
 * Background "hill" = the reqs/s ring buffer (~30 min, 1 s granularity) derived from `metric_tick`
 * history, drawn as an SVG area path. Dragging/clicking the playhead enters SEEK:
 *   1. `socket.seek()` PAUSES applying live WS frames (they shadow-buffer in the socket) but does
 *      NOT yet expose `connection==='seeking'` — the live cursors/rows stay current until the cut
 *      lands, so a seek listener (D10) is never shown live data under a `seeking` flag (finding 1);
 *   2. the drag's pixel-x maps to a wall-clock instant in the ring's span (tracked locally for the
 *      playhead so it follows the drag even before the fetch resolves);
 *   3. `SnapshotController.requestAt(ts)` fetches `/snapshot?at=<ts>` — rAF-throttled + LRU-cached
 *      by second-bucket, strictly ONE in flight (coalescing intermediate drags), so rapid drags
 *      make ≤1 fetch/frame (NO request storm);
 *   4. on resolve, ONE atomic `applySeekCut(...)` installs the frozen body-free cut (rows + cursors
 *      + `seekAtMs` + `seekMonitorSeq`) AND flips `connection==='seeking'` together — so D10/D12
 *      render the frozen moment with a frozen `seekAtMs` and the store is never `seeking` with live
 *      rows.
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

  // The snapshot controller: broadcast a fetched cut as a FROZEN seek view via ONE atomic store
  // action. `applySeekCut` installs the frozen rows + cursors + `seekAtMs` + `seekMonitorSeq` AND
  // flips `connection==='seeking'` in a single update — so the store is never observed `seeking`
  // with live rows (finding 1). `seekMonitorSeq` is the SNAPSHOT's `monitor_seq` (the authoritative
  // cut), so the D10 monitor join is bounded to the cut instant, not a live cursor.
  const controllerRef = useRef<SnapshotController | null>(null);
  if (controllerRef.current === null) {
    controllerRef.current = new SnapshotController({
      fetchSnapshot: (atMs) => client.snapshot(atMs),
      onSnapshot: (resp: SnapshotResponse) => {
        dashboardStore.getState().applySeekCut({
          rows: resp.summaries,
          cursors: resp.cursors,
          atMs: resp.at_ms,
          monitorSeq: resp.cursors.monitor_seq,
          metrics: resp.metrics,
          topology: resp.topology,
        });
        // The cut is now exposed; drop the local pre-fetch drag marker (the frozen `seekAtMs`
        // drives the playhead from here).
        setDragAtMs(null);
      },
    });
  }
  const controller = controllerRef.current;

  // Cancel any pending frame on unmount so a teardown can't fire a late fetch.
  useEffect(() => () => controller.cancel(), [controller]);

  const trackRef = useRef<HTMLDivElement>(null);
  const [hover, setHover] = useState<{ x: number; t: number; reqs: number } | null>(null);
  const draggingRef = useRef(false);
  // The drag instant BEFORE the frozen cut lands (finding 1): while the fetch is in flight the
  // store is intentionally NOT yet `'seeking'` (rows stay live), so the playhead can't read
  // `seekAtMs`. This local marker tracks the drag so the playhead follows immediately; it is
  // cleared once the cut installs (`seekAtMs` takes over) or on resume.
  const [dragAtMs, setDragAtMs] = useState<number | null>(null);
  // A seek is PENDING once the user starts dragging (socket paused) until the cut lands or resume.
  // Drives the LIVE toggle so the user can always bail out of an in-flight seek.
  const pendingSeek = dragAtMs !== null && !seeking;

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

  /**
   * Begin/continue a seek at the pointer's time. PAUSES live applying (shadow-buffer) but does NOT
   * expose `'seeking'` — only the atomic `applySeekCut` (when the fetch resolves) does, so the store
   * never reads `seeking` with live rows (finding 1). The local `dragAtMs` tracks the playhead in
   * the meantime; the controller fetches the frozen cut (rAF-throttled, ≤1 in flight).
   */
  const seekToClientX = useCallback(
    (clientX: number) => {
      const t = timeFromClientX(clientX);
      if (!socket.isPaused()) socket.seek(); // pause applying live frames (shadow-buffer)
      setDragAtMs(t); // local playhead marker; NO store `seeking` flip until the cut lands
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
    setDragAtMs(null); // drop any in-flight drag marker
    socket.live();
  }, [controller, socket]);

  // The playhead position (fraction): the frozen `seekAtMs` once the cut lands, else the in-flight
  // drag marker (`dragAtMs`) so the playhead follows the drag before the fetch resolves; live (no
  // seek, no pending drag) → pinned to the right edge (now).
  const playheadAtMs = seeking ? seekAtMs : dragAtMs;
  const playheadFrac = useMemo(() => {
    const b = reqsBounds(ring);
    if (!b || b.tEnd === b.t0) return 1;
    if (playheadAtMs !== null) {
      return Math.min(1, Math.max(0, (playheadAtMs - b.t0) / (b.tEnd - b.t0)));
    }
    return 1;
  }, [playheadAtMs, ring]);

  const hillPath = useMemo(() => buildHillPath(ring, HILL_H), [ring]);

  return (
    <div className="mx-4 mt-2 flex items-center gap-3 rounded-md border border-line bg-panel px-3 py-2" data-testid="scrubber">
      {seeking || pendingSeek ? (
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

      {/* The draggable timeline track. NOTE: the track itself is NOT `overflow-hidden` (finding 4) —
          only the inner hill layer clips, so the hover tooltip (which sits ABOVE the track at
          `-top-9`) is not clipped away. The playhead + tooltip render here, outside the clip. */}
      <div
        ref={trackRef}
        data-testid="scrubber-track"
        role="slider"
        aria-label="time-travel scrubber"
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={Math.round(playheadFrac * 100)}
        tabIndex={0}
        className="relative h-10 flex-1 cursor-pointer select-none rounded bg-panel-raised"
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={endDrag}
        onPointerLeave={(e) => {
          setHover(null);
          if (draggingRef.current) endDrag(e);
        }}
      >
        {/* Inner clipped layer: only the hill is clipped to the rounded track (finding 4). */}
        <div className="absolute inset-0 overflow-hidden rounded">
          <svg
            className="absolute inset-0 h-full w-full"
            viewBox={`0 0 100 ${HILL_H}`}
            preserveAspectRatio="none"
            aria-hidden
            data-testid="scrubber-hill"
          >
            <path d={hillPath} className="fill-accent/20 stroke-accent/60" strokeWidth={0.6} vectorEffect="non-scaling-stroke" />
          </svg>
        </div>

        {/* Playhead */}
        <div
          data-testid="scrubber-playhead"
          className={cn('absolute top-0 h-full w-0.5 bg-accent', !reduced && 'transition-[left] duration-75')}
          style={{ left: `${playheadFrac * 100}%` }}
          aria-hidden
        />

        {/* Hover tooltip — time + reqs/s at the point. Rendered OUTSIDE the clipped hill layer so the
            `-top-9` position is not cut off by `overflow-hidden` in the real UI (finding 4). */}
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
