/**
 * Live WS state, held in a zustand vanilla store and bridged to React 18 concurrent
 * rendering via `useSyncExternalStore` (see ./hooks.ts). The `DashboardSocket` feeds
 * this store; components subscribe with selector hooks.
 *
 * Frames mutate slices here; the per-domain dedup lives in the socket (D7), so by the
 * time a payload reaches a setter it is known to be fresh.
 */
import { createStore } from 'zustand/vanilla';
import type {
  FlowStatusPayload,
  FlowSummary,
  MetricsResponse,
  ProviderHealth,
  TopologyEdge,
  TopologyResponse,
  DebugWsMessage,
  Usage,
  SeqCursors,
} from '../api/types';

export type ConnectionState = 'idle' | 'connecting' | 'live' | 'seeking' | 'closed' | 'error';

/**
 * An immutable capture of the LIVE mutable slices, taken the instant a seek pauses the feed (D11
 * R2 finding 1). `applySeekCut` overwrites those slices with the FROZEN historical cut, so on LIVE
 * resume the store no longer reflects the live rows/cursors/monitor that existed at the pause. The
 * socket captures this baseline on `seek()` and `restoreLiveBaseline`s it on `live()` (when no
 * reconnect snapshot re-baselined the store), so resuming reflects the up-to-date live state before
 * the shadow-buffered frames replay — the frozen cut is gone, nothing stays rewound.
 */
export interface LiveBaseline {
  cursors: SeqCursors;
  flows: Map<string, FlowSummary>;
  flowOrder: string[];
  metrics: MetricsResponse | null;
  topologyNodes: ProviderHealth[];
  topologyEdges: TopologyEdge[];
  priceTable: TopologyResponse['price_table'];
  monitor: DebugWsMessage[];
  monitorSeqs: number[];
}

export interface DashboardState {
  connection: ConnectionState;
  /**
   * MONOTONIC connection-transition generation. Bumped on EVERY connection transition that changes
   * which store the mutable slices belong to (live ↔ seek ↔ teardown ↔ fresh snapshot). Unlike the
   * `connection` STRING (which is reusable — `live → seeking → live` returns to `'live'`), this only
   * ever increases, so an in-flight optimistic mutation captured at dispatch can detect that the app
   * has since crossed a boundary and refuse to write into a now-foreign store (useFlowDetail kill,
   * finding 1). A no-op transition (same state re-applied) does NOT bump it.
   */
  connEpoch: number;
  /** Last applied per-domain seq (mirrors the socket's dedup cursors for display). */
  cursors: SeqCursors;

  /**
   * The FROZEN time-travel cut, captured when seek begins; null while LIVE.
   *  - `seekAtMs`: the wall-clock instant the cut was taken (the snapshot `at_ms`). Elapsed for an
   *    OPEN flow derives from THIS, never `Date.now()`, so the frozen view does not tick forward
   *    past the seeked instant (finding 6 / seek coherence).
   *  - `seekMonitorSeq`: the `monitor_seq` cursor at the cut. The inspector's monitor join is
   *    bounded to it so NO segment/event/status that arrived after the cut leaks into the frozen
   *    deltas/timeline (finding 1).
   */
  seekAtMs: number | null;
  seekMonitorSeq: number | null;

  /** Flow rows keyed by `api_call_id` (insertion order preserved via `flowOrder`). */
  flows: Map<string, FlowSummary>;
  flowOrder: string[];

  metrics: MetricsResponse | null;

  topologyNodes: ProviderHealth[];
  topologyEdges: TopologyEdge[];
  priceTable: TopologyResponse['price_table'];

  /** Recent monitor (debug) messages, capped ring for the theater/inspector. */
  monitor: DebugWsMessage[];
  /**
   * Per-message arrival `monitor_seq`, sliced in LOCKSTEP with `monitor` (same length/order). A
   * monitor frame's seq stamps every message it carried, so the inspector can EXCLUDE post-cut
   * messages while seeking by dropping any whose stamp is `> seekMonitorSeq` (finding 1).
   */
  monitorSeqs: number[];

  // -- mutations (called by the socket) --
  setConnection: (s: ConnectionState) => void;
  /** Enter the frozen seek cut: marks `seeking` and captures `at_ms` + the `monitor_seq` cut. */
  enterSeek: (atMs: number) => void;
  /**
   * ATOMICALLY install a time-travel snapshot cut (D11 finding 1). In ONE update it replaces the
   * rows + cursors with the FROZEN snapshot AND flips `connection='seeking'` AND stamps
   * `seekAtMs`/`seekMonitorSeq` from the cut — so the store is NEVER observed `seeking` while the
   * rows/cursors are still LIVE. The Scrubber pauses live applying on drag-start but defers
   * exposing `'seeking'` until the fetched cut lands here, closing the window where a seek listener
   * (D10) could render live/current rows or unbounded monitor data under `connection==='seeking'`.
   */
  applySeekCut: (cut: {
    rows: FlowSummary[];
    cursors: SeqCursors;
    atMs: number;
    monitorSeq: number;
    metrics: MetricsResponse | null;
    topology: TopologyResponse | null;
  }) => void;
  setCursor: (domain: keyof SeqCursors, seq: number) => void;
  /**
   * Capture the current LIVE mutable slices (D11 R2 finding 1). The socket calls this on `seek()`
   * BEFORE any `applySeekCut` overwrites the store with the frozen cut, so `live()` can restore the
   * up-to-date live rows/cursors/monitor instead of resuming on the frozen historical cut. Returns a
   * defensively-copied snapshot (the live Maps/arrays keep mutating after capture).
   */
  captureLiveBaseline: () => LiveBaseline;
  /**
   * ATOMICALLY reinstall a previously-captured live baseline (D11 R2 finding 1). One `set` restores
   * the live rows/cursors/monitor and clears any seek freeze (`seekAtMs`/`seekMonitorSeq`) so the
   * frozen cut is fully gone; the socket then replays shadow-buffered frames on top. Crosses a
   * boundary (frozen cut → live store), so the monotonic epoch advances (finding 1).
   */
  restoreLiveBaseline: (baseline: LiveBaseline) => void;
  applySnapshot: (snap: {
    cursors: SeqCursors;
    flows: FlowSummary[];
    metrics: MetricsResponse | null;
    topology: TopologyResponse | null;
  }) => void;
  upsertFlow: (flow: FlowSummary) => void;
  /** Patch from a `flow_status` WS payload (keyed by `api_call_id`). */
  patchFlowStatus: (p: FlowStatusPayload) => void;
  /** Patch usage onto a flow by `api_call_id`. */
  patchUsage: (apiCallId: string, usage: Usage) => void;
  setMetrics: (m: MetricsResponse) => void;
  setTopology: (nodes: ProviderHealth[], edges: TopologyEdge[]) => void;
  /** Append a monitor message, stamped with the `monitor_seq` of the frame that delivered it. */
  pushMonitor: (msg: DebugWsMessage, seq?: number) => void;
  reset: () => void;
}

const MONITOR_RING_CAP = 500;

const emptyCursors = (): SeqCursors => ({
  flow_seq: 0,
  metrics_seq: 0,
  topology_seq: 0,
  monitor_seq: 0,
});

export const dashboardStore = createStore<DashboardState>((set, get) => ({
  connection: 'idle',
  connEpoch: 0,
  cursors: emptyCursors(),
  seekAtMs: null,
  seekMonitorSeq: null,
  flows: new Map(),
  flowOrder: [],
  metrics: null,
  topologyNodes: [],
  topologyEdges: [],
  priceTable: {},
  monitor: [],
  monitorSeqs: [],

  // Leaving 'seeking' (any non-seek state — typically 'live') DROPS the frozen cut so elapsed
  // resumes ticking and the monitor join unbounds. Entering 'seeking' directly via setConnection
  // (e.g. a test) captures the cut from the current cursor/clock; `enterSeek` is the explicit path.
  setConnection: (connection) =>
    set((s) => {
      // Re-applying the SAME state is a no-op for the epoch (no boundary crossed).
      if (connection === s.connection) return { connection };
      // Any real transition advances the monotonic epoch (finding 1).
      const connEpoch = s.connEpoch + 1;
      if (connection === 'seeking') {
        return { connection, connEpoch, seekAtMs: Date.now(), seekMonitorSeq: s.cursors.monitor_seq };
      }
      return { connection, connEpoch, seekAtMs: null, seekMonitorSeq: null };
    }),

  enterSeek: (atMs) =>
    set((s) => ({
      connection: 'seeking',
      // Entering seek always crosses a boundary (live store → frozen cut), so bump the epoch.
      connEpoch: s.connEpoch + 1,
      seekAtMs: atMs,
      seekMonitorSeq: s.cursors.monitor_seq,
    })),

  applySeekCut: (cut) =>
    set((s) => {
      const flows = new Map<string, FlowSummary>();
      const flowOrder: string[] = [];
      for (const f of cut.rows) {
        flows.set(f.api_call_id, f);
        flowOrder.push(f.api_call_id);
      }
      // ONE atomic update: frozen rows + cursors AND `connection='seeking'` AND the cut's
      // `seekAtMs`/`seekMonitorSeq` install together. `seekMonitorSeq` is the SNAPSHOT's
      // `monitor_seq` (the authoritative cut), not the live cursor — so the monitor join is bounded
      // to the moment the cut was taken, never to a live cursor that kept advancing pre-fetch.
      return {
        connection: 'seeking',
        // Crosses a boundary (whatever store → frozen cut); bump the monotonic epoch (finding 1).
        connEpoch: s.connEpoch + 1,
        cursors: cut.cursors,
        seekAtMs: cut.atMs,
        seekMonitorSeq: cut.monitorSeq,
        flows,
        flowOrder,
        metrics: cut.metrics,
        topologyNodes: cut.topology?.nodes ?? [],
        topologyEdges: cut.topology?.edges ?? [],
        priceTable: cut.topology?.price_table ?? {},
      };
    }),

  setCursor: (domain, seq) =>
    set((s) => ({ cursors: { ...s.cursors, [domain]: seq } })),

  // Read-only capture — defensively COPY the mutable Maps/arrays so the returned baseline is frozen
  // against later live mutation (a captured `Map`/array shared by reference would keep ticking).
  captureLiveBaseline: () => {
    const s = get();
    return {
      cursors: { ...s.cursors },
      flows: new Map(s.flows),
      flowOrder: [...s.flowOrder],
      metrics: s.metrics,
      topologyNodes: [...s.topologyNodes],
      topologyEdges: [...s.topologyEdges],
      priceTable: { ...s.priceTable },
      monitor: [...s.monitor],
      monitorSeqs: [...s.monitorSeqs],
    };
  },

  restoreLiveBaseline: (baseline) =>
    set((s) => ({
      // Crosses a boundary (frozen cut → live store); bump the monotonic epoch (finding 1).
      connEpoch: s.connEpoch + 1,
      // The frozen cut is fully gone — clear the seek freeze so elapsed ticks + the monitor unbounds.
      seekAtMs: null,
      seekMonitorSeq: null,
      cursors: { ...baseline.cursors },
      flows: new Map(baseline.flows),
      flowOrder: [...baseline.flowOrder],
      metrics: baseline.metrics,
      topologyNodes: [...baseline.topologyNodes],
      topologyEdges: [...baseline.topologyEdges],
      priceTable: { ...baseline.priceTable },
      monitor: [...baseline.monitor],
      monitorSeqs: [...baseline.monitorSeqs],
    })),

  applySnapshot: (snap) =>
    set((s) => {
      const flows = new Map<string, FlowSummary>();
      const flowOrder: string[] = [];
      for (const f of snap.flows) {
        flows.set(f.api_call_id, f);
        flowOrder.push(f.api_call_id);
      }
      return {
        cursors: snap.cursors,
        // A fresh snapshot re-establishes the authoritative LIVE store — a boundary an in-flight
        // optimistic mutation must not write across (it replaces every row), so bump the epoch.
        connEpoch: s.connEpoch + 1,
        // A fresh snapshot re-establishes the authoritative LIVE cut — clear any seek freeze.
        seekAtMs: null,
        seekMonitorSeq: null,
        flows,
        flowOrder,
        metrics: snap.metrics,
        topologyNodes: snap.topology?.nodes ?? [],
        topologyEdges: snap.topology?.edges ?? [],
        priceTable: snap.topology?.price_table ?? {},
      };
    }),

  upsertFlow: (flow) =>
    set((s) => {
      const flows = new Map(s.flows);
      const existed = flows.has(flow.api_call_id);
      flows.set(flow.api_call_id, flow);
      return {
        flows,
        flowOrder: existed ? s.flowOrder : [flow.api_call_id, ...s.flowOrder],
      };
    }),

  patchFlowStatus: (p) =>
    set((s) => {
      const flows = new Map(s.flows);
      const prev = flows.get(p.api_call_id);
      const next: FlowSummary = {
        api_call_id: p.api_call_id,
        response_id: p.response_id ?? prev?.response_id ?? null,
        method: prev?.method ?? 'POST',
        uri: prev?.uri ?? '',
        model_requested: p.model_requested ?? prev?.model_requested ?? null,
        model_served: p.model_served ?? prev?.model_served ?? null,
        upstream_target: p.upstream_target ?? prev?.upstream_target ?? null,
        usage: p.usage ?? prev?.usage ?? null,
        status: p.status,
        started_ms: prev?.started_ms ?? p.started_ms,
        finished_ms: prev?.finished_ms ?? null,
        elapsed_ms: p.elapsed_ms ?? prev?.elapsed_ms ?? null,
        terminal_reason: prev?.terminal_reason ?? null,
        cost: prev?.cost ?? null,
      };
      flows.set(p.api_call_id, next);
      return {
        flows,
        flowOrder: prev ? s.flowOrder : [p.api_call_id, ...s.flowOrder],
      };
    }),

  patchUsage: (apiCallId, usage) =>
    set((s) => {
      const prev = s.flows.get(apiCallId);
      if (!prev) return {};
      const flows = new Map(s.flows);
      flows.set(apiCallId, { ...prev, usage });
      return { flows };
    }),

  setMetrics: (metrics) => set({ metrics }),

  setTopology: (topologyNodes, topologyEdges) => set({ topologyNodes, topologyEdges }),

  pushMonitor: (msg, seq = 0) =>
    set((s) => {
      // `monitor` + `monitorSeqs` are sliced together so index i always pairs message↔arrival seq.
      const atCap = s.monitor.length >= MONITOR_RING_CAP;
      const drop = atCap ? s.monitor.length - MONITOR_RING_CAP + 1 : 0;
      const monitor = atCap ? [...s.monitor.slice(drop), msg] : [...s.monitor, msg];
      const monitorSeqs = atCap ? [...s.monitorSeqs.slice(drop), seq] : [...s.monitorSeqs, seq];
      return { monitor, monitorSeqs };
    }),

  reset: () =>
    set((s) => ({
      connection: 'idle',
      // Teardown clears the live store — a boundary an in-flight mutation must not write across
      // (finding 1). The epoch is the one slice that survives a reset (monotonic across the session).
      connEpoch: s.connEpoch + 1,
      cursors: emptyCursors(),
      seekAtMs: null,
      seekMonitorSeq: null,
      flows: new Map(),
      flowOrder: [],
      metrics: null,
      topologyNodes: [],
      topologyEdges: [],
      priceTable: {},
      monitor: [],
      monitorSeqs: [],
    })),
}));

export type DashboardStore = typeof dashboardStore;
