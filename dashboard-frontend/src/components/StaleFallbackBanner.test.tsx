/**
 * StaleFallbackBanner (U1) — idle must not look like failure. A healthy-but-idle gateway
 * (stream connected, metrics fresh, zero open flows) renders the retained-window banner in a
 * neutral informational voice; the yellow warning voice is reserved for data that is stale
 * when it should not be (degraded stream, stale publication, or traffic against an old cut).
 */
import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { act, cleanup } from '@testing-library/react';
import { StaleFallbackBanner } from './StaleFallbackBanner';
import { dashboardStore } from '../store/dashboardStore';
import type { InstantMetricSample, MetricsResponse } from '../api/types';
import { renderWithQuery, resetWorld } from './testHarness';

function win(over: Partial<InstantMetricSample> = {}): InstantMetricSample {
  return {
    interval_duration_ms: 1000, ready: true, accepted_requests: 1,
    accepted_per_sec: 0, active_streams_now: 0, failure_pct: 0,
    terminal_requests: 1, terminal_per_sec: 0, successes: 1,
    failures: 0, cancellations: 0, cancellation_pct: 0,
    p50_ms: 100, p95_ms: 100, p99_ms: 100, reported_tokens_per_sec: 10, cost_per_min: 0,
    quantile_method: 'log_histogram_nearest_rank', max_relative_error: 0.062,
    latency_overflow_count: 0, p50_quality: 'measured', p95_quality: 'measured', p99_quality: 'measured',
    latency_samples: 1, usage_samples: 1, priced_samples: 1, cost_confidence: 'confident',
    usage_anomaly_count: 0,
    ...over,
  };
}

function pushWorld(opts: { connection: 'live' | 'connecting' | 'error'; generatedAtMs: number; activeStreams?: number }) {
  const metrics: MetricsResponse = {
    metrics_seq: 1,
    generated_at_ms: opts.generatedAtMs,
    instant: win({ active_streams_now: opts.activeStreams ?? 0 }),
  };
  act(() => {
    dashboardStore.getState().setMetrics(metrics);
    dashboardStore.getState().setConnection(opts.connection);
  });
}

beforeEach(() => {
  resetWorld();
  vi.useFakeTimers();
  vi.setSystemTime(new Date('2026-07-12T13:00:00Z'));
});
afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe('StaleFallbackBanner tone (U1)', () => {
  const asOfMs = Date.parse('2026-07-12T12:53:15Z');

  it('renders NEUTRAL idle when the stream is live, metrics are fresh, and traffic is idle', () => {
    const { getByTestId } = renderWithQuery(<StaleFallbackBanner asOfMs={asOfMs} surface="overview" />);
    pushWorld({ connection: 'live', generatedAtMs: Date.now() - 1_000, activeStreams: 0 });
    const banner = getByTestId('overview-stale-fallback');
    expect(banner.getAttribute('data-tone')).toBe('idle');
    expect(banner.textContent).toContain('idle');
    expect(banner.textContent).toContain('showing last activity from');
    expect(banner.textContent).toContain('ago');
    // Neutral voice: no cooling/warning classes.
    expect(banner.className).not.toContain('status-cooling');
  });

  it('keeps the YELLOW warning when the stream is degraded', () => {
    const { getByTestId } = renderWithQuery(<StaleFallbackBanner asOfMs={asOfMs} surface="overview" />);
    pushWorld({ connection: 'error', generatedAtMs: Date.now() - 1_000, activeStreams: 0 });
    const banner = getByTestId('overview-stale-fallback');
    expect(banner.getAttribute('data-tone')).toBe('warning');
    expect(banner.textContent).toContain('stale fallback');
    expect(banner.className).toContain('status-cooling');
  });

  it('keeps the warning when the metrics publication itself is stale', () => {
    const { getByTestId } = renderWithQuery(<StaleFallbackBanner asOfMs={asOfMs} surface="topology" />);
    pushWorld({ connection: 'live', generatedAtMs: Date.now() - 60_000, activeStreams: 0 });
    expect(getByTestId('topology-stale-fallback').getAttribute('data-tone')).toBe('warning');
  });

  it('keeps the warning when traffic is in flight against an old analytics cut', () => {
    const { getByTestId } = renderWithQuery(<StaleFallbackBanner asOfMs={asOfMs} surface="sankey" />);
    pushWorld({ connection: 'live', generatedAtMs: Date.now() - 1_000, activeStreams: 2 });
    expect(getByTestId('sankey-stale-fallback').getAttribute('data-tone')).toBe('warning');
  });

  it('warns (not idle) before any metrics publication exists', () => {
    const { getByTestId } = renderWithQuery(<StaleFallbackBanner asOfMs={asOfMs} surface="overview" />);
    act(() => dashboardStore.getState().setConnection('live'));
    expect(getByTestId('overview-stale-fallback').getAttribute('data-tone')).toBe('warning');
  });
});
