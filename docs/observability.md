# Observability

| Component | File | Entry Point | Description |
|-|-|-|-|

## Metrics

**File:** `src/metrics.rs` — D5 subsystem.

- **`MetricsLayer`** (line 1113): aggregated request stats + coordinated snapshot store.
  - `MetricsLayer::new()` (line 1132): enabled instance with full snapshot quota.
  - `MetricsLayer::disabled()` (line 1142): zero-overhead no-op when `--with-debug-ui` is off.
- Three ring buffers at 1 s resolution: 1m (60 slots), 5m (300 slots), 1h (3600 slots). Each slot is keyed by `{status_class, model, endpoint, upstream}` with a fixed 128-bucket logarithmic latency histogram, normalized token counters, anomaly counts, and terminal-time cost.
- **`record_response`** (line 1168): called once per flow at the engine D3 terminal finalize seam — the single CAS-guarded choke point.
- **`record_usage`** (line 1191): token counters, same bucket key, called alongside `record_response`.
- **5-second snapshot task** (line 23): coordinated atomic cut across FlowStore, MetricsLayer, and topology store producing a body-free `DashboardSnapshot` (line 818).
- **Integration:** stored on `Gateway` (engine.rs line 168), attached via `GatewayBuilder::with_metrics()` (engine.rs line 791). Snapshot task spawned in `lib.rs` (line 115).

## Turn Capture (F1)

**File:** `src/turn_capture.rs` — Durable per-turn capture.

- **`TurnCaptureState`**: per-turn state with engine-done / served-done latches. Built in `http.rs` (line 609) when the capture gate is open.
- **`CaptureGuard`** (RAII): signals engine done on drop. `MiddlewareCaptureGuard`: backstop for turns that never reach the engine.
- **Captured sections:** inbound request, upstream request, upstream response, served response.
- **Artifact path:** `<turn_capture_dir>/<api_call_id>.json`. Each section streams incrementally to `<dir>/.work/<api_call_id>/` temp files.
- **Age-out:** `log_rotation::cleanup_dump_files` (published artifacts) and `log_rotation::cleanup_orphan_work_dirs` (abandoned `.work/` dirs).
- **Integration:** gated in `http.rs` (line 557) independently of `--with-debug-ui`. Wired to engine terminal via `CaptureGuard`. Upstream-request carrier on `BackendChatRequest` (F1d). Raw upstream response via `TurnCaptureState::write_upstream_response` (F1e).

## Dashboard

Five files under `src/dashboard_*.rs` + `src/debug_ui.rs`.

| Subsystem | File | Entry Point | Description |
|-|-|-|-|
| Flow Store (D1) | `src/dashboard_flow.rs` | `DashboardFlowStore` (line 1+) | Per-flow record store with LRU eviction (512 cap, 30 min TTL, 64 MiB summary-byte quota). Redacting streaming body capture. |
| Auth (D7a) | `src/dashboard_auth.rs` | `DashboardAuth` | Env-only secrets, stateless HMAC-SHA256 signed session cookie, login/logout handlers, CSRF double-submit, WebSocket auth with `Origin` allow-list. |
| REST API (D13) | `src/dashboard_api.rs` | `dashboard_flows`, `dashboard_flow_detail`, `dashboard_metrics`, `dashboard_topology`, `dashboard_catalog`, `dashboard_snapshot` (lines 958–1136) | `/dashboard/api/*` routes: flow listing, detail, metrics windows, provider topology, model catalog, coordinated snapshots. Computes real rates and cost. |
| WebSocket (D7b) | `src/dashboard_ws.rs` | `dashboard_ws` (line 959) | `/dashboard/ws` batched envelope (`DashboardFrame` / `DashboardPayload`). Per-domain dedup. Splits monitor messages into flow-domain (`Usage`, `FlowStatus`) and monitor-domain (`Monitor`) arms. |
| UI Server (D8) | `src/dashboard_ui.rs` | Static SPA handler | Serves embedded React+TS+Vite dashboard dist with CSP nonce bootstrap injection. |
| Legacy Debug | `src/debug_ui.rs` | — | Older `/debug` route (pre-dashboard). |

## Request Log

**File:** `src/upstream.rs` (writer) + `src/request_log.rs` (analyzer).

- **`UpstreamRequestLogger`** (upstream.rs): per-provider JSONL writer. Metadata-only is the default; `upstream_request_log_body_mode: redacted_payload` explicitly enables recursively secret-redacted and image-URI-redacted request payloads. A dedicated writer owns a 16-entry bounded queue; serving never waits for filesystem IO, overflow drops only the log entry, and power-of-two warning sampling prevents a stalled log path from amplifying disk pressure.
- **Config paths** (config.rs line 559): top-level `upstream_request_log_path`, per-provider override, plus fallback upstream paths. All collected via `Config::debug_log_dirs()` (config.rs line 1460).
- **`analyze_request_log`** (request_log.rs line 7): offline diff tool — reads JSONL, finds common prefixes between consecutive entries, reports differing JSON paths. Used via CLI (`main.rs` line 37).
- **Integration:** `UpstreamRequestLogger` constructed per `UpstreamClient` in `lib.rs` (lines 171/210/254), wired at upstream call sites in `upstream.rs`.

## Replay Store

**File:** `src/replay.rs`.

- **`ReplayStore`** (line 26): bounded LRU `HashMap<String, ReplayRecord>` keyed by SHA-256 hash of `(model, instructions, visible_history)`.
- **`insert`** (line 41): evicts oldest entry when at `max_entries` capacity.
- **`longest_prefix_match`** (line 60): finds best matching replay for repair rounds.
- **Integration:** stored on `Gateway` and used by the engine during repair-round injection. It is
  configured under `replay`, defaults disabled, and is independent from the public Responses
  `store` field. The consumed `llmconduit_replay:false` extension bypasses it per request.

## Responses State Store

**File:** `src/response_store.rs`.

- `store:true` prepares completed/incomplete canonical history in hidden state, atomically publishes
  it immediately before the terminal event, and rolls it back when delivery is cancelled; failed and
  cancelled turns are never referenceable.
- The default memory backend is a TTL-aware bounded LRU with independent 64 MiB committed and
  pending-write byte ceilings in addition to its entry limit.
- Optional SQLite persistence uses a versioned schema, transactions, hidden prepare/publish rows,
  expiry/LRU cleanup, restrictive Unix permissions, a two-second busy deadline, a single bounded
  Tokio blocking lane, and a bounded memory front cache.
- `previous_response_id` reads this store and returns a sanitized 404 for missing, expired,
  evicted, failed, cancelled, or non-stored IDs. Stored records contain canonical items and model/
  timestamp metadata, never HTTP headers or credentials.

## Tool Delta Gate

**File:** `src/tool_delta_gate.rs` — Streamed tool-call delta classifier.

- **`PendingDeltas`** (line 46): per-`call_id` buffer for leading `function_call_arguments` deltas that arrive before the tool name is resolved.
- **Two byte caps:** 256 KiB per-call (line 36), 1 MiB total (line 41) — DoS guard.
- **`DeltaDecision`** return type: allocation-free on hot path — resolved visible tools with no buffer return `One` (no map entry), pending buffers are `move`d out on flush.
- **Integration:** pure decision machine used by the engine's emission path (engine.rs). Never touches SSE channel or monitor hub directly.

## SSE Guard

**File:** `src/sse_guard/mod.rs` + `src/sse_guard/tests.rs` — Upstream SSE per-frame DoS guard (G6).

- **`SseFrameGuard`** (line 46): pure synchronous byte-accounting guard tracking bytes since last SSE event boundary. Returns `AppError` when over cap.
- **`bounded_sse_byte_stream`** (line 223): thin async `Stream` adapter that drives `SseFrameGuard` over a `bytes_stream()`.
- **Default cap:** 8 MiB (`DEFAULT_MAX_SSE_FRAME_BYTES`, line 26).
- **Integration:** wired in `upstream::stream_success_response` (upstream.rs line 4521). Configurable per-provider via `max_sse_frame_bytes`.

## Redaction

**File:** `src/redaction.rs` — Image-URI and secret-key redaction primitives.

- **`redact_image_uris`** (line 49): replaces `data:`, `https://`, `http://` URI runs (raw and JSON-escaped `\/` forms) with `<redacted uri>`. Case-insensitive.
- **`redact_image_uris_in_value`** (line 85): same but in-place on `serde_json::Value` trees.
- **`redact_payload_secrets_in_value`** (line 117): strips known sensitive JSON keys.
- **`redact_vision_text`** (line 139): redacts + caps at 4096 chars for model-visible/logged vision text.
- **Streaming body serializer** (around line 318): capped + redacting JSON serializer for dashboard capture seam.
- **Integration:** used across `http.rs` (inbound trace), `upstream.rs` (JSONL log), `dashboard_flow.rs` (capture seam), `monitor.rs` (debug WS), and `engine.rs`. Re-exported by `src/vision/mod.rs`.

## Log Rotation

**File:** `src/log_rotation.rs` — Age-based cleanup of dump files and orphan work dirs.

- **`cleanup_dump_files`** (line 51): deletes `*.json` / `*.ndjson` files older than `max_age` in a given directory.
- **`cleanup_orphan_work_dirs`** (line 118): sweeps `<turn_capture_dir>/.work/<id>/` dirs left after crashes. Scoped to `turn_capture_dir` only.
- **`spawn_cleanup`** (line 289): runs both cleanups via `spawn_blocking` at startup (main.rs line 131).
- **Eligible extensions:** `["json", "ndjson"]` (line 36). Subdirectories never removed by `cleanup_dump_files`.

## Raw Mode

**File:** `src/raw.rs` — CLI debug flag, not a subsystem.

- **`RawOutput`** (line 9): wraps an arbitrary `Write + Send` sink (defaults to stdout). `write_sse_event` (line 27) extracts and writes text deltas from `.delta` SSE events.
- **`raw_model_delta_from_sse_event`** (line 40): pure extractor — returns `Some(delta)` for any SSE event whose event name ends in `.delta` and whose `data.delta` is a JSON string.
- **Integration:** CLI flag (`--raw`), wired in `http.rs` response-body tee. Only model-output text deltas are forwarded (no event metadata, no JSON framing). Non-delta events ignored.
