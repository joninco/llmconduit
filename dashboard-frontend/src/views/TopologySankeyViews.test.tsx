import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { cleanup, fireEvent, act, waitFor } from '@testing-library/react';
import { TopologyView } from './TopologyView';
import { SankeyView } from './SankeyView';
import { dashboardStore } from '../store/dashboardStore';
import { flowFilterStore } from '../store/flowFilterStore';
import { makeFlow, renderWithQuery, resetWorld } from '../components/testHarness';
import type { OverviewResponse, ProviderHealth, TopologyResponse } from '../api/types';
import { getConnection, queryKeys } from '../api/connection';

function provider(over: Partial<ProviderHealth>): ProviderHealth {
  return {
    id: 'p', name: 'p', route: null, base_url: 'http://x', status: 'healthy',
    cooling_until_ms: null, last_error: null, served_count: 0, failover_count: 0,
    consecutive_failures: 0, catalog_fetched_ms: null, catalog_size: 0, ...over,
  };
}

const TOPOLOGY: TopologyResponse = {
  topology_seq: 1,
  nodes: [provider({ id: 'vllm-a', name: 'vllm-a' }), provider({ id: 'vllm-b', name: 'vllm-b' })],
  edges: [{ from: 'gateway', to: 'vllm-a', attempts_per_sec: 3.1, terminal_flows_per_sec: 3, reported_tokens_per_sec: 90, terminal_cost_per_sec: 0.002 }],
  price_table: {},
};

function overview(over: Partial<OverviewResponse> = {}): OverviewResponse {
  const cost = { samples: 1, total_usd: 2.5, confidence: 'confident' as const };
  const tokens = { samples: 1, prompt: 1000, completion: 500, cached: 100, reasoning: 50 };
  return {
    generated_at_ms: 1000,
    metrics_seq: 1,
    scope: { window: 'm1', mode: 'live', requested_at_ms: null, selected_at_ms: 1000, status: null, model: null, upstream: null, client: null },
    data_quality: 'measured',
    overflow: { dimension_limit: 64, slot_folded_samples: 0, aggregate_folded_samples: 0, provider_folded_samples: 0, overflowed: false, unattributable_requests: 0 },
    totals: { requests: 1, successes: 1, failures: 0, cancellations: 0, tokens, cost },
    requested_models: [], served_models: [], providers: [], clients: [], failures: [], cancellations: [],
    lanes: [{ provider: 'vllm-a', model: 'gpt-4o', requests: 1, tokens, cost }],
    context: { data_quality: 'unavailable', samples: 0, unavailable_samples: 1, effective_route_limit_min: null, input_tokens: null, average_pressure_pct: null },
    tokens,
    cost,
    cost_series: [],
    provider_attempts_global: { scope: 'global', data_quality: 'unavailable', providers: [] },
    ...over,
  };
}

function seedTopology(): void {
  act(() => {
    dashboardStore.getState().applySnapshot({
      cursors: { flow_seq: 0, metrics_seq: 0, topology_seq: 1, monitor_seq: 0 , backend_metrics_seq: 0},
      flows: [], metrics: null, topology: TOPOLOGY,
    });
    dashboardStore.getState().setConnection('live');
  });
}

beforeEach(() => {
  resetWorld();
  cleanup();
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe('TopologyView', () => {
  it('cross-links a provider node to the shared flow filter', () => {
    window.location.hash = '#/topology';
    seedTopology();
    const { container } = renderWithQuery(<TopologyView />);
    fireEvent.click(container.querySelector('[data-node-id="vllm-a"]')!);
    expect(flowFilterStore.getState().filters.upstream).toBe('vllm-a');
    expect(window.location.hash).toBe('#/flows?upstream=vllm-a');
  });

  it('distinguishes a transport failure from an empty provider list', async () => {
    window.location.hash = '#/topology';
    vi.stubGlobal('fetch', vi.fn(async () => new Response('', { status: 503 })));
    getConnection().queryClient.setDefaultOptions({ queries: { retry: false } });
    const { getByTestId } = renderWithQuery(<TopologyView />);
    await waitFor(() => expect(getByTestId('topology-error')).toBeTruthy());
  });

  // U9 — the caption promises client → gateway → providers; the compact layout renders the
  // client side from the LOADED flow rows and cross-links to the client-filtered Flows view.
  it('renders client nodes with per-client share and cross-links the client filter', () => {
    window.location.hash = '#/topology';
    act(() => {
      dashboardStore.getState().applySnapshot({
        cursors: { flow_seq: 0, metrics_seq: 0, topology_seq: 1, monitor_seq: 0, backend_metrics_seq: 0 },
        flows: [
          makeFlow({ api_call_id: 'api_c1', client_label: 'key-8fd8', client_source: 'key_hash', started_ms: 3 }),
          makeFlow({ api_call_id: 'api_c2', client_label: 'key-8fd8', client_source: 'key_hash', started_ms: 2 }),
          makeFlow({ api_call_id: 'api_c3', client_label: 'curl/8.5.0', client_source: 'user_agent', started_ms: 1 }),
        ],
        metrics: null,
        topology: TOPOLOGY,
      });
      dashboardStore.getState().setConnection('live');
    });
    const { getByTestId, getAllByTestId } = renderWithQuery(<TopologyView />);
    expect(getByTestId('topology-clients')).toBeTruthy();
    const clients = getAllByTestId('topology-client');
    // Heaviest client first, with its share of the loaded population.
    expect(clients[0]!.getAttribute('data-client')).toBe('key-8fd8');
    expect(clients[0]!.textContent).toContain('2 · 67%');
    fireEvent.click(clients[0]!);
    expect(flowFilterStore.getState().filters.client).toBe('key-8fd8');
    expect(window.location.hash).toBe('#/flows?client=key-8fd8');
  });
});

describe('SankeyView — authoritative Overview lanes', () => {
  it('renders server-authored terminal lanes and atomically filters both facets', () => {
    window.location.hash = '#/sankey';
    getConnection().queryClient.setQueryData(queryKeys.overview({ window: 'm1' }), overview());
    const { container, getByTestId } = renderWithQuery(<SankeyView />);
    expect(getByTestId('sankey-cost-per-min').textContent).toBe('$2.50/min');
    const lane = container.querySelector('[data-testid="single-lane-summary"][data-model="gpt-4o"]')!;
    expect(lane).not.toBeNull();
    fireEvent.click(lane);
    expect(flowFilterStore.getState().filters.model).toBe('gpt-4o');
    expect(flowFilterStore.getState().filters.upstream).toBe('vllm-a');
  });

  it('counts canonical prompt plus completion once even when subsets are reported', () => {
    window.location.hash = '#/sankey';
    const tokens = { samples: 1, prompt: 70_000, completion: 41_994, cached: 60_000, reasoning: 20_000 };
    const response = overview({
      tokens,
      totals: { requests: 1, successes: 1, failures: 0, cancellations: 0, tokens, cost: { samples: 0, total_usd: null, confidence: 'unavailable' } },
      cost: { samples: 0, total_usd: null, confidence: 'unavailable' },
      lanes: [{ provider: 'p', model: 'm', requests: 1, tokens, cost: { samples: 0, total_usd: null, confidence: 'unavailable' } }],
    });
    getConnection().queryClient.setQueryData(queryKeys.overview({ window: 'm1' }), response);
    const { getByTestId } = renderWithQuery(<SankeyView />);
    expect(getByTestId('sankey-companion-table').textContent).toContain('111,994');
    expect(getByTestId('sankey-cost-per-min').textContent).toBe('—');
  });

  it('uses the selected historical cut and labels it', () => {
    window.location.hash = '#/sankey?window=m5';
    const at = 123_000;
    act(() => {
      seedTopology();
      dashboardStore.getState().applySeekCut({
        rows: [], cursors: { flow_seq: 0, metrics_seq: 0, topology_seq: 1, monitor_seq: 0 , backend_metrics_seq: 0},
        atMs: at, monitorSeq: 0, metrics: null, topology: TOPOLOGY,
      });
    });
    const response = overview({
      scope: { window: 'm5', mode: 'historical', requested_at_ms: at, selected_at_ms: at, status: null, model: null, upstream: null, client: null },
      lanes: [{ provider: 'vllm-b', model: 'frozen-model', requests: 1, tokens: { samples: 1, prompt: 1, completion: 1, cached: null, reasoning: null }, cost: { samples: 1, total_usd: 1, confidence: 'estimated' } }],
      cost: { samples: 1, total_usd: 1, confidence: 'estimated' },
    });
    getConnection().queryClient.setQueryData(queryKeys.overview({ window: 'm5', at }), response);
    const { container, getByTestId } = renderWithQuery(<SankeyView />);
    expect(getByTestId('sankey-historical')).toBeTruthy();
    expect(container.querySelector('[data-model="frozen-model"]')).not.toBeNull();
  });

  it('renders an explicit terminal-analytics state for open-only scope', () => {
    window.location.hash = '#/sankey?status=open';
    const { getByTestId } = renderWithQuery(<SankeyView />);
    expect(getByTestId('sankey-unavailable').textContent).toContain('unavailable for open-only');
  });
});
