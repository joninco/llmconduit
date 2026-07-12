import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { act, cleanup, fireEvent, within } from '@testing-library/react';
import { CompactStatsStrip, StatsStrip } from './StatsStrip';
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
  it.each([320, 375, 768, 1024, 1440])('keeps metric cards structurally unclipped at %ipx', (width) => {
    const { getByTestId } = renderWithQuery(<div style={{ width }}><StatsStrip /></div>);
    pushMetrics(metrics(1));
    expect(getByTestId('stats-strip').className).not.toContain('overflow-hidden');
    expect(getByTestId('primary-metrics').className).not.toContain('overflow-hidden');
    for (const key of CHIP_METRICS) expect(getByTestId(`chip-${key}`).className).toContain('min-w-0');
  });

  it('uses a controlled accessible disclosure whose state survives live metric updates', () => {
    const { getByRole } = renderWithQuery(<StatsStrip />);
    const button = getByRole('button', { name: /More metrics/ });
    expect(button.getAttribute('aria-expanded')).toBe('false');
    fireEvent.click(button);
    expect(button.getAttribute('aria-expanded')).toBe('true');
    expect(button.getAttribute('aria-controls')).toBe('secondary-metrics');
    pushMetrics(metrics(2));
    expect(getByRole('button', { name: /Fewer metrics/ }).getAttribute('aria-expanded')).toBe('true');
  });

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
    expect(within(getByTestId('chip-reported_tokens_per_sec')).getByTestId('chip-value').textContent).toBe('1.5k tok/s');
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
    expect(within(chip).getByTestId('chip-value').textContent).toBe('288 tok/s');
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
    expect(within(getByTestId('chip-accepted_per_sec')).getByTestId('chip-value').textContent).toBe('3');
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
    expect(val('active_streams_now')).toBe('4');
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
    expect(value('active_streams_now')).toBe('0');
    expect(value('reported_tokens_per_sec')).toBe('75 tok/s');
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
    expect(value()).toBe('2');
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
    expect(value()).toBe('42');
    expect(delta()).toBe('▲');

    // RESUME: restore baseline (live seq 2) atomically with connection='live' → chip back on live.
    act(() => dashboardStore.getState().restoreLiveBaseline(baseline));
    expect(value()).toBe('2');

    // A NEW live tick continues the live history cleanly — the frozen 42 left no trace.
    pushMetrics(metrics(3, {}, { m1: { accepted_per_sec: 3 } }));
    expect(value()).toBe('3');
    expect(delta()).toBe('▲'); // 3 > 2 (the prior LIVE sample, not the frozen 42)
  });
});

describe('StatsStrip — history horizon selector', () => {
  it('changes sparkline horizon without changing the instantaneous chip value', () => {
    const { getByTestId, getByText } = renderWithQuery(<StatsStrip />);
    // Legacy-shaped override arguments are deliberately distinct; only the instant m1 value exists.
    pushMetrics(metrics(1, {}, { m1: { accepted_per_sec: 1 }, m5: { accepted_per_sec: 5 }, h1: { accepted_per_sec: 9 } }));
    // Default window is 1m.
    expect(within(getByTestId('chip-accepted_per_sec')).getByTestId('chip-value').textContent).toBe('1');

    // Switch to 5m.
    fireEvent.click(getByText('5m'));
    expect(within(getByTestId('chip-accepted_per_sec')).getByTestId('chip-value').textContent).toBe('1');

    // Switch to 1h.
    fireEvent.click(getByText('1h'));
    expect(within(getByTestId('chip-accepted_per_sec')).getByTestId('chip-value').textContent).toBe('1');

    // aria-pressed tracks the active window.
    expect(getByText('1h').getAttribute('aria-pressed')).toBe('true');
    expect(getByText('1m').getAttribute('aria-pressed')).toBe('false');
  });
});

describe('StatsStrip — total-mode sparkline suppression (R2)', () => {
  it('omits the sparkline DOM node for the cost chip in window-total mode', () => {
    const { getByTestId, getByRole } = renderWithQuery(<StatsStrip />);
    // 1 priced sample → cost renders as a window TOTAL → its rate sparkline must not render.
    pushMetrics(metrics(1, {}, { m1: { latency_samples: 8, usage_samples: 8, priced_samples: 1, cost_per_min: 0.3, interval_duration_ms: 60_000 } }));
    fireEvent.click(getByRole('button', { name: /More metrics/ }));
    const cost = getByTestId('chip-cost_per_min');
    expect(within(cost).queryByTestId('sparkline')).toBeNull();
    // A rate-mode chip keeps its sparkline.
    expect(within(getByTestId('chip-accepted_per_sec')).getByTestId('sparkline')).toBeTruthy();
  });
});

describe('CompactStatsStrip (U4)', () => {
  it('renders the one-line pulse: operational status + p50 + tok/s + expand control', () => {
    const { getByTestId } = renderWithQuery(<CompactStatsStrip onExpand={() => {}} />);
    pushMetrics(metrics(1, {
      engine_throughput: {
        generated_tokens_per_sec: 287.5,
        sampled_at_ms: 900,
        measured_sources: 2,
        total_sources: 2,
        coverage: 'full',
      },
    }, { m1: { p50_ms: 721 } }));
    const strip = getByTestId('stats-strip-compact');
    expect(within(strip).getByTestId('compact-p50').textContent).toBe('721 ms');
    expect(within(strip).getByTestId('compact-toks').textContent).toBe('288 tok/s');
    const expand = within(strip).getByTestId('stats-strip-expand');
    expect(expand.getAttribute('aria-expanded')).toBe('false');
    // The full strip's chip grid does NOT render in compact mode.
    expect(within(strip).queryByTestId('primary-metrics')).toBeNull();
  });

  it('invokes onExpand and marks a retained idle window inline', () => {
    const onExpand = vi.fn();
    const { getByTestId } = renderWithQuery(<CompactStatsStrip onExpand={onExpand} />);
    pushMetrics(metrics(1, {
      engine_throughput: {
        generated_tokens_per_sec: 152,
        sampled_at_ms: 900,
        measured_sources: 1,
        total_sources: 1,
        coverage: 'full',
      },
      last_activity: { at_ms: 500, instant: win({ p50_ms: 721, active_streams_now: 1 }) },
    }, { m1: { active_streams_now: 0, latency_samples: 0, p50_ms: null } }));
    const strip = getByTestId('stats-strip-compact');
    expect(strip.getAttribute('data-metrics-state')).toBe('retained');
    // Retained interval supplies the p50; the retained qualifier is VISIBLE, not hover-only.
    expect(within(strip).getByTestId('compact-p50').textContent).toBe('721 ms');
    expect(strip.textContent).toContain('· last active');
    fireEvent.click(within(strip).getByTestId('stats-strip-expand'));
    expect(onExpand).toHaveBeenCalledTimes(1);
  });

  it('never falls back to live REST metrics while seeking (review HIGH)', () => {
    const { getByTestId, queryClient } = renderWithQuery(<CompactStatsStrip onExpand={() => {}} />);
    // A live REST answer is cached — the trap the seek gate must not fall into.
    queryClient.setQueryData(['metrics'], metrics(9, {}, { m1: { p50_ms: 111 } }));
    act(() => {
      dashboardStore.setState({ connection: 'seeking', metrics: null, seekAtMs: 500 });
    });
    const strip = getByTestId('stats-strip-compact');
    // Frozen cut has no metrics → honest dashes, NOT the live 111ms p50.
    expect(within(strip).getByTestId('compact-p50').textContent).toBe('—');
    expect(strip.getAttribute('data-metrics-state')).toBe('empty');
  });

  it('renders honest dashes with no metrics at all', () => {
    const { getByTestId } = renderWithQuery(<CompactStatsStrip onExpand={() => {}} />);
    const strip = getByTestId('stats-strip-compact');
    expect(within(strip).getByTestId('compact-p50').textContent).toBe('—');
    expect(within(strip).getByTestId('compact-toks').textContent).toBe('—');
  });
});
