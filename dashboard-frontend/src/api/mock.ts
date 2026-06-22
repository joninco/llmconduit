/**
 * In-browser mock backend — lets all four views ship before the Rust contract is live.
 *
 * Provides:
 *  - `mockFetch`: a `fetch`-compatible function answering the D13 REST routes + D7 auth.
 *  - `MockWebSocket` + `mockWsFactory`: a WS that emits a snapshot then a live stream of
 *    batched `DashboardFrame`s, INCLUDING a multi-message `Monitor` frame (sibling
 *    `DebugWsMessage`s under one seq) — the `DebugUpdate`-equivalent the dedup test asserts.
 *
 * A dev flag (`isMockEnabled`) selects mock vs. real (see ../config/env.ts).
 */
import type {
  CatalogEntry,
  DashboardFrame,
  DebugWsMessage,
  FlowDetail,
  FlowSummary,
  FlowsResponse,
  MetricsResponse,
  MonitorPayload,
  ProviderHealth,
  SnapshotFrame,
  SnapshotResponse,
  TopologyResponse,
  WsServerMessage,
} from './types';
import type { WsLike } from './ws';

const MOCK_CSRF = 'mock-csrf-token';

// ---------------------------------------------------------------------------
// Seed data — shapes mirror the REAL Rust DTOs (D1 FlowSummary, D4 ProviderHealth).
// ---------------------------------------------------------------------------

const NODES: ProviderHealth[] = [
  { id: 'vllm-a', name: 'vllm-a (8001)', route: null, base_url: 'http://localhost:8001', status: 'healthy', cooling_until_ms: null, last_error: null, served_count: 1280, failover_count: 0, consecutive_failures: 0, catalog_fetched_ms: Date.now() - 5000, catalog_size: 12 },
  { id: 'vllm-b', name: 'vllm-b (8002)', route: null, base_url: 'http://localhost:8002', status: 'cooling', cooling_until_ms: Date.now() + 8000, last_error: 'connection refused', served_count: 610, failover_count: 3, consecutive_failures: 2, catalog_fetched_ms: Date.now() - 9000, catalog_size: 8 },
  { id: 'openai', name: 'openai-proxy', route: 'cloud', base_url: 'https://api.openai.com', status: 'down', cooling_until_ms: Date.now() + 30000, last_error: '503 upstream', served_count: 42, failover_count: 9, consecutive_failures: 7, catalog_fetched_ms: null, catalog_size: 0 },
];

const PRICE_TABLE: TopologyResponse['price_table'] = {
  'gpt-4o': { input_per_1k: 0.005, output_per_1k: 0.015, cached_per_1k: 0.0025 },
  'llama-3.1-70b': { input_per_1k: 0.0008, output_per_1k: 0.0008, cached_per_1k: 0.0004 },
};

const CATALOG: CatalogEntry[] = [
  { id: 'gpt-4o', context_limit: 128000 },
  { id: 'llama-3.1-70b', context_limit: 131072 },
  { id: 'qwen2.5-coder-32b', context_limit: 32768 },
];

function seedFlows(): FlowSummary[] {
  const now = Date.now();
  return [
    { api_call_id: 'api_001', response_id: 'resp_001', method: 'POST', uri: '/v1/responses', status: 'open', model_requested: 'gpt-4o', model_served: 'llama-3.1-70b', upstream_target: 'vllm-a', usage: { prompt: 812, completion: 240, total: 1052, cached: 128, reasoning: 0 }, started_ms: now - 2400, finished_ms: null, elapsed_ms: 2400, terminal_reason: null, cost: 0.0061 },
    { api_call_id: 'api_002', response_id: 'resp_002', method: 'POST', uri: '/v1/chat/completions', status: 'completed', model_requested: 'llama-3.1-70b', model_served: 'llama-3.1-70b', upstream_target: 'vllm-a', usage: { prompt: 1500, completion: 980, total: 2480, cached: 0, reasoning: 120 }, started_ms: now - 12000, finished_ms: now - 7800, elapsed_ms: 4200, terminal_reason: 'response.completed', cost: 0.0019 },
    { api_call_id: 'api_003', response_id: null, method: 'POST', uri: '/v1/responses', status: 'failed', model_requested: 'gpt-4o', model_served: 'gpt-4o', upstream_target: 'openai', usage: null, started_ms: now - 30000, finished_ms: now - 29200, elapsed_ms: 800, terminal_reason: 'upstream 503', cost: null },
  ];
}

function buildMetrics(): MetricsResponse {
  const win = (m: number) => {
    const samples = Math.round(252 * m);
    return {
      reqs_per_sec: 4.2 * m, active_streams: Math.round(3 * m), error_pct: 1.1,
      p50: 180, p95: 920, p99: 1840, tokens_per_sec: 142 * m, cost_per_min: 0.21 * m,
      samples,
      // The mock's window is fully measured: every finalized flow reported usage on a
      // priced model, so all three denominators equal `samples` (tok/s + $/min measurable).
      usage_samples: samples,
      priced_samples: samples,
    };
  };
  const m1 = win(1);
  return {
    metrics_seq: 1,
    reqs_per_sec: 4.2, active_streams: 3, error_pct: 1.1,
    p50: 180, p95: 920, p99: 1840, tokens_per_sec: 142, cost_per_min: 0.21,
    samples: m1.samples,
    usage_samples: m1.usage_samples,
    priced_samples: m1.priced_samples,
    windows: { m1, m5: win(0.9), h1: win(0.7) },
  };
}

function buildTopology(): TopologyResponse {
  return {
    topology_seq: 1,
    nodes: NODES,
    edges: [
      { from: 'gateway', to: 'vllm-a', throughput: 4.2, tokens_per_sec: 142, cost_per_sec: 0.003 },
      { from: 'gateway', to: 'vllm-b', throughput: 1.0, tokens_per_sec: 61, cost_per_sec: 0.001 },
    ],
    price_table: PRICE_TABLE,
  };
}

function buildSnapshot(): SnapshotFrame {
  return {
    type: 'snapshot',
    cursors: { flow_seq: 3, metrics_seq: 1, topology_seq: 1, monitor_seq: 5 },
    flows: seedFlows(),
    metrics: buildMetrics(),
    topology: buildTopology(),
  };
}

/**
 * A multi-message `Monitor` frame: ONE envelope (seq = originating DebugUpdate.sequence)
 * whose `batch` carries several sibling `DashboardPayload::Monitor`s, each NESTING a real
 * itself-`type`-tagged `DebugWsMessage` (request_upsert / segment_append / request_status —
 * the actual `src/monitor.rs` arms). The dedup test asserts ALL apply (none dropped).
 */
export function buildMonitorFrame(seq = 6, responseId = 'resp_001'): DashboardFrame {
  const now = Date.now();
  const messages: DebugWsMessage[] = [
    {
      type: 'request_upsert',
      request: {
        response_id: responseId, model: 'llama-3.1-70b', started_at_ms: now, updated_at_ms: now,
        completed_at_ms: null, status: 'running',
        stats: { input_items: 3, tool_count: 0, turn_count: 1, user_messages: 1, assistant_messages: 0, system_messages: 1, developer_messages: 0, reasoning_items: 0, function_calls: 0, function_outputs: 0, tool_items: 0, input_chars: 42, instructions_chars: 0 },
        error: null,
      },
    },
    { type: 'segment_append', response_id: responseId, segment: { timestamp_ms: now, kind: 'output', text: 'Hello' } },
    { type: 'segment_append', response_id: responseId, segment: { timestamp_ms: now, kind: 'output', text: ', world' } },
    { type: 'request_status', response_id: responseId, status: 'completed', completed_at_ms: now, error: null },
  ];
  return {
    domain: 'monitor',
    seq,
    // NESTED wire form: each payload is `{type:'monitor', message:<DebugWsMessage>}`
    // (the message is itself `type`-tagged) — see types.ts WIRE CONTRACT.
    batch: messages.map((message): MonitorPayload => ({ type: 'monitor', message })),
  };
}

/** A standalone `usage` frame (flow domain) — exercises the `usage` payload arm (finding 9). */
export function buildUsageFrame(seq: number, apiCallId = 'api_001'): DashboardFrame {
  return {
    domain: 'flow',
    seq,
    batch: [
      { type: 'usage', api_call_id: apiCallId, response_id: 'resp_001', prompt: 812, completion: 512, total: 1324, cached: 128, reasoning: 16 },
    ],
  };
}

// ---------------------------------------------------------------------------
// Mock REST
// ---------------------------------------------------------------------------

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

/** Captures kill POSTs so tests can assert the CSRF header round-tripped. */
export const mockKillLog: { id: string; csrf: string | null }[] = [];

/**
 * Reserved api_call_id whose kill route answers 401 (with a valid CSRF) — models a kill that
 * races a session expiry. Lets the 401-teardown path be driven through the real client wiring.
 */
export const MOCK_KILL_UNAUTHORIZED_ID = 'api_session_expired';

/** A `fetch`-compatible mock answering D13 REST + D7 auth. */
export const mockFetch: typeof fetch = async (input, init): Promise<Response> => {
  const url = typeof input === 'string' ? input : input instanceof URL ? input.toString() : input.url;
  const method = (init?.method ?? 'GET').toUpperCase();
  const path = url.replace(/^https?:\/\/[^/]+/, '').split('?')[0] ?? url;
  const qs = new URLSearchParams(url.includes('?') ? url.slice(url.indexOf('?')) : '');

  // -- Auth --
  if (path === '/dashboard/login' && method === 'POST') {
    return json({ ok: true });
  }
  if (path === '/dashboard/logout' && method === 'POST') {
    return json({ ok: true });
  }

  // -- Kill (CSRF) -- `:id` == api_call_id ONLY (D13 contract). CSRF checked first
  // (security gate), then the id must be a seeded api_call_id else 404 (finding 7).
  const killMatch = path.match(/^\/dashboard\/api\/flows\/([^/]+)\/kill$/);
  if (killMatch && method === 'POST') {
    const id = decodeURIComponent(killMatch[1] ?? '');
    const csrf = headerValue(init?.headers, 'X-CSRF-Token');
    mockKillLog.push({ id, csrf });
    if (!csrf) return json({ error: 'missing csrf' }, 403);
    // Test affordance: the reserved id `api_session_expired` models a kill that races a
    // session loss → 401, so the auth-teardown-vs-rollback contract can be exercised end to end.
    if (id === MOCK_KILL_UNAUTHORIZED_ID) return json({ error: 'unauthorized' }, 401);
    if (!isSeededApiCallId(id)) return json({ error: 'unknown api_call_id' }, 404);
    return json({ api_call_id: id, killed: true });
  }

  // -- Reads --
  if (path === '/dashboard/api/flows') {
    let flows = seedFlows();
    const status = qs.get('status');
    if (status) flows = flows.filter((f) => f.status === status);
    const model = qs.get('model');
    if (model) flows = flows.filter((f) => f.model_requested === model || f.model_served === model);
    const upstream = qs.get('upstream');
    if (upstream) flows = flows.filter((f) => f.upstream_target === upstream);
    const resp: FlowsResponse = { flows, total: flows.length, flow_seq: 3 };
    return json(resp);
  }
  const detailMatch = path.match(/^\/dashboard\/api\/flows\/([^/]+)$/);
  if (detailMatch) {
    const id = decodeURIComponent(detailMatch[1] ?? '');
    const detail = buildFlowDetail(id); // by api_call_id ONLY
    return detail ? json(detail) : json({ error: 'unknown api_call_id' }, 404);
  }
  if (path === '/dashboard/api/metrics') return json(buildMetrics());
  if (path === '/dashboard/api/topology') return json(buildTopology());
  if (path === '/dashboard/api/catalog') return json(CATALOG);
  if (path === '/dashboard/api/snapshot') {
    const atMs = Number(qs.get('at') ?? Date.now());
    // Snapshot summaries are body-free FlowSummary objects (identical shape, D1).
    const snap: SnapshotResponse = {
      cursors: { flow_seq: 3, metrics_seq: 1, topology_seq: 1, monitor_seq: 5 },
      at_ms: atMs,
      summaries: seedFlows(),
      metrics: buildMetrics(),
      topology: buildTopology(),
    };
    return json(snap);
  }

  return json({ error: `mock: no route for ${method} ${path}` }, 404);
};

/** True when `id` is one of the seeded `api_call_id`s (D13 `:id = api_call_id`) — finding 7. */
function isSeededApiCallId(id: string): boolean {
  return seedFlows().some((f) => f.api_call_id === id);
}

/**
 * Build the flow-detail body for a seeded `api_call_id`. Resolves by `api_call_id` ONLY
 * (NOT response_id) per the D13 `:id = api_call_id` contract; returns `null` for an unknown
 * id so the route answers 404 (finding 7).
 */
function buildFlowDetail(id: string): FlowDetail | null {
  const base = seedFlows().find((f) => f.api_call_id === id);
  if (!base) return null;
  return {
    flow_seq: 3,
    api_call_id: base.api_call_id,
    response_id: base.response_id,
    inbound_body: { model: base.model_requested, messages: [{ role: 'user', content: 'Hi' }] },
    inbound_headers: { 'content-type': 'application/json', authorization: 'Bearer ***' },
    normalized: { model: base.model_served, input: [{ role: 'user', content: 'Hi' }] },
    upstream_body: { model: base.model_served, messages: [{ role: 'user', content: 'Hi' }], stream: true },
    model_requested: base.model_requested,
    model_served: base.model_served,
    upstream_target: base.upstream_target,
    usage: base.usage,
    status: base.status,
    deltas: [
      { sequence: 1, kind: 'response.created', payload: {}, ts_ms: base.started_ms },
      { sequence: 2, kind: 'response.delta', payload: { text: 'Hello' }, ts_ms: base.started_ms + 200 },
      { sequence: 3, kind: 'response.delta', payload: { text: ', world' }, ts_ms: base.started_ms + 400 },
    ],
    terminal_reason: base.terminal_reason,
    started_ms: base.started_ms,
    finished_ms: base.finished_ms,
    elapsed_ms: base.elapsed_ms,
    cost: base.cost ?? null,
  };
}

function headerValue(headers: HeadersInit | undefined, name: string): string | null {
  if (!headers) return null;
  if (headers instanceof Headers) return headers.get(name);
  if (Array.isArray(headers)) {
    const found = headers.find(([k]) => k.toLowerCase() === name.toLowerCase());
    return found ? found[1] : null;
  }
  const rec = headers as Record<string, string>;
  const key = Object.keys(rec).find((k) => k.toLowerCase() === name.toLowerCase());
  return key ? rec[key]! : null;
}

// ---------------------------------------------------------------------------
// Mock WebSocket
// ---------------------------------------------------------------------------

/**
 * A scripted WS: on connect it pushes a snapshot, then a flow_status frame, a metrics
 * frame, a topology frame, and the multi-message Monitor frame. `emit()` lets tests/dev
 * drive additional frames. Timers are used so React can render between frames; tests can
 * call the exposed `pushScript()` synchronously instead.
 */
export class MockWebSocket implements WsLike {
  onopen: ((ev: unknown) => void) | null = null;
  onclose: ((ev: { code?: number } | undefined) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;

  private timers: ReturnType<typeof setTimeout>[] = [];
  private seq = { flow: 3, metrics: 1, topology: 1, monitor: 5 };

  constructor(_url: string) {
    // Defer so handlers attach before frames flow.
    this.timers.push(setTimeout(() => this.start(), 0));
  }

  private start(): void {
    this.onopen?.({});
    this.deliver(buildSnapshot());
    // Stagger live frames. flow_status THEN a standalone usage frame (finding 9: the
    // `usage` payload arm must be exercised on the live path), both flow-domain so their
    // seqs stay monotonic.
    let t = 50;
    const live: WsServerMessage[] = [
      this.flowFrame(),
      buildUsageFrame(++this.seq.flow),
      this.metricsFrame(),
      this.topologyFrame(),
      buildMonitorFrame(++this.seq.monitor),
    ];
    for (const frame of live) {
      this.timers.push(setTimeout(() => this.deliver(frame), t));
      t += 60;
    }
  }

  private deliver(msg: WsServerMessage): void {
    this.onmessage?.({ data: JSON.stringify(msg) });
  }

  /** Push an arbitrary frame (dev/testing). */
  emit(msg: WsServerMessage): void {
    this.deliver(msg);
  }

  private flowFrame(): DashboardFrame {
    return {
      domain: 'flow',
      seq: ++this.seq.flow,
      batch: [{
        type: 'flow_status',
        api_call_id: 'api_001',
        response_id: 'resp_001',
        status: 'completed',
        model_requested: 'gpt-4o',
        model_served: 'llama-3.1-70b',
        upstream_target: 'vllm-a',
        usage: { prompt: 812, completion: 512, total: 1324, cached: 128, reasoning: 0 },
        started_ms: Date.now() - 3100,
        elapsed_ms: 3100,
      }],
    };
  }

  private metricsFrame(): DashboardFrame {
    const m = buildMetrics();
    return {
      domain: 'metrics',
      seq: ++this.seq.metrics,
      batch: [{
        type: 'metric_tick',
        reqs_per_sec: m.reqs_per_sec, active_streams: m.active_streams, error_pct: m.error_pct,
        p50: m.p50, p95: m.p95, p99: m.p99, tokens_per_sec: m.tokens_per_sec, cost_per_min: m.cost_per_min,
        samples: m.samples,
        usage_samples: m.usage_samples,
        priced_samples: m.priced_samples,
        windows: m.windows,
      }],
    };
  }

  private topologyFrame(): DashboardFrame {
    const t = buildTopology();
    return {
      domain: 'topology',
      seq: ++this.seq.topology,
      batch: [{ type: 'topology_update', nodes: t.nodes, edges: t.edges }],
    };
  }

  send(_data: string): void {
    // The dashboard WS is server-push only; client sends are ignored by the mock.
  }

  close(code?: number): void {
    for (const id of this.timers) clearTimeout(id);
    this.timers = [];
    this.onclose?.({ code });
  }
}

export const mockWsFactory = (url: string): WsLike => new MockWebSocket(url);

/** The CSRF token the mock bootstrap exposes (mirrors the cookie the Rust shell sets). */
export const mockBootstrapCsrf = MOCK_CSRF;
