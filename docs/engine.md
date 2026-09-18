# Engine

## Entry Points

| Function | Line | Description |
|-|-|-|
| `stream_responses` | 1816 | Public entry point. Thin wrapper delegating to `stream_responses_with_api_call_id` with no `api_call_id`. Returns `ReceiverStream<SseEvent>`. |
| `stream_responses_with_api_call_id` | 1823 | Full entry point: resolve model (strict for Responses ingress), resolve stored item references, apply system prefix, G4 image agent strip, E2b residual image safety, replay lookup, tool stripping, capability validation, lowering, G3 budgeting, then dispatch either the native Responses turn (`start_native_responses_turn` when the upstream wire API is `CodexResponses`) or the Chat-lowered `run_turn` in a `tokio::spawn`. Returns `ReceiverStream<SseEvent>`. |
| `run_turn` | 3608 | Core Chat-lowered turn loop: sends upstream chat requests, handles the SSE event stream (output deltas, tool calls, reasoning), manages the tool-call loop with web search / image analysis / client tool round dispatch, and emits the terminal `response.completed` or `response.incomplete` event. Returns `AppResult<TurnCompletion>`. |
| `start_native_responses_turn` | 2828 | Native Responses path: build the upstream Responses request body, stamp routing, mint the response template, and spawn `run_native_responses_turn`. Used when the upstream speaks the Responses wire API directly (no Chat lowering). Returns `AppResult<ReceiverStream<SseEvent>>`. |
| `run_native_responses_turn` | 2971 | Native Responses turn loop: stream upstream Responses events, normalize response ids, filter to the supported event set, validate function items against the offered tool registry, reconcile streamed vs terminal output items, and emit the terminal event. Returns `AppResult<TurnCompletion>`. |

## Model Resolution

| Function | Line | Description |
|-|-|-|
| `resolve_request_model` | 1606 | Resolve `request_model` to served upstream model + `genuine` flag. Calls `config.resolve_upstream_model` then `normalize_upstream_model`. |
| `resolve_responses_model` | 1615 | Strict model resolution for raw Responses ingress: requires an explicit catalog model or configured alias; fails closed when `/v1/models` is unavailable or empty (Chat/Anthropic ingress keeps the historical default-model fallback). Returns `AppResult<String>`. |
| `normalize_upstream_model` | 5048 | Walk the normalization ladder: exact catalog id, ad-hoc route match, unique canonical-key match, default catalog id. Returns `(model, genuine)` where `genuine` is `false` only on default fallback. |
| `should_warn_model_fallback` | 5146 | Rate-limit the model-fallback WARN to once per catalog-TTL window per requested model; prunes stale entries so the map stays bounded. Returns `bool`. |
| `load_upstream_model_catalog` | 5161 | Fetch and cache the upstream `/v1/models` catalog with a 300s TTL. Returns `UpstreamModelCatalog` (ids, canonical index, per-model context limits). |
| `fresh_upstream_model_catalog` | 5186 | Return a clone of the cached catalog when it is inside the TTL window, `None` otherwise. No fetch. |
| `upstream_model_context_limit` | 5204 | Look up a resolved model's context window from the catalog for G3 non-routing fallback. Returns `Option<i64>`. |
| `candidate_context_floor` | 913 | Conservative MIN of known context windows across a `BackendCandidatePlan`'s candidates for G3 budgeting. Returns `Option<i64>`. |

## Profile Shaping

| Function | Line | Description |
|-|-|-|
| `apply_system_prompt_prefix` | 3409 | Prepend the profile-level system prompt prefix to the request's `instructions` field. Returns modified `ResponsesRequest`. |
| `push_shaped` | 5955 | Append a tail message through profile role policy (`shape_tail_message` then push). `Action::Drop` skips the message. Ensures injected history honors the same role mapping as the lowering pass. |
| `shape_tail_message` | responses_to_chat.rs:600 | Shape ONE tail message through role policy: rewrites role, applies tag wrap, or drops/rejects. Always inline (never leading). Returns `Option<ChatMessage>`. |
| `merge_adjacent_if_configured` | responses_to_chat.rs:581 | Merge adjacent same-role runs per profile config. Tail-scoped (never rewrites the replayed prefix). Idempotent. |
| `closed_tool_set_note` | 5935 | E1: constructs a `system`-role `ChatMessage` with `CLOSED_TOOL_SET_NOTE` text, injected into repair rounds so the model stops inventing tool names. |
| `synthetic_tool_result` | 5921 | E1: builds a `tool`-role `ChatMessage` keyed to a `call_id` for tainted/hallucinated tool calls in repair rounds. |
| `responses_tool_choice_for_chat` | 5996 | Lower a validated Responses tool selector (`web_search` / custom tool types) to the Chat boundary; hosted/custom selectors keep their public tool type until the final chat request. |

## Tool Loop

| Function | Line | Description |
|-|-|-|
| `handle_tool_calls` | 5391 | Classify batch as server-tools-only, client-tools-only, or mixed (error). Hand off client tools (emit `done` events), or execute server tools sequentially (web search then image analysis). |
| `run_web_search` | 5584 | Execute Brave web search: emit `in_progress`/`done` public items, run bounded search with `request_timeout`, inject result into `current_messages` as a tool result, surface structured sources via `response.web_search_results` SSE event. |
| `run_image_analysis` | 5779 | Execute server-side `analyzeImage`: resolve cached image ids, call vision backend bounded by `request_timeout` + cancellable, redact vision text, inject description as tool result into `current_messages`. No public output items emitted. |
| `extract_web_search_query` | 6991 | Extract the query string from a `WebSearchAction` or `arguments` Value. Returns `AppResult<String>`. |
| `server_tool_failure_taxonomy` | 5886 | Map a server-tool error to a bounded gateway-owned taxonomy string for model-visible tool text (never leaks upstream error messages, credentials, or image locators). |
| `public_tool_call_identity` | 5899 | Canonical public call id for one upstream Chat tool call; the diagnostic seam compares it against the normalized upstream call id (never names or arguments). |

## E1 Repair

| Function | Line | Description |
|-|-|-|
| `record_unknown_tool_outcome` | 1218 | Record one unknown-tool-call outcome into the bounded always-on counter `{provider, served_model, outcome}`. Folds into overflow key at 256 distinct keys. |
| `unknown_tool_call_counts` | 1243 | Snapshot of the unknown-tool-call counter for tests/introspection. |
| `relax_tool_choice_after_stripping_tool` | 5966 | If web_search tool was stripped (no Brave key), relax a forced `tool_choice` to `"auto"` to prevent the model from being forced into a nonexistent tool. |

## G3 Budget

| Function | Line | Description |
|-|-|-|
| `estimate_input_tokens` | 838 | Coarse deterministic estimate: serialize the lowered + sanitized chat request, compute `ceil(bytes / 4)`. Seeds the C3 `response.created` input_tokens estimate and feeds G3 pre-flight budgeting. Returns `i64`. |
| `serialized_json_size` | 862 | Byte size of a serialized JSON value via a SHA-style length-counting writer (shared by estimate and digest paths). Returns `Result<usize, serde_json::Error>`. |
| `estimate_request_from_lowered` | 794 | Build the G3 estimate request from the lowered payload using `build_upstream_chat_request` with lower-bound-safe additives, then `sanitize_chat_request`. |
| `build_upstream_chat_request` | 756 | The ONE first-upstream-request builder shared by G3 estimate and run_turn dispatch. Common base (`messages`/`tools`/`response_format`/`tool_choice`/`stream`) identical for both; `UpstreamRequestAdditives` parameterizes the seam. |
| `budget_explicit_max_output_tokens` | 891 | Cap requested `max_output_tokens` to `min(requested, available)` where `available = context_limit - estimated_input - margin(128)`. Returns `Err(ContextBudgetError)` if input+margin already exhausts context. Pure, unit-testable. |
| `ContextBudgetError` | 668 | Marker error: the local size heuristic found no remaining context budget. Call site defers to upstream provider tokenizer. |

## G4 Image Agent

| Function | Line | Description |
|-|-|-|
| `activate_image_agent` | 3441 | G4 gating + strip: gate on `image_agent_enabled`, non-none `vision_url`, latest user message has images, non-native-vision backend, `tool_choice != "none"`. If active, calls `strip_and_cache_images` and returns a per-turn session id. Returns `Option<String>`. |
| `backend_is_native_vision` | 3517 | Native-vision gating decision. Decision table: empty candidate set => strip; request override attaches only to genuinely-resolved primary candidate; else per-candidate profile/name detection (Kimi). Returns `bool`. |
| `candidate_is_native_vision` | 3574 | Per-candidate native-vision: profile `native_vision` or name sniff (Kimi). `candidate_model` is already the final backend model. Returns `bool`. |

## E2b Residual Safety

| Function | Line | Description |
|-|-|-|
| *(inline in `stream_responses_with_api_call_id`)* | 2133-2220 | E2b residual-image pass: runs unconditionally after G4 strip. On non-native-vision backends, rejects (400) or degrades residual images (instructions, old history, `tool_choice=="none"` leftovers) to text placeholders. Bypasses replay cache on degradation (`llmconduit_replay=false`) because placeholder-collapsed images would collide in the prefix hash. |

## Replay

| Function | Line | Description |
|-|-|-|
| `find_replay_baseline` | 3581 | Look up a private replay record by `longest_prefix_match_with_affinity` on model, instructions, prompt-cache affinity, and input. Disabled unless `replay.enabled`; `llmconduit_replay:false` bypasses lookup. Skipped entirely on the native Responses path (single-use continuation contract). Returns `(Option<ReplayRecord>, prefix_len)`. |
| `TurnCompletion::take_replay_record` | 953 | Extract the private `ReplayRecord` built during the turn; the spawn closure in `stream_responses_with_api_call_id` inserts it into the replay store after completion only when replay is enabled and not bypassed. Responses `store` does not gate this cache. |

## Native Responses Turn

Upstream pass-through for providers that speak the Responses wire API directly (Codex-style). No Chat lowering, no prefix replay.

| Function | Line | Description |
|-|-|-|
| `native_history_prefix_digests` | 420 | SHA-256 digests of the canonical visible-history prefix items, keyed for continuation lookup. |
| `write_native_history_item` | 444 | Serialize one `ResponseItem` into a digest writer (canonical form for prefix digests). |
| `NativeTurnTracker` (struct) | 221 | Bounded registry mapping turn identity -> recent call-id histories for native continuation dedup. `begin` (234) opens a turn, `complete` (317) closes it and returns replay data, `remember_identity` (376) records an identity, `prune` (407) evicts expired entries. Bounds: 4096 calls, 1024 histories, 2 identities per key, 2h TTL. |
| `record_function_call_identities` | 1257 | Record native function-call identities into the bounded always-on counter (overflow key past 256 keys). |
| `function_call_identity_counts` | 1309 | Snapshot of the function-call identity counter for tests/introspection. |
| `native_current_user_function_output_ids` | 6292 | Function-output ids in the maximal current-user suffix (skips adjacent user text without scanning across a completed assistant turn) for native continuation lookup. |
| `resolve_stored_item_references` | 1784 | Replace `ItemReference` items with the stored items from the response store; 404 with `item_not_found` when missing. Returns `AppResult<()>`. |
| `discard_response_state` | 146 | Best-effort rollback (bounded retries) for a prepared/published response whose terminal event was cancelled or undeliverable. Prepared rows are fail-closed and invisible. |
| `response_resource_template` | 7757 | Build the initial `ResponseResource` (id, timestamps, status `in_progress`, echoed request fields) shared by both turn paths. |
| `require_all_strict_schema_properties` | 7947 | Strengthen a strict tool schema for the native Responses wire copy: every declared property (including nested) moves into `required` (OpenAI-style requirement; Anthropic permits optional properties). The canonical request keeps the caller's schema. |
| `unsupported_native_responses_parameter` | 8000 | Name the first request parameter the native Responses path cannot forward (`temperature`, `top_p`, penalties, `stop`, non-disabled `truncation`, ...). Returns `Option<String>`. |
| `normalize_native_response_id` | 8034 | Rewrite `response_id` / `response.id` in an upstream event payload to the gateway-minted response id. |
| `native_responses_event_is_supported` | 8048 | Whether an upstream Responses event type is in the gateway's supported passthrough set. |
| `validate_native_function_item` | 8076 | Validate a streamed native function call: item id + call_id present, name matches the offered public name, arguments parse and satisfy the registry schema. Errors carry `invalid_tool_call`. |
| `prepare_native_projection` | 8127 | Convert the private Codex projection (reasoning items, encrypted content) into public output items against the tool registry, tracking seen call ids. |
| `native_output_items_equivalent` | 8195 | Structural equality between the streamed and terminal versions of an output item (message, reasoning, function/custom tool call) for terminal reconciliation. |
| `native_response_usage` | 8258 | Parse a terminal native usage payload into `ResponseUsage`; `malformed_upstream_response` on bad shape. |

## Responses state and capability gating

| Component | Description |
|-|-|
| `response_store.rs` | Bounded memory or SQLite-backed canonical history for Responses `store` / `previous_response_id`. Persistence finishes before an eligible terminal event. |
| `responses_capabilities.rs` | Resolves conservative capabilities for the selected provider+served model, rejects an incapable primary with a parameter-specific 400, and constructs a per-request allowlist that prunes only incapable nested fallbacks. |
| `stream_responses_with_api_call_id` | Resolves stored history before lowering, applies capability validation after primary routing, consumes gateway-only extensions, and keeps Responses state independent from replay. |

## Dashboard Telemetry

| Function | Line | Description |
|-|-|-|
| `prepare_terminal_pricing` | 1379 | Resolve served-model pricing onto the telemetry guard just before terminal finalize (co-located with `guard.finalize`). |
| `record_terminal_metrics` | 1417 | D5: record terminal response into metrics rings at the D3 finalize seam. No-op when metrics disabled. Sources served model, endpoint, upstream, final usage from the guard's own evict-safe inputs (not `detail()` re-read). |
| `send_event` | 1723 | Forward one SSE event to the client + raw output mirror. Cancellable via abort_token and tx.closed(). Returns `AppResult<()>`. D3: tx.send failure => cancelled (499). D6: composes kill token with send. |
| `emit_function_call_delta` | 1748 | Forward one gated `function_call_arguments` delta: mirror to monitor hub and stream as SSE event. |
| `flow_store` | 1489 | Access the dashboard FlowStore. No-op when disabled. |
| `abort_hub` | 1495 | Access the AbortHub (live-cancellation registry). No-op when disabled. |
| `abort` | 1509 | Cancel the live server-side stream for `api_call_id`. Returns `true` if a live token was found and cancelled. |
| `spawn_provider_health_publisher` | 1567 | D4: spawn topology-health publication task (1s tick + cooldown-deadline wake). Returns `JoinHandle`. |
| `next_upstream_chunk` | 5214 | Read next chunk from upstream stream, cancellable via tx.closed() + abort_token. Returns `Option<ChatCompletionChunk>`. |
| `flow_usage_from_base_and_chunk` | 7609 | D3: combine the turn-start cumulative base with a within-turn usage chunk (OpenAI chunks are cumulative within a turn; never chunk-over-chunk). |
| `response_usage_from_flow_usage` | 7639 | Convert a `FlowUsage` into client-facing `ResponseUsage` (cached / reasoning details). |

## Helper Functions

| Function | Line | Description |
|-|-|-|
| `next_cooldown_wake` | 1142 | Nearest future cooldown deadline from health vector as `Duration` from now. `None` when no provider is cooling. |
| `flow_status_artifact_str` | 965 | Map `FlowStatus` to turn-capture artifact status string. |
| `chat_request_requested_reasoning` | 980 | Whether a Chat request asked for reasoning (via `reasoning_effort` or thinking knobs in `chat_template_kwargs`). |
| `chat_reasoning_suppressed` | 1683 | Public gate: suppress `reasoning_content` in Chat output when the client did not request reasoning. |
| `build_upstream_extra_body` | 995 | Build the per-turn upstream `extra_body` from defaults, normalized request fields, and merged `request.extra_body`. |
| `remove_defaults_for_explicit_request_fields` | 1058 | Remove config-default keys from extra_body when the request explicitly sets the corresponding typed field. |
| `remove_defaults_shadowed_by_request_extra` | 1090 | Remove config-default keys from extra_body when the request's explicit `extra_body` contains an alias. |
| `remove_keys` | 1101 | Remove multiple keys from a `BTreeMap`. |
| `merge_request_extra_value` | 1107 | Merge one key into extra_body, deep-merging `chat_template_kwargs` objects. |
| `merge_json_value_prefer_source` | 1117 | Deep-merge two JSON Values, preferring the source on conflict. |
| `emit_completed_public_items` | 5233 | Emit terminal `done` events for reasoning, message, and refusal items at the end of an upstream turn. |
| `preview_json` | 6019 | Serialize a value to pretty JSON, limited to 4000 chars. |
| `preview_json_limited` | 6026 | Serialize to pretty JSON with a custom char limit (text only, no images). |
| `preview_json_limited_with_images` | 6039 | Serialize to pretty JSON with char limit, collecting image metadata cards and redacting image URIs. |
| `collect_data_image_cards` | 6080 | Recursively collect debug-UI image metadata (mime/size/path) from a JSON value — no raw bytes. |
| `extract_data_image` | 6101 | Extract a single `data:image/...` metadata card from a string value. Case-insensitive prefix. |
| `estimate_base64_payload_bytes` | 6136 | Estimate decoded byte count of a base64-encoded string. |
| `json_path_child` | 6151 | Build a child path string for debug image card addressing. |
| `summarize_response_item` | 6165 | One-line text summary of a `ResponseItem` for monitor/debug events. |
| `summarize_content` | 6236 | Summarize content items (text, images, files) into a preview string. |
| `trailing_tool_output_items` | 6276 | Reverse-walk the request tail to collect trailing tool output items. |
| `is_tool_output_item` | 6314 | Whether a `ResponseItem` is a tool output (FunctionCallOutput, CustomToolCallOutput, ToolSearchOutput). |
| `preview_text` | 6323 | Truncate text to 1024 chars on a char boundary, appending `...` if truncated. |
| `accumulate_optional` | 7544 | Add an optional reported sub-count into an accumulator slot that starts `None` (gap 07). |
| `response_item_event_id` | 7501 | Extract the event id from a `ResponseItem` for output indexing. |

## Internal Types

| Type | Line | Description |
|-|-|
| `Gateway` | 478 | Main gateway struct: holds config, replay store, upstream/search/vision clients, monitor, flow store, abort hub, provider health, metrics, turn capture, response store, model catalog. |
| `UnknownToolOutcome` | 120 | E1: `Repaired` or `Exhausted` — outcome of bounded unknown-tool-call repair. |
| `FunctionCallIdentitySnapshot` | 175 | Snapshot row of the bounded function-call identity counter (native continuation diagnostics). |
| `NativeTurnTracker` | 221 | Bounded native-turn identity/history registry (see Native Responses Turn). |
| `SseEvent` | 572 | Server-sent event with event type string and JSON data payload. |
| `CachedUpstreamModelCatalog` | 578 | TTL-cached upstream model catalog with `fetched_at` timestamp. |
| `UpstreamModelCatalog` | 584 | Parsed `/v1/models` catalog: id list, canonical-key index, per-model context limits. Methods: `from_entries` (602), `exact_id` (625), `canonical_unique` (635), `default_id` (653). |
| `ContextBudgetError` | 668 | Marker error for exhausted local context budget (see G3 Budget). |
| `UpstreamRequestAdditives` | 715 | Per-upstream-request additives for `build_upstream_chat_request`. `for_estimate()` (731) returns lower-bound-safe empties for G3. |
| `TurnCompletion` | 928 | `Completed` (genuine stop) or `Incomplete` (length truncation), carrying the terminal SSE event and an optional `ReplayRecord`. Methods: `is_incomplete` (943), `terminal_event` (947), `take_replay_record` (953). |
| `DigestWriter` | 464 | `io::Write` adapter feeding a SHA-256 digest (prefix digests, serialized sizes). |
| `JsonPreview` | 6034 | Preview text + collected image metadata cards for debug UI. |
| `OutputTarget` | 7241 | Pair of `item_id` and `output_index` for SSE event targeting. |
| `FailureSnapshot` | 7247 | Shared snapshot of the response template used to emit `response.failed` after the stream is torn down. |
| `ResponseEventState` | 7284 | Tracks output index allocation and active message/reasoning targets for SSE event emission. |
| `AccumulatedUsage` | 7524 | Per-turn running token accumulator with gap-07 optional cached/reasoning tracking. `snapshot()` (7572) produces `FlowUsage`; `into_response_usage()` (7582) produces client-facing `ResponseUsage`. |

## Constants

| Constant | Line | Value | Description |
|-|-|-|-|
| `UPSTREAM_MODEL_CATALOG_TTL_SECS` | 74 | 300 | TTL for the upstream model catalog cache. |
| `UNKNOWN_TOOL_REPAIR_CEILING` | 80 | 1 | Max in-gateway repair rounds for hallucinated tool calls. Must not exceed 2. |
| `MAX_UNKNOWN_TOOL_COUNTER_KEYS` | 88 | 256 | Max distinct `{provider, served_model}` keys in the bounded unknown-tool counter. |
| `UNKNOWN_TOOL_COUNTER_OVERFLOW_KEY` | 92 | `"__other__"` | Catch-all key when the unknown-tool counter map is full. |
| `MAX_FUNCTION_CALL_IDENTITY_COUNTER_KEYS` | 97 | 256 | Max distinct keys in the bounded function-call identity counter. |
| `FUNCTION_CALL_IDENTITY_COUNTER_OVERFLOW_KEY` | 98 | `"__other__"` | Catch-all key when the identity counter map is full. |
| `PUBLIC_TOOL_ARGUMENT_DELTA_MAX_BYTES` | 103 | 65536 | Cap on a single public tool-argument delta payload. |
| `TAINTED_TOOL_RESULT` | 107 | *(text constant)* | Synthetic tool result for valid-but-tainted calls (sibling hallucinated). |
| `CLOSED_TOOL_SET_NOTE` | 112 | *(text constant)* | System message injected in repair rounds telling the model not to invent tool names. |
| `NATIVE_TURN_TRACKER_MAX_CALLS` | 199 | 4096 | Bound on tracked native turn calls. |
| `NATIVE_TURN_TRACKER_MAX_HISTORIES` | 200 | 1024 | Bound on tracked native call-id histories. |
| `NATIVE_TURN_TRACKER_MAX_IDENTITIES_PER_KEY` | 205 | 2 | Identities remembered per native turn key. |
| `NATIVE_TURN_TRACKER_TTL` | 206 | 2h | Expiry for native turn tracker entries. |
| `CONTEXT_BUDGET_MARGIN_TOKENS` | 662 | 128 | Fixed reserve subtracted from context window when capping output budget. |
| `WEB_SEARCH_ROUNDS_HARD_CEILING` | 4823 | 25 | Absolute ceiling on web search tool rounds (declared inside `run_turn`). |
| `IMAGE_ANALYSIS_ROUNDS_HARD_CEILING` | 4840 | 8 | Absolute ceiling on image analysis tool rounds (declared inside `run_turn`). |

## SSE Event Builders

| Function | Line | Description |
|-|-|-|
| `created_event` | 7018 | `response.created` with stub id and C3 input_tokens estimate. |
| `in_progress_event` | 8342 | `response.in_progress` for the response id. |
| `completed_event` | 7029 | `response.completed` with full `ResponseResource`. |
| `incomplete_event` | 7039 | `response.incomplete` with resource and `IncompleteDetails`. |
| `failure_event` | 8304 | `response.failed` with `FailedResponse` payload. |
| `output_item_added_event` | 7653 | `response.output_item.added` with the item and output index. |
| `output_item_done_event` | 7663 | `response.output_item.done` with the item and output index. |
| `output_text_delta_event` | 7684 | `response.output_text.delta` delta chunk. |
| `output_text_done_event` | 8353 | `response.output_text.done` with final text. |
| `content_part_added_event` | 7049 | `response.content_part.added` for output content parts. |
| `content_part_done_event` | 7072 | `response.content_part.done` with final text. |
| `reasoning_raw_text_delta_event` | 7704 | `response.reasoning.delta` for raw reasoning text chunks. |
| `reasoning_signature_delta_event` | 7738 | `response.reasoning.signature_delta` for signature chunks. |
| `reasoning_summary_part_added_event` | 7096 | `response.reasoning.summary_part.added`. |
| `reasoning_summary_part_done_event` | 7114 | `response.reasoning.summary_part.done` with final text. |
| `reasoning_summary_text_delta_event` | 7719 | `response.reasoning.summary_text.delta` delta chunk. |
| `reasoning_summary_text_done_event` | 7136 | `response.reasoning.summary_text.done` with final summary text. |
| `refusal_part_added_event` | 7155 | `response.content_part.added` for a refusal content part. |
| `refusal_part_done_event` | 7177 | `response.content_part.done` with the final refusal text. |
| `refusal_delta_event` | 7200 | `response.refusal.delta` delta chunk. |
| `refusal_done_event` | 7220 | `response.refusal.done` with final refusal text. |
| `function_call_args_delta_event` | 8373 | `response.function_call_arguments.delta` with call_id, name, delta. |
| `function_call_args_done_event` | 8394 | `response.function_call_arguments.done` with final arguments. |
| `custom_tool_call_input_delta_event` | 8415 | `response.custom_tool_call_input.delta` with call_id, name, delta. |
| `custom_tool_call_input_done_event` | 8429 | `response.custom_tool_call_input.done` with final input. |
| `json_event` | 8443 | Generic SSE event builder: wraps a payload under `{event, data: {type, ...payload}}`. |

## Dashboard Telemetry — FlowStore/L1 Guard Seam

The terminal finalize seam in the `tokio::spawn` closure (lines 2616-2825 of `stream_responses_with_api_call_id`) classifies `run_turn`'s `Result` into `FlowStatus::Completed` / `Cancelled` / `Failed`, then:

1. Resolves terminal pricing onto the guard and finalizes the L1 telemetry guard (D3) with the status and reason.
2. Records terminal metrics (D5) from the guard's evict-safe inputs.
3. Finalizes the turn-capture `CaptureGuard` (F1c) with split status for incomplete-vs-completed.
4. Inserts the turn's private `ReplayRecord` when replay is enabled and not bypassed.
5. Emits an SSE `response.failed` event on error (with a fresh never-cancelled token).
