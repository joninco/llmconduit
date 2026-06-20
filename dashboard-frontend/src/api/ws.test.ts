import { describe, it, expect, beforeEach, vi } from 'vitest';
import { DashboardSocket, type WsLike } from './ws';
import { buildMonitorFrame, buildUsageFrame } from './mock';
import {
  GOLDEN_MONITOR_FRAME_JSON,
  GOLDEN_USAGE_FRAME_JSON,
  GOLDEN_FLOW_STATUS_FRAME_JSON,
  GOLDEN_METRIC_TICK_FRAME_JSON,
  GOLDEN_TOPOLOGY_FRAME_JSON,
  MALFORMED_FRAME_JSON,
} from './ws.fixtures';
import { dashboardStore } from '../store/dashboardStore';
import { isDashboardFrame, isSnapshotFrame, isDebugWsMessage } from './types';
import type { DashboardFrame, SnapshotFrame } from './types';

function snapshot(): SnapshotFrame {
  return {
    type: 'snapshot',
    cursors: { flow_seq: 0, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 },
    flows: [],
    metrics: null,
    topology: null,
  };
}

/** A controllable fake socket so tests can drive open/message/close/error. */
class FakeSocket implements WsLike {
  onopen: ((ev: unknown) => void) | null = null;
  onclose: ((ev: { code?: number } | undefined) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  closed = false;
  static instances: FakeSocket[] = [];
  constructor() {
    FakeSocket.instances.push(this);
  }
  send(): void {}
  close(): void {
    this.closed = true;
  }
}

describe('DashboardSocket — batched envelope decode + per-domain dedup', () => {
  let socket: DashboardSocket;

  beforeEach(() => {
    dashboardStore.getState().reset();
    socket = new DashboardSocket({ store: dashboardStore });
    socket.handleParsed(snapshot());
  });

  it('applies ALL sibling messages in a multi-message Monitor frame (none dropped by dedup)', () => {
    const frame = buildMonitorFrame(6, 'resp_X'); // 4 sibling DebugWsMessages under one seq
    expect(frame.batch).toHaveLength(4);

    expect(socket.applyFrame(frame)).toBe(true);
    expect(dashboardStore.getState().monitor).toHaveLength(4);
    expect(socket.getCursors().monitor).toBe(6);
  });

  it('decodes the GOLDEN nested Monitor fixture (the exact D7 bytes) → all 4 apply', () => {
    socket.handleParsed(JSON.parse(GOLDEN_MONITOR_FRAME_JSON));
    const monitor = dashboardStore.getState().monitor;
    expect(monitor).toHaveLength(4);
    // The NESTED, itself-tagged DebugWsMessage decoded correctly (no flattening).
    expect(monitor[0]?.type).toBe('request_upsert');
    const last = monitor[3];
    expect(last?.type).toBe('request_status');
    if (last?.type === 'request_status') expect(last.status).toBe('completed');
  });

  it('decodes the GOLDEN usage / flow_status / metric_tick / topology fixtures', () => {
    socket.handleParsed(JSON.parse(GOLDEN_USAGE_FRAME_JSON));
    socket.handleParsed(JSON.parse(GOLDEN_FLOW_STATUS_FRAME_JSON));
    socket.handleParsed(JSON.parse(GOLDEN_METRIC_TICK_FRAME_JSON));
    socket.handleParsed(JSON.parse(GOLDEN_TOPOLOGY_FRAME_JSON));
    const st = dashboardStore.getState();
    // Flows are keyed by api_call_id.
    expect(st.flows.get('api_001')?.status).toBe('completed');
    expect(st.metrics?.reqs_per_sec).toBe(4.2);
    expect(st.topologyNodes).toHaveLength(1);
    expect(st.topologyNodes[0]?.status).toBe('healthy');
    expect(st.topologyEdges).toHaveLength(1);
  });

  it('drops a stale frame WHOLESALE when seq <= last_seq[domain]', () => {
    socket.applyFrame(buildMonitorFrame(6));
    expect(dashboardStore.getState().monitor).toHaveLength(4);

    expect(socket.applyFrame(buildMonitorFrame(6))).toBe(false); // duplicate
    expect(dashboardStore.getState().monitor).toHaveLength(4);

    expect(socket.applyFrame(buildMonitorFrame(5))).toBe(false); // stale
    expect(dashboardStore.getState().monitor).toHaveLength(4);

    expect(socket.applyFrame(buildMonitorFrame(7))).toBe(true); // fresh
    expect(dashboardStore.getState().monitor).toHaveLength(8);
  });

  it('dedups PER DOMAIN — a stale monitor seq does not block a flow frame at the same number', () => {
    socket.applyFrame(buildMonitorFrame(6));
    const flowFrame: DashboardFrame = {
      domain: 'flow',
      seq: 1,
      batch: [{
        type: 'flow_status', api_call_id: 'api_1', status: 'open',
        model_served: 'm', upstream_target: 'u', usage: null, started_ms: 1000, elapsed_ms: 100,
      }],
    };
    expect(socket.applyFrame(flowFrame)).toBe(true);
    expect(dashboardStore.getState().flowOrder).toContain('api_1');
    expect(socket.getCursors().flow).toBe(1);
    expect(socket.getCursors().monitor).toBe(6);
  });

  it('applies a standalone usage frame to the store (finding 9)', () => {
    // Seed a flow so usage can patch it (keyed by api_call_id).
    socket.applyFrame({
      domain: 'flow', seq: 1,
      batch: [{ type: 'flow_status', api_call_id: 'api_001', status: 'open', model_served: 'm', upstream_target: 'u', usage: null, started_ms: 1000, elapsed_ms: 10 }],
    });
    expect(socket.applyFrame(buildUsageFrame(2, 'api_001'))).toBe(true);
    expect(dashboardStore.getState().flows.get('api_001')?.usage).toEqual({
      prompt: 812, completion: 512, total: 1324, cached: 128, reasoning: 16,
    });
  });
});

describe('DashboardSocket — malformed frames do NOT mutate cursor or store (finding 6)', () => {
  let socket: DashboardSocket;
  beforeEach(() => {
    dashboardStore.getState().reset();
    socket = new DashboardSocket({ store: dashboardStore });
    socket.handleParsed(snapshot());
  });

  it('a payload-invalid frame is dropped without advancing the cursor or partial-applying', () => {
    const before = socket.getCursors().flow;
    socket.handleParsed(JSON.parse(MALFORMED_FRAME_JSON)); // valid envelope, bad usage payload
    expect(socket.getCursors().flow).toBe(before); // cursor unchanged → still replayable
    expect(dashboardStore.getState().flows.size).toBe(0); // nothing applied
  });

  it('applyFrame returns false for a malformed frame and leaves the cursor intact', () => {
    expect(socket.applyFrame(JSON.parse(MALFORMED_FRAME_JSON))).toBe(false);
    expect(socket.getCursors().flow).toBe(0);
  });

  it('a non-object / garbage value is ignored', () => {
    socket.handleParsed(42);
    socket.handleParsed(null);
    socket.handleParsed({ domain: 'flow' }); // missing seq + batch
    expect(dashboardStore.getState().flows.size).toBe(0);
  });

  it('fires onFrameApplied ONLY for accepted frames', () => {
    const onFrameApplied = vi.fn();
    const s = new DashboardSocket({ store: dashboardStore, onFrameApplied });
    s.handleParsed(snapshot());
    s.applyFrame(buildMonitorFrame(6)); // accepted
    s.applyFrame(buildMonitorFrame(6)); // duplicate → dropped
    s.applyFrame(JSON.parse(MALFORMED_FRAME_JSON)); // invalid → dropped
    expect(onFrameApplied).toHaveBeenCalledTimes(1);
    expect(onFrameApplied).toHaveBeenCalledWith('monitor');
  });
});

describe('frame validation — enums, unsigned-int seq, domain↔payload compatibility (finding 5)', () => {
  const goodFlow: DashboardFrame = {
    domain: 'flow',
    seq: 1,
    batch: [{ type: 'flow_status', api_call_id: 'a', status: 'open', usage: null, started_ms: 1 }],
  };

  it('accepts a well-formed frame', () => {
    expect(isDashboardFrame(goodFlow)).toBe(true);
  });

  it('rejects a NEGATIVE seq', () => {
    expect(isDashboardFrame({ ...goodFlow, seq: -1 })).toBe(false);
  });

  it('rejects a FRACTIONAL seq', () => {
    expect(isDashboardFrame({ ...goodFlow, seq: 1.5 })).toBe(false);
  });

  it('rejects an unknown flow status string', () => {
    expect(isDashboardFrame({
      domain: 'flow', seq: 1,
      batch: [{ type: 'flow_status', api_call_id: 'a', status: 'streaming', usage: null, started_ms: 1 }],
    })).toBe(false);
  });

  it('rejects a metric_tick payload under the FLOW domain (domain↔payload mismatch)', () => {
    expect(isDashboardFrame({
      domain: 'flow', seq: 1,
      batch: [{ type: 'metric_tick', reqs_per_sec: 1, active_streams: 1, error_pct: 0, p50: 1, p95: 1, p99: 1, tokens_per_sec: 1, cost_per_min: 0, windows: { m1: win(), m5: win(), h1: win() } }],
    })).toBe(false);
  });

  it('rejects a flow_status payload under the TOPOLOGY domain', () => {
    expect(isDashboardFrame({
      domain: 'topology', seq: 1,
      batch: [{ type: 'flow_status', api_call_id: 'a', status: 'open', usage: null, started_ms: 1 }],
    })).toBe(false);
  });

  it('rejects an unknown provider status in a topology node', () => {
    expect(isDashboardFrame({
      domain: 'topology', seq: 1,
      batch: [{ type: 'topology_update', nodes: [{ id: 'x', name: 'x', status: 'degraded', served_count: 0, failover_count: 0, consecutive_failures: 0, catalog_size: 0 }], edges: [] }],
    })).toBe(false);
  });

  function win() {
    return { reqs_per_sec: 1, active_streams: 1, error_pct: 0, p50: 1, p95: 1, p99: 1, tokens_per_sec: 1, cost_per_min: 0 };
  }
});

describe('snapshot validation — full shape before applying (finding 4)', () => {
  it('accepts a fully-valid snapshot', () => {
    expect(isSnapshotFrame({
      type: 'snapshot',
      cursors: { flow_seq: 0, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 },
      flows: [{ api_call_id: 'a', method: 'POST', uri: '/v1/responses', status: 'open', started_ms: 1 }],
      metrics: null, topology: null,
    })).toBe(true);
  });

  it('rejects a snapshot whose cursors are not all unsigned ints', () => {
    expect(isSnapshotFrame({
      type: 'snapshot',
      cursors: { flow_seq: -1, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 },
      flows: [], metrics: null, topology: null,
    })).toBe(false);
  });

  it('rejects a snapshot with an invalid summary (bad status)', () => {
    expect(isSnapshotFrame({
      type: 'snapshot',
      cursors: { flow_seq: 0, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 },
      flows: [{ api_call_id: 'a', method: 'POST', uri: '/x', status: 'bogus', started_ms: 1 }],
      metrics: null, topology: null,
    })).toBe(false);
  });
});

describe('ProviderHealth + price_table validation (findings 2 + 4)', () => {
  const node = (over: Record<string, unknown> = {}) => ({
    id: 'p', name: 'p', route: null, base_url: 'http://x', status: 'healthy',
    cooling_until_ms: null, last_error: null, served_count: 0, failover_count: 0,
    consecutive_failures: 0, catalog_fetched_ms: null, catalog_size: 0, ...over,
  });
  const topoFrame = (nodes: unknown[]) => ({ domain: 'topology', seq: 1, batch: [{ type: 'topology_update', nodes, edges: [] }] });

  it('accepts a complete D4 ProviderHealth (nullable keys present-but-null)', () => {
    expect(isDashboardFrame(topoFrame([node()]))).toBe(true);
  });

  it('rejects a node MISSING base_url (required non-null) — finding 2', () => {
    const n = node();
    delete (n as Record<string, unknown>).base_url;
    expect(isDashboardFrame(topoFrame([n]))).toBe(false);
  });

  it('rejects a node with null base_url (must be non-null) — finding 2', () => {
    expect(isDashboardFrame(topoFrame([node({ base_url: null })]))).toBe(false);
  });

  it('rejects a node MISSING a required nullable key (cooling_until_ms absent) — finding 2', () => {
    const n = node();
    delete (n as Record<string, unknown>).cooling_until_ms;
    expect(isDashboardFrame(topoFrame([n]))).toBe(false);
  });

  it('accepts a topology snapshot whose price_table entries are complete ModelPrice', () => {
    expect(isSnapshotFrame({
      type: 'snapshot',
      cursors: { flow_seq: 0, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 },
      flows: [],
      metrics: null,
      topology: { topology_seq: 1, nodes: [node()], edges: [], price_table: { 'gpt-4o': { input_per_1k: 0.005, output_per_1k: 0.015, cached_per_1k: 0.0025 } } },
    })).toBe(true);
  });

  it('rejects a price_table entry with a non-finite number (finding 4)', () => {
    expect(isSnapshotFrame({
      type: 'snapshot',
      cursors: { flow_seq: 0, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 },
      flows: [],
      metrics: null,
      topology: { topology_seq: 1, nodes: [node()], edges: [], price_table: { m: { input_per_1k: Number.POSITIVE_INFINITY, output_per_1k: 0, cached_per_1k: 0 } } },
    })).toBe(false);
  });

  it('rejects a price_table entry missing a field (finding 4)', () => {
    expect(isSnapshotFrame({
      type: 'snapshot',
      cursors: { flow_seq: 0, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 },
      flows: [],
      metrics: null,
      topology: { topology_seq: 1, nodes: [node()], edges: [], price_table: { m: { input_per_1k: 0.001, output_per_1k: 0.001 } } },
    })).toBe(false);
  });
});

describe('DebugWsMessage nested validation — full DTO, no skipped casts (findings 1 + 3)', () => {
  const fullRequest = {
    response_id: 'r', model: 'm', started_at_ms: 1, updated_at_ms: 1, completed_at_ms: null,
    status: 'running',
    stats: { input_items: 0, tool_count: 0, turn_count: 0, user_messages: 0, assistant_messages: 0, system_messages: 0, developer_messages: 0, reasoning_items: 0, function_calls: 0, function_outputs: 0, tool_items: 0, input_chars: 0, instructions_chars: 0 },
    error: null,
  };

  it('accepts a complete request_upsert (full DebugRequest + stats)', () => {
    expect(isDebugWsMessage({ type: 'request_upsert', request: fullRequest })).toBe(true);
  });

  it('rejects request_upsert whose request is MISSING stats (finding 3)', () => {
    const req = { ...fullRequest } as Record<string, unknown>;
    delete req.stats;
    expect(isDebugWsMessage({ type: 'request_upsert', request: req })).toBe(false);
  });

  it('rejects request_upsert with an incomplete stats object (finding 3)', () => {
    expect(isDebugWsMessage({ type: 'request_upsert', request: { ...fullRequest, stats: { input_items: 1 } } })).toBe(false);
  });

  it('rejects a bad DebugRequest status enum (finding 3)', () => {
    expect(isDebugWsMessage({ type: 'request_upsert', request: { ...fullRequest, status: 'pending' } })).toBe(false);
  });

  it('accepts a complete segment_append; rejects a bad segment kind (finding 3)', () => {
    expect(isDebugWsMessage({ type: 'segment_append', response_id: 'r', segment: { timestamp_ms: 1, kind: 'output', text: 'hi' } })).toBe(true);
    expect(isDebugWsMessage({ type: 'segment_append', response_id: 'r', segment: { timestamp_ms: 1, kind: 'banana', text: 'hi' } })).toBe(false);
  });

  it('validates event_append images fully (rejects a malformed image) — finding 3', () => {
    const okEvent = { timestamp_ms: 1, kind: 'k', summary: 's', payload_preview: null, images: [{ id: 'i', label: 'l', path: 'p', mime_type: 'image/png', size_bytes: 10 }] };
    expect(isDebugWsMessage({ type: 'event_append', response_id: 'r', event: okEvent })).toBe(true);
    const badImg = { ...okEvent, images: [{ id: 'i' /* missing label/path/mime */ }] };
    expect(isDebugWsMessage({ type: 'event_append', response_id: 'r', event: badImg })).toBe(false);
  });

  it('rejects a request_status with a bad status enum (finding 3)', () => {
    expect(isDebugWsMessage({ type: 'request_status', response_id: 'r', status: 'idle', completed_at_ms: null, error: null })).toBe(false);
  });
});

describe('DashboardSocket — auth failure vs transient blip + reconnect (findings 3+4+7)', () => {
  /** Captured pending reconnect timers so a test can fire them synchronously. */
  let pendingTimers: Array<() => void>;
  function makeTimers() {
    pendingTimers = [];
    const setTimer = (cb: () => void) => {
      pendingTimers.push(cb);
      return pendingTimers.length as unknown as ReturnType<typeof setTimeout>;
    };
    const clearTimer = () => {};
    return { setTimer, clearTimer };
  }
  function flushTimers() {
    const due = pendingTimers;
    pendingTimers = [];
    for (const cb of due) cb();
  }

  beforeEach(() => {
    dashboardStore.getState().reset();
    FakeSocket.instances = [];
  });

  it('an EXPLICIT 4401 close bounces to login (no reconnect)', () => {
    const onUnauthorized = vi.fn();
    const { setTimer, clearTimer } = makeTimers();
    const socket = new DashboardSocket({ store: dashboardStore, factory: () => new FakeSocket(), onUnauthorized, setTimer, clearTimer });
    socket.connect();
    FakeSocket.instances[0]!.onclose?.({ code: 4401 });
    expect(onUnauthorized).toHaveBeenCalledOnce();
    expect(pendingTimers).toHaveLength(0); // no reconnect scheduled for a confirmed auth failure
  });

  it('a 1006 blip with a STILL-VALID session reconnects — does NOT log out (finding 7)', async () => {
    const onUnauthorized = vi.fn();
    const probeAuth = vi.fn().mockResolvedValue(true); // session still valid
    const { setTimer, clearTimer } = makeTimers();
    const socket = new DashboardSocket({ store: dashboardStore, factory: () => new FakeSocket(), onUnauthorized, probeAuth, setTimer, clearTimer });
    socket.connect();
    FakeSocket.instances[0]!.onclose?.({ code: 1006 });
    // The probe runs; await its resolution.
    await Promise.resolve();
    await Promise.resolve();
    expect(probeAuth).toHaveBeenCalledOnce();
    expect(onUnauthorized).not.toHaveBeenCalled(); // NOT logged out
    expect(pendingTimers.length).toBeGreaterThan(0); // a reconnect was scheduled
    // Firing the reconnect timer opens a fresh socket.
    flushTimers();
    expect(FakeSocket.instances).toHaveLength(2);
  });

  it('a 1006 blip whose probe returns 401 bounces to login (finding 7)', async () => {
    const onUnauthorized = vi.fn();
    const probeAuth = vi.fn().mockResolvedValue(false); // probe got a 401
    const { setTimer, clearTimer } = makeTimers();
    const socket = new DashboardSocket({ store: dashboardStore, factory: () => new FakeSocket(), onUnauthorized, probeAuth, setTimer, clearTimer });
    socket.connect();
    FakeSocket.instances[0]!.onclose?.({ code: 1006 });
    await Promise.resolve();
    await Promise.resolve();
    expect(probeAuth).toHaveBeenCalledOnce();
    expect(onUnauthorized).toHaveBeenCalledOnce(); // bounced
    expect(pendingTimers).toHaveLength(0); // no reconnect after a confirmed auth failure
  });

  it('an onerror with a valid-session probe reconnects (not logout)', async () => {
    const onUnauthorized = vi.fn();
    const probeAuth = vi.fn().mockResolvedValue(true);
    const { setTimer, clearTimer } = makeTimers();
    const socket = new DashboardSocket({ store: dashboardStore, factory: () => new FakeSocket(), onUnauthorized, probeAuth, setTimer, clearTimer });
    socket.connect();
    FakeSocket.instances[0]!.onerror?.(new Event('error'));
    await Promise.resolve();
    await Promise.resolve();
    expect(onUnauthorized).not.toHaveBeenCalled();
    expect(pendingTimers.length).toBeGreaterThan(0);
  });

  it('a clean close (code 1000) does NOT bounce or reconnect', () => {
    const onUnauthorized = vi.fn();
    const { setTimer, clearTimer } = makeTimers();
    const socket = new DashboardSocket({ store: dashboardStore, factory: () => new FakeSocket(), onUnauthorized, setTimer, clearTimer });
    socket.connect();
    FakeSocket.instances[0]!.onclose?.({ code: 1000 });
    expect(onUnauthorized).not.toHaveBeenCalled();
    expect(dashboardStore.getState().connection).toBe('closed');
    expect(pendingTimers).toHaveLength(0);
  });

  it('disconnect() cancels a pending reconnect (no socket re-open)', async () => {
    const probeAuth = vi.fn().mockResolvedValue(true);
    const { setTimer, clearTimer } = makeTimers();
    const socket = new DashboardSocket({ store: dashboardStore, factory: () => new FakeSocket(), probeAuth, setTimer, clearTimer });
    socket.connect();
    FakeSocket.instances[0]!.onclose?.({ code: 1006 });
    await Promise.resolve();
    await Promise.resolve();
    expect(pendingTimers.length).toBeGreaterThan(0);
    socket.disconnect(); // cancels the reconnect
    flushTimers(); // even if a stale timer fires, stopped guards it
    expect(FakeSocket.instances).toHaveLength(1); // no new socket opened
  });

  it('StrictMode reconnect: an OLD socket late close does NOT clobber the live socket (finding 4)', () => {
    const onUnauthorized = vi.fn();
    const { setTimer, clearTimer } = makeTimers();
    const socket = new DashboardSocket({ store: dashboardStore, factory: () => new FakeSocket(), onUnauthorized, setTimer, clearTimer });
    // mount → unmount → remount (StrictMode dev double-invoke).
    socket.connect();
    const oldWs = FakeSocket.instances[0]!;
    // Capture the OLD close handler BEFORE disconnect detaches it, to simulate a close
    // event the browser had already queued for the stale socket.
    const staleOnClose = oldWs.onclose;
    socket.disconnect();
    socket.connect();
    const newWs = FakeSocket.instances[1]!;
    expect(oldWs).not.toBe(newWs);

    // The captured OLD handler fires a late abnormal close AFTER the new socket is live.
    staleOnClose?.({ code: 1006 });

    // The generation guard makes the stale event a no-op: no bounce, no reconnect, no clobber.
    expect(onUnauthorized).not.toHaveBeenCalled();
    expect(pendingTimers).toHaveLength(0);
    // The new socket still drives state — prove it by applying a snapshot through it.
    newWs.onmessage?.({ data: JSON.stringify(snapshot()) });
    expect(dashboardStore.getState().connection).toBe('live');
  });

  it('a STALE probe resolving after disconnect/remount does NOT bounce a newer connection (finding 5)', async () => {
    const onUnauthorized = vi.fn();
    // A probe we resolve MANUALLY, after the connection has been torn down + remounted.
    let resolveProbe!: (authed: boolean) => void;
    const probeAuth = vi.fn().mockImplementation(() => new Promise<boolean>((res) => { resolveProbe = res; }));
    const { setTimer, clearTimer } = makeTimers();
    const socket = new DashboardSocket({ store: dashboardStore, factory: () => new FakeSocket(), onUnauthorized, probeAuth, setTimer, clearTimer });
    socket.connect();
    // Drop → probe starts (bound to this generation), pending.
    FakeSocket.instances[0]!.onclose?.({ code: 1006 });
    expect(probeAuth).toHaveBeenCalledOnce();
    // Disconnect + reconnect bumps the generation while the probe is still pending.
    socket.disconnect();
    socket.connect();
    // NOW the stale probe resolves with a 401-equivalent.
    resolveProbe(false);
    await Promise.resolve();
    await Promise.resolve();
    // The stale probe must be ignored: the NEWER connection is NOT logged out.
    expect(onUnauthorized).not.toHaveBeenCalled();
  });
});

describe('DashboardSocket — time travel (seek/live shadow buffer)', () => {
  let socket: DashboardSocket;
  beforeEach(() => {
    dashboardStore.getState().reset();
    socket = new DashboardSocket({ store: dashboardStore });
    socket.handleParsed(snapshot());
  });

  it('buffers frames while seeking and replays them on live()', () => {
    socket.seek();
    expect(socket.isPaused()).toBe(true);

    socket.handleParsed(buildMonitorFrame(6));
    socket.handleParsed(buildMonitorFrame(7));
    expect(dashboardStore.getState().monitor).toHaveLength(0);
    expect(socket.shadowBufferLength()).toBe(2);

    socket.live();
    expect(socket.isPaused()).toBe(false);
    expect(dashboardStore.getState().monitor).toHaveLength(8);
    expect(socket.shadowBufferLength()).toBe(0);
  });

  it('a RECONNECT snapshot arriving during seek does NOT clobber the frozen cut or flip to live (finding 6)', () => {
    // Freeze: apply a monitor frame, then seek. The store + connection are the frozen cut.
    socket.applyFrame(buildMonitorFrame(6));
    expect(dashboardStore.getState().monitor).toHaveLength(4);
    socket.seek();
    expect(dashboardStore.getState().connection).toBe('seeking');

    // A reconnect delivers a FRESH snapshot (different cut: empty flows, new cursors).
    const reconnectSnap: SnapshotFrame = {
      type: 'snapshot',
      cursors: { flow_seq: 99, metrics_seq: 99, topology_seq: 99, monitor_seq: 99 },
      flows: [], metrics: null, topology: null,
    };
    socket.handleParsed(reconnectSnap);

    // The frozen cut is INTACT: monitor still 4, connection still seeking, cursors unchanged.
    expect(dashboardStore.getState().monitor).toHaveLength(4);
    expect(dashboardStore.getState().connection).toBe('seeking');
    expect(socket.getCursors().monitor).toBe(6);

    // Explicit resume applies the staged snapshot (new cut) + flips to live.
    socket.live();
    expect(dashboardStore.getState().connection).toBe('live');
    expect(socket.getCursors().monitor).toBe(99);
  });
});
