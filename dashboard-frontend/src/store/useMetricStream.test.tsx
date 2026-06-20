import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { act, cleanup, render } from '@testing-library/react';
import { useMetricStream } from './useMetricStream';
import { dashboardStore } from './dashboardStore';
import type { MetricsResponse } from '../api/types';

function metrics(seq: number, reqs: number): MetricsResponse {
  const w = { reqs_per_sec: reqs, active_streams: 1, error_pct: 0, p50: 1, p95: 2, p99: 3, tokens_per_sec: 10, cost_per_min: 0.1 };
  return { metrics_seq: seq, ...w, windows: { m1: { ...w }, m5: { ...w }, h1: { ...w } } };
}

/** A probe component that records every folded sample's reqs_per_sec. */
function Probe({ folded, seed }: { folded: number[]; seed?: MetricsResponse | null }) {
  useMetricStream((s) => folded.push(s.reqs_per_sec), seed);
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

  it('primes from the seed only while the ring is empty (no stale seed after live samples)', () => {
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
});
