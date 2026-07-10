import { describe, expect, it } from 'vitest';
import type { FlowSummary } from '../api/types';
import { canRollbackOptimisticKill } from './optimisticKill';

function row(over: Partial<FlowSummary> = {}): FlowSummary {
  return {
    revision: 2,
    api_call_id: 'api_kill',
    method: 'POST',
    uri: '/v1/responses',
    status: 'cancelled',
    usage: null,
    started_ms: 1,
    cost: null,
    cost_confidence: 'unavailable',
    ...over,
  };
}

describe('canRollbackOptimisticKill', () => {
  it('allows rollback only for the exact optimistic row in the same connection epoch', () => {
    const optimistic = row();
    expect(canRollbackOptimisticKill({
      dispatchEpoch: 7,
      currentEpoch: 7,
      current: optimistic,
      optimistic,
    })).toBe(true);
  });

  it('rejects rollback after a connection boundary', () => {
    const optimistic = row();
    expect(canRollbackOptimisticKill({
      dispatchEpoch: 7,
      currentEpoch: 8,
      current: optimistic,
      optimistic,
    })).toBe(false);
  });

  it('rejects rollback after a newer authoritative revision replaces the row', () => {
    const optimistic = row();
    expect(canRollbackOptimisticKill({
      dispatchEpoch: 7,
      currentEpoch: 7,
      current: row({ revision: 3, status: 'completed' }),
      optimistic,
    })).toBe(false);
  });

  it('rejects a distinct authoritative replacement even at the same revision and status', () => {
    const optimistic = row();
    expect(canRollbackOptimisticKill({
      dispatchEpoch: 7,
      currentEpoch: 7,
      current: row({ revision: optimistic.revision, status: optimistic.status }),
      optimistic,
    })).toBe(false);
  });
});
