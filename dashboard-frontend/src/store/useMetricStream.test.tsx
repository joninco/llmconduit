import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { act, cleanup, render } from '@testing-library/react';
import { useMetricStream } from './useMetricStream';
import { dashboardStore, type LiveBaseline } from './dashboardStore';
import type { MetricsResponse } from '../api/types';

function metrics(seq: number, reqs: number): MetricsResponse {
  const w = { window_seconds: 60, observed_seconds: 60, warm: true, accepted_requests: 5,
    accepted_per_sec: reqs, terminal_requests: 5, terminal_per_sec: reqs, successes: 5,
    failures: 0, failure_pct: 0, cancellations: 0, cancellation_pct: 0, active_streams_now: 1,
    latency_samples: 5, p50_ms: 1, p95_ms: null, p99_ms: null,
    quantile_method: 'log_histogram_nearest_rank' as const, max_relative_error: 0.062,
    latency_overflow_count: 0, latency_quality: 'measured' as const, usage_samples: 5,
    reported_tokens_per_sec: 10, usage_anomaly_count: 0, priced_samples: 5,
    cost_per_min: 0.1, cost_confidence: 'estimated' as const };
  return { metrics_seq: seq, generated_at_ms: seq * 1000, headline_window: 'm1', windows: { m1: { ...w }, m5: { ...w, window_seconds: 300 }, h1: { ...w, window_seconds: 3600 } } };
}

/** A probe component that records every folded sample's accepted_per_sec. */
function Probe({ folded, seed }: { folded: number[]; seed?: MetricsResponse | null }) {
  useMetricStream((s) => folded.push(s.windows.m1.accepted_per_sec), seed);
  return null;
}

beforeEach(() => dashboardStore.getState().reset());
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe('useMetricStream', () => {
  it('folds EVERY distinct sample even when several land in one synchronous tick (no batching loss)', () => {
    const folded: number[] = [];
    render(<Probe folded={folded} />);
    act(() => {
      // Five store updates in ONE act — React would coalesce a selector to the last; the direct
      // subscription must capture all five.
      [1, 2, 3, 4, 5].forEach((r, i) => dashboardStore.getState().setMetrics(metrics(i + 1, r)));
    });
    expect(folded).toEqual([1, 2, 3, 4, 5]);
  });

  it('dedupes a repeated seq', () => {
    const folded: number[] = [];
    render(<Probe folded={folded} />);
    act(() => {
      dashboardStore.getState().setMetrics(metrics(7, 1));
      dashboardStore.getState().setMetrics(metrics(7, 99)); // same seq → ignored
      dashboardStore.getState().setMetrics(metrics(8, 2));
    });
    expect(folded).toEqual([1, 2]);
  });

  it('primes from the seed only while the ring is empty (no stale seed after live latency_samples)', () => {
    const folded: number[] = [];
    // Seed (seq 1) primes the empty ring.
    const { rerender } = render(<Probe folded={folded} seed={metrics(1, 11)} />);
    expect(folded).toEqual([11]);

    // A live sample folds.
    act(() => dashboardStore.getState().setMetrics(metrics(2, 22)));
    expect(folded).toEqual([11, 22]);

    // A LATER stale seed (seq 1 again, different value) must NOT re-fold out of order.
    rerender(<Probe folded={folded} seed={metrics(1, 999)} />);
    expect(folded).toEqual([11, 22]);
  });

  it('does NOT fold the FROZEN seek cut, and resume continues cleanly from live ticks (D11 R5)', () => {
    const folded: number[] = [];
    render(<Probe folded={folded} />);

    // Two LIVE ticks fold normally.
    act(() => {
      dashboardStore.getState().setMetrics(metrics(1, 10));
      dashboardStore.getState().setMetrics(metrics(2, 20));
    });
    expect(folded).toEqual([10, 20]);

    // SEEK: capture the live baseline, then atomically install the frozen historical cut (its own
    // metrics_seq + reqs/s) AND flip connection='seeking'. The frozen sample (99) must be SKIPPED —
    // it is historical, not a live tick, so it must not enter the live history/hill ring.
    let baseline!: LiveBaseline;
    act(() => {
      baseline = dashboardStore.getState().captureLiveBaseline();
      dashboardStore.getState().applySeekCut({
        rows: [],
        cursors: { flow_seq: 0, metrics_seq: 77, topology_seq: 0, monitor_seq: 5 , backend_metrics_seq: 0},
        atMs: Date.now(),
        monitorSeq: 5,
        metrics: metrics(77, 99), // FROZEN historical metrics (distinct seq + value)
        topology: null,
      });
    });
    expect(dashboardStore.getState().connection).toBe('seeking');
    expect(folded).toEqual([10, 20]); // frozen cut NOT folded

    // Even a second frozen metrics mutation while seeking (e.g. another snapshot) is skipped.
    act(() => dashboardStore.getState().setMetrics(metrics(88, 999)));
    expect(folded).toEqual([10, 20]);

    // RESUME: restore the captured live baseline (metrics_seq 2) atomically with connection='live'.
    // The seq dedup drops it (already folded — we never advanced past it while skipping the cut), so
    // NO duplicate baseline point lands in the live ring.
    act(() => dashboardStore.getState().restoreLiveBaseline(baseline));
    expect(dashboardStore.getState().connection).toBe('live');
    expect(folded).toEqual([10, 20]);

    // The next LIVE tick continues the history cleanly (no seek pollution, no gap).
    act(() => dashboardStore.getState().setMetrics(metrics(3, 30)));
    expect(folded).toEqual([10, 20, 30]);
  });
});
