import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import type { ReactNode } from 'react';
import { QueryClientProvider } from '@tanstack/react-query';
import { renderHook, waitFor, act, cleanup } from '@testing-library/react';
import { useFlowRows } from './useFlowRows';
import { EMPTY_FILTERS } from './filterTypes';
import type { FlowFilters } from './filterTypes';
import { getConnection } from '../../api/connection';
import { dashboardStore } from '../../store/dashboardStore';
import { makeFlow, resetWorld, seedFlows } from '../testHarness';
import type { FlowListSummaryResponse, FlowSummary, FlowsResponse } from '../../api/types';

/**
 * useFlowRows merges the live WS store with the `/flows` REST list. These lock two contracts the
 * D10 review flagged: the REST query is the PRODUCTION data source (must run against a real
 * backend — finding 2), and a live row must RETAIN the REST roll-up fields it does not carry
 * (cost / terminal_reason — finding 5).
 */

/** Render `useFlowRows` inside the connection's QueryClient (built fresh per `getConnection`). */
function renderRows(filters: FlowFilters = EMPTY_FILTERS, searchQuery = '') {
  const { queryClient } = getConnection();
  const wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
  );
  return renderHook(() => useFlowRows(filters, searchQuery), { wrapper });
}

/** A `fetch` stub answering ONLY `/flows` with the given list; everything else 404s. */
function stubFlowsFetch(flows: FlowSummary[], flowSeq = 0): void {
  const body: FlowsResponse = {
    flows, total: flows.length, flow_seq: flowSeq, as_of_event_id: flowSeq,
    generated_at_ms: Date.now(),
  };
  const summary: FlowListSummaryResponse = {
    total: flows.length,
    models: [], upstreams: [], clients: [], unattributed: 0, statuses: [], failures: [],
    context: { measurable: 0, near_limit: 0, over_limit: 0, peak_pct: null },
    as_of_event_id: flowSeq, generated_at_ms: Date.now(),
  };
  vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL) => {
    const url = typeof input === 'string' ? input : input.toString();
    if (url.includes('/flows')) {
      return new Response(JSON.stringify(url.includes('/flows/summary') ? summary : body), {
        status: 200,
        headers: {
          'Content-Type': 'application/json',
          'X-LLMConduit-Dashboard-Schema': '6',
        },
      });
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

  it('loads stable keyset pages and reports loaded / matching totals', async () => {
    const first = [
      makeFlow({ api_call_id: 'api_page_3', started_ms: 3_000 }),
      makeFlow({ api_call_id: 'api_page_2', started_ms: 2_000 }),
    ];
    const second = [makeFlow({ api_call_id: 'api_page_1', started_ms: 1_000 })];
    const summary: FlowListSummaryResponse = {
      total: 3,
      models: [], upstreams: [], clients: [], unattributed: 3, statuses: [], failures: [],
      context: { measurable: 0, near_limit: 0, over_limit: 0, peak_pct: null },
      as_of_event_id: 7, generated_at_ms: 3_000,
    };
    vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL) => {
      const url = new URL(input instanceof Request ? input.url : String(input), 'http://localhost');
      const body = url.pathname.endsWith('/flows/summary')
        ? summary
        : {
            flows: url.searchParams.get('cursor') === 'cursor-1' ? second : first,
            total: 3,
            flow_seq: 7,
            next_cursor: url.searchParams.get('cursor') === 'cursor-1' ? null : 'cursor-1',
            as_of_event_id: 7,
            generated_at_ms: 3_000,
          } satisfies FlowsResponse;
      return new Response(JSON.stringify(body), {
        status: 200,
        headers: {
          'Content-Type': 'application/json',
          'X-LLMConduit-Dashboard-Schema': '6',
        },
      });
    }));

    const { result } = renderRows();
    await waitFor(() => expect(result.current.rows).toHaveLength(2));
    expect(result.current.loaded).toBe(2);
    expect(result.current.total).toBe(3);
    expect(result.current.hasMore).toBe(true);
    act(() => result.current.loadMore());
    await waitFor(() => expect(result.current.rows).toHaveLength(3));
    expect(result.current.rows.map((row) => row.api_call_id)).toEqual([
      'api_page_3', 'api_page_2', 'api_page_1',
    ]);
    expect(result.current.loaded).toBe(3);
    expect(result.current.total).toBe(3);
    expect(result.current.hasMore).toBe(false);
  });
});

describe('useFlowRows — complete rows reconcile by revision', () => {
  beforeEach(() => resetWorld());
  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it('uses a strictly newer REST row as one coherent state', async () => {
    stubFlowsFetch([makeFlow({
      revision: 3,
      api_call_id: 'api_revision',
      status: 'completed',
      usage: { prompt: 20, completion: 4, total: 24 },
      cost: 0.42,
      cost_confidence: 'estimated',
      terminal_reason: 'response.completed',
      client_label: 'key-new',
    })]);
    seedFlows([makeFlow({
      revision: 2,
      api_call_id: 'api_revision',
      status: 'open',
      usage: { prompt: 10, completion: 0, total: 10 },
      cost: null,
      client_label: 'key-old',
    })]);
    const { result } = renderRows();
    await waitFor(() => expect(result.current.rows.find((row) => row.api_call_id === 'api_revision')?.revision).toBe(3));
    const row = result.current.rows.find((candidate) => candidate.api_call_id === 'api_revision');
    expect(row?.status).toBe('completed');
    expect(row?.usage?.total).toBe(24);
    expect(row?.cost).toBe(0.42);
    expect(row?.cost_confidence).toBe('estimated');
    expect(row?.client_label).toBe('key-new');
  });

  it('keeps the live row when its revision is newer or equal', async () => {
    stubFlowsFetch([makeFlow({ revision: 4, api_call_id: 'api_live', status: 'failed', cost: 1 })]);
    seedFlows([makeFlow({ revision: 5, api_call_id: 'api_live', status: 'completed', cost: 2, cost_confidence: 'confident' })]);
    const { result } = renderRows();
    await waitFor(() => expect(globalThis.fetch).toHaveBeenCalled());
    await act(async () => { await Promise.resolve(); });
    const row = result.current.rows.find((candidate) => candidate.api_call_id === 'api_live');
    expect(row?.revision).toBe(5);
    expect(row?.status).toBe('completed');
    expect(row?.cost).toBe(2);
  });

  it('does not treat an omitted live-cache row as an archive deletion', async () => {
    const retained = makeFlow({ api_call_id: 'api_retained', revision: 2, status: 'completed' });
    stubFlowsFetch([retained], 5);
    seedFlows([
      retained,
      makeFlow({ api_call_id: 'api_evicted_open', revision: 1, status: 'open' }),
    ]);

    const { result } = renderRows();
    await waitFor(() => expect(result.current.rows.map((row) => row.api_call_id)).toEqual([
      'api_retained', 'api_evicted_open',
    ]));
  });
});

describe('useFlowRows — provider-attempt drilldown', () => {
  beforeEach(() => resetWorld());
  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it('matches a failed primary attempt even when a different provider served the flow', async () => {
    stubFlowsFetch([]);
    seedFlows([makeFlow({
      api_call_id: 'api_failover',
      upstream_target: 'provider-b',
      attempts: [
        { provider: 'provider-a', model: 'm', start_ms: 1, end_ms: 2, status: 'failed', error_class: 'timeout' },
        { provider: 'provider-b', model: 'm', start_ms: 3, end_ms: 4, status: 'served' },
      ],
    })]);

    const { result } = renderRows({ ...EMPTY_FILTERS, upstream: 'provider-a' });
    await waitFor(() => expect(globalThis.fetch).toHaveBeenCalled());
    expect(result.current.rows.map((row) => row.api_call_id)).toEqual(['api_failover']);
  });
});

describe('useFlowRows — view-local multi-field search', () => {
  beforeEach(() => resetWorld());
  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it('searches the merged population and reports the matching population', async () => {
    stubFlowsFetch([]);
    seedFlows([
      makeFlow({ api_call_id: 'api_success', status: 'completed', response_id: 'resp_123', upstream_target: 'provider-b' }),
      makeFlow({
        api_call_id: 'api_failover', status: 'completed', upstream_target: 'provider-b',
        attempts: [
          { provider: 'provider-a', model: 'm', start_ms: 1, end_ms: 2, status: 'failed', error_class: 'timeout' },
          { provider: 'provider-b', model: 'm', start_ms: 3, end_ms: 4, status: 'served' },
        ],
      }),
    ]);

    const { result } = renderRows(EMPTY_FILTERS, 'provider-a timeout');
    await waitFor(() => expect(globalThis.fetch).toHaveBeenCalled());
    expect(result.current.rows.map((row) => row.api_call_id)).toEqual(['api_failover']);
    expect(result.current.total).toBe(1);
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
