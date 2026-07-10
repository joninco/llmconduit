import type { FlowSummary } from '../api/types';

export interface OptimisticKillGuard {
  dispatchEpoch: number;
  currentEpoch: number;
  current: FlowSummary | undefined;
  optimistic: FlowSummary | undefined;
}

/**
 * Rollback is safe only while the exact optimistic row installed by this mutation is still current.
 * Revision/status checks document the concurrency contract; identity additionally prevents a same-
 * revision authoritative replacement from being mistaken for our local object.
 */
export function canRollbackOptimisticKill(guard: OptimisticKillGuard): boolean {
  const { dispatchEpoch, currentEpoch, current, optimistic } = guard;
  return (
    dispatchEpoch === currentEpoch
    && current !== undefined
    && optimistic !== undefined
    && current === optimistic
    && current.revision === optimistic.revision
    && current.status === optimistic.status
  );
}
