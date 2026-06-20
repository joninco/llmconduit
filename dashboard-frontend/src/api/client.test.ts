import { describe, it, expect, beforeEach, vi } from 'vitest';
import { DashboardClient, UnauthorizedError, readCsrfCookie } from './client';
import { mockFetch, mockKillLog } from './mock';

describe('DashboardClient — kill includes X-CSRF-Token', () => {
  beforeEach(() => {
    mockKillLog.length = 0;
  });

  it('attaches the CSRF token header on the kill POST (via the mock backend)', async () => {
    const client = new DashboardClient({
      fetchImpl: mockFetch,
      getCsrfToken: () => 'mock-csrf-token',
    });
    const res = await client.kill('resp_001');
    expect(res.killed).toBe(true);
    expect(mockKillLog).toHaveLength(1);
    expect(mockKillLog[0]).toEqual({ id: 'resp_001', csrf: 'mock-csrf-token' });
  });

  it('mock backend rejects a kill with no CSRF token (403)', async () => {
    const client = new DashboardClient({
      fetchImpl: mockFetch,
      getCsrfToken: () => null,
    });
    await expect(client.kill('resp_001')).rejects.toThrow(/403/);
    expect(mockKillLog[0]?.csrf).toBeNull();
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

  it('catalog() returns a BARE array (no cursor)', async () => {
    const client = new DashboardClient({ fetchImpl: mockFetch });
    const cat = await client.catalog();
    expect(Array.isArray(cat)).toBe(true);
    expect(cat[0]).toHaveProperty('context_limit');
  });
});

describe('readCsrfCookie', () => {
  it('reads the double-submit token from the non-HttpOnly cookie', () => {
    document.cookie = 'llmconduit_csrf=abc123';
    expect(readCsrfCookie()).toBe('abc123');
  });
});
