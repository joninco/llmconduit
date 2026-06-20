/**
 * View-level integration for the D12 Topology + Sankey screens: the click→shared-filter→navigate
 * cross-link and the SEEK (D11) frozen-cut rendering. The viz internals (layout, colors, particles)
 * are covered by the component tests; here we assert the VIEW wiring.
 */
import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { cleanup, render, fireEvent, act } from '@testing-library/react';
import { TopologyView } from './TopologyView';
import { SankeyView } from './SankeyView';
import { dashboardStore } from '../store/dashboardStore';
import { flowFilterStore } from '../store/flowFilterStore';
import type { ProviderHealth, TopologyResponse, FlowSummary } from '../api/types';

function provider(over: Partial<ProviderHealth>): ProviderHealth {
  return {
    id: 'p', name: 'p', route: null, base_url: 'http://x', status: 'healthy',
    cooling_until_ms: null, last_error: null, served_count: 0, failover_count: 0,
    consecutive_failures: 0, catalog_fetched_ms: null, catalog_size: 0, ...over,
  };
}

const TOPOLOGY: TopologyResponse = {
  topology_seq: 1,
  nodes: [
    provider({ id: 'vllm-a', name: 'vllm-a', status: 'healthy' }),
    provider({ id: 'vllm-b', name: 'vllm-b', status: 'cooling', cooling_until_ms: Date.now() + 9000 }),
  ],
  edges: [{ from: 'gateway', to: 'vllm-a', throughput: 3, tokens_per_sec: 90, cost_per_sec: 0.002 }],
  price_table: { 'gpt-4o': { input_per_1k: 0.005, output_per_1k: 0.015, cached_per_1k: 0.0025 } },
};

function flow(over: Partial<FlowSummary>): FlowSummary {
  return {
    api_call_id: `api_${Math.random().toString(36).slice(2, 8)}`, method: 'POST', uri: '/v1/responses',
    status: 'completed', started_ms: Date.now() - 2000, finished_ms: Date.now() - 500, ...over,
  };
}

function seedLiveTopology(): void {
  act(() => {
    dashboardStore.getState().applySnapshot({
      cursors: { flow_seq: 0, metrics_seq: 0, topology_seq: 1, monitor_seq: 0 },
      flows: [],
      metrics: null,
      topology: TOPOLOGY,
    });
    dashboardStore.getState().setConnection('live');
  });
}

beforeEach(() => {
  dashboardStore.getState().reset();
  flowFilterStore.getState().clear();
  window.location.hash = '#/topology';
  cleanup();
});
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe('TopologyView — click node → shared filter + navigate to flows', () => {
  it('clicking a provider node sets the upstream filter and navigates to #/flows', () => {
    seedLiveTopology();
    const { container } = render(<TopologyView />);
    fireEvent.click(container.querySelector('[data-node-id="vllm-a"]')!);
    expect(flowFilterStore.getState().filters.upstream).toBe('vllm-a');
    expect(window.location.hash).toBe('#/flows');
  });

  it('renders the historical affordance while seeking and the frozen topology nodes', () => {
    seedLiveTopology();
    act(() => {
      dashboardStore.getState().applySeekCut({
        rows: [], cursors: { flow_seq: 0, metrics_seq: 0, topology_seq: 1, monitor_seq: 0 },
        atMs: Date.now(), monitorSeq: 0, metrics: null, topology: TOPOLOGY,
      });
    });
    const { getByTestId, container } = render(<TopologyView />);
    expect(getByTestId('topology-historical')).not.toBeNull();
    // The frozen cut's nodes are rendered (topology comes from the store's frozen slices).
    expect(container.querySelectorAll('[data-testid="topo-node"]').length).toBe(2);
  });
});

describe('SankeyView — click band → shared filter + navigate; $/min; seek', () => {
  it('renders a cost band per model and clicking it filters + navigates', () => {
    act(() => {
      dashboardStore.getState().applySnapshot({
        cursors: { flow_seq: 1, metrics_seq: 0, topology_seq: 1, monitor_seq: 0 },
        flows: [flow({ model_served: 'gpt-4o', usage: { prompt: 1000, completion: 500, total: 1500, cached: 0, reasoning: 0 } })],
        metrics: null,
        topology: TOPOLOGY,
      });
      dashboardStore.getState().setConnection('live');
    });
    const { container, getByTestId } = render(<SankeyView />);
    // $/min readout reflects the windowed cost (positive).
    expect(getByTestId('sankey-cost-per-min').textContent).toMatch(/\$\d/);
    fireEvent.click(container.querySelector('[data-testid="sankey-band"][data-model="gpt-4o"]')!);
    expect(flowFilterStore.getState().filters.model).toBe('gpt-4o');
    expect(window.location.hash).toBe('#/flows');
  });

  it('SEEK: builds the Sankey from the FROZEN cut rows + at_ms (no live data)', () => {
    // A live flow exists, but the seek cut replaces the rows with a different frozen set.
    act(() => {
      dashboardStore.getState().applySnapshot({
        cursors: { flow_seq: 1, metrics_seq: 0, topology_seq: 1, monitor_seq: 0 },
        flows: [flow({ model_served: 'live-only', usage: { prompt: 1, completion: 1, total: 2, cached: 0, reasoning: 0 } })],
        metrics: null, topology: TOPOLOGY,
      });
      dashboardStore.getState().setConnection('live');
    });
    const at = Date.now();
    act(() => {
      dashboardStore.getState().applySeekCut({
        rows: [flow({ model_served: 'frozen-model', started_ms: at - 2000, finished_ms: at - 500, usage: { prompt: 1000, completion: 1000, total: 2000, cached: 0, reasoning: 0 } })],
        cursors: { flow_seq: 1, metrics_seq: 0, topology_seq: 1, monitor_seq: 0 },
        atMs: at, monitorSeq: 0, metrics: null, topology: TOPOLOGY,
      });
    });
    const { getByTestId, container } = render(<SankeyView />);
    expect(getByTestId('sankey-historical')).not.toBeNull();
    // Only the FROZEN model lane is present; the live-only flow does not bleed in.
    expect(container.querySelector('[data-testid="sankey-band"][data-model="frozen-model"]')).not.toBeNull();
    expect(container.querySelector('[data-testid="sankey-band"][data-model="live-only"]')).toBeNull();
  });
});
