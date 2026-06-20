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

export interface DashboardState {
  connection: ConnectionState;
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
  setCursor: (domain: keyof SeqCursors, seq: number) => void;
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

export const dashboardStore = createStore<DashboardState>((set) => ({
  connection: 'idle',
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
      if (connection === 'seeking') {
        return s.connection === 'seeking'
          ? { connection }
          : { connection, seekAtMs: Date.now(), seekMonitorSeq: s.cursors.monitor_seq };
      }
      return { connection, seekAtMs: null, seekMonitorSeq: null };
    }),

  enterSeek: (atMs) =>
    set((s) => ({ connection: 'seeking', seekAtMs: atMs, seekMonitorSeq: s.cursors.monitor_seq })),

  setCursor: (domain, seq) =>
    set((s) => ({ cursors: { ...s.cursors, [domain]: seq } })),

  applySnapshot: (snap) =>
    set(() => {
      const flows = new Map<string, FlowSummary>();
      const flowOrder: string[] = [];
      for (const f of snap.flows) {
        flows.set(f.api_call_id, f);
        flowOrder.push(f.api_call_id);
      }
      return {
        cursors: snap.cursors,
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
    set({
      connection: 'idle',
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
    }),
}));

export type DashboardStore = typeof dashboardStore;
