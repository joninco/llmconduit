# AGENTS.md

Guidance for AI coding agents working in this repo.

## What this is

Rust LLM API gateway. Accepts OpenAI Responses, OpenAI Chat Completions, and Anthropic Messages on the front; forwards to OpenAI-compatible `/v1/chat/completions` upstream, and proxies legacy `/v1/completions`. Adds server-side Brave web search, per-model defaults, nested failover/model routing across upstreams, replay caching, request-log analysis, and an optional debug UI.

Full architecture map: `llmconduit-architecture.md` — read first when touching unfamiliar areas.

## Commands

```bash
cargo build --release          # build
cargo test                     # run all tests
cargo test <name>              # run single test by substring
cargo clippy --all-targets     # lint
cargo fmt                      # format
```

Run locally:

```bash
./target/release/llmconduit configure     # interactive YAML config
./target/release/llmconduit start         # serve
./target/release/llmconduit start --raw   # also write delta text to stdout
./target/release/llmconduit start --with-debug-ui   # exposes /debug + /debug/ws (and, when built, /dashboard)
./target/release/llmconduit analyze-log   # prefix-stability diff of upstream JSONL
```

Dashboard (Topic 13 — optional, opt-in embed):

```bash
LLMCONDUIT_BUILD_DASHBOARD=1 cargo build --release   # build the React SPA + embed via include_dir
cd dashboard-frontend && npm install && npm run dev  # frontend dev against an in-browser mock
cargo build --release                                 # node-less host: embeds a stub, still compiles
```
`/dashboard` + `/dashboard/api/*` + `/dashboard/ws` are registered only when `--with-debug-ui` is on.
Dashboard auth (env-only, never a persisted `Config` field): `LLMCONDUIT_DASHBOARD_TOKEN`,
`LLMCONDUIT_DASHBOARD_SESSION_KEY`, `LLMCONDUIT_DASHBOARD_PUBLIC_ORIGIN` (must be `https://` on
non-loopback — startup refuses to register `/dashboard` + `/debug` otherwise;
`LLMCONDUIT_ALLOW_INSECURE_DASHBOARD=1` overrides). Kill requires `LLMCONDUIT_DASHBOARD_ALLOW_MUTATIONS=1`
+ a CSRF token. See `.ralph/specs/D7-dashboard-auth-and-ws.md`.

## Code layout

| Path | Role |
|-|-|
| `src/main.rs` | Binary entry, tokio + tracing |
| `src/lib.rs` | DI root: `build_app_with_gateway_and_options` |
| `src/cli.rs` | clap CLI + interactive configure |
| `src/config.rs` | `Config`/`PersistedConfig`, profile resolution, env overrides |
| `src/http.rs` | axum router, body-logging middleware, secret redaction, `/v1/models` transform |
| `src/engine.rs` | `Gateway`, `run_turn` -- streaming + replay + tool-loop orchestration |
| `src/upstream.rs` | `Reqwest`/`Failover`/`Routing` upstream clients, `/v1/completions` proxy |
| `src/replay.rs` | SHA256-keyed LRU replay cache, longest-prefix match |
| `src/search.rs` | Brave Search client, `SearchClient` trait |
| `src/monitor.rs` | Debug-UI broadcast hub |
| `src/debug_ui.rs` | `/debug` HTML + WS handler |
| `src/dashboard_flow.rs` | (T13) DashboardFlowStore — authoritative per-flow records + capture seams |
| `src/metrics.rs` | (T13) MetricsLayer — ring buffers, histograms, 5 s body-free snapshots |
| `src/dashboard_api.rs` | (T13) `/dashboard/api/*` REST handlers |
| `src/dashboard_auth.rs` / `src/dashboard_ws.rs` | (T13) dashboard session-cookie auth + batched WS envelope |
| `src/dashboard_ui.rs` | (T13) `include_dir!`-embedded SPA shell + static assets |
| `dashboard-frontend/` | (T13) React + TS + Vite SPA (Vite build → `dist`, embedded when `LLMCONDUIT_BUILD_DASHBOARD=1`) |
| `src/raw.rs` | `--raw` stdout delta writer |
| `src/request_log.rs` | `analyze-log` impl |
| `src/error.rs` | `AppError` (client vs internal message split) |
| `src/adapters/` | Pure conversion layer between wire formats |
| `src/models/` | `responses.rs`, `chat.rs`, `anthropic.rs` wire types |
| `tests/gateway.rs` | Integration tests with `MockUpstream`, `MockSearch`, `PendingChunkUpstream` + wiremock |

## Canonical protocol

OpenAI Responses is the **single canonical internal protocol**. All inbound shapes convert in via adapters; all outbound shapes convert out via streaming converters. Do not add direct adapters between non-canonical shapes — go through Responses.

Adapter direction map:

| Module | Direction |
|-|-|
| `adapters/anthropic_to_responses.rs` | Anthropic request → canonical |
| `adapters/chat_completions.rs` | Chat request → canonical; canonical SSE → Chat SSE/non-stream; hides server-side `web_search` |
| `adapters/responses_to_chat.rs` | Canonical → `LoweredTurn` (chat messages + `ToolRegistry`) |
| `adapters/chat_to_responses.rs` | Upstream chunk stream → canonical SSE |
| `adapters/responses_to_anthropic.rs` | Canonical SSE → Anthropic SSE/non-stream |

## Conventions

- **`extra_body: BTreeMap<String, Value>` flattened** on `ResponsesRequest` and `ChatCompletionRequest`. Vendor-specific kwargs round-trip without schema changes. Prefer this over adding typed fields for provider-specific knobs.
- **Explicit request fields win over configured upstream defaults.** When building upstream chat requests, typed fields remove conflicting default keys (`temperature`, `top_p`, max-token aliases, penalties, `response_format`, `reasoning_effort`); request `extra_body.chat_template_kwargs` deep-merges over configured defaults.
- **No new wire fields without round-trip tests.** If you add a field, add a deserialize-then-serialize test that proves it survives.
- **`#[serde(deny_unknown_fields)]` is NOT used** on request types so unknown fields can flow into `extra_body`. Be careful adding it.
- **`tracing` for server logs, not `println!`.** CLI/reporting stdout is allowed for `configure`, `analyze-log`, and `RawOutput`.
- **Errors via `AppError`.** Use `AppError::internal(...)` when the detail must not reach the client — internal logs full message, client gets `"internal server error"`. Use `AppError::cancelled()` (HTTP 499) when the client hung up mid-stream.
- **Trait objects (`Arc<dyn UpstreamClient>`, `Arc<dyn SearchClient>`) on seams.** Tests inject mocks; don't reach for concrete types in `Gateway`.
- **Comments explain WHY**, not what. See clusters around `engine.rs:684`, `engine.rs:1027`, `engine.rs:1480` for examples.

## Hard rules in the engine

These are intentional and load-bearing. Do not change without strong reason + matching test.

- **`parallel_tool_calls: false`** forced upstream regardless of caller (`engine.rs:707-726`). Multi-tool turns interleave badly with replay + web-search loops.
- **`WEB_SEARCH_ROUNDS_HARD_CEILING = 25`** in `engine.rs:1032`, enforced regardless of config. Defense against infinite tool loops.
- **`OPENAI_MAX_STOP_SEQUENCES = 4`** in `chat.rs:81`. Returns 400 — do not silently truncate.
- **`API_LOG_BODY_LIMIT_BYTES` / `API_LOG_PAYLOAD_DUMP_LIMIT_BYTES`** in `http.rs:51-52`. Don't bypass.
- **Failover only pre-first-chunk** (`upstream.rs:407-419`). Mid-stream provider failure surfaces as error — never retry, never duplicate tokens.
- **Routing providers are not failure fallbacks.** With explicit `upstreams`, only the selected upstream's nested `fallback_upstreams` are failover candidates. Never fail over to the next routing upstream just because the selected provider failed.
- **`web_search` tool stripped from request when `brave_api_key` is unset.** Engine also relaxes `tool_choice` to `"auto"` when the only tool was stripped (`engine.rs:1536-1558`).
- **Provider-side `web_search` is single-purpose.** Runtime execution supports search/query actions only; `open_page`, `find_in_page`, and unknown actions are rejected. Failed/timed-out Brave calls are injected as model-visible text so the turn can complete.
- **Mixed provider-side and client-side tool calls are rejected.** A turn cannot hand off client tools and run Brave search in the same upstream tool-call batch (`engine.rs:1290-1357`).
- **`response.web_search_results`** is a non-standard additive SSE event consumed only by the Anthropic converter. OpenAI clients ignore unknown events, so this stays compatible. See `engine.rs:1480-1485`.
- **`previous_response_id` is unsupported** and must continue to return 400 from canonical lowering. Replay is internal SHA256-prefix state, not OpenAI hosted response retrieval.
- **Image generation tools are stripped before upstream.** They remain accepted in Responses wire types but are not sent as chat tools.

## Config resolution order

Global → matched model profile templates (`extends:` in order) → matched model profile → explicit request fields. Profile-root shorthand keys merge into `upstream_chat_kwargs` via custom `Deserialize` (`config.rs:60-89`); explicit `upstream_chat_kwargs:` wrapper overrides shorthand on conflict.

Profiles are considered against the resolved catalog model, the configured upstream-model remap target, and the original request model, de-duplicated in that order. For kwargs, later matches override earlier matches, so request-model profile settings beat backend-model profile settings on conflict. For `system_prompt_prefix`, global prefix is prepended and the most specific matched profile prefix is appended before request `instructions`.

`upstreams: [...]` switches the app to model-routing mode. `/v1/models` exposes the ordered union of primary upstream model catalogs plus fallback `exposed_model` aliases. Exact model id wins; normalized alias routing uses `canonical_model_key` and only succeeds when it maps to one unique id. Blank/missing/unavailable/ambiguous models default to the first model in the first non-empty provider catalog.

## Testing

- Integration tests: `tests/gateway.rs` (one file, ~5700 lines, 79 `#[tokio::test]` functions). Use `MockUpstream` (`tests/gateway.rs:51-99`) for in-process gateway tests, `MockSearch` for Brave behavior, `PendingChunkUpstream` for cancellation, or wiremock for HTTP-level routing/failover/proxy behavior.
- Prefer adding to `tests/gateway.rs` over creating new test files unless the new file is a focused topic suite.
- Streaming tests: collect SSE events into a `Vec<SseEvent>` and assert on the sequence, not on timing.
- Replay tests must hash the same `(model, instructions, items)` tuple as `replay::hash_visible_history` — keep them in sync.
- Adapter tests should include both streaming and non-streaming collectors when converter behavior changes.

## Don'ts

- Don't add direct converter between two non-canonical shapes — go through Responses.
- Don't add a typed field for a provider-specific knob if `extra_body` works.
- Don't bypass `redact_payload_secrets` in `http.rs` when adding new logged surfaces.
- Don't introduce blocking IO on the tokio runtime. Upstream request log uses `spawn_blocking` for a reason.
- Don't silence cancellation. Every long-running task in `run_turn` selects on `tx.closed()` so client hang-up cancels upstream work — preserve that pattern.
- Don't lower the hard ceilings listed above.
- Don't leak server-side Brave search internals into Chat Completions output. Chat hides `web_search_call`; Anthropic gets `server_tool_use` + `web_search_tool_result` from `response.web_search_results`.
- Don't add CI/CD or new top-level files without checking with the user first.
- Don't store the dashboard auth TOKEN/SESSION_KEY in the persisted `Config` struct (it's `#[derive(Debug, Clone)]` — secrets would leak) — read them env-only in the dashboard auth layer.
- Don't retain `Bytes` slices of the 256 MiB middleware body buffer in the dashboard FlowStore — copy via the capped/redacting streaming serializer (a slice keeps the whole backing allocation alive).
- Don't put dashboard snapshot bodies on historical snapshots — snapshots hold body-free `SnapshotFlowSummary` only (body retention on snapshots recreates a 135 GiB worst case).
- Don't drive a single global `seq` watermark across monitor/flow/metrics/topology — use per-domain `{domain, seq}` cursors (a global watermark discards valid lower-numbered sibling frames).

## Quick gotchas

- `flatten_content` defaults to `true` — multimodal text-only content gets flattened to bare string before going upstream. Some providers expect arrays; the option is configurable.
- `OPENAI_API_KEY` is a fallback upstream key when `upstream_api_key` is unset.
- Chat and Anthropic ingress set canonical `store=false`; raw Responses defaults to `store=true`, enabling replay unless the caller disables it.
- `/v1/messages` has HEAD/OPTIONS probe routes returning `204` with `Allow: POST, HEAD, OPTIONS`.
- `/v1/models` is reshaped to Anthropic-style pagination when `anthropic-version` or `anthropic-beta` is present; OpenAI-style responses can preserve upstream `ETag`, Anthropic-shaped responses do not.
- `/health` returns `{"status":"healthy"}` and `/` returns `{"status":"ok"}`. There is no `/healthz` route.
- `/v1/completions` is a raw upstream proxy with header filtering. In routing mode it resolves the request body `model`, including exposed fallback aliases.
- `MonitorHub::disabled()` is a no-op used when `--with-debug-ui` is off — production has zero monitor overhead, don't add unconditional broadcast sends.
- `UpstreamModelCatalog` caches `/v1/models` for 300s (`engine.rs:56`). Tests that depend on catalog changes need to construct a fresh `Gateway`.
- `RoutingUpstreamClient` also caches the union model catalog for 300s (`upstream.rs:32`). Tests that change provider catalogs need a fresh router/client or must account for the cache.
- `RawOutput` writes every SSE event whose name ends with `.delta` and has a string `delta`, including reasoning/refusal/tool-argument deltas; raw mode suppresses tracing output to keep stdout clean.
- Dashboard (T13): `/v1/completions` is a raw passthrough and is NOT instrumented by the FlowStore on purpose (it bypasses the engine); the FlowStore whitelist is `/v1/responses`,`/v1/messages`,`/v1/chat/completions` only.
- `LLMCONDUIT_BUILD_DASHBOARD` is a BUILD-time env var (build.rs runs `npm run build` + embeds via `include_dir!`); without it `cargo build` still succeeds on a node-less host (a stub shell is embedded). Runtime gating is the existing `--with-debug-ui`.
- The dashboard WS envelope is BATCHED `DashboardFrame{domain,seq,batch:Vec<payload>}` (one per `DebugUpdate`, seq = `DebugUpdate.sequence`) — `/debug/ws` keeps the BARE `DebugWsMessage` contract untouched. Don't add per-frame seq to `DebugWsMessage`.
