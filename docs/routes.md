# Routes

| Method | Path | Handler | Route line | Handler line | Description |
|-|-|-|-|-|-|
| POST | /v1/responses | `post_responses` | 135 | 1514 | OpenAI Responses API endpoint (streaming + non-streaming) |
| POST | /v1/messages | `post_messages` | 136 | 1811 | Anthropic Messages: native passthrough when selected, otherwise converted to Responses |
| POST | /v1/messages/count_tokens | `post_count_tokens` | 137 | 1828 | Anthropic token counting: native passthrough when selected, otherwise backend /tokenize |
| HEAD | /v1/messages | `probe_messages` | 138 | 905 | CORS/allow-method probe for /v1/messages |
| OPTIONS | /v1/messages | `probe_messages` | 139 | 905 | CORS/allow-method probe for /v1/messages |
| POST | /v1/chat/completions | `post_chat_completions` | 140 | 1968 | OpenAI Chat Completions API endpoint (converted to Responses internally) |
| POST | /v1/completions | `post_completions` | 141 | 2001 | Raw passthrough proxy to upstream /v1/completions |
| GET | /v1/models | `get_models` | 142 | 3060 | List models (Anthropic-format when client sends anthropic-version header) |
| GET | /metrics | `get_metrics` | 143 | 2019 | Raw Prometheus scrape passthrough from the first configured primary backend |
| GET | /health | `get_health` | 144 | 909 | Health check (unauthenticated) |
| GET | / | `get_root` | 145 | 917 | Root status check (unauthenticated, `{"status":"ok"}`) |
| GET | /dashboard/api/flows | `dashboard_flows` | 239 | dashboard_api.rs:1549 | Flow table: live FlowStore merged with durable history, filtered/paged/cursored |
| GET | /dashboard/api/flows/{id} | `dashboard_flow_detail` | 240 | dashboard_api.rs:1821 | Single flow detail (3-pane inspector: captured bodies, headers, deltas, usage, cost) |
| POST | /dashboard/api/flows/{id}/kill | `dashboard_flow_kill` | 241 | 358 | Kill a live flow (mutation-gated) |
| GET | /dashboard/api/metrics | `dashboard_metrics` | 242 | dashboard_api.rs:2000 | Live stats tiles + `metrics_seq` (per-window true per-second rates, priced) |
| GET | /dashboard/api/overview | `dashboard_overview` | 243 | dashboard_api.rs:2052 | Windowed (m1/m5/h1) overview aggregate from latest or nearest retained cut |
| GET | /dashboard/api/topology | `dashboard_topology` | 244 | dashboard_api.rs:2216 | Provider topology (nodes + edges) + price table + `topology_seq` |
| GET | /dashboard/api/catalog | `dashboard_catalog` | 245 | dashboard_api.rs:2309 | Model catalog from upstream (refreshed on demand; empty array on fetch failure) |
| GET | /dashboard/api/snapshot | `dashboard_snapshot` | 246 | dashboard_api.rs:2332 | Body-free frozen cut from the snapshot ring (`?at=` nearest ≤ ts) |
| GET | /dashboard/api/history | `dashboard_history` | 247 | dashboard_api.rs:2460 | Durable scrubber history points materialized from persisted cuts (downsampled) |
| GET | /dashboard/api/durability | `dashboard_durability` | 248 | dashboard_api.rs:2542 | Dashboard history persistence metadata (enabled state, DB bytes, dropped writes) |
| GET | /dashboard/api/theater | `dashboard_theater` | 249 | dashboard_api.rs:1683 | Last-terminal-flow theater replay view (live or `?cut_id=`) with data-quality label |
| GET | /dashboard/api/flows/summary | `dashboard_flow_summary` | 250 | dashboard_api.rs:1736 | Body-free flow summaries, filterable, live store merged with durable history |
| GET | /debug | `debug_index` | 257 | debug_ui.rs:47 | Session-gated debug UI HTML shell |
| GET | /debug/app.js | `debug_app_js` | 258 | debug_ui.rs:59 | Debug UI JavaScript bundle |
| GET | /dashboard | `dashboard_index` | 277 | dashboard_ui.rs:71 | Dashboard SPA shell (login page vs. app from auth state) |
| POST | /dashboard/login | `dashboard_login` | 278 | dashboard_auth.rs:831 | Authenticate with token, set session cookie |
| POST | /dashboard/logout | `dashboard_logout` | 279 | dashboard_auth.rs:868 | Clear session cookie |
| GET | /debug/ws | `debug_ws` | 280 | debug_ui.rs:93 | Debug WebSocket (self-gated via cookie + Origin + exp) |
| GET | /dashboard/ws | `dashboard_ws` | 281 | dashboard_ws.rs:655 | Dashboard data WebSocket (batched Monitor/Usage/FlowStatus frames) |
| GET | /dashboard/assets/{*path} | `dashboard_asset` | 282 | dashboard_ui.rs:130 | Dashboard static assets (hashed, immutable, public) |

## Route groups

### Inference API (`/v1/*`, `/metrics`, `/health`, `/`) — lines 134-145

These are the primary external-facing API routes registered by `build_router`. All share
`log_api_call`, which enforces the inbound body cap, then selects optional native Anthropic
passthrough before opening translated-request dashboard records or turn capture.
When `LLMCONDUIT_API_TOKEN` is configured, every `/v1/*` route
requires the same token as Bearer authorization or `x-api-key`; comparison is constant-time.
Loopback serving may omit the token, while startup refuses wildcard/non-loopback unauthenticated
serving unless `LLMCONDUIT_ALLOW_UNAUTHENTICATED_API=1` explicitly permits it.

The `/v1/responses`, `/v1/messages`, and `/v1/chat/completions` POST handlers use the engine's
`Gateway`; `/v1/completions` is a raw passthrough proxy. These translated/provider paths use a
header allowlist that excludes inbound credentials. `anthropic_passthrough` intercepts native Messages
and count-token requests in middleware and forwards subscription bearer authorization only to the validated
Anthropic origin. Its default selector is the Anthropic first-party model families (`claude-fable-*`,
`claude-opus-*`, `claude-haiku-*`), so every other model string reaches the local routes. That
transport preserves response bytes and bypasses capture gates.
See [native Anthropic routing](anthropic-subscription-proxy.md).

GET `/metrics` is a raw Prometheus scrape proxied from the first configured primary backend —
deliberately separate from `/dashboard/api/metrics`, whose JSON is gateway-owned rolling telemetry
rather than backend engine exposition. GET `/health` and GET `/` are un-instrumented
liveness/readiness endpoints. `/metrics`, `/health`, and `/` are unauthenticated (the API-token
layer only guards `/v1/*`); dashboard session authentication is independent of inference API token
authentication.

### Dashboard API (`/dashboard/api/*`) — lines 238-250

Protected read-only REST surface behind `require_session` (401 when unauthed), stamped with `no-store` response headers via `dashboard_api_no_store` route-level middleware (including extractor rejections). The only mutation endpoint is `POST /dashboard/api/flows/{id}/kill`, which is additionally gated by `MutationPolicy` (CSRF + `allow_mutations` config). Most read handlers accept an optional `?cut_id=` to serve the same view from a persisted historical cut instead of live state.

### Debug UI (`/debug`, `/debug/app.js`) — lines 256-258

Session-gated HTML/JS served behind `require_session`. The debug WebSocket (`/debug/ws`) is self-gated inside the handler (cookie + Origin + exp check).

### Dashboard UI (`/dashboard`, `/dashboard/login`, `/dashboard/logout`, `/dashboard/assets/{*path}`) — lines 276-282

The SPA shell and its public assets. `/dashboard/login` and `/dashboard/logout` read the auth `Extension` to sign/clear cookies but are NOT behind `require_session` (login is how you authenticate; logout must work for any state). `/dashboard/assets/{*path}` serves hashed, immutable sub-resources publicly.

### WebSocket endpoints — lines 280-281

Both `/debug/ws` and `/dashboard/ws` are self-gated inside their handlers (cookie + Origin allow-list + exp check) rather than behind `require_session`, so the WS Origin check is authoritative and the handler owns its rejection.

### Fallback — line 161

All unmatched paths yield a 404 `"not found"` via `api_not_found` (line 892).
