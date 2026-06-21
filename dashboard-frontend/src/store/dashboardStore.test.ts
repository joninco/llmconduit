import { describe, it, expect, beforeEach } from 'vitest';
import { dashboardStore } from './dashboardStore';
import type { FlowSummary, ProviderHealth, SeqCursors, TopologyResponse } from '../api/types';

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
function node(id: string): ProviderHealth {
  return {
    id, name: id, route: null, base_url: 'http://x', status: 'healthy', cooling_until_ms: null,
    last_error: null, served_count: 0, failover_count: 0, consecutive_failures: 0,
    catalog_fetched_ms: null, catalog_size: 0,
  };
}
function topology(seq: number, nodeIds: string[], price: Record<string, number> = {}): TopologyResponse {
  return {
    topology_seq: seq,
    nodes: nodeIds.map(node),
    edges: [],
    price_table: Object.fromEntries(
      Object.entries(price).map(([m, p]) => [m, { input_per_1k: p, output_per_1k: p, cached_per_1k: p }]),
    ),
  };
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

/**
 * Finding 6 — `seedTopology` reconciles the REST `/topology` read by `topology_seq`. A cached/late
 * REST response (stale seq) must NOT clobber the newer WS `topology_update` nodes/edges; only the
 * price table (which WS frames never carry) is refreshed from a stale response. A fresh-or-equal seq
 * applies fully and advances the cursor (monotonically).
 */
describe('dashboardStore — seedTopology seq reconciliation (finding 6)', () => {
  beforeEach(() => dashboardStore.getState().reset());

  it('applies nodes/edges + advances the cursor when the REST seq is >= current', () => {
    dashboardStore.getState().seedTopology(topology(3, ['vllm-a'], { 'gpt-4o': 0.01 }));
    expect(dashboardStore.getState().topologyNodes.map((n) => n.id)).toEqual(['vllm-a']);
    expect(dashboardStore.getState().cursors.topology_seq).toBe(3);
    expect(dashboardStore.getState().priceTable['gpt-4o']?.input_per_1k).toBe(0.01);
  });

  it('a STALE cached REST response does NOT overwrite newer WS nodes/edges — only the price table', () => {
    // A newer WS topology_update already applied: nodes = [vllm-a, vllm-b] at seq 5.
    dashboardStore.getState().setCursor('topology_seq', 5);
    dashboardStore.getState().setTopology([node('vllm-a'), node('vllm-b')], []);

    // A stale cached REST read (seq 3, only one node) resolves AFTER. It must not clobber the WS
    // nodes; it should only refresh the price table.
    dashboardStore.getState().seedTopology(topology(3, ['only-stale'], { 'gpt-4o': 0.02 }));

    const st = dashboardStore.getState();
    expect(st.topologyNodes.map((n) => n.id)).toEqual(['vllm-a', 'vllm-b']); // WS nodes preserved
    expect(st.cursors.topology_seq).toBe(5); // cursor not moved backwards
    expect(st.priceTable['gpt-4o']?.input_per_1k).toBe(0.02); // price table refreshed from REST
  });

  it('a fresher REST response (seq > current) replaces nodes/edges and advances the cursor', () => {
    dashboardStore.getState().setCursor('topology_seq', 5);
    dashboardStore.getState().setTopology([node('vllm-a')], []);

    dashboardStore.getState().seedTopology(topology(8, ['vllm-a', 'vllm-b', 'groq'], { m: 0.03 }));

    const st = dashboardStore.getState();
    expect(st.topologyNodes.map((n) => n.id)).toEqual(['vllm-a', 'vllm-b', 'groq']);
    expect(st.cursors.topology_seq).toBe(8);
    expect(st.priceTable['m']?.input_per_1k).toBe(0.03);
  });
});
