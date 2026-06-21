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
 * LIVE-only (D11 R5, mirrors `useMetricStream`): this builds the LIVE token trend, so it must NOT
 * absorb the FROZEN cut a seek writes into the store. `applySeekCut` overwrites `flows` AND flips
 * `connection='seeking'` in ONE atomic update, so the fold SKIPS while `seeking` (the frozen rows
 * never enter the live ring). The SankeyView builds the frozen Sankey separately from the cut's
 * summaries; on resume the accumulator continues from the next live usage bump. We also reset the
 * cumulative baselines + ring on a teardown/snapshot (`connEpoch` change) so a fresh session never
 * diffs against a stale prior total.
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

const DEFAULT_WINDOW_MS = 30_000;
/** Hard cap on retained deltas (defensive — pruning by time is the primary bound). */
const MAX_DELTAS = 5_000;

/**
 * @param windowMs the rolling window; entries older than `now - windowMs` are pruned on each fold.
 * @param now injectable clock (tests); defaults to `Date.now`.
 */
export function useSankeyWindow(windowMs = DEFAULT_WINDOW_MS, now: () => number = Date.now): SankeyWindow {
  const deltasRef = useRef<SankeyUsageDelta[]>([]);
  const baselinesRef = useRef<Map<string, Cumulative>>(new Map());
  const epochRef = useRef<number>(-1);
  const [version, setVersion] = useState(0);

  // Fold the store's current `flows` into timestamped deltas. SKIP while `seeking` (the store holds
  // the FROZEN cut, not live increments — D11 R5). On a `connEpoch` change (teardown / fresh
  // snapshot / seek→live boundary) drop the baselines + ring so we never diff against a stale total
  // nor carry pre-boundary deltas into a new session.
  const consume = useRef(() => {
    const s = dashboardStore.getState();
    if (s.connEpoch !== epochRef.current) {
      epochRef.current = s.connEpoch;
      baselinesRef.current = new Map();
      deltasRef.current = [];
    }
    if (s.connection === 'seeking') return;

    const ts = now();
    const baselines = baselinesRef.current;
    let folded = false;
    for (const f of s.flows.values()) {
      const u = f.usage;
      if (!u) continue;
      const model = f.model_served ?? f.model_requested;
      if (!model) continue;
      const prev = baselines.get(f.api_call_id);
      // Diff against the prior cumulative snapshot; a brand-new flow's first observation IS its
      // delta (prev = 0s). A non-increasing total (no growth) records nothing.
      const dTotal = u.total - (prev?.total ?? 0);
      if (dTotal > 0) {
        deltasRef.current.push({
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
    let pruned = deltasRef.current.filter((d) => d.ts >= cutoff);
    if (pruned.length > MAX_DELTAS) pruned = pruned.slice(pruned.length - MAX_DELTAS);
    const changed = folded || pruned.length !== deltasRef.current.length;
    deltasRef.current = pruned;
    if (changed) setVersion((n) => n + 1);
  }).current;

  // Fold the current state immediately, then on EVERY store change (no increment lost to batching).
  useEffect(() => {
    consume();
    return dashboardStore.subscribe(consume);
  }, [consume]);

  return { version, deltasRef };
}
