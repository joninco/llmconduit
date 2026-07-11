import { describe, it, expect, beforeEach, vi } from 'vitest';
import { DashboardClient, UnauthorizedError, readCsrfCookie } from './client';
import { mockFetch, mockKillLog } from './mock';
import { assertDashboardSchemaVersion, DashboardSchemaMismatchError } from './schemaVersion';
import { DashboardContractError } from './validation';

describe('DashboardClient — kill includes X-CSRF-Token', () => {
  beforeEach(() => {
    mockKillLog.length = 0;
  });

  it('attaches the CSRF token header on the kill POST (via the mock backend)', async () => {
    const client = new DashboardClient({
      fetchImpl: mockFetch,
      getCsrfToken: () => 'mock-csrf-token',
    });
    // `:id` == api_call_id (D13 contract).
    const res = await client.kill('api_001');
    expect(res.killed).toBe(true);
    expect(mockKillLog).toHaveLength(1);
    expect(mockKillLog[0]).toEqual({ id: 'api_001', csrf: 'mock-csrf-token' });
  });

  it('mock backend rejects a kill with no CSRF token (403)', async () => {
    const client = new DashboardClient({
      fetchImpl: mockFetch,
      getCsrfToken: () => null,
    });
    await expect(client.kill('api_001')).rejects.toThrow(/403/);
    expect(mockKillLog[0]?.csrf).toBeNull();
  });

  it('mock backend 404s a kill for an unknown api_call_id (finding 7)', async () => {
    const client = new DashboardClient({
      fetchImpl: mockFetch,
      getCsrfToken: () => 'mock-csrf-token',
    });
    // A response_id is NOT a valid kill key — `:id` must be api_call_id.
    await expect(client.kill('resp_001')).rejects.toThrow(/404/);
  });
});

describe('DashboardClient — 401 bounce-to-login', () => {
  it('fires onUnauthorized and throws UnauthorizedError on any 401', async () => {
    const onUnauthorized = vi.fn();
    const fetch401: typeof globalThis.fetch = async () =>
      new Response('nope', { status: 401 });
    const client = new DashboardClient({
      fetchImpl: fetch401,
      onUnauthorized,
    });
    await expect(client.flows()).rejects.toBeInstanceOf(UnauthorizedError);
    expect(onUnauthorized).toHaveBeenCalledOnce();
  });
});

describe('DashboardClient — typed reads against the D13 shapes (mock)', () => {
  it('flows() returns the cursor-bearing FlowsResponse', async () => {
    const client = new DashboardClient({ fetchImpl: mockFetch });
    const res = await client.flows();
    expect(typeof res.flow_seq).toBe('number');
    expect(Array.isArray(res.flows)).toBe(true);
  });

  it('catalog() returns a BARE array (no cursor) with a NULLABLE context_limit', async () => {
    const client = new DashboardClient({ fetchImpl: mockFetch });
    const cat = await client.catalog();
    expect(Array.isArray(cat)).toBe(true);
    const first = cat[0];
    expect(first).toBeDefined();
    expect(first).toHaveProperty('context_limit');
    // gap 06: a real window surfaces as a number...
    expect(typeof first?.context_limit).toBe('number');
    // ...and a model with no advertised window surfaces as `null` (unavailable),
    // NEVER a non-null `0` (the lie-with-zeros the gap removed).
    const unavailable = cat.find((e) => e.id === 'mystery-model');
    expect(unavailable).toBeDefined();
    expect(unavailable?.context_limit ?? null).toBeNull();
    expect(unavailable?.context_limit).not.toBe(0);
  });

  it('overview() validates the generated root and serializes window, scope, and historical at', async () => {
    const seen: string[] = [];
    const fetchImpl: typeof fetch = async (input, init) => {
      seen.push(typeof input === 'string' ? input : input instanceof URL ? input.toString() : input.url);
      return mockFetch(input, init);
    };
    const client = new DashboardClient({ fetchImpl });
    const res = await client.overview({
      window: 'm5', at: 1_700_000_000_000, status: 'failed', model: 'llama',
      upstream: 'vllm-b', client: 'key-a',
    });

    const url = new URL(seen[0]!, 'http://dashboard.test');
    expect(url.pathname).toBe('/dashboard/api/overview');
    expect(Object.fromEntries(url.searchParams)).toEqual({
      window: 'm5', at: '1700000000000', status: 'failed', model: 'llama',
      upstream: 'vllm-b', client: 'key-a',
    });
    expect(res.scope.window).toBe('m5');
    expect(res.scope.requested_at_ms).toBe(1_700_000_000_000);
    expect(res.provider_attempts_global.scope).toBe('global');
  });
});

describe('DashboardClient — fatal root contracts', () => {
  beforeEach(() => sessionStorage.clear());

  it('reports a repeated REST schema mismatch to the shell callback', async () => {
    // Prime the first-mismatch marker without actually navigating; the next same-source mismatch
    // models the response after the requested hard reload.
    expect(() => assertDashboardSchemaVersion(1, 'REST /metrics', () => {})).toThrow();
    const onFatal = vi.fn();
    const client = new DashboardClient({
      onFatal,
      fetchImpl: async () => new Response('{}', {
        status: 200,
        headers: { 'X-LLMConduit-Dashboard-Schema': '1' },
      }),
    });

    await expect(client.metrics()).rejects.toBeInstanceOf(DashboardSchemaMismatchError);
    expect(onFatal).toHaveBeenCalledOnce();
    expect(onFatal.mock.calls[0]?.[0].message).toContain('upgrade required');
  });

  it('reports a structurally invalid REST root instead of degrading it to empty data', async () => {
    const onFatal = vi.fn();
    const client = new DashboardClient({
      onFatal,
      fetchImpl: async () => new Response('{}', {
        status: 200,
        headers: { 'X-LLMConduit-Dashboard-Schema': '5' },
      }),
    });

    await expect(client.metrics()).rejects.toBeInstanceOf(DashboardContractError);
    expect(onFatal).toHaveBeenCalledOnce();
    expect(onFatal.mock.calls[0]?.[0].message).toContain('contract validation failed');
  });

  it('rejects a malformed Overview root before it can enter the query cache', async () => {
    const onFatal = vi.fn();
    const client = new DashboardClient({
      onFatal,
      fetchImpl: async () => new Response(JSON.stringify({ totals: { requests: 'not-a-number' } }), {
        status: 200,
        headers: { 'X-LLMConduit-Dashboard-Schema': '5' },
      }),
    });

    await expect(client.overview({ window: 'm1' })).rejects.toBeInstanceOf(DashboardContractError);
    expect(onFatal).toHaveBeenCalledOnce();
  });
});

describe('readCsrfCookie', () => {
  it('reads the double-submit token from the non-HttpOnly cookie', () => {
    document.cookie = 'llmconduit_csrf=abc123';
    expect(readCsrfCookie()).toBe('abc123');
  });
});
