import { describe, it, expect, beforeEach } from 'vitest';
import { dashboardStore } from './dashboardStore';
import type { FlowSummary, SeqCursors } from '../api/types';

/**
 * The MONOTONIC connection-transition generation (`connEpoch`) underpins the kill mutation's
 * boundary guard (useFlowDetail, finding 1). It must advance on EVERY real boundary crossing and
 * NEVER repeat — unlike the reusable `connection` STRING, which returns to `'live'` after a
 * `live → seek → live` round-trip. These lock that invariant at the store level.
 */

const CURSORS: SeqCursors = { flow_seq: 0, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 };
function flow(id: string): FlowSummary {
  return { api_call_id: id, method: 'POST', uri: '/v1/responses', status: 'open', started_ms: 1 };
}

describe('dashboardStore — monotonic connEpoch (finding 1)', () => {
  beforeEach(() => dashboardStore.getState().reset());

  it('advances on every distinct connection transition', () => {
    const e0 = dashboardStore.getState().connEpoch;
    dashboardStore.getState().setConnection('connecting');
    const e1 = dashboardStore.getState().connEpoch;
    dashboardStore.getState().setConnection('live');
    const e2 = dashboardStore.getState().connEpoch;
    expect(e1).toBeGreaterThan(e0);
    expect(e2).toBeGreaterThan(e1);
  });

  it('does NOT advance when the same state is re-applied (no boundary crossed)', () => {
    dashboardStore.getState().setConnection('live');
    const e = dashboardStore.getState().connEpoch;
    dashboardStore.getState().setConnection('live');
    expect(dashboardStore.getState().connEpoch).toBe(e);
  });

  it('a live → seek → live round-trip yields a STRICTLY GREATER epoch (the string would repeat)', () => {
    dashboardStore.getState().setConnection('live');
    const live1 = dashboardStore.getState().connEpoch;
    dashboardStore.getState().setConnection('seeking');
    dashboardStore.getState().setConnection('live');
    const live2 = dashboardStore.getState().connEpoch;
    // The connection STRING is identical at both points…
    expect(dashboardStore.getState().connection).toBe('live');
    // …but the monotonic epoch is not — so an in-flight kill captured at live1 detects the round-trip.
    expect(live2).toBeGreaterThan(live1);
  });

  it('advances on enterSeek, applySnapshot, and reset (all boundary crossings)', () => {
    dashboardStore.getState().setConnection('live');
    let prev = dashboardStore.getState().connEpoch;

    dashboardStore.getState().enterSeek(1_700_000_000_000);
    expect(dashboardStore.getState().connEpoch).toBeGreaterThan(prev);
    prev = dashboardStore.getState().connEpoch;

    dashboardStore.getState().applySnapshot({ cursors: CURSORS, flows: [flow('a')], metrics: null, topology: null });
    expect(dashboardStore.getState().connEpoch).toBeGreaterThan(prev);
    prev = dashboardStore.getState().connEpoch;

    dashboardStore.getState().reset();
    expect(dashboardStore.getState().connEpoch).toBeGreaterThan(prev);
  });
});
