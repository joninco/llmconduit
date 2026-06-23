import { describe, it, expect, beforeEach } from 'vitest';
import { dashboardStore } from './dashboardStore';
import type { Attempt, FlowStatusPayload, FlowSummary, ProviderHealth, SeqCursors, TopologyResponse } from '../api/types';

/**
 * The MONOTONIC connection-transition generation (`connEpoch`) underpins the kill mutation's
 * boundary guard (useFlowDetail, finding 1). It must advance on EVERY real boundary crossing and
 * NEVER repeat — unlike the reusable `connection` STRING, which returns to `'live'` after a
 * `live → seek → live` round-trip. These lock that invariant at the store level.
 */

const CURSORS: SeqCursors = { flow_seq: 0, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 };
function flow(id: string): FlowSummary {
  return { api_call_id: id, method: 'POST', uri: '/v1/responses', status: 'open', started_ms: 1, cost_confidence: 'unavailable' };
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
      Object.entries(price).map(([m, p]) => [m, { input_per_1k: p, output_per_1k: p, cached_per_1k: p, cached_price_configured: true }]),
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

/**
 * Gap 10b (review round 1, finding 1) — `patchFlowStatus` must THREAD the projected spine fields
 * (gap-02 phase epochs + gap-03 `attempts`/`first_upstream_byte_ms`) off the live `flow_status`
 * frame onto the store row, so the measured latency waterfall + attempt trace light up for a LIVE
 * flow. A later frame that OMITS a field must NOT erase an earlier-known value (progressive frames),
 * and an absent phase must stay ABSENT (never a fabricated `0`).
 */
describe('dashboardStore — patchFlowStatus threads projected spine fields (gap 10b finding 1)', () => {
  beforeEach(() => dashboardStore.getState().reset());

  const SERVED: Attempt = {
    provider: 'openai',
    model: 'gpt-4o',
    start_ms: 1_700_000_000_100,
    end_ms: 1_700_000_000_400,
    first_upstream_byte_ms: 1_700_000_000_350,
    status: 'served',
  };

  /** A `flow_status` frame carrying the spine fields (omitted keys fall back to defaults below). */
  function frame(over: Partial<FlowStatusPayload> = {}): FlowStatusPayload {
    return {
      type: 'flow_status',
      api_call_id: 'api_spine',
      response_id: null,
      status: 'open',
      model_requested: null,
      model_served: null,
      upstream_target: null,
      usage: null,
      started_ms: 1_700_000_000_000,
      elapsed_ms: null,
      ...over,
    };
  }

  it('lands phases / attempts / first_upstream_byte_ms from a flow_status frame into the store', () => {
    dashboardStore.getState().patchFlowStatus(
      frame({
        ingress_ms: 1_700_000_000_000,
        normalization_done_ms: 1_700_000_000_050,
        routing_decision_ms: 1_700_000_000_090,
        first_content_delta_ms: 1_700_000_000_500,
        stream_end_ms: 1_700_000_001_000,
        finalize_ms: 1_700_000_001_100,
        attempts: [SERVED],
        first_upstream_byte_ms: 1_700_000_000_350,
      }),
    );

    const row = dashboardStore.getState().flows.get('api_spine');
    expect(row?.ingress_ms).toBe(1_700_000_000_000);
    expect(row?.normalization_done_ms).toBe(1_700_000_000_050);
    expect(row?.routing_decision_ms).toBe(1_700_000_000_090);
    expect(row?.first_content_delta_ms).toBe(1_700_000_000_500);
    expect(row?.stream_end_ms).toBe(1_700_000_001_000);
    expect(row?.finalize_ms).toBe(1_700_000_001_100);
    expect(row?.first_upstream_byte_ms).toBe(1_700_000_000_350);
    expect(row?.attempts).toEqual([SERVED]);
  });

  it('a later frame OMITTING a field KEEPS the prior known value (progressive frames)', () => {
    // Frame 1 establishes the early phases + the attempt trace.
    dashboardStore.getState().patchFlowStatus(
      frame({
        ingress_ms: 1_700_000_000_000,
        routing_decision_ms: 1_700_000_000_090,
        first_content_delta_ms: 1_700_000_000_500,
        attempts: [SERVED],
        first_upstream_byte_ms: 1_700_000_000_350,
      }),
    );
    // Frame 2 is the terminal frame: it adds `stream_end_ms`/`finalize_ms` but OMITS the earlier
    // phase epochs + attempts (a real progressive stream does not re-send every field each frame).
    dashboardStore.getState().patchFlowStatus(
      frame({
        status: 'completed',
        stream_end_ms: 1_700_000_001_000,
        finalize_ms: 1_700_000_001_100,
        elapsed_ms: 1_100,
      }),
    );

    const row = dashboardStore.getState().flows.get('api_spine');
    // The new terminal fields landed…
    expect(row?.status).toBe('completed');
    expect(row?.stream_end_ms).toBe(1_700_000_001_000);
    expect(row?.finalize_ms).toBe(1_700_000_001_100);
    // …and the earlier-known fields were NOT erased by the omitting frame.
    expect(row?.ingress_ms).toBe(1_700_000_000_000);
    expect(row?.routing_decision_ms).toBe(1_700_000_000_090);
    expect(row?.first_content_delta_ms).toBe(1_700_000_000_500);
    expect(row?.first_upstream_byte_ms).toBe(1_700_000_000_350);
    expect(row?.attempts).toEqual([SERVED]);
  });

  it('an absent phase stays ABSENT (never a fabricated 0)', () => {
    dashboardStore.getState().patchFlowStatus(frame({ ingress_ms: 1_700_000_000_000 }));
    const row = dashboardStore.getState().flows.get('api_spine');
    // The unmeasured phases are absent/undefined — NOT `0` (the honesty invariant).
    expect(row?.first_content_delta_ms ?? null).toBeNull();
    expect(row?.finalize_ms ?? null).toBeNull();
    expect(row?.first_upstream_byte_ms ?? null).toBeNull();
    expect(row?.first_content_delta_ms).not.toBe(0);
    expect(row?.attempts ?? null).toBeNull();
  });
});
