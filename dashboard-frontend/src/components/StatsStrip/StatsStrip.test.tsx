import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { act, cleanup, fireEvent, within } from '@testing-library/react';
import { StatsStrip } from './StatsStrip';
import { dashboardStore, type LiveBaseline } from '../../store/dashboardStore';
import type { MetricsResponse, InstantMetricSample } from '../../api/types';
import { renderWithQuery, resetWorld } from '../testHarness';
import { CHIP_METRICS } from './chips';

function win(over: Partial<InstantMetricSample> = {}): InstantMetricSample {
  // Fully-measured default: the three denominators mirror `latency_samples` unless overridden.
  const latency_samples = over.latency_samples ?? 252;
  return {
    interval_duration_ms: 1000, ready: true, accepted_requests: 252,
    accepted_per_sec: 4.2, active_streams_now: 3, failure_pct: 1.1,
    terminal_requests: latency_samples, terminal_per_sec: 4.2, successes: latency_samples,
    failures: 0, cancellations: 0, cancellation_pct: 0,
    p50_ms: 180, p95_ms: 920, p99_ms: 1840, reported_tokens_per_sec: 142, cost_per_min: 0.21,
    quantile_method: 'log_histogram_nearest_rank', max_relative_error: 0.062,
    latency_overflow_count: 0, p50_quality: 'measured', p95_quality: 'measured', p99_quality: 'measured', usage_anomaly_count: 0,
    latency_samples,
    usage_samples: latency_samples,
    priced_samples: latency_samples,
    cost_confidence: 'estimated',
    ...over,
  };
}

function metrics(seq: number, over: Partial<MetricsResponse> = {}, windows?: { m1?: Partial<InstantMetricSample>; m5?: Partial<InstantMetricSample>; h1?: Partial<InstantMetricSample> }): MetricsResponse {
  const m1 = win(windows?.m1);
  return {
    metrics_seq: seq,
    generated_at_ms: seq * 1000,
    instant: m1,
    ...over,
  };
}

/** Drive a `metric_tick`-equivalent into the store (the action the socket calls). */
function pushMetrics(m: MetricsResponse): void {
  act(() => {
    dashboardStore.getState().setMetrics(m);
  });
}

beforeEach(() => resetWorld());
afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe('StatsStrip — chips', () => {
  it('renders every chip with a tabular-nums value', () => {
    const { getByTestId } = renderWithQuery(<StatsStrip />);
    pushMetrics(metrics(1, {}, { m1: { accepted_per_sec: 7.5, reported_tokens_per_sec: 1500 } }));
    for (const key of CHIP_METRICS) {
      const chip = getByTestId(`chip-${key}`);
      const value = within(chip).getByTestId('chip-value');
      expect(value.className).toContain('tabular-nums');
    }
    // The m1 window value surfaced (req/s chip shows 7.5).
    expect(within(getByTestId('chip-accepted_per_sec')).getByTestId('chip-value').textContent).toBe('7.5');
    // tokens compaction.
    expect(within(getByTestId('chip-reported_tokens_per_sec')).getByTestId('chip-value').textContent).toBe('1.5k');
  });

  it('uses backend generation counters when present and exposes source plus coverage', () => {
    const { getByTestId } = renderWithQuery(<StatsStrip />);
    pushMetrics(metrics(1, {
      engine_throughput: {
        generated_tokens_per_sec: 287.5,
        sampled_at_ms: 900,
        measured_sources: 1,
        total_sources: 2,
        coverage: 'partial',
      },
    }));
    const chip = getByTestId('chip-reported_tokens_per_sec');
    expect(chip.textContent).toContain('engine gen tok/s');
    expect(within(chip).getByTestId('chip-value').textContent).toBe('288');
    expect(chip.getAttribute('data-source')).toBe('engine');
    expect(chip.getAttribute('data-quality')).toBe('partial');
    expect(chip.getAttribute('title')).toContain('1/2 physically distinct metrics sources');
  });

  it('renders a sparkline per metric and updates from successive MetricTick frames', () => {
    const { getByTestId, getAllByTestId } = renderWithQuery(<StatsStrip />);
    pushMetrics(metrics(1, {}, { m1: { accepted_per_sec: 1 } }));
    // One sparkline per chip metric.
    expect(getAllByTestId('sparkline')).toHaveLength(CHIP_METRICS.length);

    // A second, third frame deepens the series; the chip value reflects the latest.
    pushMetrics(metrics(2, {}, { m1: { accepted_per_sec: 2 } }));
    pushMetrics(metrics(3, {}, { m1: { accepted_per_sec: 3 } }));
    expect(within(getByTestId('chip-accepted_per_sec')).getByTestId('chip-value').textContent).toBe('3.0');
  });

  it('turns the err% chip red above the 5% threshold', () => {
    const { getByTestId } = renderWithQuery(<StatsStrip />);
    pushMetrics(metrics(1, {}, { m1: { failure_pct: 1.1 } }));
    expect(within(getByTestId('chip-failure_pct')).getByTestId('chip-value').className).not.toContain('text-status-down');
    pushMetrics(metrics(2, {}, { m1: { failure_pct: 9.9 } }));
    expect(within(getByTestId('chip-failure_pct')).getByTestId('chip-value').className).toContain('text-status-down');
  });

  it('shows a delta arrow once a second sample arrives', () => {
    const { getByTestId } = renderWithQuery(<StatsStrip />);
    pushMetrics(metrics(1, {}, { m1: { accepted_per_sec: 4 } }));
    pushMetrics(metrics(2, {}, { m1: { accepted_per_sec: 6 } }));
    expect(within(getByTestId('chip-accepted_per_sec')).getByTestId('chip-delta').textContent).toBe('▲');
  });

  // Gap 01 — don't lie with zeros (end-to-end through the component).
  it('renders latency/tok-s/cost as "—" (not 0) for a zero-sample window, keeping req/s numeric', () => {
    const { getByTestId } = renderWithQuery(<StatsStrip />);
    // Traffic in flight (req/s 2.5, 4 active) but NOTHING finalized → m1.latency_samples = 0.
    pushMetrics(metrics(1, {}, {
      m1: { latency_samples: 0, failure_pct: 0, p50_ms: 0, p95_ms: 0, p99_ms: 0, reported_tokens_per_sec: 0, cost_per_min: 0, accepted_per_sec: 2.5, active_streams_now: 4 },
    }));
    const val = (k: string) => within(getByTestId(`chip-${k}`)).getByTestId('chip-value').textContent;
    expect(val('p50_ms')).toBe('—');
    expect(val('p95_ms')).toBe('—');
    expect(val('reported_tokens_per_sec')).toBe('—');
    expect(val('cost_per_min')).toBe('—');
    expect(val('failure_pct')).toBe('—');
    // The genuinely-measured req/s + the live active count stay numeric.
    expect(val('accepted_per_sec')).toBe('2.5');
    expect(val('active_streams_now')).toBe('4.0');
  });

  // Gap 01 finding 4 — provenance/quality is rendered on every chip (data-quality).
  it('renders a data-quality provenance tag on every chip', () => {
    const { getByTestId } = renderWithQuery(<StatsStrip />);
    pushMetrics(metrics(1));
    const quality = (k: string) => getByTestId(`chip-${k}`).getAttribute('data-quality');
    expect(quality('accepted_per_sec')).toBe('measured');
    expect(quality('active_streams_now')).toBe('measured');
    expect(quality('p50_ms')).toBe('derived');
    expect(quality('reported_tokens_per_sec')).toBe('derived');
    expect(quality('cost_per_min')).toBe('estimated'); // priced → labelled estimated
  });

  it('flips a chip data-quality to "unavailable" when its metric is unmeasurable', () => {
    const { getByTestId } = renderWithQuery(<StatsStrip />);
    // No usage reported (usage/priced 0) though latency IS measured (latency_samples 12).
    pushMetrics(metrics(1, {}, { m1: { latency_samples: 12, usage_samples: 0, priced_samples: 0 } }));
    const quality = (k: string) => getByTestId(`chip-${k}`).getAttribute('data-quality');
    expect(quality('p50_ms')).toBe('derived'); // latency measured
    expect(quality('reported_tokens_per_sec')).toBe('unavailable'); // no usage → gap
    expect(quality('cost_per_min')).toBe('unavailable'); // no priced usage → gap
    expect(quality('accepted_per_sec')).toBe('measured'); // never gated
  });
});

describe('StatsStrip — connection semantics', () => {
  it('distinguishes initial connection, reconnecting retained data, and disconnect', () => {
    const { getByTestId } = renderWithQuery(<StatsStrip />);
    act(() => dashboardStore.getState().setConnection('connecting'));
    expect(getByTestId('status-connection').textContent).toContain('Connecting');

    pushMetrics(metrics(1));
    act(() => {
      dashboardStore.getState().setConnection('live');
      dashboardStore.getState().setConnection('connecting');
    });
    expect(getByTestId('status-connection').textContent).toContain('Reconnecting');
    expect(getByTestId('status-freshness')).toBeTruthy();

    act(() => dashboardStore.getState().setConnection('error'));
    expect(getByTestId('status-connection').textContent).toContain('Disconnected');
  });
});

describe('StatsStrip — instantaneous idle retention', () => {
  it('shows current instantaneous cuts while active, then freezes values while traffic becomes idle', () => {
    vi.useFakeTimers();
    vi.setSystemTime(10_000);
    const { getByTestId } = renderWithQuery(<StatsStrip />);
    const value = (key: string) => within(getByTestId(`chip-${key}`)).getByTestId('chip-value').textContent;

    const active = win({ accepted_requests: 6, accepted_per_sec: 6.5, active_streams_now: 1 });
    pushMetrics(metrics(1, {
      generated_at_ms: 10_000,
      instant: active,
      last_activity: { at_ms: 10_000, instant: active },
    }));
    expect(value('accepted_per_sec')).toBe('6.5');
    expect(getByTestId('stats-strip').getAttribute('data-metrics-state')).toBe('instant');

    vi.setSystemTime(11_000);
    const idle = win({
      accepted_requests: 0,
      accepted_per_sec: 0,
      terminal_requests: 0,
      terminal_per_sec: 0,
      successes: 0,
      failures: 0,
      failure_pct: null,
      cancellations: 0,
      cancellation_pct: null,
      active_streams_now: 0,
      latency_samples: 0,
      p50_ms: null,
      p95_ms: null,
      p99_ms: null,
      p50_quality: 'unavailable',
      p95_quality: 'unavailable',
      p99_quality: 'unavailable',
      usage_samples: 0,
      reported_tokens_per_sec: null,
      priced_samples: 0,
      cost_per_min: null,
      cost_confidence: 'unavailable',
    });
    pushMetrics(metrics(2, {
      generated_at_ms: 11_000,
      instant: idle,
      last_activity: { at_ms: 10_000, instant: active },
      engine_throughput: {
        generated_tokens_per_sec: 75,
        sampled_at_ms: 10_500,
        measured_sources: 1,
        total_sources: 1,
        coverage: 'full',
      },
    }));

    // Request-derived values stay on the last instantaneous cut, but the live active count is 0.
    expect(value('accepted_per_sec')).toBe('6.5');
    expect(value('active_streams_now')).toBe('0.0');
    expect(value('reported_tokens_per_sec')).toBe('75.0');
    expect(getByTestId('chip-reported_tokens_per_sec').textContent).toContain('engine gen tok/s');
    expect(getByTestId('stats-strip').getAttribute('data-metrics-state')).toBe('retained');
    expect(getByTestId('chip-accepted_per_sec').getAttribute('data-retained')).toBe('true');
    act(() => vi.advanceTimersByTime(1_000));
    expect(getByTestId('status-traffic').textContent).toContain('Idle');
    expect(getByTestId('status-traffic').getAttribute('title')).toContain('Last request');

    act(() => vi.advanceTimersByTime(4_000));
    expect(getByTestId('status-traffic').getAttribute('title')).toContain('6s ago');
  });

  it('uses an explicit idle/no-request state before any request has been observed', () => {
    const { getByTestId } = renderWithQuery(<StatsStrip />);
    pushMetrics(metrics(1, {
      instant: win({
        accepted_requests: 0,
        accepted_per_sec: 0,
        terminal_requests: 0,
        terminal_per_sec: 0,
        active_streams_now: 0,
      }),
    }));
    expect(getByTestId('status-traffic').getAttribute('data-state')).toBe('idle');
    expect(getByTestId('status-traffic').getAttribute('title')).toContain('No request has been observed yet');
  });
});

describe('StatsStrip — seek isolation (D11 R5)', () => {
  it('reads the FROZEN snapshot value while seeking, flat delta, then returns to live on resume', () => {
    const { getByTestId } = renderWithQuery(<StatsStrip />);
    const value = () => within(getByTestId('chip-accepted_per_sec')).getByTestId('chip-value').textContent;
    const delta = () => within(getByTestId('chip-accepted_per_sec')).getByTestId('chip-delta').textContent;
    // Two LIVE ticks build the live history; chip reads the latest live (2) with an UP delta.
    pushMetrics(metrics(1, {}, { m1: { accepted_per_sec: 1 } }));
    pushMetrics(metrics(2, {}, { m1: { accepted_per_sec: 2 } }));
    expect(value()).toBe('2.0');
    expect(delta()).toBe('▲');

    // SEEK: install a FROZEN cut (reqs/s = 42) atomically with connection='seeking'.
    let baseline!: LiveBaseline;
    act(() => {
      baseline = dashboardStore.getState().captureLiveBaseline();
      dashboardStore.getState().applySeekCut({
        rows: [],
        cursors: { flow_seq: 0, metrics_seq: 50, topology_seq: 0, monitor_seq: 3 , backend_metrics_seq: 0},
        atMs: Date.now(),
        monitorSeq: 3,
        metrics: metrics(50, {}, { m1: { accepted_per_sec: 42 } }),
        topology: null,
      });
    });
    // Chip CURRENT value reads the frozen sample and compares it with the preceding historical point.
    expect(value()).toBe('42.0');
    expect(delta()).toBe('▲');

    // RESUME: restore baseline (live seq 2) atomically with connection='live' → chip back on live.
    act(() => dashboardStore.getState().restoreLiveBaseline(baseline));
    expect(value()).toBe('2.0');

    // A NEW live tick continues the live history cleanly — the frozen 42 left no trace.
    pushMetrics(metrics(3, {}, { m1: { accepted_per_sec: 3 } }));
    expect(value()).toBe('3.0');
    expect(delta()).toBe('▲'); // 3 > 2 (the prior LIVE sample, not the frozen 42)
  });
});

describe('StatsStrip — history horizon selector', () => {
  it('changes sparkline horizon without changing the instantaneous chip value', () => {
    const { getByTestId, getByText } = renderWithQuery(<StatsStrip />);
    // Legacy-shaped override arguments are deliberately distinct; only the instant m1 value exists.
    pushMetrics(metrics(1, {}, { m1: { accepted_per_sec: 1 }, m5: { accepted_per_sec: 5 }, h1: { accepted_per_sec: 9 } }));
    // Default window is 1m.
    expect(within(getByTestId('chip-accepted_per_sec')).getByTestId('chip-value').textContent).toBe('1.0');

    // Switch to 5m.
    fireEvent.click(getByText('5m'));
    expect(within(getByTestId('chip-accepted_per_sec')).getByTestId('chip-value').textContent).toBe('1.0');

    // Switch to 1h.
    fireEvent.click(getByText('1h'));
    expect(within(getByTestId('chip-accepted_per_sec')).getByTestId('chip-value').textContent).toBe('1.0');

    // aria-pressed tracks the active window.
    expect(getByText('1h').getAttribute('aria-pressed')).toBe('true');
    expect(getByText('1m').getAttribute('aria-pressed')).toBe('false');
  });
});
