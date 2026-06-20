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
  DashboardPayload,
  DebugWsMessage,
  FlowDetail,
  FlowSummary,
  FlowsResponse,
  MetricsResponse,
  ProviderHealth,
  SnapshotFrame,
  SnapshotResponse,
  TopologyResponse,
  WsServerMessage,
} from './types';
import type { WsLike } from './ws';

const MOCK_CSRF = 'mock-csrf-token';

// ---------------------------------------------------------------------------
// Seed data
// ---------------------------------------------------------------------------

const NODES: ProviderHealth[] = [
  { id: 'vllm-a', name: 'vllm-a (8001)', status: 'healthy', base_url: 'http://localhost:8001', in_flight: 3, error_streak: 0, tokens_per_sec: 142 },
  { id: 'vllm-b', name: 'vllm-b (8002)', status: 'cooling', base_url: 'http://localhost:8002', in_flight: 1, cooldown_until_ms: Date.now() + 8000, error_streak: 2, tokens_per_sec: 61 },
  { id: 'openai', name: 'openai-proxy', status: 'down', base_url: 'https://api.openai.com', in_flight: 0, error_streak: 7, tokens_per_sec: 0 },
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
    { response_id: 'resp_001', status: 'streaming', model_requested: 'gpt-4o', model_served: 'llama-3.1-70b', upstream_target: 'vllm-a', usage: { prompt: 812, completion: 240, total: 1052, cached: 128, reasoning: 0 }, started_ms: now - 2400, elapsed_ms: 2400, cost: 0.0061 },
    { response_id: 'resp_002', status: 'completed', model_requested: 'llama-3.1-70b', model_served: 'llama-3.1-70b', upstream_target: 'vllm-a', usage: { prompt: 1500, completion: 980, total: 2480, cached: 0, reasoning: 120 }, started_ms: now - 12000, elapsed_ms: 4200, cost: 0.0019 },
    { response_id: 'resp_003', status: 'failed', model_requested: 'gpt-4o', model_served: 'gpt-4o', upstream_target: 'openai', usage: null, started_ms: now - 30000, elapsed_ms: 800, cost: null },
  ];
}

function buildMetrics(): MetricsResponse {
  const win = (m: number) => ({
    reqs_per_sec: 4.2 * m, active_streams: Math.round(3 * m), error_pct: 1.1,
    p50: 180, p95: 920, p99: 1840, tokens_per_sec: 142 * m, cost_per_min: 0.21 * m,
  });
  return {
    metrics_seq: 1,
    reqs_per_sec: 4.2, active_streams: 3, error_pct: 1.1,
    p50: 180, p95: 920, p99: 1840, tokens_per_sec: 142, cost_per_min: 0.21,
    windows: { m1: win(1), m5: win(0.9), h1: win(0.7) },
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
 * whose `batch` carries several sibling `DebugWsMessage`s — the `DebugUpdate`-equivalent.
 * The dedup test asserts ALL of these apply (none dropped).
 */
export function buildMonitorFrame(seq = 6, responseId = 'resp_001'): DashboardFrame {
  const siblings: DebugWsMessage[] = [
    { kind: 'request.normalized', response_id: responseId, sequence: seq, payload: { model: 'llama-3.1-70b' }, ts_ms: Date.now() },
    { kind: 'upstream.request', response_id: responseId, sequence: seq, payload: { target: 'vllm-a' }, ts_ms: Date.now() },
    { kind: 'response.delta', response_id: responseId, sequence: seq, payload: { text: 'Hello' }, ts_ms: Date.now() },
    { kind: 'response.delta', response_id: responseId, sequence: seq, payload: { text: ', world' }, ts_ms: Date.now() },
  ];
  return {
    domain: 'monitor',
    seq,
    batch: siblings.map((message): DashboardPayload => ({ type: 'monitor', message })),
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

  // -- Kill (CSRF) --
  const killMatch = path.match(/^\/dashboard\/api\/flows\/([^/]+)\/kill$/);
  if (killMatch && method === 'POST') {
    const id = decodeURIComponent(killMatch[1] ?? '');
    const csrf = headerValue(init?.headers, 'X-CSRF-Token');
    mockKillLog.push({ id, csrf });
    if (!csrf) return json({ error: 'missing csrf' }, 403);
    return json({ response_id: id, killed: true });
  }

  // -- Reads --
  if (path === '/dashboard/api/flows') {
    let flows = seedFlows();
    const status = qs.get('status');
    if (status) flows = flows.filter((f) => f.status === status);
    const resp: FlowsResponse = { flows, total: flows.length, flow_seq: 3 };
    return json(resp);
  }
  const detailMatch = path.match(/^\/dashboard\/api\/flows\/([^/]+)$/);
  if (detailMatch) {
    const id = decodeURIComponent(detailMatch[1] ?? '');
    return json(buildFlowDetail(id));
  }
  if (path === '/dashboard/api/metrics') return json(buildMetrics());
  if (path === '/dashboard/api/topology') return json(buildTopology());
  if (path === '/dashboard/api/catalog') return json(CATALOG);
  if (path === '/dashboard/api/snapshot') {
    const atMs = Number(qs.get('at') ?? Date.now());
    const snap: SnapshotResponse = {
      cursors: { flow_seq: 3, metrics_seq: 1, topology_seq: 1, monitor_seq: 5 },
      at_ms: atMs,
      summaries: seedFlows().map((f) => ({
        response_id: f.response_id, status: f.status, model_served: f.model_served,
        upstream_target: f.upstream_target, usage: f.usage, started_ms: f.started_ms, elapsed_ms: f.elapsed_ms,
      })),
      metrics: buildMetrics(),
      topology: buildTopology(),
    };
    return json(snap);
  }

  return json({ error: `mock: no route for ${method} ${path}` }, 404);
};

function buildFlowDetail(id: string): FlowDetail {
  const base = seedFlows().find((f) => f.response_id === id) ?? seedFlows()[0]!;
  return {
    flow_seq: 3,
    inbound_body: { model: base.model_requested, messages: [{ role: 'user', content: 'Hi' }] },
    inbound_headers: { 'content-type': 'application/json', authorization: 'Bearer ***' },
    normalized: { model: base.model_served, input: [{ role: 'user', content: 'Hi' }] },
    upstream_body: { model: base.model_served, messages: [{ role: 'user', content: 'Hi' }], stream: true },
    model_requested: base.model_requested,
    model_served: base.model_served,
    upstream_target: base.upstream_target,
    usage: base.usage,
    deltas: [
      { sequence: 1, kind: 'response.created', payload: {}, ts_ms: base.started_ms },
      { sequence: 2, kind: 'response.delta', payload: { text: 'Hello' }, ts_ms: base.started_ms + 200 },
      { sequence: 3, kind: 'response.delta', payload: { text: ', world' }, ts_ms: base.started_ms + 400 },
    ],
    terminal_reason: base.status === 'completed' ? 'stop' : null,
    started_ms: base.started_ms,
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
    // Stagger live frames.
    let t = 50;
    const live: WsServerMessage[] = [
      this.flowFrame(),
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
        response_id: 'resp_001',
        status: 'completed',
        served_model: 'llama-3.1-70b',
        upstream_target: 'vllm-a',
        usage: { prompt: 812, completion: 512, total: 1324, cached: 128, reasoning: 0 },
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
