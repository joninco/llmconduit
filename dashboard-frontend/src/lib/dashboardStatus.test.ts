import { describe, expect, it } from 'vitest';
import { deriveDashboardStatus, METRICS_STALE_AFTER_MS } from './dashboardStatus';

const NOW = 1_000_000;

function status(over: Partial<Parameters<typeof deriveDashboardStatus>[0]> = {}) {
  return deriveDashboardStatus({
    connection: 'live',
    hasDashboardData: true,
    generatedAtMs: NOW - 2_000,
    activeStreams: 0,
    lastActivityAtMs: NOW - 300_000,
    nowMs: NOW,
    ...over,
  });
}

describe('dashboard status model', () => {
  it('keeps a connected idle dashboard distinct from a disconnect', () => {
    const model = status();
    expect(model.connection.label).toBe('Connected');
    expect(model.freshness.label).toBe('Fresh');
    expect(model.traffic.label).toBe('Idle');
    expect(model.traffic.detail).toBe('Last request 5m ago.');
  });

  it('describes connected, fresh, active traffic', () => {
    const model = status({ activeStreams: 3, lastActivityAtMs: NOW });
    expect(model.connection.label).toBe('Connected');
    expect(model.freshness.label).toBe('Fresh');
    expect(model.traffic.label).toBe('Active');
    expect(model.traffic.detail).toContain('3 requests');
  });

  it('marks stale metrics independently of connection and activity', () => {
    const model = status({ generatedAtMs: NOW - METRICS_STALE_AFTER_MS - 1 });
    expect(model.connection.label).toBe('Connected');
    expect(model.freshness.label).toBe('Stale');
    expect(model.traffic.label).toBe('Idle');
  });

  it('distinguishes reconnecting from disconnected', () => {
    expect(status({ connection: 'connecting' }).connection.label).toBe('Reconnecting');
    expect(status({ connection: 'connecting', hasDashboardData: false }).connection.label).toBe('Connecting');
    expect(status({ connection: 'error' }).connection.label).toBe('Disconnected');
  });
});
