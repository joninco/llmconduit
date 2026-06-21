import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { StrictMode } from 'react';
import { act, cleanup, render, fireEvent, within } from '@testing-library/react';
import { TheaterView } from './TheaterView';
import { River } from '../components/viz/River';
import { buildRivers } from '../components/viz/riverModel';
import { dashboardStore } from '../store/dashboardStore';
import type { DebugWsMessage, DebugRequestStatus, FlowSummary } from '../api/types';

function upsert(id: string, model: string, status: DebugRequestStatus = 'running'): DebugWsMessage {
  return {
    type: 'request_upsert',
    request: {
      response_id: id, model, started_at_ms: 1000, updated_at_ms: 1000, completed_at_ms: null, status,
      stats: { input_items: 0, tool_count: 0, turn_count: 0, user_messages: 0, assistant_messages: 0, system_messages: 0, developer_messages: 0, reasoning_items: 0, function_calls: 0, function_outputs: 0, tool_items: 0, input_chars: 0, instructions_chars: 0 },
      error: null,
    },
  };
}
function seg(id: string, kind: 'output' | 'reasoning' | 'tool', text: string, ts: number): DebugWsMessage {
  return { type: 'segment_append', response_id: id, segment: { timestamp_ms: ts, kind, text } };
}

function pushMonitor(msgs: DebugWsMessage[]): void {
  act(() => {
    for (const m of msgs) dashboardStore.getState().pushMonitor(m, 1);
    dashboardStore.getState().setConnection('live');
  });
}

beforeEach(() => {
  dashboardStore.getState().reset();
  cleanup();
});
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe('River — renders output/reasoning/tool deltas with tok/s + cursor', () => {
  it('shows output (with cursor while running), tool cards, and collapsible reasoning', () => {
    // Monotonic timestamps (a real stream): 40 output chars across a 2s window → ≈10 tok / 2s.
    const [river] = buildRivers([
      upsert('r1', 'gpt-4o'),
      seg('r1', 'reasoning', 'because', 1000),
      seg('r1', 'tool', 'search()', 1100),
      seg('r1', 'output', 'x'.repeat(40), 1200),
      seg('r1', 'output', '', 3000),
    ]);
    const { container, getByTestId, queryByTestId } = render(<River river={river!} />);
    expect(getByTestId('river-output').textContent).toContain('x'.repeat(40));
    // Running → blinking cursor present; tok/s meter shows a positive derived rate.
    expect(getByTestId('river-cursor')).not.toBeNull();
    expect(river!.tokensPerSec).toBeGreaterThan(0);
    expect(getByTestId('river-tps').textContent).toMatch(/[\d.]+ tok\/s/);
    // Tool card rendered.
    expect(within(getByTestId('river-tools')).getByText('search()')).not.toBeNull();
    // Reasoning collapsed by default, revealed on toggle.
    expect(queryByTestId('river-reasoning')).toBeNull();
    fireEvent.click(getByTestId('river-reasoning-toggle'));
    expect(getByTestId('river-reasoning').textContent).toBe('because');
    void container;
  });

  it('a completed river shows NO cursor', () => {
    const [river] = buildRivers([
      upsert('r1', 'm', 'completed'),
      seg('r1', 'output', 'done', 1000),
    ]);
    const { queryByTestId } = render(<River river={river!} />);
    expect(queryByTestId('river-cursor')).toBeNull();
  });
});

describe('TheaterView — live rivers from segment_append, auto-grid, fullscreen', () => {
  it('renders one river per active stream from the monitor ring', () => {
    const { getAllByTestId, getByTestId } = render(<TheaterView />);
    pushMonitor([
      upsert('r1', 'gpt-4o'), seg('r1', 'output', 'hi', 1000),
      upsert('r2', 'llama'), seg('r2', 'output', 'yo', 1000),
    ]);
    expect(getAllByTestId('river')).toHaveLength(2);
    // 2 streams → 2-column grid.
    expect(getByTestId('theater-grid').getAttribute('data-cols')).toBe('2');
  });

  it('auto-grid: 1 → 1col, 3 → 3col', () => {
    const { getByTestId, rerender } = render(<TheaterView />);
    pushMonitor([upsert('r1', 'm'), seg('r1', 'output', 'a', 1000)]);
    expect(getByTestId('theater-grid').getAttribute('data-cols')).toBe('1');
    pushMonitor([upsert('r2', 'm'), seg('r2', 'output', 'b', 1000), upsert('r3', 'm'), seg('r3', 'output', 'c', 1000)]);
    rerender(<TheaterView />);
    expect(getByTestId('theater-grid').getAttribute('data-cols')).toBe('3');
  });

  it('fullscreen toggle flips the container into the fixed overlay', () => {
    const { getByTestId } = render(<TheaterView />);
    pushMonitor([upsert('r1', 'm'), seg('r1', 'output', 'a', 1000)]);
    const view = getByTestId('theater-view');
    expect(view.getAttribute('data-fullscreen')).toBeNull();
    fireEvent.click(getByTestId('theater-fullscreen-toggle'));
    expect(getByTestId('theater-view').getAttribute('data-fullscreen')).toBe('true');
  });

  it('empty monitor → an explicit empty state, no grid', () => {
    const { getByTestId, queryByTestId } = render(<TheaterView />);
    act(() => dashboardStore.getState().setConnection('live'));
    expect(getByTestId('theater-empty')).not.toBeNull();
    expect(queryByTestId('theater-grid')).toBeNull();
  });
});

describe('TheaterView — StrictMode-safe', () => {
  it('mounts/unmounts/remounts cleanly under StrictMode (no error, one view)', () => {
    pushMonitorBare([upsert('r1', 'm'), seg('r1', 'output', 'hi', 1000)]);
    const { container, unmount } = render(
      <StrictMode>
        <TheaterView />
      </StrictMode>,
    );
    expect(container.querySelectorAll('[data-testid="theater-view"]').length).toBe(1);
    unmount();
    expect(container.querySelectorAll('[data-testid="theater-view"]').length).toBe(0);
  });
});

/** Seed the monitor ring WITHOUT a render mounted (for the StrictMode mount test). */
function pushMonitorBare(msgs: DebugWsMessage[]): void {
  for (const m of msgs) dashboardStore.getState().pushMonitor(m, 1);
  dashboardStore.getState().setConnection('live');
}

describe('TheaterView — terminated rivers linger-then-fade-then-remove (finding 4)', () => {
  it('keeps a completed river during the linger, fades it, then removes it', () => {
    vi.useFakeTimers();
    try {
      const { getByTestId, queryByTestId } = render(<TheaterView />);
      // A running river is present.
      act(() => {
        dashboardStore.getState().pushMonitor(upsert('r1', 'gpt-4o', 'running'), 1);
        dashboardStore.getState().pushMonitor(seg('r1', 'output', 'hi', 1000), 1);
        dashboardStore.getState().setConnection('live');
      });
      expect(getByTestId('river')).not.toBeNull();

      // Flip it terminal — it must NOT vanish immediately (it lingers).
      act(() => {
        dashboardStore.getState().pushMonitor({ type: 'request_status', response_id: 'r1', status: 'completed', completed_at_ms: 2000, error: null }, 1);
      });
      expect(getByTestId('river').getAttribute('data-status')).toBe('completed');
      expect(getByTestId('river').getAttribute('data-exiting')).toBeNull();

      // After the linger (4s) it enters the fade phase (data-exiting), still rendered.
      act(() => { vi.advanceTimersByTime(4_000); });
      expect(getByTestId('river').getAttribute('data-exiting')).toBe('true');

      // After the fade (0.4s) it is removed from the grid entirely.
      act(() => { vi.advanceTimersByTime(400); });
      expect(queryByTestId('river')).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it('clears its linger timers on unmount (no leaked timer fires post-unmount — StrictMode-safe)', () => {
    vi.useFakeTimers();
    try {
      const { unmount } = render(<TheaterView />);
      act(() => {
        dashboardStore.getState().pushMonitor(upsert('r1', 'm', 'completed'), 1);
        dashboardStore.getState().pushMonitor(seg('r1', 'output', 'done', 1000), 1);
        dashboardStore.getState().setConnection('live');
      });
      unmount();
      // Advancing past linger+fade after unmount must not throw (timers were cleared, no setState
      // on an unmounted tree).
      expect(() => act(() => { vi.advanceTimersByTime(10_000); })).not.toThrow();
    } finally {
      vi.useRealTimers();
    }
  });
});

describe('TheaterView — SEEK shows historical summaries, NOT a live river', () => {
  function frozenFlow(over: Partial<FlowSummary>): FlowSummary {
    return {
      api_call_id: 'api_x', method: 'POST', uri: '/v1/responses', status: 'completed',
      started_ms: 1_700_000_000_000, ...over,
    };
  }

  it('renders the "deltas not replayed" banner + terminal summaries from the frozen cut', () => {
    // Live monitor activity exists, but a seek must NOT replay it as a river.
    pushMonitorBare([upsert('r1', 'gpt-4o'), seg('r1', 'output', 'live text', 1000)]);
    const { getByTestId, queryByTestId } = render(<TheaterView />);
    act(() => {
      dashboardStore.getState().applySeekCut({
        rows: [frozenFlow({ model_served: 'gpt-4o', terminal_reason: 'response.completed', usage: { prompt: 10, completion: 20, total: 30, cached: 0, reasoning: 0 } })],
        cursors: { flow_seq: 1, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 },
        atMs: 1_700_000_000_000,
        monitorSeq: 0,
        metrics: null,
        topology: null,
      });
    });
    // The explicit body-free-snapshot affordance is shown.
    expect(getByTestId('theater-historical-banner').textContent).toContain('deltas not replayed');
    // A terminal summary card (NOT a live river) is rendered for the frozen flow.
    const card = getByTestId('theater-summary-card');
    expect(card.textContent).toContain('gpt-4o');
    expect(card.textContent).toContain('30 tokens');
    // No live river / grid leaks the post-seek monitor text into the frozen view.
    expect(queryByTestId('river')).toBeNull();
    expect(queryByTestId('theater-grid')).toBeNull();
  });
});
