# Routes

| Method | Path | Handler | Route line | Handler line | Description |
|-|-|-|-|-|-|
| POST | /v1/responses | `post_responses` | 109 | 1301 | OpenAI Responses API endpoint (streaming + non-streaming) |
| POST | /v1/messages | `post_messages` | 110 | 1320 | Anthropic Messages API endpoint (converted to Responses internally) |
| POST | /v1/messages/count_tokens | `post_count_tokens` | 111 | 1332 | Anthropic token counting (lowers to Chat, proxies /tokenize upstream) |
| HEAD | /v1/messages | `probe_messages` | 112 | 697 | CORS/allow-method probe for /v1/messages |
| OPTIONS | /v1/messages | `probe_messages` | 113 | 697 | CORS/allow-method probe for /v1/messages |
| POST | /v1/chat/completions | `post_chat_completions` | 114 | 1417 | OpenAI Chat Completions API endpoint (converted to Responses internally) |
| POST | /v1/completions | `post_completions` | 115 | 1450 | Raw passthrough proxy to upstream /v1/completions |
| GET | /v1/models | `get_models` | 116 | 1921 | List models (Anthropic-format when client sends anthropic-version header) |
| GET | /health | `get_health` | 117 | 701 | Health check (unauthenticated) |
| GET | / | `get_root` | 118 | 709 | Root status check (unauthenticated, `{"status":"ok"}`) |
| GET | /dashboard/api/flows | `dashboard_flows` | 183 | dashboard_api.rs:958 | List dashboard flow records (paginated) |
| GET | /dashboard/api/flows/{id} | `dashboard_flow_detail` | 184 | dashboard_api.rs:1003 | Single flow record detail |
| POST | /dashboard/api/flows/{id}/kill | `dashboard_flow_kill` | 185 | 291 | Kill a live flow (mutation-gated) |
| GET | /dashboard/api/metrics | `dashboard_metrics` | 186 | dashboard_api.rs:1082 | Dashboard aggregate metrics |
| GET | /dashboard/api/topology | `dashboard_topology` | 187 | dashboard_api.rs:1092 | Live model topology |
| GET | /dashboard/api/catalog | `dashboard_catalog` | 188 | dashboard_api.rs:1113 | Model catalog from upstream (refreshed on demand) |
| GET | /dashboard/api/snapshot | `dashboard_snapshot` | 189 | dashboard_api.rs:1136 | Snapshot of all active flows |
| GET | /debug | `debug_index` | 196 | debug_ui.rs:47 | Session-gated debug UI HTML shell |
| GET | /debug/app.js | `debug_app_js` | 197 | debug_ui.rs:59 | Debug UI JavaScript bundle |
| GET | /dashboard | `dashboard_index` | 216 | dashboard_ui.rs:66 | Dashboard SPA shell (login page vs. app from auth state) |
| POST | /dashboard/login | `dashboard_login` | 217 | dashboard_auth.rs:831 | Authenticate with token, set session cookie |
| POST | /dashboard/logout | `dashboard_logout` | 218 | dashboard_auth.rs:868 | Clear session cookie |
| GET | /debug/ws | `debug_ws` | 219 | debug_ui.rs:93 | Debug WebSocket (self-gated via cookie + Origin + exp) |
| GET | /dashboard/ws | `dashboard_ws` | 220 | dashboard_ws.rs:959 | Dashboard data WebSocket (batched Monitor/Usage/FlowStatus frames) |
| GET | /dashboard/assets/{*path} | `dashboard_asset` | 221 | dashboard_ui.rs:122 | Dashboard static assets (hashed, immutable, public) |

## Route groups

### Inference API (`/v1/*`) — lines 108-118

These are the primary external-facing API routes registered in `build_router`. All share the `log_api_call` middleware that enforces the inbound body cap, opens dashboard flow records (D1), and manages turn capture (F1b). The `/v1/responses`, `/v1/messages`, and `/v1/chat/completions` POST handlers go through the engine's `Gateway`; `/v1/completions` is a raw passthrough proxy.

GET `/health` and GET `/` are unauthenticated, un-instrumented liveness/readiness endpoints.

### Dashboard API (`/dashboard/api/*`) — lines 182-190

Protected read-only REST surface behind `require_session` (401 when unauthed), stamped with `no-store` response headers via `dashboard_api_no_store` middleware. The only mutation endpoint is `POST /dashboard/api/flows/{id}/kill`, which is additionally gated by `MutationPolicy` (CSRF + `allow_mutations` config).

### Debug UI (`/debug`, `/debug/app.js`) — lines 195-197

Session-gated HTML/JS served behind `require_session`. The debug WebSocket (`/debug/ws`) is self-gated inside the handler (cookie + Origin + exp check).

### Dashboard UI (`/dashboard`, `/dashboard/login`, `/dashboard/logout`, `/dashboard/assets/{*path}`) — lines 215-221

The SPA shell and its public assets. `/dashboard/login` and `/dashboard/logout` read the auth `Extension` to sign/clear cookies but are NOT behind `require_session` (login is how you authenticate; logout must work for any state). `/dashboard/assets/{*path}` serves hashed, immutable sub-resources publicly.

### WebSocket endpoints — lines 219-220

Both `/debug/ws` and `/dashboard/ws` are self-gated inside their handlers (cookie + Origin allow-list + exp check) rather than behind `require_session`, so the WS Origin check is authoritative and the handler owns its rejection.

### Fallback — line 134

All unmatched paths yield a 404 `"not found"` via `api_not_found` (line 684).
