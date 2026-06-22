/**
 * Wire types — the FROZEN contract between the Rust gateway and this SPA.
 *
 * These mirror the ACTUAL implemented Rust DTOs (not guesses):
 *  - `src/dashboard_flow.rs` (D1): `FlowStatus` (open/completed/failed/cancelled),
 *    `FlowUsage` (i64 prompt/completion/total/cached/reasoning), `SnapshotFlowSummary`
 *    (keyed by `api_call_id`, with optional `response_id`).
 *  - `src/monitor.rs`: `DebugWsMessage` is itself `#[serde(tag="type", rename_all="snake_case")]`
 *    with arms hello/request_upsert/segment_append/event_append/request_status/request_remove/
 *    snapshot_done — modeled as a real discriminated union below.
 *  - `.ralph/specs/D4-...md`: `ProviderHealth` (id/name/route/base_url/status/cooling_until_ms/
 *    last_error/served_count/failover_count/consecutive_failures/catalog_fetched_ms/catalog_size).
 *  - `.ralph/specs/D7-...md` + `.ralph/specs/D13-...md`: the WS envelope + REST shapes.
 *
 * Discriminated unions are exhaustive and `any`-free (D9 constraint). `assertNever` turns a
 * missing switch arm into a COMPILE error. Runtime validators (bottom of file) reject any
 * frame whose shape/enum/seq does not match BEFORE it can mutate state.
 */

// ---------------------------------------------------------------------------
// Shared scalars
// ---------------------------------------------------------------------------

/**
 * Authoritative per-call identifier minted at the HTTP boundary (`api_call_id`). The REST
 * routes' `:id` == `api_call_id` (D13); detail + kill route by it. Distinct from
 * `response_id` (the engine's id) — the two COEXIST on a flow and must not be conflated.
 */
export type ApiCallId = string;
/** The engine-assigned response id; optional on a flow until `link()` binds it. */
export type ResponseId = string;

/** Lifecycle status of a flow — the EXACT `FlowStatus` enum from `dashboard_flow.rs`. */
export type FlowStatus = 'open' | 'completed' | 'failed' | 'cancelled';
export const FLOW_STATUSES: readonly FlowStatus[] = ['open', 'completed', 'failed', 'cancelled'];

/** Provider health state — the EXACT D4 `ProviderHealth.status` (snake_case serialize). */
export type ProviderStatus = 'healthy' | 'cooling' | 'down';
export const PROVIDER_STATUSES: readonly ProviderStatus[] = ['healthy', 'cooling', 'down'];

/** The debug request lifecycle status (`DebugRequestStatus`, monitor.rs). */
export type DebugRequestStatus = 'running' | 'completed' | 'failed';
/** Debug segment kind (`DebugSegmentKind`, monitor.rs). */
export type DebugSegmentKind = 'output' | 'reasoning' | 'tool';

/**
 * Token-accounting block (`FlowUsage`). The Rust fields are `i64`; we keep them `number`
 * and the validators require finite values. (Non-negative is the contract but not enforced
 * here, since a transient negative would be a Rust bug, not a wire-poisoning vector.)
 */
export interface Usage {
  prompt: number;
  completion: number;
  total: number;
  cached: number;
  reasoning: number;
}

// ---------------------------------------------------------------------------
// DebugWsMessage — the REAL discriminated union from `src/monitor.rs`
// (`#[serde(tag="type", rename_all="snake_case")]`). Carried (nested) inside the
// `Monitor` payload arm. A single `DebugUpdate` (one `sequence`) bundles a `Vec` of
// these, so the batched envelope MUST surface every sibling.
// ---------------------------------------------------------------------------

export interface DebugEventImage {
  id: string;
  label: string;
  path: string;
  mime_type: string;
  size_bytes?: number | null;
}

export interface DebugRequestStats {
  input_items: number;
  tool_count: number;
  turn_count: number;
  user_messages: number;
  assistant_messages: number;
  system_messages: number;
  developer_messages: number;
  reasoning_items: number;
  function_calls: number;
  function_outputs: number;
  tool_items: number;
  input_chars: number;
  instructions_chars: number;
}

export interface DebugRequest {
  response_id: string;
  model: string;
  started_at_ms: number;
  updated_at_ms: number;
  completed_at_ms?: number | null;
  status: DebugRequestStatus;
  stats: DebugRequestStats;
  error?: string | null;
}

export interface DebugSegment {
  timestamp_ms: number;
  kind: DebugSegmentKind;
  text: string;
}

export interface DebugTimelineEvent {
  timestamp_ms: number;
  kind: string;
  summary: string;
  payload_preview?: string | null;
  images: DebugEventImage[];
}

export type DebugWsMessage =
  | { type: 'hello'; protocol_version: number; history_limit: number; history_retention_ms: number }
  | { type: 'request_upsert'; request: DebugRequest }
  | { type: 'segment_append'; response_id: string; segment: DebugSegment }
  | { type: 'event_append'; response_id: string; event: DebugTimelineEvent }
  | {
      type: 'request_status';
      response_id: string;
      status: DebugRequestStatus;
      completed_at_ms?: number | null;
      error?: string | null;
    }
  // D3: cumulative token usage for a flow, keyed by `response_id` (the monitor's id; NOT
  // `api_call_id` — the flow-domain `usage` payload is the api_call_id-keyed one). Emitted live
  // on each usage-bearing chunk AND replayed once per flow right after its `request_upsert` in a
  // `snapshot()`/transcript batch. The theater ignores it (token totals live on the flow rows),
  // but it MUST validate — a batch carrying it (every replayed flow with usage does) is otherwise
  // rejected WHOLESALE, dropping the entire monitor replay (the theater would never initialize).
  | {
      type: 'usage';
      response_id: string;
      prompt: number;
      completion: number;
      total: number;
      cached: number;
      reasoning: number;
    }
  | { type: 'request_remove'; response_id: string; reason: string }
  | { type: 'snapshot_done' };

export const DEBUG_WS_KINDS: readonly DebugWsMessage['type'][] = [
  'hello',
  'request_upsert',
  'segment_append',
  'event_append',
  'request_status',
  'usage',
  'request_remove',
  'snapshot_done',
];

// ---------------------------------------------------------------------------
// DashboardPayload — the discriminated union (D7). Discriminant: `type`.
//
// WIRE CONTRACT (frozen target for D7 — the Rust side MUST match this):
//   `DashboardPayload` is internally tagged `#[serde(tag = "type", rename_all = "snake_case")]`.
//   Its `Monitor` arm holds a `DebugWsMessage`, which is ITSELF `type`-tagged — so the
//   monitor payload NESTS the message under `message` (it CANNOT be flattened: both carry a
//   `type` field and would collide). Wire shape:
//       { "type": "monitor", "message": { "type": "segment_append", "response_id": "...",
//                                          "segment": { ... } } }
//   The other arms are plain internally-tagged structs (fields inline next to `type`).
//   `ws.fixtures.ts` holds the exact byte-for-byte target bytes D7 must emit.
// ---------------------------------------------------------------------------

/** The Monitor arm: one per `DebugWsMessage` in the originating `DebugUpdate` batch. */
export interface MonitorPayload {
  type: 'monitor';
  message: DebugWsMessage;
}

/**
 * Per-flow usage update.
 *
 * CONTRACT RECONCILIATION (orchestrator-reconciled across D1/D7/D13 — specs are frozen and
 * NOT edited): the AUTHORITATIVE wire shape = D1's `FlowRecord` (keyed by `api_call_id`) +
 * D13 (`/flows/:id` with `:id == api_call_id`). D7's spec SKETCH shows `Usage{response_id,
 * …}` — that field name is ILLUSTRATIVE (the spec uses block-comment placeholders) and is
 * SUPERSEDED. D7 emits BOTH ids: `api_call_id` (REQUIRED — the authoritative correlation
 * key for the flow row) plus `response_id` (OPTIONAL — retained as a secondary correlation
 * to the engine's response id). Validator: require `api_call_id`, accept optional `response_id`.
 */
export interface UsagePayload {
  type: 'usage';
  /** REQUIRED authoritative flow key (matches D1 FlowRecord + D13 `:id`). */
  api_call_id: ApiCallId;
  /** OPTIONAL secondary correlation id (the engine response id); coexists with api_call_id. */
  response_id?: ResponseId | null;
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
  /**
   * Count of TERMINAL (finalized) flows in this window — the data-quality signal for
   * "don't lie with zeros" (gap 01). When `samples === 0` the latency/tok-s/cost/error-%
   * fields were never MEASURED (no finalized flow fed them), so the strip renders them
   * `unavailable` (`—`); `reqs_per_sec` (a genuine `0` for an idle window) and
   * `active_streams` (live open-flow count) stay numeric. A finite `u64` on the wire.
   */
  samples: number;
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
  /** Headline (`m1`) terminal-flow sample count — mirrors `windows.m1.samples`. */
  samples: number;
  windows: {
    m1: MetricWindow;
    m5: MetricWindow;
    h1: MetricWindow;
  };
}

/**
 * Per-flow status update.
 *
 * CONTRACT RECONCILIATION (orchestrator-reconciled across D1/D7/D13 — specs are frozen and
 * NOT edited): the AUTHORITATIVE wire shape = D1's `FlowRecord` (keyed by `api_call_id`,
 * field `model_served`) + D13 (`/flows/:id` with `:id == api_call_id`). D7's spec SKETCH
 * shows `FlowStatus{response_id, served_model, …}` — those field names are ILLUSTRATIVE
 * (the spec uses block-comment placeholders) and are SUPERSEDED. D7 emits `api_call_id`
 * (REQUIRED — authoritative key + D6 kill key), `response_id` (OPTIONAL — secondary
 * correlation), and `model_served` (the served identity; the sketch's `served_model` is
 * superseded). `model_served`/`upstream_target` may be absent until D2 attaches them
 * (mirrors the `Option<String>` fields on `FlowRecord`). Validator: require `api_call_id`,
 * accept optional `response_id`.
 */
export interface FlowStatusPayload {
  type: 'flow_status';
  /** REQUIRED authoritative flow key (matches D1 FlowRecord + D6 kill + D13 `:id`). */
  api_call_id: ApiCallId;
  /** OPTIONAL secondary correlation id (the engine response id); coexists with api_call_id. */
  response_id?: ResponseId | null;
  status: FlowStatus;
  model_requested?: string | null;
  /** Served identity (D1 `model_served`; supersedes D7 sketch's `served_model`). */
  model_served?: string | null;
  upstream_target?: string | null;
  usage: Usage | null;
  started_ms: number;
  elapsed_ms?: number | null;
}

/**
 * One provider's health snapshot (topology node) — the EXACT D4 `ProviderHealth` DTO.
 * `route` is set only by the routing client; the counters drive the topology view.
 */
export interface ProviderHealth {
  id: string;
  name: string;
  /** REQUIRED key; value `string | null` (serde emits the key, null-not-absent) — finding 2. */
  route: string | null;
  /** REQUIRED non-null base URL (D4) — finding 2. */
  base_url: string;
  status: ProviderStatus;
  /** REQUIRED keys whose value is `T | null` (serde always emits them) — finding 2. */
  cooling_until_ms: number | null;
  last_error: string | null;
  served_count: number;
  failover_count: number;
  consecutive_failures: number;
  catalog_fetched_ms: number | null;
  catalog_size: number;
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

/**
 * Which payload `type`s are legal under each domain (finding 5: a `metric_tick` under
 * domain `flow` is invalid → rejected). Monitor frames carry only `monitor`; flow frames
 * carry flow_status/usage; etc.
 */
export const DOMAIN_PAYLOADS: Record<Domain, ReadonlySet<DashboardPayload['type']>> = {
  flow: new Set(['flow_status', 'usage']),
  metrics: new Set(['metric_tick']),
  topology: new Set(['topology_update']),
  monitor: new Set(['monitor']),
};

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

/**
 * Row in the flow table (`GET /flows`) — mirrors D1's `SnapshotFlowSummary` (body-free).
 * Keyed by `api_call_id`; `response_id`, models, target, usage, and the timing fields are
 * optional exactly as the Rust `Option<_>` fields are. `cost` is a D13 roll-up addition.
 */
export interface FlowSummary {
  api_call_id: ApiCallId;
  response_id?: ResponseId | null;
  method: string;
  uri: string;
  model_requested?: string | null;
  model_served?: string | null;
  upstream_target?: string | null;
  usage?: Usage | null;
  status: FlowStatus;
  started_ms: number;
  finished_ms?: number | null;
  elapsed_ms?: number | null;
  terminal_reason?: string | null;
  cost?: number | null;
}

/** Body-free frozen summary in a snapshot — identical shape to `FlowSummary` (D1). */
export type SnapshotFlowSummary = FlowSummary;

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

/** A single streamed delta replayed into the inspector (from MonitorHub snapshot). */
export interface FlowDelta {
  sequence: number;
  kind: string;
  /** Heterogeneous delta body; narrow at the use site. */
  payload?: unknown;
  ts_ms?: number;
}

/**
 * `GET /dashboard/api/flows/:id` — the 3-pane inspector body (D13). The summary fields +
 * the three captured bodies (absent, not error, when evicted) + replayed deltas. Keyed by
 * `api_call_id`.
 */
export interface FlowDetail {
  flow_seq: number;
  api_call_id: ApiCallId;
  response_id?: ResponseId | null;
  /** Absent when the body has been evicted by the summary-byte quota (D1). */
  inbound_body?: unknown;
  inbound_headers?: Record<string, string>;
  normalized?: unknown;
  upstream_body?: unknown;
  model_requested?: string | null;
  model_served?: string | null;
  upstream_target?: string | null;
  usage?: Usage | null;
  status: FlowStatus;
  deltas: FlowDelta[];
  terminal_reason?: string | null;
  started_ms: number;
  finished_ms?: number | null;
  elapsed_ms?: number | null;
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
  /** Headline (`m1`) terminal-flow sample count — mirrors `windows.m1.samples`. */
  samples: number;
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
  api_call_id: ApiCallId;
  killed: boolean;
}

// ---------------------------------------------------------------------------
// Auth shapes (D7)
// ---------------------------------------------------------------------------

/** `POST /dashboard/login` body. */
export interface LoginRequest {
  token: string;
}

/**
 * SPA bootstrap embedded by the Rust shell at `window.__LLMCONDUIT_DASHBOARD__` (D7). The
 * frozen field name for auth state is `authenticated` (boolean). `csrf_token` is the
 * double-submit token echo; `mutations_enabled` gates the kill control.
 */
export interface DashboardBootstrap {
  authenticated: boolean;
  csrf_token: string | null;
  mutations_enabled: boolean;
}

// ---------------------------------------------------------------------------
// Runtime validation (the WS pipe must NOT trust the wire — findings 4/5/6).
// A frame is validated WHOLLY (envelope + every payload arm, exact enums, unsigned-int
// seq, domain↔payload compatibility) BEFORE the socket touches any cursor or store.
// ---------------------------------------------------------------------------

function isObj(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}
function isNum(v: unknown): v is number {
  return typeof v === 'number' && Number.isFinite(v);
}
/** A non-negative integer (the wire `u64`/`u128`/`usize` fields). */
function isUint(v: unknown): v is number {
  return typeof v === 'number' && Number.isInteger(v) && v >= 0;
}
function isStr(v: unknown): v is string {
  return typeof v === 'string';
}
/** Optional string: absent, null, or a string. */
function isOptStr(v: unknown): boolean {
  return v === undefined || v === null || isStr(v);
}
/** Optional unsigned int: absent, null, or a uint. */
function isOptUint(v: unknown): boolean {
  return v === undefined || v === null || isUint(v);
}
function isOneOf<T extends string>(v: unknown, set: readonly T[]): v is T {
  return isStr(v) && (set as readonly string[]).includes(v);
}
/** REQUIRED key whose value is `string | null` (present, but may be null) — finding 2. */
function isNullableStr(v: unknown): v is string | null {
  return v === null || isStr(v);
}
/** REQUIRED key whose value is `uint | null` (present, but may be null) — finding 2. */
function isNullableUint(v: unknown): v is number | null {
  return v === null || isUint(v);
}

const DOMAINS: readonly Domain[] = ['flow', 'metrics', 'topology', 'monitor'];
export function isDomain(v: unknown): v is Domain {
  return isOneOf(v, DOMAINS);
}

function isUsage(v: unknown): v is Usage {
  return isObj(v) && isNum(v.prompt) && isNum(v.completion) && isNum(v.total) && isNum(v.cached) && isNum(v.reasoning);
}
function isUsageOrNull(v: unknown): v is Usage | null {
  return v === null || isUsage(v);
}
/** Optional usage: absent, null, or a valid usage. */
function isOptUsage(v: unknown): boolean {
  return v === undefined || isUsageOrNull(v);
}

function isMetricWindow(v: unknown): v is MetricWindow {
  return (
    isObj(v) && isNum(v.reqs_per_sec) && isNum(v.active_streams) && isNum(v.error_pct) &&
    isNum(v.p50) && isNum(v.p95) && isNum(v.p99) && isNum(v.tokens_per_sec) && isNum(v.cost_per_min) &&
    // `samples` is a non-negative integer count (gap 01 measured/unavailable signal).
    isUint(v.samples)
  );
}
function isMetricWindows(v: unknown): boolean {
  return isObj(v) && isMetricWindow(v.m1) && isMetricWindow(v.m5) && isMetricWindow(v.h1);
}

function isProviderHealth(v: unknown): v is ProviderHealth {
  return (
    isObj(v) &&
    isStr(v.id) && isStr(v.name) &&
    // base_url REQUIRED non-null; route/cooling_until_ms/last_error/catalog_fetched_ms are
    // REQUIRED keys whose value may be null (serde null-not-absent) — finding 2.
    isNullableStr(v.route) && isStr(v.base_url) &&
    isOneOf(v.status, PROVIDER_STATUSES) &&
    isNullableUint(v.cooling_until_ms) && isNullableStr(v.last_error) &&
    isUint(v.served_count) && isUint(v.failover_count) && isUint(v.consecutive_failures) &&
    isNullableUint(v.catalog_fetched_ms) && isUint(v.catalog_size)
  );
}
function isTopologyEdge(v: unknown): v is TopologyEdge {
  return isObj(v) && isStr(v.from) && isStr(v.to) && isNum(v.throughput) && isNum(v.tokens_per_sec) && isNum(v.cost_per_sec);
}

/** Validates a complete `ModelPrice` with FINITE numbers (rejects NaN/Inf/missing) — finding 4. */
function isModelPrice(v: unknown): v is ModelPrice {
  return isObj(v) && isNum(v.input_per_1k) && isNum(v.output_per_1k) && isNum(v.cached_per_1k);
}
/** Validates a `price_table` map: every value a complete finite `ModelPrice` — finding 4. */
function isPriceTable(v: unknown): v is Record<string, ModelPrice> {
  return isObj(v) && Object.values(v).every(isModelPrice);
}

const DEBUG_REQUEST_STATUSES: readonly DebugRequestStatus[] = ['running', 'completed', 'failed'];
const DEBUG_SEGMENT_KINDS: readonly DebugSegmentKind[] = ['output', 'reasoning', 'tool'];

/** Fully validates `DebugRequestStats` — every field a uint (finding 3). */
function isDebugRequestStats(v: unknown): v is DebugRequestStats {
  return (
    isObj(v) &&
    isUint(v.input_items) && isUint(v.tool_count) && isUint(v.turn_count) &&
    isUint(v.user_messages) && isUint(v.assistant_messages) && isUint(v.system_messages) &&
    isUint(v.developer_messages) && isUint(v.reasoning_items) && isUint(v.function_calls) &&
    isUint(v.function_outputs) && isUint(v.tool_items) && isUint(v.input_chars) &&
    isUint(v.instructions_chars)
  );
}

/** Fully validates a `DebugRequest` incl. its stats, status enum, and timestamps (finding 3). */
function isDebugRequest(v: unknown): v is DebugRequest {
  return (
    isObj(v) &&
    isStr(v.response_id) && isStr(v.model) &&
    isUint(v.started_at_ms) && isUint(v.updated_at_ms) && isNullableUint(v.completed_at_ms) &&
    isOneOf(v.status, DEBUG_REQUEST_STATUSES) &&
    isDebugRequestStats(v.stats) &&
    isNullableStr(v.error)
  );
}

function isDebugEventImage(v: unknown): v is DebugEventImage {
  return (
    isObj(v) && isStr(v.id) && isStr(v.label) && isStr(v.path) && isStr(v.mime_type) &&
    (v.size_bytes === undefined || isNullableUint(v.size_bytes))
  );
}

/** Fully validates a `DebugSegment` (kind enum + timestamp + text) — finding 3. */
function isDebugSegment(v: unknown): v is DebugSegment {
  return isObj(v) && isUint(v.timestamp_ms) && isOneOf(v.kind, DEBUG_SEGMENT_KINDS) && isStr(v.text);
}

/** Fully validates a `DebugTimelineEvent` incl. its images array — finding 3. */
function isDebugTimelineEvent(v: unknown): v is DebugTimelineEvent {
  return (
    isObj(v) && isUint(v.timestamp_ms) && isStr(v.kind) && isStr(v.summary) &&
    (v.payload_preview === undefined || isNullableStr(v.payload_preview)) &&
    Array.isArray(v.images) && v.images.every(isDebugEventImage)
  );
}

/**
 * Fully validates a nested `DebugWsMessage` (itself `type`-tagged) — findings 1+3. Every
 * arm's nested DTO is validated (DebugRequest/stats, segment, event/images, status enums,
 * timestamp types); no `as` cast skips validation.
 */
export function isDebugWsMessage(v: unknown): v is DebugWsMessage {
  if (!isObj(v) || !isStr(v.type)) return false;
  switch (v.type) {
    case 'hello':
      return isUint(v.protocol_version) && isUint(v.history_limit) && isUint(v.history_retention_ms);
    case 'request_upsert':
      return isDebugRequest(v.request);
    case 'segment_append':
      return isStr(v.response_id) && isDebugSegment(v.segment);
    case 'event_append':
      return isStr(v.response_id) && isDebugTimelineEvent(v.event);
    case 'request_status':
      return isStr(v.response_id) && isOneOf(v.status, DEBUG_REQUEST_STATUSES) && isNullableUint(v.completed_at_ms) && isNullableStr(v.error);
    case 'usage':
      // D3 cumulative usage (keyed by response_id) — finite token counts; rejecting it would drop
      // the whole monitor batch it rides in (a replayed flow's usage echo).
      return (
        isStr(v.response_id) &&
        isNum(v.prompt) && isNum(v.completion) && isNum(v.total) && isNum(v.cached) && isNum(v.reasoning)
      );
    case 'request_remove':
      return isStr(v.response_id) && isStr(v.reason);
    case 'snapshot_done':
      return true;
    default:
      return false;
  }
}

/** Validates a single decoded payload against its `type` arm. */
export function isDashboardPayload(v: unknown): v is DashboardPayload {
  if (!isObj(v) || !isStr(v.type)) return false;
  switch (v.type) {
    case 'monitor':
      // Nested, itself-tagged DebugWsMessage (NOT flattened) — finding 1.
      return isDebugWsMessage(v.message);
    case 'usage':
      return (
        isStr(v.api_call_id) && isOptStr(v.response_id) &&
        isNum(v.prompt) && isNum(v.completion) && isNum(v.total) && isNum(v.cached) && isNum(v.reasoning)
      );
    case 'metric_tick':
      return (
        isNum(v.reqs_per_sec) && isNum(v.active_streams) && isNum(v.error_pct) &&
        isNum(v.p50) && isNum(v.p95) && isNum(v.p99) && isNum(v.tokens_per_sec) && isNum(v.cost_per_min) &&
        isUint(v.samples) && isMetricWindows(v.windows)
      );
    case 'flow_status':
      return (
        isStr(v.api_call_id) && isOptStr(v.response_id) &&
        isOneOf(v.status, FLOW_STATUSES) &&
        isOptStr(v.model_requested) && isOptStr(v.model_served) && isOptStr(v.upstream_target) &&
        isUsageOrNull(v.usage) && isUint(v.started_ms) && isOptUint(v.elapsed_ms)
      );
    case 'topology_update':
      return Array.isArray(v.nodes) && Array.isArray(v.edges) && v.nodes.every(isProviderHealth) && v.edges.every(isTopologyEdge);
    default:
      return false;
  }
}

/**
 * Validates the whole batched envelope: a valid `domain`, an UNSIGNED-INTEGER `seq`
 * (rejects negative/fractional — finding 5), every payload valid, AND every payload's
 * `type` legal under `domain` (domain↔payload compatibility — finding 5).
 */
export function isDashboardFrame(v: unknown): v is DashboardFrame {
  if (!isObj(v) || !isDomain(v.domain) || !isUint(v.seq) || !Array.isArray(v.batch)) {
    return false;
  }
  const allowed = DOMAIN_PAYLOADS[v.domain];
  return v.batch.every((p) => isDashboardPayload(p) && allowed.has(p.type));
}

function isSeqCursors(v: unknown): v is SeqCursors {
  return isObj(v) && isUint(v.flow_seq) && isUint(v.metrics_seq) && isUint(v.topology_seq) && isUint(v.monitor_seq);
}

/** Validates a body-free flow summary (each snapshot summary — finding 4). */
function isFlowSummary(v: unknown): v is FlowSummary {
  return (
    isObj(v) &&
    isStr(v.api_call_id) && isOptStr(v.response_id) &&
    isStr(v.method) && isStr(v.uri) &&
    isOptStr(v.model_requested) && isOptStr(v.model_served) && isOptStr(v.upstream_target) &&
    isOptUsage(v.usage) &&
    isOneOf(v.status, FLOW_STATUSES) &&
    isUint(v.started_ms) && isOptUint(v.finished_ms) && isOptUint(v.elapsed_ms) &&
    isOptStr(v.terminal_reason)
  );
}

function isMetricsResponse(v: unknown): v is MetricsResponse {
  return (
    isObj(v) && isUint(v.metrics_seq) &&
    isNum(v.reqs_per_sec) && isNum(v.active_streams) && isNum(v.error_pct) &&
    isNum(v.p50) && isNum(v.p95) && isNum(v.p99) && isNum(v.tokens_per_sec) && isNum(v.cost_per_min) &&
    isUint(v.samples) && isMetricWindows(v.windows)
  );
}

function isTopologyResponse(v: unknown): v is TopologyResponse {
  return (
    isObj(v) && isUint(v.topology_seq) &&
    Array.isArray(v.nodes) && v.nodes.every(isProviderHealth) &&
    Array.isArray(v.edges) && v.edges.every(isTopologyEdge) &&
    // Every price_table entry is a complete finite ModelPrice (finding 4).
    isPriceTable(v.price_table)
  );
}

/**
 * Fully validates a snapshot envelope (finding 4): cursors are the four unsigned-int
 * fields, every summary is a valid body-free flow summary, and metrics/topology are either
 * null or their full valid shapes — BEFORE the snapshot can be applied.
 */
export function isSnapshotFrame(v: unknown): v is SnapshotFrame {
  return (
    isObj(v) && v.type === 'snapshot' &&
    isSeqCursors(v.cursors) &&
    Array.isArray(v.flows) && v.flows.every(isFlowSummary) &&
    (v.metrics === null || isMetricsResponse(v.metrics)) &&
    (v.topology === null || isTopologyResponse(v.topology))
  );
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
