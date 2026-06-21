/**
 * `useSankeyWindow` (D12, finding 2) — proves the rolling-window accumulator counts usage GROWTH as
 * timestamped deltas, never a flow's cumulative lifetime total, prunes by the window, and skips the
 * frozen seek cut (LIVE-only, like `useMetricStream`).
 */
import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { renderHook, act, cleanup } from '@testing-library/react';
import { useSankeyWindow } from './useSankeyWindow';
import { dashboardStore } from './dashboardStore';
import type { FlowSummary, Usage } from '../api/types';

function usage(over: Partial<Usage> = {}): Usage {
  return { prompt: 0, completion: 0, total: 0, cached: 0, reasoning: 0, ...over };
}
function flow(over: Partial<FlowSummary> = {}): FlowSummary {
  return {
    api_call_id: 'api_x', method: 'POST', uri: '/v1/responses', status: 'completed',
    started_ms: 1_700_000_000_000, model_served: 'gpt-4o', upstream_target: 'vllm-a', ...over,
  };
}

/** A mutable clock so pruning is deterministic. */
function clockAt(ref: { now: number }): () => number {
  return () => ref.now;
}

beforeEach(() => {
  dashboardStore.getState().reset();
  // Start LIVE so the accumulator folds (it skips while seeking).
  dashboardStore.getState().setConnection('live');
  cleanup();
});
afterEach(() => cleanup());

describe('useSankeyWindow — timestamped deltas, not cumulative totals (finding 2)', () => {
  it("a flow's first observation contributes its cumulative as ONE delta; a repeat with no growth adds nothing", () => {
    const ref = { now: 1000 };
    act(() => {
      dashboardStore.getState().upsertFlow(flow({ api_call_id: 'a', usage: usage({ prompt: 1_000_000, total: 1_000_000 }) }));
    });
    const { result } = renderHook(() => useSankeyWindow(30_000, clockAt(ref)));
    // First fold: the 1M cumulative IS the initial delta.
    expect(result.current.deltasRef.current).toHaveLength(1);
    expect(result.current.deltasRef.current[0]!.total).toBe(1_000_000);

    // A re-emit with the SAME total (no growth) records NO new delta — the lifetime total is never
    // re-counted as a fresh window contribution.
    act(() => {
      dashboardStore.getState().patchUsage('a', usage({ prompt: 1_000_000, total: 1_000_000 }));
    });
    expect(result.current.deltasRef.current).toHaveLength(1);
  });

  it('only the GROWTH between snapshots is recorded as the delta', () => {
    const ref = { now: 1000 };
    act(() => {
      dashboardStore.getState().upsertFlow(flow({ api_call_id: 'a', usage: usage({ prompt: 100, completion: 0, total: 100 }) }));
    });
    const { result } = renderHook(() => useSankeyWindow(30_000, clockAt(ref)));
    expect(result.current.deltasRef.current[0]!.total).toBe(100);

    // Grow to 250 total → the new delta is the +150 increment, not the 250 cumulative.
    act(() => {
      dashboardStore.getState().patchUsage('a', usage({ prompt: 200, completion: 50, total: 250 }));
    });
    const totals = result.current.deltasRef.current.map((d) => d.total);
    expect(totals).toEqual([100, 150]);
    // The summed windowed tokens equal the latest cumulative (no double counting).
    expect(totals.reduce((a, b) => a + b, 0)).toBe(250);
  });

  it('prunes deltas older than the window as the clock advances', () => {
    const ref = { now: 1000 };
    act(() => {
      dashboardStore.getState().upsertFlow(flow({ api_call_id: 'a', usage: usage({ prompt: 100, total: 100 }) }));
    });
    const { result } = renderHook(() => useSankeyWindow(30_000, clockAt(ref)));
    expect(result.current.deltasRef.current).toHaveLength(1);

    // Advance the clock past the window, then trigger a re-fold via a fresh growth on another flow.
    ref.now = 1000 + 40_000;
    act(() => {
      dashboardStore.getState().upsertFlow(flow({ api_call_id: 'b', usage: usage({ prompt: 5, total: 5 }) }));
    });
    // The first delta (ts=1000) is now > 30s old → pruned; only the new one (ts=41000) remains.
    const ids = result.current.deltasRef.current.map((d) => d.total);
    expect(ids).toEqual([5]);
  });

  it('skips folding while seeking (the frozen cut never enters the live ring — D11 R5)', () => {
    const ref = { now: 1000 };
    const { result } = renderHook(() => useSankeyWindow(30_000, clockAt(ref)));
    // Enter seek with a frozen flow carrying usage; the accumulator must NOT fold it.
    act(() => {
      dashboardStore.getState().applySeekCut({
        rows: [flow({ api_call_id: 'frozen', usage: usage({ prompt: 999, total: 999 }) })],
        cursors: { flow_seq: 1, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 },
        atMs: 1000, monitorSeq: 0, metrics: null, topology: null,
      });
    });
    expect(result.current.deltasRef.current).toHaveLength(0);
  });

  it('a route remount does NOT restamp an existing cumulative total as fresh traffic (finding 3)', () => {
    const ref = { now: 1000 };
    act(() => {
      dashboardStore.getState().upsertFlow(flow({ api_call_id: 'long', usage: usage({ prompt: 1_000_000, total: 1_000_000 }) }));
    });
    // First mount folds the 1M cumulative once (its initial delta).
    const first = renderHook(() => useSankeyWindow(30_000, clockAt(ref)));
    expect(first.result.current.deltasRef.current.map((d) => d.total)).toEqual([1_000_000]);

    // Time passes; that 1M ages out of the window (the flow is idle, no new growth).
    ref.now = 1000 + 40_000;
    // Unmount (navigate away) then remount (navigate back) — the singleton baseline SURVIVES, so the
    // remount must NOT re-emit the 1M cumulative as a fresh Date.now()-stamped band.
    first.unmount();
    const second = renderHook(() => useSankeyWindow(30_000, clockAt(ref)));
    // The aged-out delta is pruned and NO fresh 1M band is restamped → empty window.
    expect(second.result.current.deltasRef.current).toEqual([]);
  });

  it('a seek round-trip (live→seeking→live) does NOT restamp the resumed flows (finding 3)', () => {
    const ref = { now: 1000 };
    act(() => {
      dashboardStore.getState().upsertFlow(flow({ api_call_id: 'a', usage: usage({ prompt: 500, total: 500 }) }));
    });
    const { result } = renderHook(() => useSankeyWindow(30_000, clockAt(ref)));
    expect(result.current.deltasRef.current.map((d) => d.total)).toEqual([500]);

    // Enter seek (frozen cut), then resume LIVE. The live store again holds flow 'a' at the SAME
    // cumulative 500 (continuity preserved across the round-trip).
    act(() => {
      dashboardStore.getState().applySeekCut({
        rows: [flow({ api_call_id: 'frozen', usage: usage({ prompt: 999, total: 999 }) })],
        cursors: { flow_seq: 1, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 },
        atMs: 1000, monitorSeq: 0, metrics: null, topology: null,
      });
    });
    expect(result.current.deltasRef.current.map((d) => d.total)).toEqual([500]); // frozen never folds
    act(() => {
      // Resume via the real baseline-restore path: live store holds 'a' at the unchanged total 500.
      dashboardStore.getState().restoreLiveBaseline({
        cursors: { flow_seq: 1, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 },
        flows: new Map([['a', flow({ api_call_id: 'a', usage: usage({ prompt: 500, total: 500 }) })]]),
        flowOrder: ['a'], metrics: null, topologyNodes: [], topologyEdges: [], priceTable: {},
        monitor: [], monitorSeqs: [],
      });
    });
    // 'a' is unchanged across the round-trip → diffs to 0 → NO new band restamped (still just [500]).
    expect(result.current.deltasRef.current.map((d) => d.total)).toEqual([500]);

    // A REAL post-resume growth on 'a' (500 → 650) folds as the +150 increment only.
    act(() => {
      dashboardStore.getState().patchUsage('a', usage({ prompt: 650, total: 650 }));
    });
    expect(result.current.deltasRef.current.map((d) => d.total)).toEqual([500, 150]);
  });

  it('resets baselines + ring on a connEpoch change (fresh session never diffs a stale total)', () => {
    const ref = { now: 1000 };
    act(() => {
      dashboardStore.getState().upsertFlow(flow({ api_call_id: 'a', usage: usage({ prompt: 100, total: 100 }) }));
    });
    const { result } = renderHook(() => useSankeyWindow(30_000, clockAt(ref)));
    expect(result.current.deltasRef.current).toHaveLength(1);

    // A teardown/reset crosses a boundary (epoch bump). A subsequent fresh snapshot with the SAME
    // api_call_id at a LOWER cumulative must be treated as a new flow (delta = its full total), not
    // a negative diff against the pre-reset baseline.
    act(() => {
      dashboardStore.getState().reset();
      dashboardStore.getState().applySnapshot({
        cursors: { flow_seq: 1, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 },
        flows: [flow({ api_call_id: 'a', usage: usage({ prompt: 30, total: 30 }) })],
        metrics: null, topology: null,
      });
      dashboardStore.getState().setConnection('live');
    });
    const totals = result.current.deltasRef.current.map((d) => d.total);
    expect(totals).toEqual([30]);
  });
});
