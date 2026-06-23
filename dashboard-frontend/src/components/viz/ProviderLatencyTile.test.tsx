import { describe, it, expect, afterEach } from 'vitest';
import { cleanup, render } from '@testing-library/react';
import { ProviderLatencyTile } from './ProviderLatencyTile';
import { buildProviderLatency, OVERFLOW_PROVIDER_KEY, UNKNOWN_PROVIDER_KEY } from './providerLatency';
import type { ProviderLatency } from '../../api/types';

function per(over: Partial<ProviderLatency> = {}): ProviderLatency {
  return {
    provider: 'vllm-a', data_quality: 'derived', samples: 50, served: 48, failed: 2,
    p50: 82, p95: 190, p99: 240, error_rate: 4, errors: { http_status: 2 }, ...over,
  };
}

function renderTile(p: ProviderLatency | null | undefined, hint?: string) {
  return render(<ProviderLatencyTile model={buildProviderLatency(p, hint)} />);
}

describe('ProviderLatencyTile (gap 13)', () => {
  afterEach(cleanup);

  it('renders per-provider p50/p95/p99 (derived) + a measured error rate', () => {
    const { getByTestId } = renderTile(per());
    expect(getByTestId('provider-latency-tile')).toHaveAttribute('data-available', 'true');
    expect(getByTestId('provider-p50')).toHaveTextContent('82ms');
    expect(getByTestId('provider-p50')).toHaveAttribute('data-quality', 'derived');
    expect(getByTestId('provider-p95')).toHaveTextContent('190ms');
    expect(getByTestId('provider-p99')).toHaveTextContent('240ms');
    const err = getByTestId('provider-error-rate');
    expect(err).toHaveTextContent('4.0%');
    expect(err).toHaveAttribute('data-quality', 'measured');
    expect(getByTestId('provider-samples')).toHaveTextContent('48/50');
  });

  it('an all-served provider shows a MEASURED 0% (distinct from the unavailable —)', () => {
    const { getByTestId } = renderTile(per({ served: 50, failed: 0, error_rate: 0, errors: {} }));
    const err = getByTestId('provider-error-rate');
    expect(err).toHaveTextContent('0%'); // a real measured zero
    expect(err).toHaveAttribute('data-quality', 'measured');
  });

  it('a no-sample provider renders — everywhere (unavailable), NEVER 0ms/0%', () => {
    const { getByTestId, queryByTestId } = renderTile(null, 'never-seen');
    expect(getByTestId('provider-latency-tile')).toHaveAttribute('data-available', 'false');
    const unavail = getByTestId('provider-latency-unavailable');
    expect(unavail).toHaveAttribute('data-quality', 'unavailable');
    expect(unavail).toHaveTextContent('—');
    // No fabricated figures.
    expect(queryByTestId('provider-p50')).toBeNull();
    expect(queryByTestId('provider-error-rate')).toBeNull();
  });

  it('renders the per-class error distribution (only occurring classes)', () => {
    const { getByTestId, queryByTestId } = renderTile(
      per({ failed: 5, errors: { http_status: 3, timeout: 2 } }),
    );
    expect(getByTestId('provider-error-distribution')).toBeTruthy();
    expect(getByTestId('provider-error-http_status')).toHaveTextContent('3');
    expect(getByTestId('provider-error-timeout')).toHaveTextContent('2');
    expect(queryByTestId('provider-error-connect')).toBeNull(); // absent class omitted
  });

  it('labels the __other__ overflow bucket honestly (an overflow hint)', () => {
    const { getByTestId } = renderTile(per({ provider: OVERFLOW_PROVIDER_KEY }));
    expect(getByTestId('provider-latency-overflow-hint')).toHaveTextContent('overflow');
  });

  it('labels the unknown provider sentinel', () => {
    const { getByTestId } = renderTile(per({ provider: UNKNOWN_PROVIDER_KEY }));
    expect(getByTestId('provider-latency-overflow-hint')).toHaveTextContent('unknown');
  });
});
