/**
 * Wire types — the FROZEN contract between the Rust gateway and this SPA.
 *
 * Sources (the oracle):
 *  - D7 §"Batched WS envelope": `DashboardFrame { domain, seq, batch: DashboardPayload[] }`,
 *    `Domain = Flow | Metrics | Topology | Monitor`, and the `DashboardPayload` enum arms.
 *  - D13 §"Jobs to Be Done": the REST endpoint response shapes (per-domain seq cursors).
 *
 * Discriminated unions are exhaustive and `any`-free (D9 constraint). The `assertNever`
 * helper turns a missing switch arm into a COMPILE error.
 */

// ---------------------------------------------------------------------------
// Shared scalars
// ---------------------------------------------------------------------------

/** Server-assigned per-call identifier. `:id` in the REST routes == `api_call_id`. */
export type ResponseId = string;

/** Terminal lifecycle states of a flow. Mirrors the Rust `TerminalReason` wire strings. */
export type FlowStatus =
  | 'streaming'
  | 'completed'
  | 'failed'
  | 'cancelled'
  | 'killed'
  | 'replayed';

/** Provider health state used by topology nodes + status chips. */
export type ProviderStatus = 'healthy' | 'serving' | 'cooling' | 'degraded' | 'down' | 'error' | 'unknown';

/** Token-accounting block attached to flows + usage frames. */
export interface Usage {
  prompt: number;
  completion: number;
  total: number;
  cached: number;
  reasoning: number;
}

// ---------------------------------------------------------------------------
// Monitor (debug) messages — the `/debug/ws` bare `DebugWsMessage`, carried inside
// a `Monitor` payload arm. A single originating `DebugUpdate` (one `sequence`) carries
// a `Vec<DebugWsMessage>`, so the batched envelope MUST surface every sibling.
// ---------------------------------------------------------------------------

/**
 * The bare debug-stream message. We keep the body loosely typed (`unknown` payload)
 * because `/debug/ws` carries heterogeneous transcript events; views narrow on `kind`.
 * NOTE: `unknown` (not `any`) preserves type-safety — consumers must narrow.
 */
export interface DebugWsMessage {
  kind: string;
  response_id?: ResponseId;
  sequence?: number;
  /** Heterogeneous event body; narrow by `kind` at the use site. */
  payload?: unknown;
  ts_ms?: number;
}

// ---------------------------------------------------------------------------
// DashboardPayload — the discriminated union (D7). Discriminant: `type`.
//
// WIRE CONTRACT (frozen target for D7 — the Rust side MUST match this):
//   The Rust `DashboardPayload` enum is serialized INTERNALLY TAGGED via
//   `#[serde(tag = "type", rename_all = "snake_case")]`. With internal tagging serde
//   FLATTENS a newtype variant's inner struct fields alongside the tag, so
//   `Monitor(DebugWsMessage)` serializes as:
//       { "type": "monitor", "kind": "...", "response_id": "...", "sequence": N,
//         "payload": <any>, "ts_ms": N }
//   i.e. the `DebugWsMessage` fields are INLINE next to `type` — there is NO nested
//   `{ "message": {...} }` wrapper. `MonitorPayload` below therefore extends
//   `DebugWsMessage` (flattened), and the decoder reads the message fields off the
//   payload object directly. The other arms are plain internally-tagged structs.
//   The golden fixture in `ws.fixtures.ts` is the exact byte-for-byte target D7 emits.
// ---------------------------------------------------------------------------

/**
 * The Monitor arm: one per `DebugWsMessage` in the originating `DebugUpdate` batch.
 * FLATTENED — the `DebugWsMessage` fields sit inline alongside the `type` discriminant
 * (see the WIRE CONTRACT note above). `monitorMessage()` extracts the message half.
 */
export interface MonitorPayload extends DebugWsMessage {
  type: 'monitor';
}

/** Extracts the `DebugWsMessage` carried (flattened) inside a `MonitorPayload`. */
export function monitorMessage(p: MonitorPayload): DebugWsMessage {
  return {
    kind: p.kind,
    response_id: p.response_id,
    sequence: p.sequence,
    payload: p.payload,
    ts_ms: p.ts_ms,
  };
}

export interface UsagePayload {
  type: 'usage';
  response_id: ResponseId;
  prompt: number;
  completion: number;
  total: number;
  cached: number;
  reasoning: number;
}

/** Sliding-window metric tiles (mirrors `/dashboard/api/metrics`, sans cursor). */
export interface MetricWindow {
  reqs_per_sec: number;
  active_streams: number;
  error_pct: number;
  p50: number;
  p95: number;
  p99: number;
  tokens_per_sec: number;
  cost_per_min: number;
}

export interface MetricTickPayload {
  type: 'metric_tick';
  reqs_per_sec: number;
  active_streams: number;
  error_pct: number;
  p50: number;
  p95: number;
  p99: number;
  tokens_per_sec: number;
  cost_per_min: number;
  windows: {
    m1: MetricWindow;
    m5: MetricWindow;
    h1: MetricWindow;
  };
}

export interface FlowStatusPayload {
  type: 'flow_status';
  response_id: ResponseId;
  status: FlowStatus;
  served_model: string;
  upstream_target: string;
  usage: Usage | null;
  elapsed_ms: number;
}

/** A single provider's health snapshot (topology node). */
export interface ProviderHealth {
  id: string;
  name: string;
  status: ProviderStatus;
  base_url?: string;
  in_flight: number;
  cooldown_until_ms?: number | null;
  error_streak: number;
  tokens_per_sec: number;
}

export interface TopologyUpdatePayload {
  type: 'topology_update';
  nodes: ProviderHealth[];
  edges: TopologyEdge[];
}

/**
 * The full `DashboardPayload` union. Every arm carries a `type` discriminant; an
 * exhaustive switch over `type` is enforced at compile time via `assertNever`.
 */
export type DashboardPayload =
  | MonitorPayload
  | UsagePayload
  | MetricTickPayload
  | FlowStatusPayload
  | TopologyUpdatePayload;

// ---------------------------------------------------------------------------
// DashboardFrame — the batched WS envelope (D7). ONE frame per `DebugUpdate`
// for the Monitor domain (seq = DebugUpdate.sequence). Per-domain whole-frame dedup.
// ---------------------------------------------------------------------------

export type Domain = 'flow' | 'metrics' | 'topology' | 'monitor';

export interface DashboardFrame {
  domain: Domain;
  seq: number;
  batch: DashboardPayload[];
}

/** The first WS message after connect: a full snapshot the live frames build upon. */
export interface SnapshotFrame {
  type: 'snapshot';
  cursors: SeqCursors;
  flows: FlowSummary[];
  metrics: MetricsResponse | null;
  topology: TopologyResponse | null;
}

/** Either a snapshot (once, first) or a live batched frame. */
export type WsServerMessage = SnapshotFrame | DashboardFrame;

// ---------------------------------------------------------------------------
// REST shapes (D13). Cursor-bearing reads carry their per-domain seq; /catalog is bare.
// ---------------------------------------------------------------------------

export interface SeqCursors {
  flow_seq: number;
  metrics_seq: number;
  topology_seq: number;
  monitor_seq: number;
}

/** Row in the flow table (`GET /flows`). */
export interface FlowSummary {
  response_id: ResponseId;
  status: FlowStatus;
  model_requested: string;
  model_served: string;
  upstream_target: string;
  usage: Usage | null;
  started_ms: number;
  elapsed_ms: number;
  cost?: number | null;
}

/** `GET /dashboard/api/flows` */
export interface FlowsResponse {
  flows: FlowSummary[];
  total: number;
  flow_seq: number;
}

/** Query params for the flow list. */
export interface FlowsQuery {
  status?: FlowStatus;
  model?: string;
  upstream?: string;
  page?: number;
  limit?: number;
}

/** A single streamed delta replayed into the inspector. */
export interface FlowDelta {
  sequence: number;
  kind: string;
  /** Heterogeneous delta body; narrow at the use site. */
  payload?: unknown;
  ts_ms?: number;
}

/** `GET /dashboard/api/flows/:id` — the 3-pane inspector body. */
export interface FlowDetail {
  flow_seq: number;
  /** Absent (not error) when the body has been evicted. */
  inbound_body?: unknown;
  inbound_headers?: Record<string, string>;
  normalized?: unknown;
  upstream_body?: unknown;
  model_requested: string;
  model_served: string;
  upstream_target: string;
  usage: Usage | null;
  deltas: FlowDelta[];
  terminal_reason?: string | null;
  started_ms: number;
  elapsed_ms: number;
  cost?: number | null;
}

/** `GET /dashboard/api/metrics` */
export interface MetricsResponse {
  metrics_seq: number;
  reqs_per_sec: number;
  active_streams: number;
  error_pct: number;
  p50: number;
  p95: number;
  p99: number;
  tokens_per_sec: number;
  cost_per_min: number;
  windows: {
    m1: MetricWindow;
    m5: MetricWindow;
    h1: MetricWindow;
  };
}

export interface TopologyEdge {
  from: string;
  to: string;
  throughput: number;
  tokens_per_sec: number;
  cost_per_sec: number;
}

export interface ModelPrice {
  input_per_1k: number;
  output_per_1k: number;
  cached_per_1k: number;
}

/** `GET /dashboard/api/topology` — nodes/edges + the price table. */
export interface TopologyResponse {
  topology_seq: number;
  nodes: ProviderHealth[];
  edges: TopologyEdge[];
  price_table: Record<string, ModelPrice>;
}

/** Catalog entry (`GET /dashboard/api/catalog` returns a BARE array — no cursor). */
export interface CatalogEntry {
  id: string;
  context_limit: number;
}

/** Body-free frozen summary in a snapshot. */
export interface SnapshotFlowSummary {
  response_id: ResponseId;
  status: FlowStatus;
  model_served: string;
  upstream_target: string;
  usage: Usage | null;
  started_ms: number;
  elapsed_ms: number;
}

/** `GET /dashboard/api/snapshot?at=<unix_ms>` */
export interface SnapshotResponse {
  cursors: SeqCursors;
  at_ms: number;
  summaries: SnapshotFlowSummary[];
  metrics: MetricsResponse | null;
  topology: TopologyResponse | null;
}

/** `POST /dashboard/api/flows/:id/kill` */
export interface KillResponse {
  response_id: ResponseId;
  killed: boolean;
}

// ---------------------------------------------------------------------------
// Auth shapes (D7)
// ---------------------------------------------------------------------------

/** `POST /dashboard/login` body. */
export interface LoginRequest {
  token: string;
}

/** SPA bootstrap embedded by the Rust shell (D7 double-submit CSRF echo + auth state). */
export interface DashboardBootstrap {
  authenticated: boolean;
  csrf_token: string | null;
  mutations_enabled: boolean;
}

// ---------------------------------------------------------------------------
// Runtime validation (the WS pipe must NOT trust the wire — D9 finding 6).
// A frame is validated WHOLLY (envelope + every payload arm) BEFORE the socket
// touches any cursor or store, so a malformed frame drops without partial apply.
// ---------------------------------------------------------------------------

function isObj(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}
function isNum(v: unknown): v is number {
  return typeof v === 'number' && Number.isFinite(v);
}
function isStr(v: unknown): v is string {
  return typeof v === 'string';
}

const DOMAINS: readonly Domain[] = ['flow', 'metrics', 'topology', 'monitor'];
export function isDomain(v: unknown): v is Domain {
  return isStr(v) && (DOMAINS as readonly string[]).includes(v);
}

function isUsageOrNull(v: unknown): v is Usage | null {
  if (v === null) return true;
  return isObj(v) && isNum(v.prompt) && isNum(v.completion) && isNum(v.total) && isNum(v.cached) && isNum(v.reasoning);
}

function isMetricWindow(v: unknown): v is MetricWindow {
  return (
    isObj(v) && isNum(v.reqs_per_sec) && isNum(v.active_streams) && isNum(v.error_pct) &&
    isNum(v.p50) && isNum(v.p95) && isNum(v.p99) && isNum(v.tokens_per_sec) && isNum(v.cost_per_min)
  );
}

/** Validates a single decoded payload against its `type` arm. */
export function isDashboardPayload(v: unknown): v is DashboardPayload {
  if (!isObj(v) || !isStr(v.type)) return false;
  switch (v.type) {
    case 'monitor':
      // Flattened DebugWsMessage: `kind` is the only required field.
      return isStr(v.kind);
    case 'usage':
      return isStr(v.response_id) && isNum(v.prompt) && isNum(v.completion) && isNum(v.total) && isNum(v.cached) && isNum(v.reasoning);
    case 'metric_tick':
      return (
        isNum(v.reqs_per_sec) && isNum(v.active_streams) && isNum(v.error_pct) &&
        isNum(v.p50) && isNum(v.p95) && isNum(v.p99) && isNum(v.tokens_per_sec) && isNum(v.cost_per_min) &&
        isObj(v.windows) && isMetricWindow(v.windows.m1) && isMetricWindow(v.windows.m5) && isMetricWindow(v.windows.h1)
      );
    case 'flow_status':
      return isStr(v.response_id) && isStr(v.status) && isStr(v.served_model) && isStr(v.upstream_target) && isUsageOrNull(v.usage) && isNum(v.elapsed_ms);
    case 'topology_update':
      return Array.isArray(v.nodes) && Array.isArray(v.edges) && v.nodes.every(isProviderHealth) && v.edges.every(isTopologyEdge);
    default:
      return false;
  }
}

function isProviderHealth(v: unknown): v is ProviderHealth {
  return isObj(v) && isStr(v.id) && isStr(v.name) && isStr(v.status) && isNum(v.in_flight) && isNum(v.error_streak) && isNum(v.tokens_per_sec);
}
function isTopologyEdge(v: unknown): v is TopologyEdge {
  return isObj(v) && isStr(v.from) && isStr(v.to) && isNum(v.throughput) && isNum(v.tokens_per_sec) && isNum(v.cost_per_sec);
}

/** Validates the whole batched envelope: domain + seq + EVERY payload in the batch. */
export function isDashboardFrame(v: unknown): v is DashboardFrame {
  return (
    isObj(v) && isDomain(v.domain) && isNum(v.seq) &&
    Array.isArray(v.batch) && v.batch.every(isDashboardPayload)
  );
}

/** Validates a snapshot envelope (loose on nested cursor presence). */
export function isSnapshotFrame(v: unknown): v is SnapshotFrame {
  return isObj(v) && v.type === 'snapshot' && isObj(v.cursors) && Array.isArray(v.flows);
}

// ---------------------------------------------------------------------------
// Exhaustiveness helper
// ---------------------------------------------------------------------------

/**
 * Compile-time exhaustiveness guard. Placing `assertNever(x)` in the `default` arm
 * of a switch over a discriminated union turns any unhandled case into a TS error.
 * At runtime it throws (should be unreachable).
 */
export function assertNever(value: never): never {
  throw new Error(`Unhandled discriminated union member: ${JSON.stringify(value)}`);
}
