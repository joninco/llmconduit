/**
 * `useSankeyWindow` (D12, finding 2) — proves the rolling-window accumulator counts usage GROWTH as
 * timestamped deltas, never a flow's cumulative lifetime total, prunes by the window, and skips the
 * frozen seek cut (LIVE-only, like `useMetricStream`).
 */
import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { renderHook, act, cleanup } from '@testing-library/react';
import { useSankeyWindow, startSankeyFold, __resetSankeyFold } from './useSankeyWindow';
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
  // Drop the app-lifetime fold engine so each case re-installs it with its own injected clock and a
  // fresh ring/baselines (the engine is module-global and survives across tests otherwise).
  __resetSankeyFold();
  dashboardStore.getState().reset();
  // Start LIVE so the accumulator folds (it skips while seeking).
  dashboardStore.getState().setConnection('live');
  cleanup();
});
afterEach(() => {
  cleanup();
  __resetSankeyFold();
});

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

  it('resets baselines + ring on a teardown (reset → idle) so a fresh session never diffs a stale total', () => {
    const ref = { now: 1000 };
    act(() => {
      dashboardStore.getState().upsertFlow(flow({ api_call_id: 'a', usage: usage({ prompt: 100, total: 100 }) }));
    });
    const { result } = renderHook(() => useSankeyWindow(30_000, clockAt(ref)));
    expect(result.current.deltasRef.current).toHaveLength(1);

    // `reset()` enters the `idle` teardown state (continuity broken). A subsequent fresh snapshot with
    // the SAME api_call_id at a LOWER cumulative must be treated as a new flow (delta = its full
    // total), not a negative diff against the pre-reset baseline.
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

describe('useSankeyWindow — APP-LIFETIME fold, not mount-scoped (D12 R3)', () => {
  it('folds usage growth that happens WHILE NO SankeyView is mounted, stamped at its REAL arrival time', () => {
    const ref = { now: 1000 };
    // The app-lifetime engine starts at bootstrap — NOT on SankeyView mount. Start it with no view.
    startSankeyFold(30_000, clockAt(ref));
    act(() => {
      dashboardStore.getState().upsertFlow(flow({ api_call_id: 'a', usage: usage({ prompt: 100, total: 100 }) }));
    });

    // Usage GROWS while the Sankey is unmounted; the delta must be stamped NOW (ts=5000), the real
    // arrival instant — not deferred to a later remount.
    ref.now = 5000;
    act(() => {
      dashboardStore.getState().patchUsage('a', usage({ prompt: 400, total: 400 }));
    });

    // Mount the view only AFTER the growth already happened. It READS the maintained ring; it must NOT
    // restamp the +300 increment at the mount instant.
    ref.now = 9000;
    const { result } = renderHook(() => useSankeyWindow(30_000, clockAt(ref)));
    const deltas = result.current.deltasRef.current;
    expect(deltas.map((d) => d.total)).toEqual([100, 300]);
    // The growth delta carries its REAL arrival time (5000), not the 9000 remount instant.
    expect(deltas[1]!.ts).toBe(5000);
  });

  it('growth that happened while unmounted is AGED OUT by the 30s window at remount (real arrival time, not restamped)', () => {
    const ref = { now: 1000 };
    startSankeyFold(30_000, clockAt(ref));
    act(() => {
      dashboardStore.getState().upsertFlow(flow({ api_call_id: 'a', usage: usage({ prompt: 100, total: 100 }) }));
    });
    // A burst of growth lands at ts=2000 while no view is mounted.
    ref.now = 2000;
    act(() => {
      dashboardStore.getState().patchUsage('a', usage({ prompt: 900, total: 900 }));
    });

    // 40s pass with the Sankey still unmounted; both deltas (ts=1000, ts=2000) are now older than 30s.
    ref.now = 2000 + 40_000;
    const { result } = renderHook(() => useSankeyWindow(30_000, clockAt(ref)));
    // Read-time pruning ages them out by their REAL timestamps — NOT lumped into the window at the
    // remount instant (which is exactly the mount-scoped bug this fix removes).
    expect(result.current.deltasRef.current).toEqual([]);
  });

  it('a later mounted reader sees deltas the engine folded before any reader subscribed', () => {
    const ref = { now: 1000 };
    startSankeyFold(30_000, clockAt(ref));
    // Two distinct growth events arrive (still no reader): the engine folds both at their arrival times.
    act(() => {
      dashboardStore.getState().upsertFlow(flow({ api_call_id: 'a', usage: usage({ prompt: 50, total: 50 }) }));
    });
    ref.now = 3000;
    act(() => {
      dashboardStore.getState().patchUsage('a', usage({ prompt: 50, completion: 70, total: 120 }));
    });

    ref.now = 4000;
    const { result } = renderHook(() => useSankeyWindow(30_000, clockAt(ref)));
    const deltas = result.current.deltasRef.current;
    expect(deltas.map((d) => d.total)).toEqual([50, 70]);
    expect(deltas.map((d) => d.ts)).toEqual([1000, 3000]);
  });
});
