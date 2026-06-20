/**
 * `useMetricStream` — subscribe to EVERY distinct `metrics` sample the store receives and fold it
 * into a caller-owned ring, returning a version counter that bumps on each fold.
 *
 * Why a direct store subscription (not `useDashboard` + an effect): React batches synchronous
 * store updates into ONE commit, so a `useSyncExternalStore` selector only exposes the LATEST
 * `metrics` — intermediate `metric_tick` samples (and thus their sparkline/hill points) would be
 * lost whenever several land in the same tick. Subscribing to the vanilla store directly runs the
 * fold on every change, so no sample is dropped. Dedup is by `metrics_seq` (each unique seq folds
 * once); an optional `seed` (the `/metrics` query result) primes the ring before the first tick.
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
  // still deduping a repeated frame. Returns whether it folded (the seed uses this).
  const consume = useRef((sample: MetricsResponse | null | undefined): boolean => {
    if (sample && sample.metrics_seq !== lastSeqRef.current) {
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
    if (lastSeqRef.current === -1) consume(seed);
  }, [seed, consume]);

  // Subscribe to EVERY store change and fold any new `metrics` — no sample dropped to batching.
  useEffect(() => {
    consume(dashboardStore.getState().metrics);
    const unsub = dashboardStore.subscribe((state) => consume(state.metrics));
    return unsub;
  }, [consume]);

  return { version };
}
