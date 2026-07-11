# Dashboard

## Overview

Live-monitor subsystem with auth, REST API, WebSocket streaming, and an embedded SPA. All routes register only when `--with-debug-ui` is set AND the D7 startup decision permits it. Every response carries `Cache-Control: no-store`, a locked-down CSP, `nosniff`, `no-referrer`, and `X-Frame-Options: DENY`.

## Authentication (`dashboard_auth.rs`)

Stateless HMAC-SHA256 signed session cookie — no server-side session table. Security is env-only (never on persisted `Config`).

### Env vars

| Variable | Purpose |
|-|-|
| `LLMCONDUIT_DASHBOARD_TOKEN` | Bearer/login token (required on non-loopback unless insecure override) |
| `LLMCONDUIT_DASHBOARD_SESSION_KEY` | Base64 HMAC key, >= 32 decoded bytes |
| `LLMCONDUIT_DASHBOARD_PUBLIC_ORIGIN` | Exact `https://host[:port]` for cookie Secure flag + WS Origin allow-list |
| `LLMCONDUIT_ALLOW_INSECURE_DASHBOARD` | Boolean: allow plaintext / tokenless off-loopback |
| `LLMCONDUIT_DASHBOARD_ALLOW_MUTATIONS` | Boolean: enable kill route |

### Key types

- `DashboardEnv` (line 98) — env snapshot.
- `DashboardAuth` (line 237) — built once, stored behind `Arc` as an Axum extension. Redacts secrets in `Debug`.
- `DashboardAuthBuild` (line 290) — `(auth, warnings)` pair returned by `from_env`.
- `PublicOrigin` (line 146) — validated `scheme://host[:port]`, rejects path/query/fragment/userinfo.
- `AuthSession` (line 884) — `{exp: u64}` attached to authenticated requests.
- `MutationPolicy` trait (line 611) — gates mutation routes (CSRF double-submit).
- `MutationDenied` enum (line 623) — `Disabled` or `CsrfInvalid`.
- `RouteDecision` enum (line 706) — `Register { warnings }` or `Refuse(RouteRefusal)`.
- `RouteRefusal` enum (line 665) — `MissingToken`, `MissingSessionKey`, `MissingHttpsOrigin`.

### Cookies

| Name | Type | HttpOnly | SameSite | Max-Age |
|-|-|-|-|-|
| `llmconduit_session` | HMAC-SHA256 `base64url(mac).{exp}:{nonce}` | Yes | Strict | 86400 s |
| `llmconduit_csrf` | UUID v4 | No | Strict | 86400 s |

- `x-csrf-token` header must match the CSRF cookie for mutation requests.
- Bearer `Authorization` fallback works for HTTP but is intentionally **not** honored for WebSocket (browsers cannot set it on a `WebSocket`).

### Key functions

- `DashboardAuth::from_env(bind_addr, env)` (line 314) — construct + validate.
- `DashboardAuth::authenticate(&self, headers)` (line 509) — dev-open, cookie, then bearer.
- `DashboardAuth::authenticate_ws(&self, headers)` (line 533) — cookie + Origin allow-list.
- `DashboardAuth::verify_token(&self, presented)` (line 441) — constant-time SHA-256 digest compare.
- `DashboardAuth::issue_session()` (line 455) — mint `(cookie_value, exp)`.
- `DashboardAuth::verify_session(&self, cookie_value)` (line 472) — MAC verify + exp check.
- `DashboardAuth::verify_csrf(&self, headers)` (line 587) — double-submit constant-time compare.
- `dashboard_login()` (line 831) — POST, sets session + CSRF cookies.
- `dashboard_logout()` (line 868) — POST, clears both cookies.
- `require_session` middleware (line 897) — validates on HTTP routes.
- `AuthSession` FromRequestParts (line 913) — extractor form.
- `startup_route_decision()` (line 735) — pure, testable registration decision.

### Security headers (`no_store`, line 1026)

`Cache-Control: no-store`, `Content-Security-Policy: default-src 'none'; frame-ancestors 'none'`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `X-Frame-Options: DENY`.

## REST API (`dashboard_api.rs`)

All handlers take `State(Arc<Gateway>)`. Every response flows through `json_no_store` (line 1248) which serializes JSON + applies `no_store` headers.

### Endpoints

| Method | Path | Handler (line) | Description |
|-|-|-|-|
| GET | `/dashboard/api/flows` | `dashboard_flows` (958) | Paged flow list, filterable by status/model/upstream, priced |
| GET | `/dashboard/api/flows/:id` | `dashboard_flow_detail` (1003) | Inspector detail with bodies, headers, deltas, cost |
| GET | `/dashboard/api/metrics` | `dashboard_metrics` (1082) | Live stat tiles: req/s, actives, error%, p50/p95/p99, tok/s, $/min |
| GET | `/dashboard/api/topology` | `dashboard_topology` (1092) | Provider node/edge graph with per-upstream rates |
| GET | `/dashboard/api/catalog` | `dashboard_catalog` (1113) | Model catalog bare array `[{id, context_limit?}]` |
| GET | `/dashboard/api/snapshot?at=` | `dashboard_snapshot` (1136) | Body-free frozen cut (time-travel) |
| GET | `/dashboard/api/history` | `dashboard_history` | Downsampled SQLite cut index for the scrubber |

All historical endpoints also accept the stable `cut_id` returned by `/history` or `/snapshot`.
Passing it to `/snapshot`, `/flows`, `/flows/:id`, `/metrics`, `/overview`, or `/topology`
keeps every view on one coordinated cut rather than independently rounding a timestamp.

### Key DTOs

- `FlowRow` (line 80) — list row with `cost`, `cost_confidence`, flattened `PhaseTimings`, `Attempt` trace.
- `FlowsResponse` (line 226) — `{flows, total, flow_seq}`.
- `FlowQuery` (line 236) — `status`, `model`, `upstream`, `page`, `limit`.
- `FlowDetailBody` (line 292) — inspector body with 4 captured body fields, headers, `FlowDelta` deltas.
- `FlowUpstreamResponse` (line 275) — `{body, truncated}` for gap-05 response capture.
- `FlowDelta` (line 250) — `{sequence, kind, payload?, ts_ms?}` for monitor replay.
- `CatalogEntry` (line 378) — `{id, context_limit?}`.
- `SnapshotResponse` (line 394) — `{cursors, at_ms, summaries, metrics?, topology?}`.
- `CostConfidence` (line 451) — `confident` / `estimated` / `unavailable`.

### Cost helpers

- `cost_for_usage()` (line 429) — prices prompt/cached/completion at per-model rates.
- `cost_confidence()` (line 479) — classifies a flow's cost as confident/estimated/unavailable.
- `window_total_tokens()` (line 536) — aggregate ring buckets.
- `window_total_cost()` (line 553) — prices ring buckets per served model.
- `window_cost_confidence()` (line 595) — aggregate window confidence.
- `metrics_body()` (line 716) — builds `MetricsSnapshot` from `MetricsView`.
- `topology_body()` (line 749) — builds `TopologySnapshot` from `ProviderHealthSnapshot`.
- `active_stream_count()` (line 835) — live open-flow count from FlowStore.
- `cut_active_stream_count()` (line 849) — FROZEN snapshot open-flow count.
- `replay_deltas()` (line 868) — filtered MonitorHub replay by `response_id`.

## Flow Store (`dashboard_flow.rs`)

The authoritative per-flow record store with capped/redacted body capture.

### Key types

- `DashboardFlowStore` (line 1084) — `{enabled, response_capture_enabled, state, summary_quota_bytes}`.
- `DashboardFlowState` (line 1065) — `{by_id, order, link_index, live_summary_bytes, seq}`.
- `FlowRecord` (line 693) — live record with `Arc<AtomicU8>` claim, body `Arc<[u8]>`s, all scalar/cost fields.
- `SnapshotFlowSummary` (line 968) — body-free projection for REST/WS/snapshots.
- `FlowStatus` (line 205) — `open`, `completed`, `failed`, `cancelled`.
- `FlowUsage` (line 227) — `{prompt, completion, total, cached?, reasoning?}`.
- `ClientAttribution` (line 279) — `{label?, source?}` derived from raw headers.
- `ClientSource` (line 254) — `key_hash`, `configured_header`, `user_agent`.
- `Attempt` (line 528) — per-provider dispatch provenance.
- `AttemptStatus` (line 464) — `served`, `failed`.
- `AttemptErrorClass` (line 479) — `connect`, `http_status`, `timeout`, `stream`, `terminal`, `other`.
- `AttemptFailoverReason` (line 503) — `provider_failed`, `request_rejected`, `terminal_no_failover`.
- `PhaseTimings` (line 797) — `{ingress_ms, normalization_done_ms, routing_decision_ms, first_content_delta_ms, stream_end_ms, finalize_ms}`.
- `TerminalMetricsInputs` (line 593) — evict-safe metrics payload.
- `CapturedBody` (line 616) — newtype: redacted + capped `Arc<[u8]>` <= `BODY_CAP` (128 KiB).
- `CapturedHeaders` (line 635) — redacted name/value pairs.
- `CapturedResponseBody` (line 646) — gap-05: body + `truncated` flag.
- `UpstreamResponseBody` (line 668) — record-facing response body.
- `AbortHub` (line 118) — D6 cancellation registry keyed by `api_call_id`.
- `MiddlewareGuard` (line 1791) — L0 RAII guard: finalizes on Drop if still Open.
- `FlowSnapshotGuard` (line 1049) — lock RAII for coordinated snapshot.

### Store operations

- `open()` (line 1173) — create record with capped body + headers + client attribution.
- `link()` (line 1241) — bind `response_id` -> `api_call_id` (first-link-wins).
- `set_upstream()` (line 1271) — attach served model, upstream target, upstream body.
- `set_upstream_response()` (line 1313) — gap-05: attach upstream error body (env-gated).
- `set_normalized()` (line 1333) — attach canonical body + requested model + stamp normalization phase.
- `finalize()` (line 1373) — mark terminal, stamp finalize phase, set upstream if empty.
- `record_usage()` (line 1412) — upsert cumulative token usage.
- `record_attempts()` (line 1433) — thread failover trace + wire TTFB.
- `stamp_routing_decision()` (line 1472) — phase-only: routing settled.
- `stamp_first_content_delta()` (line 1497) — phase-only: first content SSE delta.
- `stamp_stream_end()` (line 1516) — phase-only: clean stream completion.
- `engine_guard()` (line 1562) — L1 guard: CAS `OpenL0 -> ClaimedL1`, registers abort token.
- `middleware_guard()` (line 1540) — L0 guard for middleware lifecycle.
- `list()` (line 1611) — newest-first, prunes expired.
- `detail()` (line 1627) — resolve by `api_call_id` or `response_id`.
- `detail_with_seq()` (line 1653) — detail + record's own mutation watermark.
- `snapshot_summaries()` (line 1671) — body-free newest-first.
- `snapshot_summaries_with_seq()` (line 1697) — summaries + flow_seq in one lock.
- `with_summaries_under_lock()` (line 1736) — FIXED lock-order: FlowStore -> Metrics.

### Caps

| Cap | Value | Behavior |
|-|-|-|
| `FLOW_CAP` | 512 records | Oldest evicted on overflow |
| `FLOW_TTL_MS` | 30 min | Expired records pruned on any mutation/read |
| `DEFAULT_SUMMARY_QUOTA_BYTES` | 64 MiB | Body `Arc<[u8]>`s evicted oldest-first; record survives as body-free summary |
| `BODY_CAP` | 128 KiB | Single captured body max |
| `SCALAR_CAP` | 4 KiB | Per dynamic scalar string max |

### Env for response capture

`LLMCONDUIT_DASHBOARD_CAPTURE_UPSTREAM_RESPONSE` (line 73) — off by default, arms gap-05 upstream error body capture.

### Durable dashboard history

`LLMCONDUIT_DASHBOARD_HISTORY_DB=/path/to/dashboard.sqlite3` enables SQLite history when
`--with-debug-ui` is active. `LLMCONDUIT_DASHBOARD_HISTORY_RETENTION_HOURS` controls retention
(default 24 hours). The store uses WAL mode and a dedicated bounded writer queue; request handling
never performs SQLite I/O. Every five-second coordinated cut persists metrics, topology, flow
versions, and domain cursors. Monitor updates are persisted separately through the cut's monitor
cursor, so historical Theater and flow timelines replay only data known at that cut.

Large bodies remain in the existing atomic `turn_capture_dir/<api_call_id>.json` artifacts rather
than in SQLite/WAL. The artifact now contains inbound, normalized, final upstream request, raw final
upstream response, and served response sections when available. SQLite indexes those files at
startup, and flow detail also resolves the deterministic path for newly completed turns. Retention
of the artifact files remains governed by `debug_log_max_age_hours`; align it with the SQLite
retention if historical cuts must retain full captured I/O for the same duration.

## WebSocket (`dashboard_ws.rs`)

Batched envelope (`/dashboard/ws`) — `DashboardFrame` with per-domain whole-frame dedup.

### Wire format

```text
DashboardFrame { domain, seq, batch: Vec<DashboardPayload> }
```

### Domain enum (line 114)

`Flow`, `Metrics`, `Topology`, `Monitor` — each has its own `seq` cursor for client-side dedup (`seq <= last_seq[domain]` drops the frame).

### DashboardPayload arms (line 233, `#[serde(tag = "type")]`)

| type tag | Fields | Source |
|-|-|-|
| `monitor` | `{message: DebugWsMessage}` | MonitorHub broadcast, 1:1 (sibling-no-drop) |
| `usage` | `{api_call_id, response_id?, prompt, completion, total, cached?, reasoning?}` | Monitor `Usage` enriched via FlowStore |
| `metric_tick` | `{generated_at_ms, headline_window: "m1", windows: {m1, m5, h1}}`; each window carries accepted/terminal rates, active-now, separate failure/cancellation percentages, nullable percentile/token/cost values, coverage, and quality | MetricsLayer tick, 1 s interval |
| `flow_status` | `{api_call_id, response_id?, status, model_requested?, model_served?, upstream_target?, usage?, started_ms, elapsed_ms?, phases, attempts, first_upstream_byte_ms?}` | Monitor `RequestStatus` enriched via FlowStore |
| `topology_update` | `{nodes, edges}` | ProviderHealthSnapshot poll, 2 s interval |

### Initial message

`SnapshotMessage` (line 206) — `type: "snapshot"`, FIRST frame on connection. Carries `SeqCursors` baseline, body-free flow summaries, metrics snapshot, topology snapshot. SPA buffers all frames until this lands.

### SeqCursors (line 139)

`{flow_seq, metrics_seq, topology_seq, monitor_seq}` — seeded by the snapshot, live frames stamp their own domain's cursor.

### Key types

- `SeqCursors` (line 139) — dedup baseline.
- `MetricsSnapshot` (line 150) — REST-shaped metrics cut.
- `MetricWindow` (line 365) — one sliding window using the schema-v3 fields documented in `dashboard-metrics.md`.
- `MetricWindows` (line 349) — `{m1, m5, h1}`.
- `TopologySnapshot` (line 181) — `{topology_seq, nodes, edges, price_table}`.
- `TopologyNode` (line 407) — provider node with gap-12 per_provider metrics.
- `TopologyEdge` (line 486) — `{from, to, attempts_per_sec, terminal_flows_per_sec, reported_tokens_per_sec, terminal_cost_per_sec}`.

### Key functions

- `dashboard_ws()` (line 959) — handler: validates cookie + Origin, upgrades.
- `dashboard_socket()` (line 990) — main loop: snapshot first, then multiplex monitor + metrics tick + topology poll, racing session expiry.
- `frames_for_update()` (line 536) — build flow-enrichment + monitor frames from one `DebugUpdate`.
- `metric_tick_frame()` (line 815) — build metrics frame from `MetricsView`.
- `topology_frame()` (line 847) — build topology frame from `ProviderHealthSnapshot`.
- `snapshot_message()` (line 926) — build the initial `type:"snapshot"` message.
- `send_initial()` (line 1314) — snapshot FIRST, then replay frames, racing expiry.

### Auth close code

`4401` (line 90) — SPA bounces to login on this code instead of reconnecting.

## UI (`dashboard_ui.rs`)

Serves the React+TS+Vite SPA embedded at compile time via `include_dir!`.

### Routes

| Method | Path | Handler (line) | Description |
|-|-|-|-|
| GET | `/dashboard` | `dashboard_index` (66) | SPA shell (auth'd) or login shell (un-auth'd) |
| GET | `/dashboard/assets/{*path}` | `dashboard_asset` (122) | Static assets under `dist/assets/` |

### Bootstrap

The authenticated shell injects a `<script nonce="{NONCE}">` carrying `window.__LLMCONDUIT_DASHBOARD__ = {authenticated, csrf_token, mutations_enabled}`. CSP uses per-response `'nonce-<n>'` for the bootstrap script, `script-src 'self'` for SPA bundles.

### CSP (line 51)

`default-src 'self'; script-src 'self'{NONCE}; connect-src 'self' ws: wss:; style-src 'self' 'unsafe-inline'; img-src 'self' data:; object-src 'none'; base-uri 'self'; frame-ancestors 'none'`

### Credential response capture

`LLMCONDUIT_DASHBOARD_CAPTURE_UPSTREAM_RESPONSE` env var (off by default, `dashboard_flow.rs` line 73) — when ON, failed-turns' upstream RESPONSE/ERROR body is captured and appears on the live `GET /dashboard/api/flows/:id` detail body as `upstream_response.body` (gap 05). Truncated bodies carry `truncated: true`.

## Login page (`dashboard_login.html`)

Static inline-HTML token-entry form. Posts JSON `{token}` to `/dashboard/login` via `fetch`. On 200 OK, reloads to the SPA shell. Styled dark-theme card with a password input and error display. Script authorized via `{NONCE}` placeholder replaced by the server at serve time.

## Engine integration

- **Flow store** (`DashboardFlowStore`) — cloned into `Gateway`, provides `list()`, `detail()`, `open()`, `link()`, `set_upstream()`, `set_normalized()`, `finalize()`, `record_usage()`, `record_attempts()`, phase stamping.
- **Abort hub** (`AbortHub`) — cloned into `Gateway`, keyed by `api_call_id`. The L1 `TelemetryGuard` registers on CAS win, removes on every finalize path. `POST /dashboard/api/flows/:id/kill` calls `abort_hub.abort(id)`.
- **Metrics layer** — `Gateway::metrics()` for `view_with_seq()`, `snapshot_at()`, `latest_snapshot()`.
- **Provider health** — `Gateway::provider_health_publisher().latest()` for topology.
- **Monitor hub** — `Gateway::subscribe_monitor()` for live broadcast; `Gateway::debug_snapshot()` for retained transcript.
- **Price table** — `Gateway::price_table()` for cost roll-ups.
- **Dashboard auth** — `Gateway::dashboard_auth()` for WS auth.
