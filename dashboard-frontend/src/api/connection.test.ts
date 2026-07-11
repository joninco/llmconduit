import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { getConnection, resetConnection, teardownSession, queryKeys } from './connection';
import { DashboardClient } from './client';
import { mockKillLog, buildMonitorFrame } from './mock';
import { authStore } from '../store/authStore';
import { dashboardStore } from '../store/dashboardStore';
import type { FlowStatusPayload } from './types';
import { flowFilterStore } from '../store/flowFilterStore';
import { assertDashboardSchemaVersion } from './schemaVersion';
import { DashboardContractError } from './validation';

function flowPayload(over: Partial<FlowStatusPayload> = {}): FlowStatusPayload {
  return {
    type: 'flow_status',
    phase: 'progress',
    revision: 1,
    api_call_id: 'api_r1',
    method: 'POST',
    uri: '/v1/responses',
    status: 'open',
    model_served: 'm',
    upstream_target: 'u',
    usage: null,
    normalized_usage: null,
    usage_anomaly_count: 0,
    started_ms: 1000,
    elapsed_ms: 5,
    cost: null,
    cost_confidence: 'unavailable',
    ...over,
  };
}

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
    const res = await client.kill('api_001');
    expect(res.killed).toBe(true);
    // The kill carried the COOKIE token, not a stale bootstrap value.
    expect(mockKillLog.at(-1)?.csrf).toBe('fresh-login-token');
  });

  it('falls back to the auth-store token when no cookie is present', async () => {
    const { client } = getConnection();
    authStore.getState().setCsrfToken('store-token');
    const res = await client.kill('api_002');
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

  it('open/progress and standalone usage patch the row without invalidating list or detail queries', () => {
    const { socket, queryClient } = getConnection();
    const spy = vi.spyOn(queryClient, 'invalidateQueries');
    // Prime snapshot so live frames apply.
    socket.handleParsed({
      type: 'snapshot',
      schema_version: 3,
      cursors: { flow_seq: 0, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 },
      flows: [], metrics: null, topology: null,
    });
    socket.applyFrame({
      domain: 'flow', seq: 1,
      batch: [flowPayload()],
    });
    socket.applyFrame({
      domain: 'flow', seq: 2,
      batch: [{ type: 'usage', api_call_id: 'api_r1', prompt: 1, completion: 2, total: 3 }],
    });
    expect(spy).not.toHaveBeenCalled();
  });

  it('a terminal row invalidates exactly that flow detail, never the flow-list family', () => {
    const { socket, queryClient } = getConnection();
    const spy = vi.spyOn(queryClient, 'invalidateQueries');
    socket.handleParsed({
      type: 'snapshot',
      schema_version: 3,
      cursors: { flow_seq: 0, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 },
      flows: [], metrics: null, topology: null,
    });
    socket.applyFrame({
      domain: 'flow', seq: 1,
      batch: [flowPayload({ phase: 'terminal', revision: 2, status: 'completed' })],
    });
    expect(spy).toHaveBeenCalledTimes(1);
    expect(spy).toHaveBeenCalledWith({ queryKey: queryKeys.flowDetail('api_r1'), exact: true });
    expect(spy).not.toHaveBeenCalledWith({ queryKey: queryKeys.flows });
  });

  it('a metrics frame invalidates metrics, exact Overview cuts, and topology provider health', () => {
    const { socket, queryClient } = getConnection();
    socket.handleParsed({
      type: 'snapshot',
      schema_version: 3,
      cursors: { flow_seq: 0, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 },
      flows: [], metrics: null, topology: null,
    });
    const spy = vi.spyOn(queryClient, 'invalidateQueries');
    // A minimal valid `metric_tick` (the per-domain validator requires the full shape).
    const w = {
      window_seconds: 60, observed_seconds: 60, warm: true,
      accepted_requests: 1,
      accepted_per_sec: 1, active_streams_now: 0, failure_pct: 0, p50_ms: 10, p95_ms: 20, p99_ms: 30,
      terminal_requests: 1, terminal_per_sec: 1, successes: 1, failures: 0,
      cancellations: 0, cancellation_pct: 0,
      quantile_method: 'log_histogram_nearest_rank' as const, max_relative_error: 0.062,
      latency_overflow_count: 0, latency_quality: 'measured' as const, usage_anomaly_count: 0,
      reported_tokens_per_sec: 5, cost_per_min: 0, latency_samples: 1, usage_samples: 1, priced_samples: 1,
      cost_confidence: 'estimated' as const,
    };
    socket.applyFrame({
      domain: 'metrics', seq: 1,
      batch: [{ type: 'metric_tick', generated_at_ms: 1000, headline_window: 'm1', windows: { m1: w, m5: w, h1: w } }],
    });
    // One publisher tick advances the global strip, every mounted exact Overview scope, and the
    // compatibility /topology provider join together.
    expect(spy).toHaveBeenCalledWith({ queryKey: queryKeys.metrics });
    expect(spy).toHaveBeenCalledWith({ queryKey: queryKeys.overviewRoot });
    expect(spy).toHaveBeenCalledWith({ queryKey: queryKeys.topology });
  });

  it('a DROPPED (duplicate) frame does NOT invalidate', () => {
    const { socket, queryClient } = getConnection();
    socket.handleParsed({
      type: 'snapshot',
      schema_version: 3,
      cursors: { flow_seq: 5, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 },
      flows: [], metrics: null, topology: null,
    });
    const spy = vi.spyOn(queryClient, 'invalidateQueries');
    // seq 5 <= cursor 5 → dropped, no invalidation.
    socket.applyFrame({
      domain: 'flow', seq: 5,
      batch: [flowPayload()],
    });
    expect(spy).not.toHaveBeenCalled();
  });

  it('a monitor frame does NOT invalidate any REST query (no mirror)', () => {
    const { socket, queryClient } = getConnection();
    socket.handleParsed({
      type: 'snapshot',
      schema_version: 3,
      cursors: { flow_seq: 0, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 },
      flows: [], metrics: null, topology: null,
    });
    const spy = vi.spyOn(queryClient, 'invalidateQueries');
    socket.applyFrame(buildMonitorFrame(6));
    expect(spy).not.toHaveBeenCalled();
  });
});

describe('teardownSession — clears cache + resets stores + disconnects WS (finding 1)', () => {
  beforeEach(() => resetConnection());
  afterEach(() => resetConnection());

  it('clears the query cache, resets both stores, and disconnects the socket', () => {
    const { socket, queryClient } = getConnection();
    // Seed session-scoped state: a cached query, live store data, and auth secrets.
    queryClient.setQueryData(queryKeys.flows, { flows: [], total: 0, flow_seq: 1 });
    socket.handleParsed({
      type: 'snapshot',
      schema_version: 3,
      cursors: { flow_seq: 1, metrics_seq: 0, topology_seq: 0, monitor_seq: 5 },
      flows: [], metrics: null, topology: null,
    });
    socket.applyFrame(buildMonitorFrame(6));
    authStore.getState().setAuthenticated(true);
    authStore.getState().setCsrfToken('secret-token');
    authStore.getState().setMutationsEnabled(true);
    flowFilterStore.getState().setFilters({ status: 'failed', model: 'm', upstream: 'u', client: 'c' });
    window.location.hash = '#/flows/api_secret?window=h1&status=failed&client=c';
    const clearSpy = vi.spyOn(queryClient, 'clear');
    const disconnectSpy = vi.spyOn(socket, 'disconnect');

    teardownSession();

    // REST cache cleared (no leaked bodies/usage across sessions).
    expect(clearSpy).toHaveBeenCalledOnce();
    expect(queryClient.getQueryData(queryKeys.flows)).toBeUndefined();
    // WS disconnected.
    expect(disconnectSpy).toHaveBeenCalledOnce();
    // Live store reset.
    expect(dashboardStore.getState().monitor).toHaveLength(0);
    expect(dashboardStore.getState().flows.size).toBe(0);
    expect(dashboardStore.getState().cursors.monitor_seq).toBe(0);
    // Auth store fully reset (token + mutation flag cleared, not just `authenticated`).
    expect(authStore.getState().authenticated).toBe(false);
    expect(authStore.getState().csrfToken).toBeNull();
    expect(authStore.getState().mutationsEnabled).toBe(false);
    expect(flowFilterStore.getState().filters).toEqual({ status: null, model: null, upstream: null, client: null });
    expect(window.location.hash).toBe('#/overview');
  });

  it('a real 401 from a client read routes through teardownSession (wired onUnauthorized)', async () => {
    // Build a client wired with the SAME onUnauthorized the connection uses
    // (teardownSession), against a fetch that 401s — then assert teardown ran.
    const { queryClient } = getConnection();
    queryClient.setQueryData(queryKeys.metrics, { metrics_seq: 1 });
    authStore.getState().setAuthenticated(true);
    authStore.getState().setCsrfToken('secret');
    flowFilterStore.getState().setModel('private-model');
    window.location.hash = '#/flows/private-id?model=private-model';
    const fetch401: typeof globalThis.fetch = async () => new Response('no', { status: 401 });
    const client = new DashboardClient({ fetchImpl: fetch401, onUnauthorized: teardownSession });
    await expect(client.metrics()).rejects.toBeTruthy(); // 401 → UnauthorizedError
    // The 401 fired onUnauthorized === teardownSession: cache cleared, auth reset.
    expect(queryClient.getQueryData(queryKeys.metrics)).toBeUndefined();
    expect(authStore.getState().authenticated).toBe(false);
    expect(authStore.getState().csrfToken).toBeNull();
    expect(flowFilterStore.getState().filters).toEqual({ status: null, model: null, upstream: null, client: null });
    expect(window.location.hash).toBe('#/overview');
  });
});

describe('connection — fatal REST roots surface through dashboardStore', () => {
  beforeEach(() => {
    resetConnection();
    dashboardStore.getState().reset();
    sessionStorage.clear();
    window.__LLMCONDUIT_DASHBOARD__ = {
      authenticated: true,
      csrf_token: 'csrf',
      mutations_enabled: false,
      schema_version: 3,
    };
  });
  afterEach(() => {
    resetConnection();
    delete window.__LLMCONDUIT_DASHBOARD__;
    vi.unstubAllGlobals();
    sessionStorage.clear();
  });

  it('turns a REST contract failure into the explicit fatal shell state', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => new Response('{}', {
      status: 200,
      headers: { 'X-LLMConduit-Dashboard-Schema': '3' },
    })));
    const { client } = getConnection();

    await expect(client.metrics()).rejects.toBeInstanceOf(DashboardContractError);
    expect(dashboardStore.getState().connection).toBe('error');
    expect(dashboardStore.getState().fatalError).toContain('contract validation failed');
  });

  it('turns a repeated same-source schema mismatch into the explicit upgrade state', async () => {
    expect(() => assertDashboardSchemaVersion(1, 'REST /metrics', () => {})).toThrow();
    vi.stubGlobal('fetch', vi.fn(async () => new Response('{}', {
      status: 200,
      headers: { 'X-LLMConduit-Dashboard-Schema': '1' },
    })));
    const { client } = getConnection();

    await expect(client.metrics()).rejects.toThrow(/upgrade required/);
    expect(dashboardStore.getState().fatalError).toContain('upgrade required');
  });
});
