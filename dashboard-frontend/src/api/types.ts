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
// ---------------------------------------------------------------------------

export interface MonitorPayload {
  type: 'monitor';
  /** One `DebugWsMessage` per message in the originating `DebugUpdate` batch. */
  message: DebugWsMessage;
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
