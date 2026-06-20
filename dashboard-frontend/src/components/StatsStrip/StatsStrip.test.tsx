import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { act, cleanup, fireEvent, within } from '@testing-library/react';
import { StatsStrip } from './StatsStrip';
import { dashboardStore } from '../../store/dashboardStore';
import type { MetricsResponse, MetricWindow } from '../../api/types';
import { renderWithQuery, resetWorld } from '../testHarness';
import { CHIP_METRICS } from './chips';

function win(over: Partial<MetricWindow> = {}): MetricWindow {
  return {
    reqs_per_sec: 4.2, active_streams: 3, error_pct: 1.1,
    p50: 180, p95: 920, p99: 1840, tokens_per_sec: 142, cost_per_min: 0.21,
    ...over,
  };
}

function metrics(seq: number, over: Partial<MetricsResponse> = {}, windows?: { m1?: Partial<MetricWindow>; m5?: Partial<MetricWindow>; h1?: Partial<MetricWindow> }): MetricsResponse {
  return {
    metrics_seq: seq,
    reqs_per_sec: 4.2, active_streams: 3, error_pct: 1.1,
    p50: 180, p95: 920, p99: 1840, tokens_per_sec: 142, cost_per_min: 0.21,
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
