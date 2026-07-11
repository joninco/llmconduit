/**
 * Live WS state, held in a zustand vanilla store and bridged to React 18 concurrent
 * rendering via `useSyncExternalStore` (see ./hooks.ts). The `DashboardSocket` feeds
 * this store; components subscribe with selector hooks.
 *
 * Frames mutate slices here; the per-domain dedup lives in the socket (D7), so by the
 * time a payload reaches a setter it is known to be fresh.
 */
import { createStore } from 'zustand/vanilla';
import { createRiverFold, foldRiverMessage, type RiverFold } from '../components/viz/riverModel';
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
  riverFold: RiverFold;
}

export interface DashboardState {
  connection: ConnectionState;
  /** Fatal contract/version error that requires a server/client upgrade. */
  fatalError: string | null;
  /**
   * The bounded seek shadow buffer overflowed. The historical cut remains visible, but it can no
   * longer be advanced safely; returning LIVE must establish a fresh socket snapshot.
   */
  resyncRequired: boolean;
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
  /** Stable SQLite cut selected by the scrubber; null for live/legacy in-memory cuts. */
  seekCutId: number | null;
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
  /**
   * The theater's INCREMENTAL river fold, fed one message at a time by `pushMonitor` — NOT derived
   * from the capped `monitor` ring. The ring evicts old `segment_append`s at `MONITOR_RING_CAP`, so
   * rivers rebuilt from it lose their head on long streams (the theater visibly deleted tokens from
   * the top, and reasoning — which streams first — vanished entirely). The fold keeps FULL stream
   * text; its own caps in riverModel bound memory (per-channel head-trim + `truncated` flag,
   * `MAX_RIVERS`). Captured/restored with the live baseline like the ring, cleared on reset.
   */
  riverFold: RiverFold;

  // -- mutations (called by the socket) --
  setConnection: (s: ConnectionState) => void;
  setFatalError: (message: string | null) => void;
  setResyncRequired: (required: boolean) => void;
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
    cutId?: number | null;
    monitorSeq: number;
    metrics: MetricsResponse | null;
    topology: TopologyResponse | null;
    monitorMessages?: DebugWsMessage[];
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
   * ATOMICALLY reinstall a previously-captured live baseline AND flip back to `'live'` (D11 R2
   * finding 1 + D11 R3). ONE `set` restores the live rows/cursors/monitor, clears the seek freeze
   * (`seekAtMs`/`seekMonitorSeq`) so the frozen cut is fully gone, AND sets `connection='live'` —
   * so the transition to live and the restored baseline land together BEFORE the socket replays any
   * shadow-buffered frame. Were the live flip deferred to a trailing `setConnection('live')`, the
   * replay would run live data into the store while `connection` was still `'seeking'`, the exact
   * state D10 must never observe (never 'seeking' with live rows). Crosses a boundary (frozen cut →
   * live store), so the monotonic epoch advances (finding 1).
   */
  restoreLiveBaseline: (baseline: LiveBaseline) => void;
  applySnapshot: (snap: {
    cursors: SeqCursors;
    flows: FlowSummary[];
    metrics: MetricsResponse | null;
    topology: TopologyResponse | null;
  }) => void;
  /**
   * ATOMICALLY install a fresh snapshot AS the live store AND flip to `'live'` (D11 R6) — the
   * staged-reconnect twin of `restoreLiveBaseline`. When `live()` resumes from a seek that staged a
   * RECONNECT snapshot, the store still holds the FROZEN cut under `connection==='seeking'`; this
   * replaces every row/cursor with the snapshot, clears the seek freeze, AND sets `connection='live'`
   * in ONE `set`, so the socket's subsequent shadow-buffer replay never runs live data into the store
   * while `connection` is still `'seeking'` (the window D10 must never observe). `applySnapshot`
   * alone leaves `connection` unchanged (it is also the INITIAL-snapshot path, where the flip to live
   * is a separate step), so the staged-resume path needs this combined action. Crosses a boundary
   * (frozen cut → live snapshot store), so the monotonic epoch advances (finding 1).
   */
  restoreLiveSnapshot: (snap: {
    cursors: SeqCursors;
    flows: FlowSummary[];
    metrics: MetricsResponse | null;
    topology: TopologyResponse | null;
  }) => void;
  upsertFlow: (flow: FlowSummary) => void;
  /**
   * Reconcile the live map from an unfiltered, cursor-bearing `/flows` response. A strictly newer
   * REST cursor can contain removals that the row-only WS publisher cannot represent (TTL/count/
   * quota eviction), so this replaces the retained id set atomically. Stale/equal cuts are ignored.
   */
  reconcileFlowRows: (flows: FlowSummary[], flowSeq: number) => void;
  /** Patch from a `flow_status` WS payload (keyed by `api_call_id`). */
  patchFlowStatus: (p: FlowStatusPayload) => void;
  /** Patch usage onto a flow by `api_call_id`. */
  patchUsage: (apiCallId: string, usage: Usage) => void;
  setMetrics: (m: MetricsResponse) => void;
  setTopology: (nodes: ProviderHealth[], edges: TopologyEdge[]) => void;
  /**
   * Seed the topology nodes/edges AND the price table from the `/topology` REST read (D13, finding
   * 5). The WS `topology_update` frames carry ONLY nodes/edges (no price table), so the REST seed is
   * the price table's source; the caller gates this to LIVE so it never overwrites a frozen seek cut.
   */
  seedTopology: (topology: TopologyResponse) => void;
  /** Append a monitor message, stamped with the `monitor_seq` of the frame that delivered it. */
  pushMonitor: (msg: DebugWsMessage, seq?: number) => void;
  /** Append one accepted monitor frame atomically, preserving payload order and one seq stamp. */
  pushMonitorBatch: (messages: DebugWsMessage[], seq?: number) => void;
  reset: () => void;
}

const MONITOR_RING_CAP = 500;
// Mirrors `dashboard_flow::FLOW_CAP`. This is a defense-in-depth browser bound; the periodic REST
// reconciliation below is still authoritative when the server's byte quota evicts fewer rows.
const FLOW_ROW_CAP = 512;

function capFlowRows(
  flows: Map<string, FlowSummary>,
  order: string[],
): { flows: Map<string, FlowSummary>; flowOrder: string[] } {
  if (order.length <= FLOW_ROW_CAP) return { flows, flowOrder: order };
  const flowOrder = order.slice(0, FLOW_ROW_CAP);
  const keep = new Set(flowOrder);
  for (const id of flows.keys()) if (!keep.has(id)) flows.delete(id);
  return { flows, flowOrder };
}

const emptyCursors = (): SeqCursors => ({
  flow_seq: 0,
  metrics_seq: 0,
  topology_seq: 0,
  monitor_seq: 0,
  backend_metrics_seq: 0,
});

export const dashboardStore = createStore<DashboardState>((set, get) => ({
  connection: 'idle',
  fatalError: null,
  resyncRequired: false,
  connEpoch: 0,
  cursors: emptyCursors(),
  seekAtMs: null,
  seekCutId: null,
  seekMonitorSeq: null,
  flows: new Map(),
  flowOrder: [],
  metrics: null,
  topologyNodes: [],
  topologyEdges: [],
  priceTable: {},
  monitor: [],
  monitorSeqs: [],
  riverFold: createRiverFold(),

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
        return { connection, connEpoch, seekAtMs: Date.now(), seekCutId: null, seekMonitorSeq: s.cursors.monitor_seq };
      }
      return { connection, connEpoch, seekAtMs: null, seekCutId: null, seekMonitorSeq: null };
    }),
  setFatalError: (fatalError) => set({ fatalError }),
  setResyncRequired: (resyncRequired) => set({ resyncRequired }),

  enterSeek: (atMs) =>
    set((s) => ({
      connection: 'seeking',
      // Entering seek always crosses a boundary (live store → frozen cut), so bump the epoch.
      connEpoch: s.connEpoch + 1,
      seekAtMs: atMs,
      seekCutId: null,
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
      const monitor = (cut.monitorMessages ?? []).slice(-MONITOR_RING_CAP);
      const monitorSeqs = monitor.map(() => cut.monitorSeq);
      let riverFold = createRiverFold();
      for (const message of monitor) riverFold = foldRiverMessage(riverFold, message);
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
        seekCutId: cut.cutId ?? null,
        seekMonitorSeq: cut.monitorSeq,
        flows,
        flowOrder,
        metrics: cut.metrics,
        topologyNodes: cut.topology?.nodes ?? [],
        topologyEdges: cut.topology?.edges ?? [],
        priceTable: cut.topology?.price_table ?? {},
        monitor,
        monitorSeqs,
        riverFold,
      };
    }),

  setCursor: (domain, seq) =>
    set((s) => ({ cursors: { ...s.cursors, [domain]: Math.max(s.cursors[domain], seq) } })),

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
      // Shallow copy is a real freeze: fold updates are immutable (fresh Map + fresh river object
      // per applied message), so the captured Map's river objects can never mutate underneath.
      riverFold: { rivers: new Map(s.riverFold.rivers), order: [...s.riverFold.order] },
    };
  },

  restoreLiveBaseline: (baseline) =>
    set((s) => ({
      // Flip back to LIVE in the SAME atomic update as the baseline restore (D11 R3): the socket
      // replays shadow-buffered frames AFTER this returns, so deferring the flip to a trailing
      // `setConnection('live')` would expose `connection==='seeking'` with restored/replayed live
      // rows — the invariant D10 relies on (never 'seeking' with live data). One `set` closes it.
      connection: 'live',
      // Crosses a boundary (frozen cut → live store); bump the monotonic epoch (finding 1).
      connEpoch: s.connEpoch + 1,
      // The frozen cut is fully gone — clear the seek freeze so elapsed ticks + the monitor unbounds.
      seekAtMs: null,
      seekCutId: null,
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
      riverFold: { rivers: new Map(baseline.riverFold.rivers), order: [...baseline.riverFold.order] },
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
        seekCutId: null,
        seekMonitorSeq: null,
        flows,
        flowOrder,
        metrics: snap.metrics,
        topologyNodes: snap.topology?.nodes ?? [],
        topologyEdges: snap.topology?.edges ?? [],
        priceTable: snap.topology?.price_table ?? {},
      };
    }),

  restoreLiveSnapshot: (snap) =>
    set((s) => {
      const flows = new Map<string, FlowSummary>();
      const flowOrder: string[] = [];
      for (const f of snap.flows) {
        flows.set(f.api_call_id, f);
        flowOrder.push(f.api_call_id);
      }
      return {
        // Flip to LIVE in the SAME atomic update as installing the snapshot rows/cursors (D11 R6):
        // the socket replays shadow-buffered frames AFTER this returns, so deferring the flip to a
        // trailing `setConnection('live')` would expose `connection==='seeking'` with the snapshot's
        // live rows/cursors/metrics applied — the invariant D10 relies on (never 'seeking' with live
        // data). The same window `restoreLiveBaseline` closes for the baseline-restore resume path.
        connection: 'live',
        // Crosses a boundary (frozen cut → live snapshot store); bump the monotonic epoch (finding 1).
        connEpoch: s.connEpoch + 1,
        // The frozen cut is fully gone — clear the seek freeze so elapsed ticks + the monitor unbounds.
        seekAtMs: null,
        seekCutId: null,
        seekMonitorSeq: null,
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
      const existed = flows.has(flow.api_call_id);
      flows.set(flow.api_call_id, flow);
      return capFlowRows(flows, existed ? s.flowOrder : [flow.api_call_id, ...s.flowOrder]);
    }),

  reconcileFlowRows: (rows, flowSeq) =>
    set((s) => {
      // Historical rows are frozen, and an older/equal REST cut cannot add information. Requiring
      // a strictly newer cursor also keeps the common initial WS-snapshot + equal REST read cheap.
      if (s.connection === 'seeking' || flowSeq <= s.cursors.flow_seq) return {};
      const flows = new Map<string, FlowSummary>();
      const flowOrder: string[] = [];
      for (const row of rows.slice(0, FLOW_ROW_CAP)) {
        // Preserve a matching optimistic kill row until the server reaches its revision. Absence is
        // still authoritative: an evicted row is removed even if a mutation was in flight.
        const current = s.flows.get(row.api_call_id);
        flows.set(row.api_call_id, current && current.revision > row.revision ? current : row);
        flowOrder.push(row.api_call_id);
      }
      return {
        flows,
        flowOrder,
        cursors: { ...s.cursors, flow_seq: flowSeq },
      };
    }),

  patchFlowStatus: (p) =>
    set((s) => {
      const prev = s.flows.get(p.api_call_id);
      // A v2 mutation carries the COMPLETE post-mutation FlowRow. Never field-merge two revisions:
      // doing so can manufacture a row that never existed (for example new usage paired with an
      // old cost). A delayed lower revision is ignored; an equal revision is allowed so a server
      // echo can replace an optimistic local object at the same version.
      if (prev && p.revision < prev.revision) return {};
      const flows = new Map(s.flows);
      const next = flowRowFromStatus(p);
      flows.set(p.api_call_id, next);
      return capFlowRows(flows, prev ? s.flowOrder : [p.api_call_id, ...s.flowOrder]);
    }),

  patchUsage: (apiCallId, usage) =>
    set((s) => {
      const prev = s.flows.get(apiCallId);
      if (!prev) return {};
      const flows = new Map(s.flows);
      // Schema-v1 compatibility: a standalone usage frame does not carry a priced roll-up. Once
      // usage changes, any inherited cost belongs to the old usage and must be cleared until an
      // authoritative full row supplies a newly priced value.
      flows.set(apiCallId, {
        ...prev,
        usage,
        cost: null,
        cost_confidence: 'unavailable',
      });
      return { flows };
    }),

  setMetrics: (metrics) => set({ metrics }),

  setTopology: (topologyNodes, topologyEdges) => set({ topologyNodes, topologyEdges }),

  // Seed nodes/edges + price table from `/topology` (finding 5). The caller seeds only while LIVE,
  // so this never overwrites the frozen seek cut `applySeekCut` installed.
  //
  // SEQ RECONCILIATION (finding 6): the REST read can resolve from react-query's CACHE (or land
  // after a newer WS `topology_update` already applied), so its `topology_seq` may be STALE relative
  // to the store's current topology cursor. Apply the nodes/edges + advance the cursor ONLY when the
  // REST `topology_seq >= cursors.topology_seq` (≥ so an equal-seq re-seed is idempotent); a STALE
  // response updates ONLY the price table (the WS frames carry no prices, so REST is the price
  // source regardless of seq) and leaves the newer WS nodes/edges + cursor intact. The cursor only
  // ever moves forward (`max`), never backwards.
  seedTopology: (topology) =>
    set((s) => {
      if (topology.topology_seq >= s.cursors.topology_seq) {
        return {
          topologyNodes: topology.nodes,
          topologyEdges: topology.edges,
          priceTable: topology.price_table,
          cursors: { ...s.cursors, topology_seq: topology.topology_seq },
        };
      }
      // Stale REST: keep the newer WS nodes/edges + cursor; refresh only the price table.
      return { priceTable: topology.price_table };
    }),

  pushMonitor: (msg, seq = 0) => get().pushMonitorBatch([msg], seq),

  pushMonitorBatch: (messages, seq = 0) =>
    set((s) => {
      if (messages.length === 0) return {};
      // Clone and cap ONCE for the whole accepted frame. `monitor` + `monitorSeqs` are sliced in
      // lockstep so index i always pairs message↔arrival seq, even when one large replay batch
      // evicts the ring head.
      const monitor = [...s.monitor, ...messages].slice(-MONITOR_RING_CAP);
      const monitorSeqs = [
        ...s.monitorSeqs,
        ...messages.map(() => seq),
      ].slice(-MONITOR_RING_CAP);
      // Fold in wire order inside the same mutation. The theater accumulator survives monitor-ring
      // eviction, so adjacent output/tool fragments must observe every preceding sibling.
      let riverFold = s.riverFold;
      for (const message of messages) riverFold = foldRiverMessage(riverFold, message);
      return { monitor, monitorSeqs, riverFold };
    }),

  reset: () =>
    set((s) => ({
      connection: 'idle',
      fatalError: null,
      resyncRequired: false,
      // Teardown clears the live store — a boundary an in-flight mutation must not write across
      // (finding 1). The epoch is the one slice that survives a reset (monotonic across the session).
      connEpoch: s.connEpoch + 1,
      cursors: emptyCursors(),
      seekAtMs: null,
      seekCutId: null,
      seekMonitorSeq: null,
      flows: new Map(),
      flowOrder: [],
      metrics: null,
      topologyNodes: [],
      topologyEdges: [],
      priceTable: {},
      monitor: [],
      monitorSeqs: [],
      riverFold: createRiverFold(),
    })),
}));

/** Strip the WS-only discriminants while preserving every field of the complete authoritative row. */
function flowRowFromStatus(payload: FlowStatusPayload): FlowSummary {
  const row = { ...payload };
  Reflect.deleteProperty(row, 'type');
  Reflect.deleteProperty(row, 'phase');
  return row;
}

export type DashboardStore = typeof dashboardStore;
