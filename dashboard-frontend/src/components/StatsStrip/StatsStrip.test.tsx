import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { act, cleanup, fireEvent, within } from '@testing-library/react';
import { StatsStrip } from './StatsStrip';
import { dashboardStore, type LiveBaseline } from '../../store/dashboardStore';
import type { MetricsResponse, MetricWindow } from '../../api/types';
import { renderWithQuery, resetWorld } from '../testHarness';
import { CHIP_METRICS } from './chips';

function win(over: Partial<MetricWindow> = {}): MetricWindow {
  return {
    reqs_per_sec: 4.2, active_streams: 3, error_pct: 1.1,
    p50: 180, p95: 920, p99: 1840, tokens_per_sec: 142, cost_per_min: 0.21,
    samples: 252,
    ...over,
  };
}

function metrics(seq: number, over: Partial<MetricsResponse> = {}, windows?: { m1?: Partial<MetricWindow>; m5?: Partial<MetricWindow>; h1?: Partial<MetricWindow> }): MetricsResponse {
  return {
    metrics_seq: seq,
    reqs_per_sec: 4.2, active_streams: 3, error_pct: 1.1,
    p50: 180, p95: 920, p99: 1840, tokens_per_sec: 142, cost_per_min: 0.21,
    samples: 252,
    windows: { m1: win(windows?.m1), m5: win(windows?.m5), h1: win(windows?.h1) },
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
  vi.restoreAllMocks();
});

describe('StatsStrip — chips', () => {
  it('renders every chip with a tabular-nums value', () => {
    const { getByTestId } = renderWithQuery(<StatsStrip />);
    pushMetrics(metrics(1, {}, { m1: { reqs_per_sec: 7.5, tokens_per_sec: 1500 } }));
    for (const key of CHIP_METRICS) {
      const chip = getByTestId(`chip-${key}`);
      const value = within(chip).getByTestId('chip-value');
      expect(value.className).toContain('tabular-nums');
    }
    // The m1 window value surfaced (req/s chip shows 7.5).
    expect(within(getByTestId('chip-reqs_per_sec')).getByTestId('chip-value').textContent).toBe('7.5');
    // tokens compaction.
    expect(within(getByTestId('chip-tokens_per_sec')).getByTestId('chip-value').textContent).toBe('1.5k');
  });

  it('renders a sparkline per metric and updates from successive MetricTick frames', () => {
    const { getByTestId, getAllByTestId } = renderWithQuery(<StatsStrip />);
    pushMetrics(metrics(1, {}, { m1: { reqs_per_sec: 1 } }));
    // One sparkline per chip metric.
    expect(getAllByTestId('sparkline')).toHaveLength(CHIP_METRICS.length);

    // A second, third frame deepens the series; the chip value reflects the latest.
    pushMetrics(metrics(2, {}, { m1: { reqs_per_sec: 2 } }));
    pushMetrics(metrics(3, {}, { m1: { reqs_per_sec: 3 } }));
    expect(within(getByTestId('chip-reqs_per_sec')).getByTestId('chip-value').textContent).toBe('3.0');
  });

  it('turns the err% chip red above the 5% threshold', () => {
    const { getByTestId } = renderWithQuery(<StatsStrip />);
    pushMetrics(metrics(1, {}, { m1: { error_pct: 1.1 } }));
    expect(within(getByTestId('chip-error_pct')).getByTestId('chip-value').className).not.toContain('text-status-down');
    pushMetrics(metrics(2, {}, { m1: { error_pct: 9.9 } }));
    expect(within(getByTestId('chip-error_pct')).getByTestId('chip-value').className).toContain('text-status-down');
  });

  it('shows a delta arrow once a second sample arrives', () => {
    const { getByTestId } = renderWithQuery(<StatsStrip />);
    pushMetrics(metrics(1, {}, { m1: { reqs_per_sec: 4 } }));
    pushMetrics(metrics(2, {}, { m1: { reqs_per_sec: 6 } }));
    expect(within(getByTestId('chip-reqs_per_sec')).getByTestId('chip-delta').textContent).toBe('▲');
  });

  // Gap 01 — don't lie with zeros (end-to-end through the component).
  it('renders latency/tok-s/cost as "—" (not 0) for a zero-sample window, keeping req/s numeric', () => {
    const { getByTestId } = renderWithQuery(<StatsStrip />);
    // Traffic in flight (req/s 2.5, 4 active) but NOTHING finalized → m1.samples = 0.
    pushMetrics(metrics(1, {}, {
      m1: { samples: 0, error_pct: 0, p50: 0, p95: 0, p99: 0, tokens_per_sec: 0, cost_per_min: 0, reqs_per_sec: 2.5, active_streams: 4 },
    }));
    const val = (k: string) => within(getByTestId(`chip-${k}`)).getByTestId('chip-value').textContent;
    expect(val('p50')).toBe('—');
    expect(val('p95')).toBe('—');
    expect(val('tokens_per_sec')).toBe('—');
    expect(val('cost_per_min')).toBe('—');
    expect(val('error_pct')).toBe('—');
    // The genuinely-measured req/s + the live active count stay numeric.
    expect(val('reqs_per_sec')).toBe('2.5');
    expect(val('active_streams')).toBe('4.0');
  });
});

describe('StatsStrip — seek isolation (D11 R5)', () => {
  it('reads the FROZEN snapshot value while seeking, flat delta, then returns to live on resume', () => {
    const { getByTestId } = renderWithQuery(<StatsStrip />);
    const value = () => within(getByTestId('chip-reqs_per_sec')).getByTestId('chip-value').textContent;
    const delta = () => within(getByTestId('chip-reqs_per_sec')).getByTestId('chip-delta').textContent;
    // Two LIVE ticks build the live history; chip reads the latest live (2) with an UP delta.
    pushMetrics(metrics(1, {}, { m1: { reqs_per_sec: 1 } }));
    pushMetrics(metrics(2, {}, { m1: { reqs_per_sec: 2 } }));
    expect(value()).toBe('2.0');
    expect(delta()).toBe('▲');

    // SEEK: install a FROZEN cut (reqs/s = 42) atomically with connection='seeking'.
    let baseline!: LiveBaseline;
    act(() => {
      baseline = dashboardStore.getState().captureLiveBaseline();
      dashboardStore.getState().applySeekCut({
        rows: [],
        cursors: { flow_seq: 0, metrics_seq: 50, topology_seq: 0, monitor_seq: 3 },
        atMs: Date.now(),
        monitorSeq: 3,
        metrics: metrics(50, {}, { m1: { reqs_per_sec: 42 } }),
        topology: null,
      });
    });
    // Chip CURRENT value now reads the FROZEN snapshot (42) — as-of the seeked moment — with a FLAT
    // delta (a point-in-time snapshot is not a live trend; the frozen cut never folded into history).
    expect(value()).toBe('42.0');
    expect(delta()).toBe('·');

    // RESUME: restore baseline (live seq 2) atomically with connection='live' → chip back on live.
    act(() => dashboardStore.getState().restoreLiveBaseline(baseline));
    expect(value()).toBe('2.0');

    // A NEW live tick continues the live history cleanly — the frozen 42 left no trace.
    pushMetrics(metrics(3, {}, { m1: { reqs_per_sec: 3 } }));
    expect(value()).toBe('3.0');
    expect(delta()).toBe('▲'); // 3 > 2 (the prior LIVE sample, not the frozen 42)
  });
});

describe('StatsStrip — window selector', () => {
  it('switches the source window so chips read metrics.windows.{m1,m5,h1}', () => {
    const { getByTestId, getByText } = renderWithQuery(<StatsStrip />);
    // Distinct values per window so we can prove the switch.
    pushMetrics(metrics(1, {}, { m1: { reqs_per_sec: 1 }, m5: { reqs_per_sec: 5 }, h1: { reqs_per_sec: 9 } }));
    // Default window is 1m.
    expect(within(getByTestId('chip-reqs_per_sec')).getByTestId('chip-value').textContent).toBe('1.0');

    // Switch to 5m.
    fireEvent.click(getByText('5m'));
    expect(within(getByTestId('chip-reqs_per_sec')).getByTestId('chip-value').textContent).toBe('5.0');

    // Switch to 1h.
    fireEvent.click(getByText('1h'));
    expect(within(getByTestId('chip-reqs_per_sec')).getByTestId('chip-value').textContent).toBe('9.0');

    // aria-pressed tracks the active window.
    expect(getByText('1h').getAttribute('aria-pressed')).toBe('true');
    expect(getByText('1m').getAttribute('aria-pressed')).toBe('false');
  });
});
