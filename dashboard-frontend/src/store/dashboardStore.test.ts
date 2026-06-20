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

  it('advances on enterSeek, applySnapshot, applySeekCut, and reset (all boundary crossings)', () => {
    dashboardStore.getState().setConnection('live');
    let prev = dashboardStore.getState().connEpoch;

    dashboardStore.getState().enterSeek(1_700_000_000_000);
    expect(dashboardStore.getState().connEpoch).toBeGreaterThan(prev);
    prev = dashboardStore.getState().connEpoch;

    dashboardStore.getState().applySnapshot({ cursors: CURSORS, flows: [flow('a')], metrics: null, topology: null });
    expect(dashboardStore.getState().connEpoch).toBeGreaterThan(prev);
    prev = dashboardStore.getState().connEpoch;

    dashboardStore.getState().applySeekCut({ rows: [flow('b')], cursors: CURSORS, atMs: 1, monitorSeq: 0, metrics: null, topology: null });
    expect(dashboardStore.getState().connEpoch).toBeGreaterThan(prev);
    prev = dashboardStore.getState().connEpoch;

    dashboardStore.getState().reset();
    expect(dashboardStore.getState().connEpoch).toBeGreaterThan(prev);
  });
});

/**
 * D11 finding 1 — the seek cut installs ATOMICALLY: the store must NEVER be observed in a state
 * where `connection==='seeking'` but the rows/cursors are still the LIVE (pre-cut) ones. A single
 * `set` in `applySeekCut` guarantees this; these lock it so a future refactor can't reintroduce the
 * non-atomic `enter seeking → fetch → install rows` window that let D10 render live data as frozen.
 */
describe('dashboardStore — atomic seek cut (finding 1)', () => {
  beforeEach(() => dashboardStore.getState().reset());

  it('never exposes `seeking` with the live (pre-cut) rows', () => {
    // Establish a LIVE store with a distinct live row + live monitor cursor.
    dashboardStore.getState().applySnapshot({ cursors: CURSORS, flows: [flow('live-row')], metrics: null, topology: null });
    dashboardStore.getState().setConnection('live');
    dashboardStore.getState().setCursor('monitor_seq', 99); // a LIVE cursor that kept advancing

    // Invariant checker on EVERY store transition: if we ever read `seeking`, the rows + monitor cut
    // must already be the FROZEN ones (the live-row / live cursor must be gone).
    const violations: string[] = [];
    const unsub = dashboardStore.subscribe((s) => {
      if (s.connection === 'seeking') {
        if (s.flows.has('live-row')) violations.push('seeking with live row still present');
        if (s.seekMonitorSeq === 99) violations.push('seeking with the live monitor cursor (99) as the cut');
        if (s.seekAtMs === null) violations.push('seeking with a null seekAtMs');
      }
    });

    // The atomic install: frozen rows + cursors + cut, AND connection='seeking', in ONE update.
    dashboardStore.getState().applySeekCut({
      rows: [flow('frozen-row')],
      cursors: { ...CURSORS, monitor_seq: 5 },
      atMs: 1_700_000_000_000,
      monitorSeq: 5,
      metrics: null,
      topology: null,
    });
    unsub();

    expect(violations).toEqual([]);
    const st = dashboardStore.getState();
    expect(st.connection).toBe('seeking');
    expect(st.flows.has('frozen-row')).toBe(true);
    expect(st.flows.has('live-row')).toBe(false);
    expect(st.seekMonitorSeq).toBe(5);
    expect(st.seekAtMs).toBe(1_700_000_000_000);
  });
});
