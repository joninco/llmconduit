import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { StrictMode } from 'react';
import { act, cleanup, render, fireEvent, waitFor, within } from '@testing-library/react';
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
    expect(getByTestId('river-tps').getAttribute('data-quality')).toBe('estimated');
    // State is written in text, not encoded only by the colored dot.
    expect(getByTestId('river-status').textContent).toContain('streaming');
    expect(getByTestId('river-tokens').textContent).toMatch(/≈\d+ tok/);
    expect(getByTestId('river-tokens').getAttribute('data-quality')).toBe('estimated');
    // Tool card rendered.
    expect(within(getByTestId('river-tools')).getByText('search()')).not.toBeNull();
    // Reasoning EXPANDED by default (it streams first), collapsible via the toggle.
    expect(getByTestId('river-reasoning').textContent).toBe('because');
    fireEvent.click(getByTestId('river-reasoning-toggle'));
    expect(queryByTestId('river-reasoning')).toBeNull();
    void container;
  });

  it('renders reasoning ABOVE the output (it streams first)', () => {
    const [river] = buildRivers([
      upsert('r1', 'gpt-4o'),
      seg('r1', 'reasoning', 'thinking first', 1000),
      seg('r1', 'output', 'answer after', 1100),
    ]);
    const { getByTestId } = render(<River river={river!} />);
    const reasoning = getByTestId('river-reasoning');
    const output = getByTestId('river-output');
    // The reasoning node precedes the output node in document order.
    expect(reasoning.compareDocumentPosition(output) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  it('shows the explicit trimmed marker only when a memory cap head-trimmed the river', () => {
    const [river] = buildRivers([upsert('r1', 'm'), seg('r1', 'output', 'hi', 1000)]);
    const { queryByTestId, rerender } = render(<River river={river!} />);
    expect(queryByTestId('river-truncated')).toBeNull();
    rerender(<River river={{ ...river!, truncated: true }} />);
    expect(queryByTestId('river-truncated')).not.toBeNull();
  });

  it('a completed river shows NO cursor', () => {
    const [river] = buildRivers([
      upsert('r1', 'm', 'completed'),
      seg('r1', 'output', 'done', 1000),
    ]);
    const { getByTestId, queryByTestId } = render(<River river={river!} />);
    expect(queryByTestId('river-cursor')).toBeNull();
    expect(getByTestId('river-status').textContent).toContain('complete');
    expect(getByTestId('river-elapsed').textContent).toContain('0ms');
  });

  it('ticks elapsed time while streaming and cleans up its clock on unmount', () => {
    vi.useFakeTimers();
    try {
      vi.setSystemTime(100_000);
      const started = upsert('r1', 'm');
      if (started.type !== 'request_upsert') throw new Error('fixture shape');
      started.request.started_at_ms = 97_500;
      const [river] = buildRivers([
        started,
        seg('r1', 'output', 'some streamed text', 99_000),
        seg('r1', 'output', ' continues', 99_500),
      ]);
      const view = render(<River river={river!} />);
      expect(view.getByTestId('river-elapsed').textContent).toContain('2.5s');
      act(() => { vi.advanceTimersByTime(1_000); });
      expect(view.getByTestId('river-elapsed').textContent).toContain('3.5s');
      view.unmount();
      expect(vi.getTimerCount()).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it('labels a failed stream and surfaces its terminal error instead of relying on red', () => {
    const error = 'upstream timed out before the first token';
    const [river] = buildRivers([
      upsert('r1', 'm'),
      seg('r1', 'tool', `failed: ${error}`, 1_500),
      { type: 'request_status', response_id: 'r1', status: 'failed', completed_at_ms: 2_000, error },
    ]);
    const { getByTestId, queryByTestId } = render(<River river={river!} />);
    expect(getByTestId('river-status').textContent).toContain('failed');
    expect(getByTestId('river-error').textContent).toContain(error);
    expect(getByTestId('river-error').getAttribute('data-quality')).toBe('measured');
    // monitor.rs also emits `failed: <error>` as a tool segment; suppress that exact duplicate.
    expect(queryByTestId('river-tools')).toBeNull();
  });

  it('makes an unreported failure detail explicit', () => {
    const [river] = buildRivers([
      upsert('r1', 'm'),
      { type: 'request_status', response_id: 'r1', status: 'failed', completed_at_ms: 2_000, error: null },
    ]);
    const { getByTestId } = render(<River river={river!} />);
    expect(getByTestId('river-error').textContent).toContain('No failure detail was reported.');
    expect(getByTestId('river-error').getAttribute('data-quality')).toBe('unavailable');
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

  it('fullscreen uses a modal dialog, handles the native Escape/cancel path, and restores focus', async () => {
    const { getByTestId } = render(<TheaterView />);
    pushMonitor([upsert('r1', 'm'), seg('r1', 'output', 'a', 1000)]);
    const view = getByTestId('theater-view');
    expect(view.getAttribute('data-fullscreen')).toBeNull();
    const trigger = getByTestId('theater-fullscreen-toggle');
    act(() => trigger.focus());
    fireEvent.click(trigger);

    const dialog = getByTestId('theater-view') as HTMLDialogElement;
    expect(dialog.tagName).toBe('DIALOG');
    expect(dialog.open).toBe(true);
    expect(dialog.getAttribute('data-fullscreen')).toBe('true');
    await waitFor(() => expect(dialog.contains(document.activeElement)).toBe(true));

    // Browsers dispatch `cancel` when Escape is pressed on a modal dialog. The handler prevents the
    // implicit close, exits through React state, then focuses the logical fullscreen trigger again.
    fireEvent(dialog, new Event('cancel', { bubbles: false, cancelable: true }));
    await waitFor(() => expect(getByTestId('theater-view').tagName).toBe('DIV'));
    const restoredTrigger = getByTestId('theater-fullscreen-toggle');
    await waitFor(() => expect(document.activeElement).toBe(restoredTrigger));
  });

  it('keeps the FULL stream text past the monitor ring cap — no tokens deleted from the top', () => {
    // 600 segments blow past MONITOR_RING_CAP (500). The old ring-rebuild lost the head (the
    // theater visibly deleted tokens from the top); the incremental fold keeps everything.
    const { getByTestId } = render(<TheaterView />);
    const msgs: DebugWsMessage[] = [upsert('r1', 'gpt-4o')];
    for (let i = 0; i < 600; i++) msgs.push(seg('r1', 'output', `w${i} `, 1000 + i));
    pushMonitor(msgs);
    const text = getByTestId('river-output').textContent ?? '';
    expect(text).toContain('w0 '); // the head survives eviction
    expect(text).toContain('w599 '); // the tail is appended
    expect(dashboardStore.getState().monitor.length).toBeLessThanOrEqual(500); // ring still capped
    // No cap was hit — the honest trimmed marker must NOT show.
    expect(getByTestId('river-body').querySelector('[data-testid="river-truncated"]')).toBeNull();
  });

  it('reasoning that streamed BEFORE ring eviction still renders (it is not lost to the ring)', () => {
    const { getByTestId } = render(<TheaterView />);
    const msgs: DebugWsMessage[] = [upsert('r1', 'gpt-4o'), seg('r1', 'reasoning', 'the plan', 999)];
    // 550 output segments push the reasoning segment out of the ring.
    for (let i = 0; i < 550; i++) msgs.push(seg('r1', 'output', 'x', 1000 + i));
    pushMonitor(msgs);
    expect(getByTestId('river-reasoning').textContent).toBe('the plan');
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

describe('TheaterView — retains the latest response and ages out older terminal rivers', () => {
  it('keeps the latest completed response indefinitely, including after monitor removal, and ticks its stale clock', () => {
    vi.useFakeTimers();
    try {
      const finishedAt = Date.now();
      const { getByTestId } = render(<TheaterView />);
      act(() => {
        dashboardStore.getState().pushMonitor(upsert('r1', 'gpt-4o', 'running'), 1);
        dashboardStore.getState().pushMonitor(seg('r1', 'output', 'hi', 1000), 1);
        dashboardStore.getState().setConnection('live');
      });
      act(() => {
        dashboardStore.getState().pushMonitor({ type: 'request_status', response_id: 'r1', status: 'completed', completed_at_ms: finishedAt, error: null }, 1);
      });
      expect(getByTestId('river').getAttribute('data-status')).toBe('completed');
      expect(getByTestId('river').getAttribute('data-retained')).toBe('true');
      expect(getByTestId('river-retained-badge').textContent).toContain('last response');
      expect(getByTestId('theater-view').getAttribute('data-stale')).toBe('true');
      expect(getByTestId('theater-stale-age').textContent).toBe('00:00');

      // It remains after the old 4.4s removal boundary and the fixed-width clock keeps advancing.
      act(() => { vi.advanceTimersByTime(65_000); });
      expect(getByTestId('river').getAttribute('data-retained')).toBe('true');
      expect(getByTestId('theater-stale-age').textContent).toBe('01:05');

      // Backend monitor retention may later remove the request; Theater's bounded one-response
      // cache intentionally survives that automatic cleanup.
      act(() => {
        dashboardStore.getState().pushMonitor({ type: 'request_remove', response_id: 'r1', reason: 'evicted' }, 2);
      });
      expect(getByTestId('river-output').textContent).toContain('hi');
      expect(getByTestId('river').getAttribute('data-retained')).toBe('true');
    } finally {
      vi.useRealTimers();
    }
  });

  it('a new running stream releases the old retained response to fade, then becomes the retained response', () => {
    vi.useFakeTimers();
    try {
      const firstFinishedAt = Date.now();
      const { getAllByTestId, getByTestId, queryByTestId } = render(<TheaterView />);
      act(() => {
        dashboardStore.getState().pushMonitor(upsert('r1', 'gpt-4o', 'running'), 1);
        dashboardStore.getState().pushMonitor(seg('r1', 'output', 'first', firstFinishedAt), 1);
        dashboardStore.getState().pushMonitor({ type: 'request_status', response_id: 'r1', status: 'completed', completed_at_ms: firstFinishedAt, error: null }, 1);
        dashboardStore.getState().setConnection('live');
      });
      expect(getByTestId('river').getAttribute('data-retained')).toBe('true');

      // Running activity hides the stale clock and releases r1 into its absolute fade lifecycle.
      act(() => {
        dashboardStore.getState().pushMonitor(upsert('r2', 'llama', 'running'), 2);
        dashboardStore.getState().pushMonitor(seg('r2', 'output', 'second', Date.now()), 2);
      });
      expect(getAllByTestId('river')).toHaveLength(2);
      expect(queryByTestId('theater-stale-state')).toBeNull();
      act(() => { vi.advanceTimersByTime(4_400); });
      expect(getAllByTestId('river')).toHaveLength(1);
      expect(getByTestId('river').getAttribute('data-river-id')).toBe('r2');

      // Once r2 terminates it becomes the new permanent idle tile.
      act(() => {
        dashboardStore.getState().pushMonitor({ type: 'request_status', response_id: 'r2', status: 'completed', completed_at_ms: Date.now(), error: null }, 3);
      });
      expect(getByTestId('river').getAttribute('data-retained')).toBe('true');
      expect(getByTestId('river-output').textContent).toContain('second');
      expect(getByTestId('theater-stale-state')).not.toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it('a long-finished response remains across Theater remounts with its absolute age', () => {
    vi.useFakeTimers();
    try {
      const finishedAt = Date.now() - 10_000;
      pushMonitorBare([
        upsert('r1', 'm', 'running'),
        seg('r1', 'output', 'done', finishedAt),
        { type: 'request_status', response_id: 'r1', status: 'completed', completed_at_ms: finishedAt, error: null },
        { type: 'request_remove', response_id: 'r1', reason: 'evicted' },
      ]);
      const { getByTestId, unmount } = render(<TheaterView />);
      expect(getByTestId('theater-stale-age').textContent).toBe('00:10');
      expect(getByTestId('river-output').textContent).toContain('done');
      unmount();
      const remount = render(<TheaterView />);
      expect(remount.getByTestId('theater-stale-age').textContent).toBe('00:10');
      expect(remount.getByTestId('river').getAttribute('data-retained')).toBe('true');
    } finally {
      vi.useRealTimers();
    }
  });

  it('clears the stale clock interval on unmount (StrictMode-safe)', () => {
    vi.useFakeTimers();
    try {
      const finishedAt = Date.now();
      pushMonitorBare([
        upsert('r1', 'm', 'running'),
        seg('r1', 'output', 'done', finishedAt),
        { type: 'request_status', response_id: 'r1', status: 'completed', completed_at_ms: finishedAt, error: null },
      ]);
      const { unmount } = render(<TheaterView />);
      expect(vi.getTimerCount()).toBeGreaterThan(0);
      unmount();
      expect(vi.getTimerCount()).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });
});

describe('TheaterView — SEEK shows historical summaries, NOT a live river', () => {
  function frozenFlow(over: Partial<FlowSummary>): FlowSummary {
    return {
      api_call_id: 'api_x', method: 'POST', uri: '/v1/responses', status: 'completed',
      started_ms: 1_700_000_000_000, revision: 1, cost_confidence: 'unavailable', ...over,
    };
  }

  it('renders the "deltas not replayed" banner + terminal summaries from the frozen cut', () => {
    // Live monitor activity exists, but a seek must NOT replay it as a river.
    pushMonitorBare([upsert('r1', 'gpt-4o'), seg('r1', 'output', 'live text', 1000)]);
    const { getByTestId, queryByTestId } = render(<TheaterView />);
    act(() => {
      dashboardStore.getState().applySeekCut({
        rows: [frozenFlow({ model_served: 'gpt-4o', terminal_reason: 'response.completed', usage: { prompt: 10, completion: 20, total: 30, cached: 0, reasoning: 0 } })],
        cursors: { flow_seq: 1, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 , backend_metrics_seq: 0},
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
