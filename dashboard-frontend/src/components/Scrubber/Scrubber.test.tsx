import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { act, cleanup, fireEvent, waitFor } from '@testing-library/react';
import { Scrubber } from './Scrubber';
import { dashboardStore } from '../../store/dashboardStore';
import { getConnection } from '../../api/connection';
import type { MetricsResponse, MetricWindow } from '../../api/types';
import { renderWithQuery, resetWorld } from '../testHarness';

function win(over: Partial<MetricWindow> = {}): MetricWindow {
  return {
    reqs_per_sec: 4.2, active_streams: 3, error_pct: 1.1,
    p50: 180, p95: 920, p99: 1840, tokens_per_sec: 142, cost_per_min: 0.21,
    ...over,
  };
}
function metrics(seq: number, reqs: number): MetricsResponse {
  return {
    metrics_seq: seq, reqs_per_sec: reqs, active_streams: 3, error_pct: 1.1,
    p50: 180, p95: 920, p99: 1840, tokens_per_sec: 142, cost_per_min: 0.21,
    windows: { m1: win({ reqs_per_sec: reqs }), m5: win(), h1: win() },
  };
}

/** Push N reqs/s samples into the store so the hill has a span (each a fresh seq). */
function seedHill(samples: number[]): void {
  act(() => {
    samples.forEach((r, i) => dashboardStore.getState().setMetrics(metrics(i + 1, r)));
  });
}

/** Give the track a real width so fraction math works (jsdom reports 0). */
function stubTrackWidth(width = 200): () => void {
  const original = HTMLElement.prototype.getBoundingClientRect;
  HTMLElement.prototype.getBoundingClientRect = function () {
    return { x: 0, y: 0, top: 0, left: 0, right: width, bottom: 40, width, height: 40, toJSON: () => ({}) } as DOMRect;
  };
  return () => { HTMLElement.prototype.getBoundingClientRect = original; };
}

/**
 * jsdom has NO `PointerEvent` constructor, so testing-library's `fireEvent.pointerX` falls back to
 * a base Event that DROPS `clientX`/`pointerId`. Polyfill it as a MouseEvent carrying those fields
 * (representative of a real browser) so the drag/hover coordinate math runs.
 */
function installPointerEvent(): () => void {
  const had = 'PointerEvent' in window;
  const prev = (window as { PointerEvent?: unknown }).PointerEvent;
  class PointerEventPolyfill extends window.MouseEvent {
    pointerId: number;
    constructor(type: string, init: PointerEventInit = {}) {
      super(type, init as MouseEventInit);
      this.pointerId = init.pointerId ?? 1;
    }
  }
  (window as { PointerEvent?: unknown }).PointerEvent = PointerEventPolyfill as unknown as typeof PointerEvent;
  (globalThis as { PointerEvent?: unknown }).PointerEvent = PointerEventPolyfill as unknown as typeof PointerEvent;
  return () => {
    if (had) (window as { PointerEvent?: unknown }).PointerEvent = prev;
    else delete (window as { PointerEvent?: unknown }).PointerEvent;
    (globalThis as { PointerEvent?: unknown }).PointerEvent = (window as { PointerEvent?: unknown }).PointerEvent;
  };
}

let restoreRect: (() => void) | null = null;
let restorePE: (() => void) | null = null;

beforeEach(() => {
  resetWorld({ mock: true });
  restoreRect = stubTrackWidth();
  restorePE = installPointerEvent();
});
afterEach(() => {
  cleanup();
  restoreRect?.();
  restoreRect = null;
  restorePE?.();
  restorePE = null;
  vi.restoreAllMocks();
  vi.useRealTimers();
});

describe('Scrubber — hill + hover', () => {
  it('renders the reqs/s hill path from history', () => {
    const { socket } = getConnection();
    const { getByTestId } = renderWithQuery(<Scrubber socket={socket} />);
    seedHill([1, 4, 2, 6, 3]);
    const hill = getByTestId('scrubber-hill').querySelector('path')!;
    // A non-trivial area path (multiple line segments + close) is drawn.
    expect(hill.getAttribute('d')).toBeTruthy();
    expect(hill.getAttribute('d')!.split('L').length).toBeGreaterThan(3);
  });

  it('hover shows a tooltip with the time + reqs/s at the point', () => {
    const { socket } = getConnection();
    const { getByTestId, queryByTestId } = renderWithQuery(<Scrubber socket={socket} />);
    seedHill([1, 4, 2, 6, 3]);
    expect(queryByTestId('scrubber-tooltip')).toBeNull();
    fireEvent.pointerMove(getByTestId('scrubber-track'), { clientX: 100 });
    const tip = getByTestId('scrubber-tooltip');
    expect(tip.textContent).toMatch(/req\/s/);
    expect(tip.textContent).toMatch(/\d{2}:\d{2}:\d{2}/); // HH:MM:SS clock
  });
});

describe('Scrubber — seek + LIVE', () => {
  it('clicking/dragging pauses live WS, fetches /snapshot?at=, and applies the frozen cut', async () => {
    const { socket, client } = getConnection();
    const seekSpy = vi.spyOn(socket, 'seek');
    const snapSpy = vi.spyOn(client, 'snapshot');
    const applySpy = vi.spyOn(dashboardStore.getState(), 'applySnapshot');

    const { getByTestId } = renderWithQuery(<Scrubber socket={socket} />);
    seedHill([1, 4, 2, 6, 3]);

    // Click partway along the track → enter seek.
    act(() => {
      fireEvent.pointerDown(getByTestId('scrubber-track'), { clientX: 80, pointerId: 1 });
    });
    // Live WS applying is paused immediately (shadow-buffer).
    expect(seekSpy).toHaveBeenCalled();
    expect(socket.isPaused()).toBe(true);
    expect(dashboardStore.getState().connection).toBe('seeking');

    // The snapshot fetch fires (after the rAF frame) and the frozen cut is broadcast.
    await waitFor(() => expect(snapSpy).toHaveBeenCalled());
    await waitFor(() => expect(applySpy).toHaveBeenCalled());
    // After apply + re-enter-seek, the store still reads as seeking with a frozen instant.
    await waitFor(() => {
      const st = dashboardStore.getState();
      expect(st.connection).toBe('seeking');
      expect(st.seekAtMs).not.toBeNull();
    });
  });

  it('rapid drag moves coalesce to a bounded number of fetches (no request storm)', async () => {
    vi.useFakeTimers();
    const { socket, client } = getConnection();
    const snapSpy = vi.spyOn(client, 'snapshot').mockResolvedValue({
      cursors: { flow_seq: 0, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 }, at_ms: Date.now(), summaries: [], metrics: null, topology: null,
    });
    const { getByTestId } = renderWithQuery(<Scrubber socket={socket} />);
    act(() => {
      [1, 4, 2, 6, 3, 5, 7, 2, 8].forEach((r, i) => dashboardStore.getState().setMetrics(metrics(i + 1, r)));
    });

    const track = getByTestId('scrubber-track');
    act(() => {
      fireEvent.pointerDown(track, { clientX: 10, pointerId: 1 });
      // 40 rapid moves WITHIN the same animation frame.
      for (let i = 0; i < 40; i++) fireEvent.pointerMove(track, { clientX: 10 + i * 4, pointerId: 1 });
    });
    // Flush the single coalesced rAF frame (jsdom shims rAF on a timer).
    await act(async () => {
      vi.advanceTimersByTime(32);
    });
    // 40 moves + the down → ONE coalesced fetch this frame (NOT ~41).
    expect(snapSpy.mock.calls.length).toBeLessThanOrEqual(1);
  });

  it('the LIVE toggle resumes the socket (replays buffered frames)', () => {
    const { socket } = getConnection();
    const liveSpy = vi.spyOn(socket, 'live');
    const { getByTestId } = renderWithQuery(<Scrubber socket={socket} />);
    seedHill([1, 4, 2]);

    // Enter seek first so the LIVE toggle is shown.
    act(() => {
      socket.seek();
      dashboardStore.getState().enterSeek(Date.now());
    });
    fireEvent.click(getByTestId('live-toggle'));
    expect(liveSpy).toHaveBeenCalled();
  });
});

describe('Scrubber — prefers-reduced-motion', () => {
  it('does not pulse the live indicator when reduced motion is set', () => {
    vi.stubGlobal('matchMedia', (q: string) => ({
      matches: q.includes('prefers-reduced-motion'),
      media: q, addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {}, onchange: null, dispatchEvent: () => false,
    }));
    const { socket } = getConnection();
    const { getByTestId } = renderWithQuery(<Scrubber socket={socket} />);
    // The live indicator's dot must NOT carry the pulse animation class under reduced motion.
    const dot = getByTestId('live-indicator').querySelector('span[aria-hidden]')!;
    expect(dot.className).not.toContain('animate-pulse');
    vi.unstubAllGlobals();
  });
});
