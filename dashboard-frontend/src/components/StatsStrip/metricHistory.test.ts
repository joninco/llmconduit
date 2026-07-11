import { describe, expect, it } from 'vitest';
import type { HistoryPoint, InstantMetricSample, MetricsResponse } from '../../api/types';
import { appendTick, emptyHistory, horizon, latest, mergeRetained, previous, seriesFor } from './metricHistory';

function instant(rate: number | null): InstantMetricSample {
  return {
    interval_duration_ms: 1000, ready: true,
    accepted_requests: rate ?? 0, accepted_per_sec: rate,
    terminal_requests: 0, terminal_per_sec: 0,
    successes: 0, failures: 0, failure_pct: null,
    cancellations: 0, cancellation_pct: null, active_streams_now: 0,
    latency_samples: 0, p50_ms: null, p95_ms: null, p99_ms: null,
    p50_quality: 'unavailable', p95_quality: 'unavailable', p99_quality: 'unavailable',
    quantile_method: 'log_histogram_nearest_rank', max_relative_error: 0.062,
    latency_overflow_count: 0, usage_samples: 0, reported_tokens_per_sec: null,
    usage_anomaly_count: 0, priced_samples: 0, cost_per_min: null,
    cost_confidence: 'unavailable',
  };
}

function tick(seq: number, t: number, rate: number): MetricsResponse {
  return { metrics_seq: seq, generated_at_ms: t, instant: instant(rate) };
}

describe('instant metric history', () => {
  it('slices one point series by real timestamp horizons without changing current', () => {
    let history = emptyHistory();
    history = appendTick(history, tick(1, 0, 1));
    history = appendTick(history, tick(2, 4 * 60_000, 2));
    history = appendTick(history, tick(3, 5 * 60_000, 3));
    expect(horizon(history, 'm1').map((point) => point.instant.accepted_per_sec)).toEqual([2, 3]);
    expect(horizon(history, 'm5')).toHaveLength(3);
    expect(latest(history)?.accepted_per_sec).toBe(3);
  });

  it('merges retained/live points, deduplicates identity, sorts, and prunes past one hour', () => {
    const retained: HistoryPoint[] = [{
      cut_id: 1, at_ms: 3_600_000, cursors: { flow_seq: 0, metrics_seq: 2, topology_seq: 0, monitor_seq: 0, backend_metrics_seq: 0 }, instant: instant(2),
    }];
    let live = appendTick(emptyHistory(), tick(2, 3_600_000, 99));
    live = appendTick(live, tick(3, 7_200_001, 3));
    const merged = mergeRetained(live, retained);
    expect(merged).toHaveLength(1);
    expect(merged[0]?.instant.accepted_per_sec).toBe(3);
  });

  it('uses the preceding historical point for seek deltas and emits gaps', () => {
    let history = appendTick(emptyHistory(), tick(1, 1_000, 1));
    history = appendTick(history, tick(2, 2_000, 2));
    history = appendTick(history, { metrics_seq: 3, generated_at_ms: 3_000, instant: instant(null) });
    expect(previous(history, 3_000)?.accepted_per_sec).toBe(2);
    const series = seriesFor(history, 'accepted_per_sec');
    expect(series.times).toEqual([1, 2, 3]);
    expect(Number.isNaN(series.values[2])).toBe(true);
  });
});
