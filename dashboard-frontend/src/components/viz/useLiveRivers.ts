/**
 * `useLiveRivers` — the theater's read of the store's INCREMENTAL river fold (fed by `pushMonitor`
 * at message arrival, so river text survives the monitor ring's eviction — see riverModel header).
 * Finalization (tool-run splitting, tok/s) is pure and memoized on the fold reference, which only
 * changes when a river-bearing message was folded.
 */
import { useMemo } from 'react';
import { finalizeRivers, type River } from './riverModel';
import { useDashboard } from '../../store/hooks';

export function useLiveRivers(): River[] {
  const fold = useDashboard((s) => s.riverFold);
  return useMemo(() => finalizeRivers(fold), [fold]);
}
