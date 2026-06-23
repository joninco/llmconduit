import { describe, it, expect } from 'vitest';
import { pickAttempts, sameAttempts } from './attempts';
import type { Attempt } from './types';

/**
 * `pickAttempts` (gap 10b review round 2) — the shared "non-empty wins, else backfill" rule the
 * useFlowRows merge, the FlowDetail spine merge, and the patchFlowStatus reducer all share. The
 * core hazard: an EMPTY `attempts: []` is the serialization for "no attempt recorded yet", so `??`
 * would wrongly treat it as authoritative and drop a populated trace from the other source.
 */

const SERVED: Attempt = {
  provider: 'openai',
  model: 'gpt-4o',
  start_ms: 1_700_000_000_100,
  end_ms: 1_700_000_000_400,
  first_upstream_byte_ms: 1_700_000_000_350,
  status: 'served',
};
const FAILED: Attempt = {
  provider: 'vllm-a',
  model: 'llama-3.1-70b',
  start_ms: 1_700_000_000_000,
  end_ms: 1_700_000_000_080,
  status: 'failed',
  error_class: 'http_status',
};

describe('pickAttempts — non-empty wins, empty is absent for backfill', () => {
  it('an EMPTY fresh list does NOT block a populated fallback (the core bug)', () => {
    // live/snapshot `[]` must NOT win over a populated REST list.
    expect(pickAttempts([], [SERVED])).toEqual([SERVED]);
  });

  it('a NON-EMPTY fresh list WINS over the fallback (freshest authoritative trace)', () => {
    expect(pickAttempts([FAILED, SERVED], [SERVED])).toEqual([FAILED, SERVED]);
  });

  it('falls back to the populated list when fresh is absent (null/undefined)', () => {
    expect(pickAttempts(undefined, [SERVED])).toEqual([SERVED]);
    expect(pickAttempts(null, [SERVED])).toEqual([SERVED]);
  });

  it('both empty/absent ⇒ undefined (honestly no attempts — never a fabricated entry)', () => {
    expect(pickAttempts([], [])).toBeUndefined();
    expect(pickAttempts(undefined, undefined)).toBeUndefined();
    expect(pickAttempts([], null)).toBeUndefined();
    expect(pickAttempts(null, [])).toBeUndefined();
  });

  it('reducer ordering: a LATER empty frame does NOT erase a prior non-empty trace', () => {
    // patchFlowStatus calls pickAttempts(p.attempts, prev?.attempts): a later `[]` keeps prior.
    expect(pickAttempts([], [SERVED])).toEqual([SERVED]);
    // …but a later NON-EMPTY frame still updates it.
    expect(pickAttempts([FAILED, SERVED], [SERVED])).toEqual([FAILED, SERVED]);
  });
});

describe('sameAttempts — empty and absent are the same "no trace" state', () => {
  it('treats []/null/undefined as mutually equal (no churn for a no-trace row)', () => {
    expect(sameAttempts([], undefined)).toBe(true);
    expect(sameAttempts(undefined, [])).toBe(true);
    expect(sameAttempts(null, undefined)).toBe(true);
    expect(sameAttempts([], [])).toBe(true);
  });

  it('a non-empty list vs absent/empty is unequal (a real trace change ⇒ re-render)', () => {
    expect(sameAttempts([SERVED], undefined)).toBe(false);
    expect(sameAttempts([], [SERVED])).toBe(false);
  });

  it('compares non-empty lists by reference (same ref equal, different ref unequal)', () => {
    const list = [SERVED];
    expect(sameAttempts(list, list)).toBe(true);
    expect(sameAttempts([SERVED], [SERVED])).toBe(false); // distinct refs
  });
});
