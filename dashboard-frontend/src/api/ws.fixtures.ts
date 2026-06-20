/**
 * GOLDEN WIRE FIXTURES — the exact JSON bytes the Rust D7 side MUST emit on
 * `/dashboard/ws`. These are the concrete contract target: the decode tests assert the
 * SPA decodes these verbatim, and D7 should round-trip its serializer against them.
 *
 * Tagging: internally tagged `#[serde(tag = "type", rename_all = "snake_case")]`. The
 * `Monitor` newtype variant is FLATTENED — `DebugWsMessage` fields sit inline next to
 * `"type": "monitor"` (NO nested `message` wrapper). See types.ts "WIRE CONTRACT".
 *
 * Stored as JSON strings (not objects) so the test exercises the real `JSON.parse`
 * decode path a browser WebSocket `onmessage` would hit.
 */

/**
 * One `DashboardFrame` for the Monitor domain: ONE envelope per `DebugUpdate`
 * (seq = `DebugUpdate.sequence` = 6), `batch` = its 4 sibling `DebugWsMessage`s, each
 * flattened under `type:"monitor"`. The dedup test asserts ALL 4 apply.
 */
export const GOLDEN_MONITOR_FRAME_JSON = JSON.stringify({
  domain: 'monitor',
  seq: 6,
  batch: [
    { type: 'monitor', kind: 'request.normalized', response_id: 'resp_001', sequence: 6, payload: { model: 'llama-3.1-70b' }, ts_ms: 1718900000000 },
    { type: 'monitor', kind: 'upstream.request', response_id: 'resp_001', sequence: 6, payload: { target: 'vllm-a' }, ts_ms: 1718900000001 },
    { type: 'monitor', kind: 'response.delta', response_id: 'resp_001', sequence: 6, payload: { text: 'Hello' }, ts_ms: 1718900000002 },
    { type: 'monitor', kind: 'response.delta', response_id: 'resp_001', sequence: 6, payload: { text: ', world' }, ts_ms: 1718900000003 },
  ],
});

/** A standalone `usage` frame (flow domain). */
export const GOLDEN_USAGE_FRAME_JSON = JSON.stringify({
  domain: 'flow',
  seq: 4,
  batch: [
    { type: 'usage', response_id: 'resp_001', prompt: 812, completion: 240, total: 1052, cached: 128, reasoning: 0 },
  ],
});

/** A `flow_status` frame (flow domain). */
export const GOLDEN_FLOW_STATUS_FRAME_JSON = JSON.stringify({
  domain: 'flow',
  seq: 5,
  batch: [
    { type: 'flow_status', response_id: 'resp_001', status: 'completed', served_model: 'llama-3.1-70b', upstream_target: 'vllm-a', usage: { prompt: 812, completion: 512, total: 1324, cached: 128, reasoning: 0 }, elapsed_ms: 3100 },
  ],
});

/** A `metric_tick` frame (metrics domain). */
export const GOLDEN_METRIC_TICK_FRAME_JSON = JSON.stringify({
  domain: 'metrics',
  seq: 2,
  batch: [
    {
      type: 'metric_tick',
      reqs_per_sec: 4.2, active_streams: 3, error_pct: 1.1,
      p50: 180, p95: 920, p99: 1840, tokens_per_sec: 142, cost_per_min: 0.21,
      windows: {
        m1: { reqs_per_sec: 4.2, active_streams: 3, error_pct: 1.1, p50: 180, p95: 920, p99: 1840, tokens_per_sec: 142, cost_per_min: 0.21 },
        m5: { reqs_per_sec: 3.8, active_streams: 3, error_pct: 1.0, p50: 175, p95: 900, p99: 1800, tokens_per_sec: 128, cost_per_min: 0.19 },
        h1: { reqs_per_sec: 2.9, active_streams: 2, error_pct: 0.8, p50: 160, p95: 850, p99: 1700, tokens_per_sec: 100, cost_per_min: 0.15 },
      },
    },
  ],
});

/** A `topology_update` frame (topology domain). */
export const GOLDEN_TOPOLOGY_FRAME_JSON = JSON.stringify({
  domain: 'topology',
  seq: 2,
  batch: [
    {
      type: 'topology_update',
      nodes: [
        { id: 'vllm-a', name: 'vllm-a (8001)', status: 'healthy', base_url: 'http://localhost:8001', in_flight: 3, error_streak: 0, tokens_per_sec: 142 },
      ],
      edges: [
        { from: 'gateway', to: 'vllm-a', throughput: 4.2, tokens_per_sec: 142, cost_per_sec: 0.003 },
      ],
    },
  ],
});

/** A malformed frame: valid envelope, but a payload arm missing required fields. */
export const MALFORMED_FRAME_JSON = JSON.stringify({
  domain: 'flow',
  seq: 99,
  batch: [
    { type: 'usage', response_id: 'resp_x' /* missing prompt/completion/total/cached/reasoning */ },
  ],
});
