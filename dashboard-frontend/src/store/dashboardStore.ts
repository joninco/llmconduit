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

  /** Flow rows keyed by response_id (insertion order preserved via `flowOrder`). */
  flows: Map<string, FlowSummary>;
  flowOrder: string[];

  metrics: MetricsResponse | null;

  topologyNodes: ProviderHealth[];
  topologyEdges: TopologyEdge[];
  priceTable: TopologyResponse['price_table'];

  /** Recent monitor (debug) messages, capped ring for the theater/inspector. */
  monitor: DebugWsMessage[];

  // -- mutations (called by the socket) --
  setConnection: (s: ConnectionState) => void;
  setCursor: (domain: keyof SeqCursors, seq: number) => void;
  applySnapshot: (snap: {
    cursors: SeqCursors;
    flows: FlowSummary[];
    metrics: MetricsResponse | null;
    topology: TopologyResponse | null;
  }) => void;
  upsertFlow: (flow: FlowSummary) => void;
  patchFlowStatus: (p: {
    response_id: string;
    status: FlowSummary['status'];
    served_model: string;
    upstream_target: string;
    usage: Usage | null;
    elapsed_ms: number;
  }) => void;
  patchUsage: (response_id: string, usage: Usage) => void;
  setMetrics: (m: MetricsResponse) => void;
  setTopology: (nodes: ProviderHealth[], edges: TopologyEdge[]) => void;
  pushMonitor: (msg: DebugWsMessage) => void;
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
  flows: new Map(),
  flowOrder: [],
  metrics: null,
  topologyNodes: [],
  topologyEdges: [],
  priceTable: {},
  monitor: [],

  setConnection: (connection) => set({ connection }),

  setCursor: (domain, seq) =>
    set((s) => ({ cursors: { ...s.cursors, [domain]: seq } })),

  applySnapshot: (snap) =>
    set(() => {
      const flows = new Map<string, FlowSummary>();
      const flowOrder: string[] = [];
      for (const f of snap.flows) {
        flows.set(f.response_id, f);
        flowOrder.push(f.response_id);
      }
      return {
        cursors: snap.cursors,
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
      const existed = flows.has(flow.response_id);
      flows.set(flow.response_id, flow);
      return {
        flows,
        flowOrder: existed ? s.flowOrder : [flow.response_id, ...s.flowOrder],
      };
    }),

  patchFlowStatus: (p) =>
    set((s) => {
      const flows = new Map(s.flows);
      const prev = flows.get(p.response_id);
      const next: FlowSummary = {
        response_id: p.response_id,
        status: p.status,
        model_requested: prev?.model_requested ?? p.served_model,
        model_served: p.served_model,
        upstream_target: p.upstream_target,
        usage: p.usage ?? prev?.usage ?? null,
        started_ms: prev?.started_ms ?? Date.now() - p.elapsed_ms,
        elapsed_ms: p.elapsed_ms,
        cost: prev?.cost ?? null,
      };
      flows.set(p.response_id, next);
      return {
        flows,
        flowOrder: prev ? s.flowOrder : [p.response_id, ...s.flowOrder],
      };
    }),

  patchUsage: (response_id, usage) =>
    set((s) => {
      const prev = s.flows.get(response_id);
      if (!prev) return {};
      const flows = new Map(s.flows);
      flows.set(response_id, { ...prev, usage });
      return { flows };
    }),

  setMetrics: (metrics) => set({ metrics }),

  setTopology: (topologyNodes, topologyEdges) => set({ topologyNodes, topologyEdges }),

  pushMonitor: (msg) =>
    set((s) => {
      const monitor = s.monitor.length >= MONITOR_RING_CAP
        ? [...s.monitor.slice(s.monitor.length - MONITOR_RING_CAP + 1), msg]
        : [...s.monitor, msg];
      return { monitor };
    }),

  reset: () =>
    set({
      connection: 'idle',
      cursors: emptyCursors(),
      flows: new Map(),
      flowOrder: [],
      metrics: null,
      topologyNodes: [],
      topologyEdges: [],
      priceTable: {},
      monitor: [],
    }),
}));

export type DashboardStore = typeof dashboardStore;
