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

  it('decodes the GOLDEN flattened Monitor fixture (the exact D7 bytes) → all 4 apply', () => {
    socket.handleParsed(JSON.parse(GOLDEN_MONITOR_FRAME_JSON));
    const monitor = dashboardStore.getState().monitor;
    expect(monitor).toHaveLength(4);
    // The flattened DebugWsMessage fields decoded correctly (no nested `message`).
    expect(monitor[0]?.kind).toBe('request.normalized');
    expect(monitor[3]?.payload).toEqual({ text: ', world' });
  });

  it('decodes the GOLDEN usage / flow_status / metric_tick / topology fixtures', () => {
    socket.handleParsed(JSON.parse(GOLDEN_USAGE_FRAME_JSON));
    socket.handleParsed(JSON.parse(GOLDEN_FLOW_STATUS_FRAME_JSON));
    socket.handleParsed(JSON.parse(GOLDEN_METRIC_TICK_FRAME_JSON));
    socket.handleParsed(JSON.parse(GOLDEN_TOPOLOGY_FRAME_JSON));
    const st = dashboardStore.getState();
    expect(st.flows.get('resp_001')?.status).toBe('completed');
    expect(st.metrics?.reqs_per_sec).toBe(4.2);
    expect(st.topologyNodes).toHaveLength(1);
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
        type: 'flow_status', response_id: 'resp_1', status: 'streaming',
        served_model: 'm', upstream_target: 'u', usage: null, elapsed_ms: 100,
      }],
    };
    expect(socket.applyFrame(flowFrame)).toBe(true);
    expect(dashboardStore.getState().flowOrder).toContain('resp_1');
    expect(socket.getCursors().flow).toBe(1);
    expect(socket.getCursors().monitor).toBe(6);
  });

  it('applies a standalone usage frame to the store (finding 9)', () => {
    // Seed a flow so usage can patch it.
    socket.applyFrame({
      domain: 'flow', seq: 1,
      batch: [{ type: 'flow_status', response_id: 'resp_001', status: 'streaming', served_model: 'm', upstream_target: 'u', usage: null, elapsed_ms: 10 }],
    });
    expect(socket.applyFrame(buildUsageFrame(2, 'resp_001'))).toBe(true);
    expect(dashboardStore.getState().flows.get('resp_001')?.usage).toEqual({
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

describe('DashboardSocket — auth failure + reconnect safety (findings 3 + 4)', () => {
  beforeEach(() => {
    dashboardStore.getState().reset();
    FakeSocket.instances = [];
  });

  it('an ABNORMAL close (upgrade-rejected, code 1006) bounces to login', () => {
    const onUnauthorized = vi.fn();
    const socket = new DashboardSocket({ store: dashboardStore, factory: () => new FakeSocket(), onUnauthorized });
    socket.connect();
    const ws = FakeSocket.instances[0]!;
    ws.onclose?.({ code: 1006 }); // abnormal — handshake rejected / dropped
    expect(onUnauthorized).toHaveBeenCalledOnce();
    expect(dashboardStore.getState().connection).toBe('error');
  });

  it('an explicit 4401 close bounces to login', () => {
    const onUnauthorized = vi.fn();
    const socket = new DashboardSocket({ store: dashboardStore, factory: () => new FakeSocket(), onUnauthorized });
    socket.connect();
    FakeSocket.instances[0]!.onclose?.({ code: 4401 });
    expect(onUnauthorized).toHaveBeenCalledOnce();
  });

  it('an onerror (handshake failure with no close) bounces to login', () => {
    const onUnauthorized = vi.fn();
    const socket = new DashboardSocket({ store: dashboardStore, factory: () => new FakeSocket(), onUnauthorized });
    socket.connect();
    FakeSocket.instances[0]!.onerror?.(new Event('error'));
    expect(onUnauthorized).toHaveBeenCalledOnce();
  });

  it('a clean close (code 1000) does NOT bounce to login', () => {
    const onUnauthorized = vi.fn();
    const socket = new DashboardSocket({ store: dashboardStore, factory: () => new FakeSocket(), onUnauthorized });
    socket.connect();
    FakeSocket.instances[0]!.onclose?.({ code: 1000 });
    expect(onUnauthorized).not.toHaveBeenCalled();
    expect(dashboardStore.getState().connection).toBe('closed');
  });

  it('StrictMode reconnect: an OLD socket late close does NOT clobber the live socket (finding 4)', () => {
    const onUnauthorized = vi.fn();
    const socket = new DashboardSocket({ store: dashboardStore, factory: () => new FakeSocket(), onUnauthorized });
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

    // The generation guard makes the stale event a no-op: no bounce, no state clobber.
    expect(onUnauthorized).not.toHaveBeenCalled();
    // The new socket still drives state — prove it by applying a snapshot through it.
    newWs.onmessage?.({ data: JSON.stringify(snapshot()) });
    expect(dashboardStore.getState().connection).toBe('live');
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
});
