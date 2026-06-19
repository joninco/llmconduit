---
name: llmconduit-architecture
description: High-level architecture map of the llmconduit Rust gateway — ingress APIs, internal Responses model, egress chat/completions, key modules and their interactions.
metadata:
  type: project
---

# llmconduit (Rust, edition 2024)

LLM API gateway: accepts OpenAI Responses, OpenAI Chat Completions, and Anthropic Messages requests, normalizes to the internal Responses model, calls an OpenAI-compatible upstream `/v1/chat/completions`, then converts streamed deltas back to the caller's protocol. It also proxies legacy `/v1/completions`, supports Brave Search as a server-side `web_search` tool, per-model profiles/templates, multi-upstream routing, per-upstream nested failover, JSONL upstream request logging, prefix-replay caching, and an optional debug UI over WebSocket.

## Crate Layout
- `src/main.rs` — clap CLI entry, tracing init, raw-mode log suppression, `axum::serve`
- `src/lib.rs` — `build_app_with_gateway_and_options` wires `Config -> reqwest::Client -> ReplayStore -> UpstreamClient (Reqwest/Failover/Routing) -> BraveSearchClient -> Gateway -> Router`. `AppOptions { with_debug_ui }`.
- `src/cli.rs` — `Cli`, `Commands { Start, Configure, AnalyzeLog }`, `run_configure_flow` (dialoguer). `analyze-log` can take `--path` or read `upstream_request_log_path` from config.
- `src/config.rs` (1800L) — `Config`/`PersistedConfig` with env overrides (`LLMCONDUIT_*`, `OPENAI_API_KEY`, `BRAVE_SEARCH_API_KEY`), explicit `upstreams`, legacy top-level upstreams, nested fallback upstreams, model profiles + templates (`extends`), and per-request resolution of `upstream_model`, upstream chat kwargs, and system prefix.
- `src/http.rs` (1265L) — axum Router: `POST /v1/responses`, `POST|HEAD|OPTIONS /v1/messages`, `POST /v1/chat/completions`, `POST /v1/completions` (raw proxy), `GET /v1/models`, `GET /health`, `GET /`, optional `GET /debug` + `/debug/ws`. `log_api_call` reads bodies up to 256 MiB, redacts secrets, hashes payloads, and logs per-protocol summaries. `transform_models_response_for_anthropic` returns Anthropic-shaped paginated model data when `anthropic-version` or `anthropic-beta` is present.
- `src/engine.rs` (2541L) — `Gateway::stream_responses` is the core orchestrator. `run_turn` emits `response.created`/`in_progress`, applies system prefix, finds replay baseline (longest prefix match), builds upstream defaults while removing values shadowed by explicit request fields, lowers Responses->chat, streams chunks through `StreamState::apply_chunk`, dispatches `StreamEmission` variants to SSE, handles client tool calls (handoff) vs `web_search` (in-process Brave + result injection), enforces configured `max_web_search_rounds` plus a hard ceiling of 25, stores replay records, and emits `response.completed` or `response.incomplete`. Hosts an `UpstreamModelCatalog` cache (TTL 300s) for model-name normalization.
- `src/upstream.rs` (1981L) — `UpstreamClient` trait + `ReqwestUpstreamClient` (chat/models/proxy_completions), `FailoverUpstreamClient` (prefetches first chunk before committing; cooldown state via `Mutex<Vec<ProviderCooldownState>>`; can fail over `/v1/completions` on 408/429/5xx), `RoutingUpstreamClient` (300s catalog union; routes chat and completions by model id or exposed fallback alias). `sanitize_chat_request` removes empty auto `tool_choice`, stringifies structured message/tool fields, and optionally flattens text-only content arrays.
- `src/replay.rs` — `ReplayStore` keyed by SHA256 of (model, instructions, visible_history). LRU via `VecDeque`, async `RwLock`. Stores internal `ChatMessage` history alongside public `ResponseItem` history so we can resume mid-turn without reconverting.
- `src/search.rs` — `SearchClient` trait + `BraveSearchClient` (X-Subscription-Token header). Returns `SearchOutcome { formatted, sources }`.
- `src/monitor.rs` (1215L) — `MonitorHub` broadcast channel + retained history (30 min, 512 events) for debug UI; `MonitorEventKind` enum mirrors run_turn lifecycle. Data-image URLs are redacted/extracted.
- `src/debug_ui.rs` — `/debug` serves bundled `debug.html`; `/debug/ws` upgrades to WebSocket and replays snapshot + live stream.
- `src/raw.rs` — `RawOutput` writes any `*.delta` SSE event's string `delta` field to stdout (when `--raw`)
- `src/request_log.rs` — `analyze_request_log` diffs consecutive JSONL entries to find prefix-instability hotspots
- `src/error.rs` — `AppError` (status/message/client_message split so internal errors hide details; cancelled=499)

## Models (`src/models/`)
- `responses.rs` — Internal canonical type. `ResponsesRequest` accepts bare-string `input`, defaults `store=true`, and flattens unknown keys into `extra_body: BTreeMap`. `ResponseItem` covers Message/Reasoning/FunctionCall/FunctionCallOutput/CustomToolCall(+Output)/ToolSearchCall(+Output)/LocalShellCall/WebSearchCall/ImageGenerationCall. `ToolSpec`: Function/Namespace/ToolSearch/LocalShell/WebSearch/Custom/ImageGeneration.
- `chat.rs` — OpenAI Chat Completions wire types. Blank/missing model deserializes to `""`; `deserialize_opt_stop` accepts string|array|null; `normalize_stop` drops empty strings and rejects >4 sequences. `max_tokens` accepts aliases `max_output_tokens`/`max_completion_tokens`.
- `anthropic.rs` — Anthropic Messages wire types (request + stream events + content blocks including `thinking`, `redacted_thinking`, `tool_use`, `server_tool_use`, `web_search_tool_result`). Tools without `input_schema` default to `{type:"object",properties:{}}`.

## Adapters (`src/adapters/`)
Each direction is a separate module; conversion is pure (no IO):
- `anthropic_to_responses.rs` (1539L) — `convert_request(AnthropicRequest) -> ResponsesRequest`. Rejects unsupported `top_k`; maps system/messages/images/tools, `thinking` budget -> Responses reasoning, `output_config.format` -> `TextControls`, and `stop_sequences` into `extra_body.stop`. Also strips Claude Code billing/date/local-command artifacts and lifts skill/system reminder content into private instructions.
- `chat_completions.rs` (990L) — `convert_request` (Chat -> Responses, `store=false`) plus `ChatCompletionStreamConverter` and `ChatCompletionCollector` (Responses SSE -> Chat SSE/non-streaming). Chat function tool named `web_search` maps to provider-side `ToolSpec::WebSearch`; server-side `web_search_call` items are hidden from Chat output.
- `chat_to_responses.rs` (1218L) — `StreamState` + `StreamEmission` + `FinalizedAssistantTurn`. Consumes raw upstream `ChatCompletionChunk`s and emits internal events (`OutputTextDelta`, `ReasoningTextDelta`, `FunctionCallArgumentsDelta`, refusal/content-part events, etc.). Supports OpenRouter sparse tool chunks, legacy `function_call`, nested `thinking`, and Kimi/vLLM sentinel cleanup before JSON argument parsing.
- `responses_to_chat.rs` (1468L) — `lower_request(ResponsesRequest, baseline_messages) -> LoweredTurn { messages, tools, tool_registry, ... }`. Maps canonical Responses into OpenAI chat messages + tool schemas, validates `tool_choice`, rejects `previous_response_id`, normalizes `developer` to `system`, hoists initial system messages, normalizes reasoning effort (`medium`/unknown -> `high`), strips `ImageGeneration` tools, and builds a `ToolRegistry` so streamed tool names map back to public kinds (Function/Custom/LocalShell/ToolSearch/WebSearch).
- `responses_to_anthropic.rs` (1671L) — `AnthropicStreamConverter` + `AnthropicStreamCollector`. Translates internal Responses SSE into Anthropic `ping`/`message_start`/`content_block_*`/`message_delta`/`message_stop`, estimates progressive output usage, maps incomplete max-token stops, handles `response.web_search_results` (additive event) as `server_tool_use` + `web_search_tool_result`, and `finalize()` emits synthetic terminal events if the canonical stream ends without `response.completed`.

## Data Flow (single request)
1. axum middleware `log_api_call` reads the body, redacts secrets for payload dumps, emits body SHA256 + protocol summary, then reconstructs the request.
2. Handler deserializes the wire request. Chat/Anthropic handlers call `gateway.resolve_request_model` for response model hints; the gateway also resolves again for canonical execution.
3. Per-protocol handler converts request -> canonical `ResponsesRequest` (`chat_completions::convert_request`, `anthropic_to_responses::convert_request`, or identity for `/v1/responses`).
4. `Gateway::stream_responses` applies global/profile system prefix, finds a replay baseline when `store=true`, strips cached input prefix, strips `web_search` when Brave is disabled, relaxes impossible forced tool choices, lowers to chat, returns `ReceiverStream<SseEvent>`, and spawns `run_turn`.
5. `run_turn` builds an upstream `ChatCompletionRequest` with `stream=true`, `stream_options.include_usage=true`, and `parallel_tool_calls=false`; typed request fields remove conflicting configured defaults, while `chat_template_kwargs` is deep-merged with explicit request values winning.
6. Upstream stream loop pipes chunks into `StreamState::apply_chunk`, dispatches `StreamEmission`s as canonical Responses SSE events, mirrors deltas to `RawOutput` when enabled, accumulates usage, and checks `tx.closed()` between long-running steps.
7. After each upstream stream ends, `finalize` resolves tool calls. Client-side tools are handed off (`function_call_arguments.done` + output item); all-provider-side `web_search` calls run Brave, inject a tool result into internal chat history, emit `response.web_search_results` for Anthropic clients, relax forced `tool_choice` to `auto`, and loop. Mixed provider-side/client-side tool batches are rejected.
8. If no loop continues, the turn stores a replay record when `store=true`, marks `response.incomplete` when upstream finish_reason is `length`, otherwise emits `response.completed`.
9. Caller-facing handler wraps the canonical SSE stream with the protocol-specific converter (`ChatCompletionStreamConverter` or `AnthropicStreamConverter`) or collector for non-streaming.
10. On `tx.closed()` disconnect, the spawned task returns `AppError::cancelled()` and the monitor records `Failed { "client disconnected" }` instead of continuing upstream work.

## Key Conventions / Quirks
- Internal protocol is always OpenAI Responses; both ingress conversion and egress conversion happen on the edges.
- `extra_body` is the catch-all: `flatten`'d via serde into `BTreeMap` so upstream-specific kwargs round-trip unchanged. Do not add typed fields for provider-specific knobs unless a canonical field exists.
- Configured upstream kwargs merge in layers. `Config::merge_json_maps` deep-merges objects; request-level `extra_body.chat_template_kwargs` is also deep-merged into defaults, with request values winning. Explicit typed request fields remove conflicting default keys such as `temperature`, `top_p`, `max_tokens`/`max_output_tokens`/`max_completion_tokens`, `response_format`, and `reasoning_effort`.
- `model_profile_templates` + `model_profiles` with `extends:` give ordered template inheritance. Profile-root shorthand keys merge into `upstream_chat_kwargs`; explicit `upstream_chat_kwargs:` overrides shorthand on conflict.
- Runtime profile lookup considers resolved catalog model, configured remap target, then original request model, de-duplicated in that order. For kwargs this means request-model profile wins over backend-model profile on conflicts. For system prefix, global prefix is prepended and the most specific matched profile prefix is appended before request `instructions`.
- Multi-upstream `upstreams: [...]` and per-upstream `fallback_upstreams: [...]`. Failover applies ONLY within an upstream's nested fallbacks. Cross-upstream "next provider" is for routing by model id, not failure.
- Model routing uses `canonical_model_key` (ASCII alphanumeric lowercase). Exact id wins; a normalized alias routes only when it maps to one unique id. Missing, blank, unavailable, or ambiguous requests default to the first model from the first provider catalog.
- `exposed_model` makes a nested fallback advertise an alias in `/v1/models`; routing maps that alias back to the declaring fallback provider for both chat and completions proxy.
- `FailoverUpstreamClient::prefetch_first_chunk` is a defensive pattern: chat failover ONLY happens before the first chunk is yielded; mid-stream failures are surfaced as errors. Completions proxy can fail over before returning a response on 408/429/5xx.
- Web search rounds are hard-capped at 25. `max_web_search_rounds = 0` means "use the ceiling", not unlimited. Runtime `web_search` execution supports only search/query actions; `open_page`, `find_in_page`, and unknown actions return upstream errors.
- After stripping `web_search` tool (no Brave key), `relax_tool_choice_after_stripping_tool` rewrites forced choices to `"auto"` so the model is not trapped. After a successful provider-side search round, forced `tool_choice` is also relaxed for the continuation round.
- ReplayStore hashes `(model, instructions, items)`. `longest_prefix_match` walks from full length down to 0, returning the first hit. Records store both `visible_history` (Responses items) and `internal_messages` (ChatMessages) so replay can resume without re-lowering. Chat and Anthropic ingress set `store=false`; raw Responses defaults to `store=true`.
- `/v1/models` passes through upstream ETag only for OpenAI-style responses. Anthropic-shaped model responses omit ETag and support `after_id`, `before_id`, and `limit` (1..=1000, default 20).
- Anthropic stream converter has `finalize()` that emits synthetic `message_delta` + `message_stop` if the canonical stream ends without `response.completed`, so clients are not left hanging behind SSE keep-alive.
- `MonitorHub::disabled()` returns before touching state; debug payload previews are only produced when the hub is enabled. Data-image URLs in debug previews are extracted/redacted with metadata.
- `AppError::cancelled()` uses status 499 (nginx convention). `AppError::internal` hides detail from clients (`client_message: "internal server error"`) but logs the full message.
- CLI/reporting paths (`configure`, `analyze-log`, `--raw`) intentionally write to stdout; server logs use `tracing`.

## Tests
- `tests/gateway.rs` (5709L) — 79 `#[tokio::test]` integration tests using `MockUpstream`, `PendingChunkUpstream`, `MockSearch`, and wiremock for HTTP-level routing/failover/proxy tests.
- Unit tests in most modules (`#[cfg(test)] mod tests`) cover config/profile merging, streaming adapters, upstream sanitization, replay, request-log analysis, raw output, monitor formatting, and error response privacy.

## Dependencies
axum 0.8 (json, macros, ws), reqwest 0.12 (rustls), tokio 1, serde/serde_json/serde_yaml, async-stream, eventsource-stream, futures, sha2/hex, uuid, clap 4.5, dialoguer 0.12, dirs 6, thiserror 2, tracing/tracing-subscriber, url. dev: wiremock 0.6, tower, pretty_assertions, http-body-util.

## Docker
Distroless cc-debian12:nonroot, listens on 0.0.0.0:4000 (overrides default 127.0.0.1).
