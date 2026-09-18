# Routes

| Method | Path | Handler | Route line | Handler line | Description |
|-|-|-|-|-|-|
| POST | /v1/responses | `post_responses` | 115 | 1470 | OpenAI Responses API endpoint (streaming + non-streaming) |
| POST | /v1/messages | `post_messages` | 116 | 1767 | Anthropic Messages API endpoint (converted to Responses internally) |
| POST | /v1/messages/count_tokens | `post_count_tokens` | 117 | 1784 | Anthropic token counting (lowers to Chat, proxies /tokenize upstream) |
| HEAD | /v1/messages | `probe_messages` | 118 | 861 | CORS/allow-method probe for /v1/messages |
| OPTIONS | /v1/messages | `probe_messages` | 119 | 861 | CORS/allow-method probe for /v1/messages |
| POST | /v1/chat/completions | `post_chat_completions` | 120 | 1924 | OpenAI Chat Completions API endpoint (converted to Responses internally) |
| POST | /v1/completions | `post_completions` | 121 | 1957 | Raw passthrough proxy to upstream /v1/completions |
| GET | /v1/models | `get_models` | 122 | 3016 | List models (Anthropic-format when client sends anthropic-version header) |
| GET | /metrics | `get_metrics` | 123 | 1975 | Raw Prometheus scrape passthrough from the first configured primary backend |
| GET | /health | `get_health` | 124 | 865 | Health check (unauthenticated) |
| GET | / | `get_root` | 125 | 873 | Root status check (unauthenticated, `{"status":"ok"}`) |
| GET | /dashboard/api/flows | `dashboard_flows` | 217 | dashboard_api.rs:1549 | Flow table: live FlowStore merged with durable history, filtered/paged/cursored |
| GET | /dashboard/api/flows/{id} | `dashboard_flow_detail` | 218 | dashboard_api.rs:1821 | Single flow detail (3-pane inspector: captured bodies, headers, deltas, usage, cost) |
| POST | /dashboard/api/flows/{id}/kill | `dashboard_flow_kill` | 219 | 336 | Kill a live flow (mutation-gated) |
| GET | /dashboard/api/metrics | `dashboard_metrics` | 220 | dashboard_api.rs:2000 | Live stats tiles + `metrics_seq` (per-window true per-second rates, priced) |
| GET | /dashboard/api/overview | `dashboard_overview` | 221 | dashboard_api.rs:2052 | Windowed (m1/m5/h1) overview aggregate from latest or nearest retained cut |
| GET | /dashboard/api/topology | `dashboard_topology` | 222 | dashboard_api.rs:2216 | Provider topology (nodes + edges) + price table + `topology_seq` |
| GET | /dashboard/api/catalog | `dashboard_catalog` | 223 | dashboard_api.rs:2309 | Model catalog from upstream (refreshed on demand; empty array on fetch failure) |
| GET | /dashboard/api/snapshot | `dashboard_snapshot` | 224 | dashboard_api.rs:2332 | Body-free frozen cut from the snapshot ring (`?at=` nearest ≤ ts) |
| GET | /dashboard/api/history | `dashboard_history` | 225 | dashboard_api.rs:2460 | Durable scrubber history points materialized from persisted cuts (downsampled) |
| GET | /dashboard/api/durability | `dashboard_durability` | 226 | dashboard_api.rs:2542 | Dashboard history persistence metadata (enabled state, DB bytes, dropped writes) |
| GET | /dashboard/api/theater | `dashboard_theater` | 227 | dashboard_api.rs:1683 | Last-terminal-flow theater replay view (live or `?cut_id=`) with data-quality label |
| GET | /dashboard/api/flows/summary | `dashboard_flow_summary` | 228 | dashboard_api.rs:1736 | Body-free flow summaries, filterable, live store merged with durable history |
| GET | /debug | `debug_index` | 235 | debug_ui.rs:47 | Session-gated debug UI HTML shell |
| GET | /debug/app.js | `debug_app_js` | 236 | debug_ui.rs:59 | Debug UI JavaScript bundle |
| GET | /dashboard | `dashboard_index` | 255 | dashboard_ui.rs:71 | Dashboard SPA shell (login page vs. app from auth state) |
| POST | /dashboard/login | `dashboard_login` | 256 | dashboard_auth.rs:831 | Authenticate with token, set session cookie |
| POST | /dashboard/logout | `dashboard_logout` | 257 | dashboard_auth.rs:868 | Clear session cookie |
| GET | /debug/ws | `debug_ws` | 258 | debug_ui.rs:93 | Debug WebSocket (self-gated via cookie + Origin + exp) |
| GET | /dashboard/ws | `dashboard_ws` | 259 | dashboard_ws.rs:655 | Dashboard data WebSocket (batched Monitor/Usage/FlowStatus frames) |
| GET | /dashboard/assets/{*path} | `dashboard_asset` | 260 | dashboard_ui.rs:130 | Dashboard static assets (hashed, immutable, public) |

## Route groups

### Inference API (`/v1/*`, `/metrics`, `/health`, `/`) — lines 114-125

These are the primary external-facing API routes registered in `build_router`. All share the
`log_api_call` middleware that enforces the inbound body cap, opens dashboard flow records (D1),
and manages turn capture (F1b). When `LLMCONDUIT_API_TOKEN` is configured, every `/v1/*` route
requires the same token as Bearer authorization or `x-api-key`; comparison is constant-time.
Loopback serving may omit the token, while startup refuses wildcard/non-loopback unauthenticated
serving unless `LLMCONDUIT_ALLOW_UNAUTHENTICATED_API=1` explicitly permits it.

The `/v1/responses`, `/v1/messages`, and `/v1/chat/completions` POST handlers go through the
engine's `Gateway`; `/v1/completions` is a raw passthrough proxy. Proxy/header forwarding uses a
narrow allowlist and never sends inbound authorization/API-key headers, cookies, proxy credentials,
or dashboard/session headers to an upstream.

GET `/metrics` is a raw Prometheus scrape proxied from the first configured primary backend —
deliberately separate from `/dashboard/api/metrics`, whose JSON is gateway-owned rolling telemetry
rather than backend engine exposition. GET `/health` and GET `/` are un-instrumented
liveness/readiness endpoints. `/metrics`, `/health`, and `/` are unauthenticated (the API-token
layer only guards `/v1/*`); dashboard session authentication is independent of inference API token
authentication.

### Dashboard API (`/dashboard/api/*`) — lines 216-228

Protected read-only REST surface behind `require_session` (401 when unauthed), stamped with `no-store` response headers via `dashboard_api_no_store` route-level middleware (including extractor rejections). The only mutation endpoint is `POST /dashboard/api/flows/{id}/kill`, which is additionally gated by `MutationPolicy` (CSRF + `allow_mutations` config). Most read handlers accept an optional `?cut_id=` to serve the same view from a persisted historical cut instead of live state.

### Debug UI (`/debug`, `/debug/app.js`) — lines 234-236

Session-gated HTML/JS served behind `require_session`. The debug WebSocket (`/debug/ws`) is self-gated inside the handler (cookie + Origin + exp check).

### Dashboard UI (`/dashboard`, `/dashboard/login`, `/dashboard/logout`, `/dashboard/assets/{*path}`) — lines 254-260

The SPA shell and its public assets. `/dashboard/login` and `/dashboard/logout` read the auth `Extension` to sign/clear cookies but are NOT behind `require_session` (login is how you authenticate; logout must work for any state). `/dashboard/assets/{*path}` serves hashed, immutable sub-resources publicly.

### WebSocket endpoints — lines 258-259

Both `/debug/ws` and `/dashboard/ws` are self-gated inside their handlers (cookie + Origin allow-list + exp check) rather than behind `require_session`, so the WS Origin check is authoritative and the handler owns its rejection.

### Fallback — line 141

All unmatched paths yield a 404 `"not found"` via `api_not_found` (line 848).
