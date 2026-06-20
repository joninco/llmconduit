import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import type { ReactNode } from 'react';
import { QueryClientProvider } from '@tanstack/react-query';
import { renderHook, waitFor, act, cleanup } from '@testing-library/react';
import { useFlowRows } from './useFlowRows';
import { EMPTY_FILTERS } from './filterTypes';
import { getConnection } from '../../api/connection';
import { dashboardStore } from '../../store/dashboardStore';
import { makeFlow, resetWorld, seedFlows } from '../testHarness';
import type { FlowSummary, FlowsResponse } from '../../api/types';

/**
 * useFlowRows merges the live WS store with the `/flows` REST list. These lock two contracts the
 * D10 review flagged: the REST query is the PRODUCTION data source (must run against a real
 * backend — finding 2), and a live row must RETAIN the REST roll-up fields it does not carry
 * (cost / terminal_reason — finding 5).
 */

/** Render `useFlowRows` inside the connection's QueryClient (built fresh per `getConnection`). */
function renderRows() {
  const { queryClient } = getConnection();
  const wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
  );
  return renderHook(() => useFlowRows(EMPTY_FILTERS), { wrapper });
}

/** A `fetch` stub answering ONLY `/flows` with the given list; everything else 404s. */
function stubFlowsFetch(flows: FlowSummary[]): void {
  const body: FlowsResponse = { flows, total: flows.length, flow_seq: 1 };
  vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL) => {
    const url = typeof input === 'string' ? input : input.toString();
    if (url.includes('/flows')) {
      return new Response(JSON.stringify(body), { status: 200, headers: { 'Content-Type': 'application/json' } });
    }
    return new Response('{}', { status: 404 });
  }));
}

describe('useFlowRows — REST query enabled for the real backend (finding 2)', () => {
  beforeEach(() => resetWorld()); // real (non-mock) bootstrap → exercises the production path
  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it('runs the `/flows` query against a real backend and seeds REST-only rows', async () => {
    // The store is EMPTY; the only way a row appears is if the REST query actually fired.
    stubFlowsFetch([makeFlow({ api_call_id: 'api_rest_only', status: 'completed', cost: 0.5 })]);
    const { result } = renderRows();
    await waitFor(() => expect(result.current.rows.some((r) => r.api_call_id === 'api_rest_only')).toBe(true));
    expect(globalThis.fetch).toHaveBeenCalled();
  });
});

describe('useFlowRows — live row retains REST roll-up fields (finding 5)', () => {
  beforeEach(() => resetWorld());
  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it('keeps the REST cost / terminal_reason when the live row lacks them', async () => {
    // REST row carries the server roll-up; the live store row (same id) does NOT (a flow_status
    // patch defaults cost/terminal_reason to null until a frame carries them).
    stubFlowsFetch([
      makeFlow({ api_call_id: 'api_live', status: 'completed', cost: 0.42, terminal_reason: 'response.completed' }),
    ]);
    seedFlows([makeFlow({ api_call_id: 'api_live', status: 'open', cost: null, terminal_reason: null })]);

    const { result } = renderRows();
    // Once the REST list resolves, the merged row exposes the server roll-up…
    await waitFor(() => {
      const row = result.current.rows.find((r) => r.api_call_id === 'api_live');
      expect(row?.cost).toBe(0.42);
    });
    const row = result.current.rows.find((r) => r.api_call_id === 'api_live');
    // …while the live status still WINS over the REST row.
    expect(row?.status).toBe('open');
    expect(row?.terminal_reason).toBe('response.completed');
  });

  it('a live cost is NOT overwritten by the REST roll-up (live wins when present)', async () => {
    stubFlowsFetch([makeFlow({ api_call_id: 'api_live2', status: 'completed', cost: 0.42 })]);
    seedFlows([makeFlow({ api_call_id: 'api_live2', status: 'open', cost: 0.99 })]);
    const { result } = renderRows();
    await waitFor(() => expect(globalThis.fetch).toHaveBeenCalled());
    // Let the REST data settle into the merge.
    await act(async () => { await Promise.resolve(); });
    const row = dashboardStore.getState().flows.get('api_live2');
    expect(row?.cost).toBe(0.99); // store untouched
    const merged = result.current.rows.find((r) => r.api_call_id === 'api_live2');
    expect(merged?.cost).toBe(0.99); // live roll-up preferred over REST
  });
});
