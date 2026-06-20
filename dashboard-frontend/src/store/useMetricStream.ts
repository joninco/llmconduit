/**
 * `useMetricStream` — subscribe to EVERY distinct LIVE `metrics` sample the store receives and fold
 * it into a caller-owned ring, returning a version counter that bumps on each fold.
 *
 * Why a direct store subscription (not `useDashboard` + an effect): React batches synchronous
 * store updates into ONE commit, so a `useSyncExternalStore` selector only exposes the LATEST
 * `metrics` — intermediate `metric_tick` samples (and thus their sparkline/hill points) would be
 * lost whenever several land in the same tick. Subscribing to the vanilla store directly runs the
 * fold on every change, so no sample is dropped. Dedup is by `metrics_seq` (each unique seq folds
 * once); an optional `seed` (the `/metrics` query result) primes the ring before the first tick.
 *
 * LIVE-only fold (D11 R5): the history this builds is the LIVE metric trend, so it must NOT absorb
 * the FROZEN historical metrics a seek writes into the store. A seek's `applySeekCut` overwrites
 * `metrics` with the snapshot's frozen cut AND flips `connection='seeking'` in ONE atomic update,
 * so the fold SKIPS while `connection==='seeking'` — the frozen sample never enters the live ring
 * (it would otherwise append as if it arrived now, stamped with the live wall-clock, polluting the
 * StatsStrip sparkline rings + the Scrubber hill). On resume, `restoreLiveBaseline` reinstates the
 * pre-seek live `metrics` (its original `metrics_seq`) atomically with `connection='live'`; the seq
 * dedup then drops it as already-folded (we never advanced past it while skipping the frozen cut),
 * so the live history continues cleanly from the next live tick — no seek pollution, no duplicate
 * baseline.
 */
import { useEffect, useRef, useState } from 'react';
import { dashboardStore } from './dashboardStore';
import type { MetricsResponse } from '../api/types';

export interface MetricStream {
  /** Bumps once per folded sample so the component re-renders to read the (ref-held) ring. */
  version: number;
}

/**
 * @param fold called for each distinct sample (the store metrics, deduped by `metrics_seq`).
 *   It MUST be a stable reference (wrap in `useRef`/module scope) — it is captured once.
 * @param seed optional priming sample (e.g. the `/metrics` REST seed) folded before any tick.
 */
export function useMetricStream(fold: (sample: MetricsResponse) => void, seed?: MetricsResponse | null): MetricStream {
  const foldRef = useRef(fold);
  foldRef.current = fold;
  /** Last folded seq; -1 until the first sample (also gates whether the seed may prime). */
  const lastSeqRef = useRef<number>(-1);
  const [version, setVersion] = useState(0);

  // Fold a sample if its seq differs from the last folded one; bump the version to re-render. A
  // `!==` (not `>`) check folds both forward ticks AND a reconnect that resets the seq, while
  // still deduping a repeated frame. SKIP entirely while `seeking` (D11 R5): the store `metrics`
  // then holds the FROZEN historical cut, not a live sample — folding it would inject historical
  // data into the live ring (and advancing `lastSeqRef` to the snapshot seq would then let the
  // restored baseline re-fold as a duplicate on resume). Not advancing the seq keeps the resume
  // baseline deduped. Returns whether it folded (the seed uses this).
  const consume = useRef((sample: MetricsResponse | null | undefined, seeking: boolean): boolean => {
    if (!seeking && sample && sample.metrics_seq !== lastSeqRef.current) {
      lastSeqRef.current = sample.metrics_seq;
      foldRef.current(sample);
      setVersion((n) => n + 1);
      return true;
    }
    return false;
  }).current;

  // Prime from the seed (the `/metrics` query) ONLY while the ring is still empty (no live sample
  // folded yet), so a stale cached seed can never append AFTER newer live samples (out of order).
  useEffect(() => {
    if (lastSeqRef.current === -1) consume(seed, dashboardStore.getState().connection === 'seeking');
  }, [seed, consume]);

  // Subscribe to EVERY store change and fold any new LIVE `metrics` — no sample dropped to batching,
  // and the frozen seek cut skipped (the `applySeekCut`/`restoreLiveBaseline` updates flip
  // `connection` atomically with `metrics`, so the gate reads coherently with the sample).
  useEffect(() => {
    const st = dashboardStore.getState();
    consume(st.metrics, st.connection === 'seeking');
    const unsub = dashboardStore.subscribe((state) => consume(state.metrics, state.connection === 'seeking'));
    return unsub;
  }, [consume]);

  return { version };
}
