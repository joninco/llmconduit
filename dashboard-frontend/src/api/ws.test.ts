import { describe, it, expect, beforeEach } from 'vitest';
import { DashboardSocket } from './ws';
import { buildMonitorFrame } from './mock';
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

describe('DashboardSocket — batched envelope decode + per-domain dedup', () => {
  let socket: DashboardSocket;

  beforeEach(() => {
    dashboardStore.getState().reset();
    socket = new DashboardSocket({ store: dashboardStore });
    // Prime with a snapshot so live frames apply (snapshot-then-live).
    socket.handleMessage(snapshot());
  });

  it('applies ALL sibling messages in a multi-message Monitor frame (none dropped by dedup)', () => {
    const frame = buildMonitorFrame(6, 'resp_X'); // 4 sibling DebugWsMessages under one seq
    expect(frame.batch).toHaveLength(4);

    const applied = socket.applyFrame(frame);
    expect(applied).toBe(true);

    // The whole batch landed — every sibling pushed to the monitor ring.
    expect(dashboardStore.getState().monitor).toHaveLength(4);
    expect(socket.getCursors().monitor).toBe(6);
  });

  it('drops a stale frame WHOLESALE when seq <= last_seq[domain]', () => {
    socket.applyFrame(buildMonitorFrame(6)); // advance monitor cursor to 6
    expect(dashboardStore.getState().monitor).toHaveLength(4);

    // A frame at seq 6 (== cursor) is a duplicate → entire batch dropped.
    const dup = buildMonitorFrame(6);
    expect(socket.applyFrame(dup)).toBe(false);
    expect(dashboardStore.getState().monitor).toHaveLength(4); // unchanged

    // A frame at seq 5 (< cursor) is stale → dropped.
    const stale = buildMonitorFrame(5);
    expect(socket.applyFrame(stale)).toBe(false);
    expect(dashboardStore.getState().monitor).toHaveLength(4);

    // A fresh frame at seq 7 processes.
    expect(socket.applyFrame(buildMonitorFrame(7))).toBe(true);
    expect(dashboardStore.getState().monitor).toHaveLength(8);
  });

  it('dedups PER DOMAIN — a stale monitor seq does not block a flow frame at the same number', () => {
    socket.applyFrame(buildMonitorFrame(6)); // monitor cursor = 6
    const flowFrame: DashboardFrame = {
      domain: 'flow',
      seq: 1, // flow cursor is still 0 → applies despite < monitor cursor
      batch: [{
        type: 'flow_status',
        response_id: 'resp_1',
        status: 'streaming',
        served_model: 'm',
        upstream_target: 'u',
        usage: null,
        elapsed_ms: 100,
      }],
    };
    expect(socket.applyFrame(flowFrame)).toBe(true);
    expect(dashboardStore.getState().flowOrder).toContain('resp_1');
    expect(socket.getCursors().flow).toBe(1);
    expect(socket.getCursors().monitor).toBe(6);
  });

  it('decodes a raw JSON string frame via handleMessage', () => {
    const frame = buildMonitorFrame(6);
    socket.handleMessage(JSON.parse(JSON.stringify(frame)) as DashboardFrame);
    expect(dashboardStore.getState().monitor).toHaveLength(4);
  });
});

describe('DashboardSocket — time travel (seek/live shadow buffer)', () => {
  let socket: DashboardSocket;
  beforeEach(() => {
    dashboardStore.getState().reset();
    socket = new DashboardSocket({ store: dashboardStore });
    socket.handleMessage(snapshot());
  });

  it('buffers frames while seeking and replays them on live()', () => {
    socket.seek();
    expect(socket.isPaused()).toBe(true);

    socket.handleMessage(buildMonitorFrame(6));
    socket.handleMessage(buildMonitorFrame(7));
    // Paused: nothing applied yet, frames buffered.
    expect(dashboardStore.getState().monitor).toHaveLength(0);
    expect(socket.shadowBufferLength()).toBe(2);

    socket.live();
    expect(socket.isPaused()).toBe(false);
    // Replayed in order: 4 + 4 siblings.
    expect(dashboardStore.getState().monitor).toHaveLength(8);
    expect(socket.shadowBufferLength()).toBe(0);
  });
});
