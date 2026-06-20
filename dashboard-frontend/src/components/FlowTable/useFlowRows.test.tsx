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

describe('useFlowRows — a WS-created row backfills REST-authoritative fields (finding 3)', () => {
  beforeEach(() => resetWorld());
  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it('fills endpoint/method/uri + finished/elapsed/cost/terminal from REST, keeping live status/usage', async () => {
    // The REST row is the authoritative request line + roll-up.
    stubFlowsFetch([
      makeFlow({
        api_call_id: 'api_ws',
        method: 'POST',
        uri: '/v1/responses',
        upstream_target: 'openai',
        status: 'completed',
        finished_ms: 1_700_000_005_000,
        elapsed_ms: 5_000,
        cost: 0.5,
        terminal_reason: 'response.completed',
      }),
    ]);
    // A WS-CREATED row: minted by a `flow_status` patch BEFORE the REST list arrived, so it carries
    // PLACEHOLDER method ('POST') + uri ('') and null roll-up fields, with a live 'open' status.
    act(() =>
      dashboardStore.getState().patchFlowStatus({
        type: 'flow_status',
        api_call_id: 'api_ws',
        response_id: null,
        status: 'open',
        model_requested: null,
        model_served: null,
        upstream_target: null,
        usage: { prompt: 11, completion: 0, total: 11, cached: 0, reasoning: 0 },
        started_ms: 1_700_000_000_000,
        elapsed_ms: null,
      }),
    );
    // Sanity: the WS placeholder row really has an empty uri before REST merges.
    expect(dashboardStore.getState().flows.get('api_ws')?.uri).toBe('');

    const { result } = renderRows();
    await waitFor(() => {
      const r = result.current.rows.find((row) => row.api_call_id === 'api_ws');
      expect(r?.uri).toBe('/v1/responses');
    });
    const row = result.current.rows.find((r) => r.api_call_id === 'api_ws');
    // REST-authoritative request line + roll-up are backfilled…
    expect(row?.method).toBe('POST');
    expect(row?.uri).toBe('/v1/responses');
    expect(row?.upstream_target).toBe('openai');
    expect(row?.finished_ms).toBe(1_700_000_005_000);
    expect(row?.elapsed_ms).toBe(5_000);
    expect(row?.cost).toBe(0.5);
    expect(row?.terminal_reason).toBe('response.completed');
    // …while the LIVE status + usage still win over the completed REST row.
    expect(row?.status).toBe('open');
    expect(row?.usage?.prompt).toBe(11);
    // The store itself is untouched (merge is view-only).
    expect(dashboardStore.getState().flows.get('api_ws')?.uri).toBe('');
  });
});

describe('useFlowRows — combined union is globally newest-on-top (finding 4)', () => {
  beforeEach(() => resetWorld());
  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it('sorts a newer REST-only row ABOVE an older live row (not just appended after)', async () => {
    // The live store holds an OLDER flow; the REST list seeds a NEWER, store-unseen flow. Appending
    // REST rows after live ones would sort the newer REST row BELOW the older live one — the global
    // started_ms-desc sort must place the newer REST row on top.
    stubFlowsFetch([makeFlow({ api_call_id: 'api_rest_new', status: 'completed', started_ms: 1_700_000_500_000 })]);
    seedFlows([makeFlow({ api_call_id: 'api_live_old', status: 'open', started_ms: 1_700_000_000_000 })]);

    const { result } = renderRows();
    await waitFor(() => expect(result.current.rows.some((r) => r.api_call_id === 'api_rest_new')).toBe(true));
    const ids = result.current.rows.map((r) => r.api_call_id);
    // Newer REST row is first; the older live row follows — newest-on-top across BOTH sources.
    expect(ids).toEqual(['api_rest_new', 'api_live_old']);
  });

  it('orders multiple live rows by started_ms desc regardless of store insertion order', async () => {
    // Two live rows whose store order is oldest-first; the global sort must still surface newest-on-top.
    stubFlowsFetch([]);
    seedFlows([
      makeFlow({ api_call_id: 'api_a_old', status: 'completed', started_ms: 1_700_000_000_000 }),
      makeFlow({ api_call_id: 'api_b_new', status: 'completed', started_ms: 1_700_000_900_000 }),
    ]);
    const { result } = renderRows();
    await waitFor(() => expect(globalThis.fetch).toHaveBeenCalled());
    const ids = result.current.rows.map((r) => r.api_call_id);
    expect(ids).toEqual(['api_b_new', 'api_a_old']);
  });
});

describe('useFlowRows — time-travel seek shows ONLY the frozen snapshot (finding 1)', () => {
  beforeEach(() => resetWorld());
  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it('does not leak post-seek REST flows into the frozen snapshot rows', async () => {
    // The `/flows` REST list carries a flow that started AFTER the seeked instant ("the future").
    stubFlowsFetch([makeFlow({ api_call_id: 'api_future', status: 'open', started_ms: 1_700_000_999_999 })]);
    // The store holds the FROZEN snapshot the scrubber paused on (one historical flow).
    seedFlows([makeFlow({ api_call_id: 'api_snapshot', status: 'completed', started_ms: 1_700_000_000_000 })]);
    // Enter seek (D11 paused) BEFORE render — the live REST merge must be suppressed.
    act(() => dashboardStore.getState().setConnection('seeking'));

    const { result } = renderRows();
    // Give any (suppressed) fetch a tick to (not) resolve into the merge.
    await act(async () => { await Promise.resolve(); });

    const ids = result.current.rows.map((r) => r.api_call_id);
    expect(ids).toContain('api_snapshot'); // the frozen snapshot row renders
    expect(ids).not.toContain('api_future'); // the post-seek REST flow does NOT leak in
    expect(result.current.total).toBe(1); // only the snapshot row is counted
  });

  it('resumes merging the REST list once back LIVE', async () => {
    stubFlowsFetch([makeFlow({ api_call_id: 'api_future', status: 'open', started_ms: 1_700_000_999_999 })]);
    seedFlows([makeFlow({ api_call_id: 'api_snapshot', status: 'completed', started_ms: 1_700_000_000_000 })]);
    act(() => dashboardStore.getState().setConnection('seeking'));
    const { result } = renderRows();
    await act(async () => { await Promise.resolve(); });
    expect(result.current.rows.map((r) => r.api_call_id)).not.toContain('api_future');

    // Leave seek → live: the REST query enables, fires, and its rows re-join the merge.
    act(() => dashboardStore.getState().setConnection('live'));
    await waitFor(() => expect(result.current.rows.some((r) => r.api_call_id === 'api_future')).toBe(true));
  });
});
