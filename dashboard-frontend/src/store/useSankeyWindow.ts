/**
 * `useSankeyWindow` — the LIVE rolling-window accumulator for the token Sankey (D12, finding 2).
 * Subscribes to the dashboard store DIRECTLY (like `useMetricStream`) and folds every distinct
 * usage GROWTH per flow into a TIMESTAMPED delta, returning a ref-held ring pruned to the window.
 *
 * Why deltas (not cumulative totals): a `FlowSummary.usage` is the flow's LIFETIME cumulative
 * count. The band must be tokens/30 s, so we cannot sum cumulative totals of every flow that
 * overlaps the window — a single long-running flow would report its entire lifetime as the 30 s
 * rate. Instead we record the INCREMENT each flow grew by, stamped with the wall-clock instant we
 * observed it, and the model sums only the increments inside `[now - windowMs, now]`. A flow that
 * streamed a million tokens an hour ago but is idle now contributes nothing to the current band.
 *
 * Why a direct store subscription (not `useDashboard` + an effect): React batches synchronous store
 * updates into ONE commit, so a `useSyncExternalStore` selector only exposes the LATEST `flows` —
 * intermediate usage bumps landing in the same tick would be lost. Subscribing to the vanilla store
 * directly runs the fold on every change, so no increment is dropped.
 *
 * STABLE SINGLETON COLLECTOR (finding 3 — the fix for "route entry / seek resume restamps every
 * cumulative total as fresh traffic"): the baselines + delta ring live at MODULE scope, NOT in a
 * per-mount ref, so they SURVIVE a SankeyView remount (route navigation) and a seek round-trip
 * (`live → seeking → live`). A flow already at cumulative total T keeps baseline T, so its first
 * observation after a remount/resume diffs to 0 and emits NOTHING — its lifetime total is never
 * re-stamped at `Date.now()` as a fresh 30 s band. Only TRUE incremental growth folds, with its
 * real arrival time. The baselines + ring are cleared ONLY on a genuine TEARDOWN edge — the
 * connection entering `'idle'`/`'closed'` (a `reset()` / fresh session), where cumulative continuity
 * is actually broken and a reused `api_call_id` may restart at a lower total. That clear is driven
 * by a module-level store subscription (below), so it fires even when no `SankeyView` is mounted at
 * the instant of teardown (e.g. a reset between route mounts).
 */
import { useEffect, useRef, useState } from 'react';
import { dashboardStore } from './dashboardStore';
import type { SankeyUsageDelta } from '../components/viz/sankeyModel';

export interface SankeyWindow {
  /** Bumps once per folded increment so the component re-renders to read the (ref-held) ring. */
  version: number;
  /** The current windowed deltas (ref-held — read after a `version` bump). */
  deltasRef: React.MutableRefObject<SankeyUsageDelta[]>;
}

/** The last cumulative usage we saw per flow, to diff the next snapshot into a delta. */
interface Cumulative {
  prompt: number;
  cached: number;
  completion: number;
  total: number;
}

/**
 * The MODULE-level collector — one per app, surviving component remounts (route navigation) and
 * seek round-trips so a flow's cumulative baseline is never re-stamped as fresh traffic. Cleared
 * only on a genuine teardown edge (see the module subscription below).
 */
interface Collector {
  baselines: Map<string, Cumulative>;
  deltas: SankeyUsageDelta[];
}

const collector: Collector = { baselines: new Map(), deltas: [] };

/** Connection states that signal a genuine session teardown (cumulative continuity broken). */
function isTeardown(connection: string): boolean {
  return connection === 'idle' || connection === 'closed';
}

/**
 * Clear the collector on a TEARDOWN EDGE, driven by a module-level store subscription so it fires
 * regardless of whether a `SankeyView` is mounted at that instant (a `reset()` may land between
 * route mounts). A teardown is an edge INTO `'idle'`/`'closed'` (was-not-teardown → is-teardown);
 * a seek round-trip never enters those states, so it does NOT clear (the baselines are preserved and
 * an unchanged cumulative total diffs to 0 on resume — no restamp). Subscribing at module scope is
 * a deliberate app-lifetime global (mirrors the singleton collector); it is never torn down.
 */
let prevConnection = dashboardStore.getState().connection as string;
dashboardStore.subscribe(() => {
  const connection = dashboardStore.getState().connection as string;
  if (isTeardown(connection) && !isTeardown(prevConnection)) {
    collector.baselines = new Map();
    collector.deltas = [];
  }
  prevConnection = connection;
});

const DEFAULT_WINDOW_MS = 30_000;
/** Hard cap on retained deltas (defensive — pruning by time is the primary bound). */
const MAX_DELTAS = 5_000;

/**
 * @param windowMs the rolling window; entries older than `now - windowMs` are pruned on each fold.
 * @param now injectable clock (tests); defaults to `Date.now`.
 */
export function useSankeyWindow(windowMs = DEFAULT_WINDOW_MS, now: () => number = Date.now): SankeyWindow {
  const deltasRef = useRef<SankeyUsageDelta[]>(collector.deltas);
  const [version, setVersion] = useState(0);

  // Fold the store's current `flows` into timestamped deltas using the MODULE collector. SKIP while
  // `seeking` (the store holds the FROZEN cut, not live increments — D11 R5). Teardown clearing is
  // handled by the module subscription above, NOT here, so a remount/seek-resume preserves the
  // baselines and an unchanged cumulative total diffs to 0 (no restamp at Date.now()).
  const consume = useRef(() => {
    const s = dashboardStore.getState();
    // Keep the public ref pointed at the (possibly just-cleared) singleton ring before any early
    // return, so a teardown that emptied the ring is reflected even when there is nothing to fold.
    if (deltasRef.current !== collector.deltas) {
      deltasRef.current = collector.deltas;
      setVersion((n) => n + 1);
    }
    if (s.connection === 'seeking') return;

    const ts = now();
    const baselines = collector.baselines;
    let folded = false;
    for (const f of s.flows.values()) {
      const u = f.usage;
      if (!u) continue;
      const model = f.model_served ?? f.model_requested;
      if (!model) continue;
      const prev = baselines.get(f.api_call_id);
      // Diff against the prior cumulative snapshot; a brand-new flow's first observation IS its
      // delta (prev = 0s). A non-increasing total (no growth) records nothing — so a historical
      // cumulative total already captured in the baseline (after a remount/seek-resume) emits
      // NOTHING, never a fresh Date.now()-stamped band.
      const dTotal = u.total - (prev?.total ?? 0);
      if (dTotal > 0) {
        collector.deltas.push({
          ts,
          upstream: f.upstream_target ?? null,
          model,
          prompt: Math.max(0, u.prompt - (prev?.prompt ?? 0)),
          cached: Math.max(0, u.cached - (prev?.cached ?? 0)),
          completion: Math.max(0, u.completion - (prev?.completion ?? 0)),
          total: dTotal,
        });
        folded = true;
      }
      // Always advance the baseline to the latest cumulative (even on a non-positive diff, e.g. a
      // corrected/reset total) so the NEXT diff is against the current truth.
      baselines.set(f.api_call_id, { prompt: u.prompt, cached: u.cached, completion: u.completion, total: u.total });
    }

    // Prune by time (primary bound) + a hard cap (defensive against a pathological burst).
    const cutoff = ts - windowMs;
    let pruned = collector.deltas.filter((d) => d.ts >= cutoff);
    if (pruned.length > MAX_DELTAS) pruned = pruned.slice(pruned.length - MAX_DELTAS);
    const changed = folded || pruned.length !== collector.deltas.length;
    collector.deltas = pruned;
    deltasRef.current = collector.deltas;
    if (changed) setVersion((n) => n + 1);
  }).current;

  // Fold the current state immediately, then on EVERY store change (no increment lost to batching).
  useEffect(() => {
    consume();
    return dashboardStore.subscribe(consume);
  }, [consume]);

  return { version, deltasRef };
}
