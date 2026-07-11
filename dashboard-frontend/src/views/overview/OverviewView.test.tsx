import { act, cleanup, fireEvent, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { OverviewCost, OverviewDimensionRollup, OverviewResponse } from '../../api/types';
import { getConnection } from '../../api/connection';
import { dashboardStore } from '../../store/dashboardStore';
import { flowFilterStore } from '../../store/flowFilterStore';
import { makeFlow, renderWithQuery, resetWorld, seedFlows } from '../../components/testHarness';
import { OverviewView } from './OverviewView';

const TOKENS = { samples: 3, prompt: 3_000, completion: 1_500, cached: null, reasoning: 300 } as const;
const COST = { samples: 2, total_usd: 0.031, confidence: 'estimated' } as const;

function rollup(key: string, requests = 2, cost: OverviewCost = COST): OverviewDimensionRollup {
  return { key, requests, tokens: TOKENS, cost };
}

function response(over: Partial<OverviewResponse> = {}): OverviewResponse {
  const now = 1_700_000_000_000;
  return {
    generated_at_ms: now,
    metrics_seq: 42,
    scope: {
      window: 'm1',
      requested_at_ms: null,
      selected_at_ms: now,
      status: null,
      model: null,
      upstream: null,
      client: null,
    },
    data_quality: 'measured',
    overflow: { dimension_limit: 64, slot_folded_samples: 0, aggregate_folded_samples: 0, provider_folded_samples: 0, overflowed: false, unattributable_requests: 0 },
    totals: { requests: 3, successes: 1, failures: 1, cancellations: 1, tokens: TOKENS, cost: COST },
    requested_models: [rollup('llama-requested')],
    served_models: [rollup('llama-served'), rollup('unpriced', 1, { samples: 0, total_usd: null, confidence: 'unavailable' })],
    providers: [rollup('vllm-a')],
    clients: [rollup('key-a1b2c3')],
    failures: [rollup('timeout', 1, { samples: 0, total_usd: null, confidence: 'unavailable' })],
    cancellations: [rollup('cancelled', 1, { samples: 0, total_usd: null, confidence: 'unavailable' })],
    lanes: [],
    context: {
      data_quality: 'partial',
      samples: 2,
      unavailable_samples: 1,
      effective_route_limit_min: 32_768,
      input_tokens: 3_000,
      average_pressure_pct: 72.5,
    },
    tokens: TOKENS,
    cost: COST,
    cost_series: [
      { at_ms: now - 2_000, data_quality: 'measured', requests: 1, cost: { samples: 1, total_usd: 0.01, confidence: 'confident' } },
      { at_ms: now - 1_000, data_quality: 'measured', requests: 2, cost: { samples: 1, total_usd: 0.021, confidence: 'estimated' } },
    ],
    provider_attempts_global: {
      scope: 'global',
      data_quality: 'derived',
      providers: [{
        provider: 'vllm-a', data_quality: 'derived', samples: 10, served: 8, failed: 2,
        p50: 90, p95: 240, p99: 600, error_rate: 20, errors: { timeout: 2 },
      }],
    },
    ...over,
  };
}

function emptyResponse(over: Partial<OverviewResponse> = {}): OverviewResponse {
  const emptyTokens = { samples: 0, prompt: null, completion: null, cached: null, reasoning: null } as const;
  const emptyCost = { samples: 0, total_usd: null, confidence: 'unavailable' } as const;
  return response({
    data_quality: 'unavailable',
    totals: { requests: 0, successes: 0, failures: 0, cancellations: 0, tokens: emptyTokens, cost: emptyCost },
    requested_models: [],
    served_models: [],
    providers: [],
    clients: [],
    failures: [],
    cancellations: [],
    lanes: [],
    context: {
      data_quality: 'unavailable', samples: 0, unavailable_samples: 0,
      effective_route_limit_min: null, input_tokens: null, average_pressure_pct: null,
    },
    tokens: emptyTokens,
    cost: emptyCost,
    cost_series: [],
    provider_attempts_global: { scope: 'global', data_quality: 'unavailable', providers: [] },
    ...over,
  });
}

function install(result: OverviewResponse) {
  return vi.spyOn(getConnection().client, 'overview').mockResolvedValue(result);
}

beforeEach(() => resetWorld());
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe('OverviewView — authoritative query scope', () => {
  it('sends the hash window, all flow filters, and the frozen seek instant', async () => {
    window.history.replaceState(null, '', '#/overview?window=m5&status=failed&model=llama&upstream=vllm-b&client=key-client');
    window.dispatchEvent(new HashChangeEvent('hashchange'));
    act(() => dashboardStore.getState().enterSeek(1_699_999_900_000));
    const spy = install(response());

    renderWithQuery(<OverviewView />);
    await waitFor(() => expect(spy).toHaveBeenCalledWith({
      window: 'm5',
      at: 1_699_999_900_000,
      status: 'failed',
      model: 'llama',
      upstream: 'vllm-b',
      client: 'key-client',
    }));
  });

  it('renders only the server response, never a divergent live flow-list rollup', async () => {
    seedFlows([makeFlow({ model_served: 'local-only-model', upstream_target: 'local-only-provider' })]);
    install(response());
    const { getAllByTestId, queryAllByText, queryByText, queryByTestId } = renderWithQuery(<OverviewView />);

    await waitFor(() => expect(getAllByTestId('overview-leaderboard-row').length).toBeGreaterThan(0));
    expect(queryAllByText('llama-served').length).toBeGreaterThan(0);
    expect(queryByText('local-only-model')).toBeNull();
    expect(queryByText('local-only-provider')).toBeNull();
    // The global StatsStrip is the one headline; Overview no longer repeats it.
    expect(queryByTestId('overview-headline')).toBeNull();
  });
});

describe('OverviewView — exact server rollups and honest quality', () => {
  it('renders the server cost series, scoped rollups, effective route limit, and optional-token gap', async () => {
    install(response());
    const { getByTestId, getAllByTestId } = renderWithQuery(<OverviewView />);

    await waitFor(() => expect(getByTestId('overview-cost-trend')).toBeTruthy());
    expect(getByTestId('overview-cost-total').textContent).toContain('$0.0310');
    expect(getByTestId('overview-context-pressure').textContent).toBe('72.5%');
    expect(getByTestId('overview-context-limit').textContent).toContain('32.8k tok');
    expect(getByTestId('overview-token-prompt').getAttribute('data-quality')).toBe('measured');
    expect(getByTestId('overview-token-cached').getAttribute('data-quality')).toBe('unavailable');
    expect(getByTestId('overview-token-cached').textContent).toBe('—');
    expect(getAllByTestId('overview-leaderboard-row').some((row) => row.getAttribute('data-key') === 'llama-served')).toBe(true);
  });

  it('makes bounded overflow explicit and marks otherwise-priced output partial', async () => {
    install(response({
      data_quality: 'partial',
      overflow: { dimension_limit: 64, slot_folded_samples: 7, aggregate_folded_samples: 3, provider_folded_samples: 2, overflowed: true, unattributable_requests: 10 },
    }));
    const { getByTestId } = renderWithQuery(<OverviewView />);

    await waitFor(() => expect(getByTestId('overview-partial')).toBeTruthy());
    expect(getByTestId('overview-partial').textContent).toContain('7 slot samples');
    expect(getByTestId('overview-partial').textContent).toContain('3 window samples');
    expect(getByTestId('overview-partial').textContent).toContain('provider health folded 2');
    expect(getByTestId('overview-cost-total').closest('[data-testid="overview-cost-trend"]')?.getAttribute('data-quality')).toBe('partial');
  });

  it('renders a complete unavailable state without fabricated zero metrics', async () => {
    install(emptyResponse());
    const { getByTestId } = renderWithQuery(<OverviewView />);

    await waitFor(() => expect(getByTestId('overview-provenance')).toBeTruthy());
    expect(getByTestId('overview-top-models-volume').getAttribute('data-available')).toBe('false');
    expect(getByTestId('overview-providers').getAttribute('data-available')).toBe('false');
    expect(getByTestId('overview-failures-rate').textContent).toBe('—');
    expect(getByTestId('overview-context-pressure').textContent).toBe('—');
    expect(getByTestId('overview-token-mix').getAttribute('data-available')).toBe('false');
    expect(getByTestId('overview-cost-total').textContent).toBe('—');
  });

  it('labels provider-attempt health Global even when flow rollups are scoped', async () => {
    install(response({ scope: { ...response().scope, model: 'llama' } }));
    const { getByTestId, getByText } = renderWithQuery(<OverviewView />);

    await waitFor(() => expect(getByTestId('overview-provider')).toBeTruthy());
    expect(getByText('Provider attempts · Global')).toBeTruthy();
    expect(getByTestId('overview-provider').tagName).toBe('BUTTON');
    expect(getByTestId('provider-p50').getAttribute('data-quality')).toBe('derived');
  });
});

describe('OverviewView — keyboard-native drill-down rows preserve scope', () => {
  it('uses buttons for every row family and links provider/model/client/failure to scoped Flows', async () => {
    window.history.replaceState(null, '', '#/overview?window=h1');
    window.dispatchEvent(new HashChangeEvent('hashchange'));
    install(response());
    const { getByTestId, getAllByTestId } = renderWithQuery(<OverviewView />);
    await waitFor(() => expect(getByTestId('overview-provider')).toBeTruthy());

    const model = getAllByTestId('overview-leaderboard-row').find((row) => row.getAttribute('data-key') === 'llama-served')!;
    const client = getByTestId('overview-client-row');
    const failure = getByTestId('overview-failure-group');
    const provider = getByTestId('overview-provider');
    for (const row of [model, client, failure, provider]) expect(row.tagName).toBe('BUTTON');

    fireEvent.click(model);
    await waitFor(() => expect(window.location.hash).toContain('#/flows'));
    expect(new URLSearchParams(window.location.hash.split('?')[1]).get('window')).toBe('h1');
    expect(flowFilterStore.getState().filters.model).toBe('llama-served');

    await waitFor(() => expect(getByTestId('overview-client-row')).toBeTruthy());
    fireEvent.click(getByTestId('overview-client-row'));
    expect(flowFilterStore.getState().filters.client).toBe('key-a1b2c3');
    await waitFor(() => expect(getByTestId('overview-provider')).toBeTruthy());
    fireEvent.click(getByTestId('overview-provider'));
    expect(flowFilterStore.getState().filters.upstream).toBe('vllm-a');
    await waitFor(() => expect(getByTestId('overview-failure-group')).toBeTruthy());
    fireEvent.click(getByTestId('overview-failure-group'));
    expect(flowFilterStore.getState().filters.status).toBe('failed');
  });
});

describe('OverviewView — transport states', () => {
  it('shows a retryable error instead of stale derived content', async () => {
    const spy = vi.spyOn(getConnection().client, 'overview').mockRejectedValue(new Error('contract mismatch'));
    const { getByTestId, getByRole } = renderWithQuery(<OverviewView />);
    await waitFor(() => expect(getByTestId('overview-error')).toBeTruthy(), { timeout: 3_000 });
    expect(getByTestId('overview-error').textContent).toContain('contract mismatch');
    fireEvent.click(getByRole('button', { name: 'Retry' }));
    await waitFor(() => expect(spy.mock.calls.length).toBeGreaterThanOrEqual(2));
  });
});
