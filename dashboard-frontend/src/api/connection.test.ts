import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { getConnection, resetConnection, queryKeys } from './connection';
import { mockKillLog, buildMonitorFrame } from './mock';
import { authStore } from '../store/authStore';
import { dashboardStore } from '../store/dashboardStore';

function clearCsrfCookie(): void {
  document.cookie = 'llmconduit_csrf=; expires=Thu, 01 Jan 1970 00:00:00 GMT';
}

describe('connection — CSRF resolved dynamically (cookie-first) on kill (finding 2)', () => {
  beforeEach(() => {
    resetConnection();
    mockKillLog.length = 0;
    clearCsrfCookie();
    authStore.getState().setCsrfToken(null);
  });
  afterEach(() => {
    resetConnection();
    clearCsrfCookie();
  });

  it('a token issued AFTER boot (fresh login cookie) reaches the kill POST', async () => {
    const { client } = getConnection();
    // Simulate a fresh login setting the double-submit cookie AFTER the connection booted.
    document.cookie = 'llmconduit_csrf=fresh-login-token';
    const res = await client.kill('resp_001');
    expect(res.killed).toBe(true);
    // The kill carried the COOKIE token, not a stale bootstrap value.
    expect(mockKillLog.at(-1)?.csrf).toBe('fresh-login-token');
  });

  it('falls back to the auth-store token when no cookie is present', async () => {
    const { client } = getConnection();
    authStore.getState().setCsrfToken('store-token');
    const res = await client.kill('resp_002');
    expect(res.killed).toBe(true);
    expect(mockKillLog.at(-1)?.csrf).toBe('store-token');
  });
});

describe('connection — WS-driven REST invalidation (finding 10)', () => {
  beforeEach(() => {
    resetConnection();
    dashboardStore.getState().reset();
  });
  afterEach(() => resetConnection());

  it('an ACCEPTED flow frame invalidates the flows query', () => {
    const { socket, queryClient } = getConnection();
    const spy = vi.spyOn(queryClient, 'invalidateQueries');
    // Prime snapshot so live frames apply.
    socket.handleParsed({
      type: 'snapshot',
      cursors: { flow_seq: 0, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 },
      flows: [], metrics: null, topology: null,
    });
    socket.applyFrame({
      domain: 'flow', seq: 1,
      batch: [{ type: 'flow_status', response_id: 'r1', status: 'streaming', served_model: 'm', upstream_target: 'u', usage: null, elapsed_ms: 5 }],
    });
    expect(spy).toHaveBeenCalledWith({ queryKey: queryKeys.flows });
  });

  it('a DROPPED (duplicate) frame does NOT invalidate', () => {
    const { socket, queryClient } = getConnection();
    socket.handleParsed({
      type: 'snapshot',
      cursors: { flow_seq: 5, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 },
      flows: [], metrics: null, topology: null,
    });
    const spy = vi.spyOn(queryClient, 'invalidateQueries');
    // seq 5 <= cursor 5 → dropped, no invalidation.
    socket.applyFrame({
      domain: 'flow', seq: 5,
      batch: [{ type: 'flow_status', response_id: 'r1', status: 'streaming', served_model: 'm', upstream_target: 'u', usage: null, elapsed_ms: 5 }],
    });
    expect(spy).not.toHaveBeenCalled();
  });

  it('a monitor frame does NOT invalidate any REST query (no mirror)', () => {
    const { socket, queryClient } = getConnection();
    socket.handleParsed({
      type: 'snapshot',
      cursors: { flow_seq: 0, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 },
      flows: [], metrics: null, topology: null,
    });
    const spy = vi.spyOn(queryClient, 'invalidateQueries');
    socket.applyFrame(buildMonitorFrame(6));
    expect(spy).not.toHaveBeenCalled();
  });
});
