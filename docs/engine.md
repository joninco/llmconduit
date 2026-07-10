# Engine

## Entry Points

| Function | Line | Description |
|-|-|-|
| `stream_responses` | 1200 | Public entry point. Thin wrapper delegating to `stream_responses_with_api_call_id` with no `api_call_id`. Returns `ReceiverStream<SseEvent>`. |
| `stream_responses_with_api_call_id` | 1207 | Full entry point: resolve model, apply system prefix, G4 image agent strip, E2b residual image safety, replay lookup, tool stripping, lowering, G3 budgeting, then spawn `run_turn`. Returns `ReceiverStream<SseEvent>`. |
| `run_turn` | 1847 | Core turn loop: sends upstream chat requests, handles the SSE event stream (output deltas, tool calls, reasoning), manages the tool-call loop with web search / image analysis / client tool round dispatch, and emits the terminal `response.completed` or `response.incomplete` event. Returns `AppResult<TurnCompletion>`. |

## Model Resolution

| Function | Line | Description |
|-|-|-|
| `resolve_request_model` | 1023 | Resolve `request_model` to served upstream model + `genuine` flag. Calls `config.resolve_upstream_model` then `normalize_upstream_model`. |
| `normalize_upstream_model` | 3037 | Walk the normalization ladder: exact catalog id, ad-hoc route match, unique canonical-key match, default catalog id. Returns `(model, genuine)` where `genuine` is `false` only on default fallback. |
| `load_upstream_model_catalog` | 3150 | Fetch and cache the upstream `/v1/models` catalog with a 300s TTL. Returns `UpstreamModelCatalog` (ids, canonical index, per-model context limits). |
| `upstream_model_context_limit` | 3177 | Look up a resolved model's context window from the catalog for G3 non-routing fallback. Returns `Option<i64>`. |
| `candidate_context_floor` | 529 | Conservative MIN of known context windows across a `BackendCandidatePlan`'s candidates for G3 budgeting. Returns `Option<i64>`. |

## Profile Shaping

| Function | Line | Description |
|-|-|-|
| `apply_system_prompt_prefix` | 1668 | Prepend the profile-level system prompt prefix to the request's `instructions` field. Returns modified `ResponsesRequest`. |
| `push_shaped` | 3773 | Append a tail message through profile role policy (`shape_tail_message` then push). `Action::Drop` skips the message. Ensures injected history honors the same role mapping as the lowering pass. |
| `shape_tail_message` | responses_to_chat.rs:476 | Shape ONE tail message through role policy: rewrites role, applies tag wrap, or drops/rejects. Always inline (never leading). Returns `Option<ChatMessage>`. |
| `merge_adjacent_if_configured` | responses_to_chat.rs:457 | Merge adjacent same-role runs per profile config. Tail-scoped (never rewrites the replayed prefix). Idempotent. |
| `closed_tool_set_note` | 3753 | E1: constructs a `system`-role `ChatMessage` with `CLOSED_TOOL_SET_NOTE` text, injected into repair rounds so the model stops inventing tool names. |
| `synthetic_tool_result` | 3739 | E1: builds a `tool`-role `ChatMessage` keyed to a `call_id` for tainted/hallucinated tool calls in repair rounds. |

## Tool Loop

| Function | Line | Description |
|-|-|-|
| `handle_tool_calls` | 3335 | Classify batch as server-tools-only, client-tools-only, or mixed (error). Hand off client tools (emit `done` events), or execute server tools sequentially (web search then image analysis). |
| `run_web_search` | 3454 | Execute Brave web search: emit `in_progress`/`done` public items, run bounded search with `request_timeout`, inject result into `current_messages` as a tool result, surface structured sources via `response.web_search_results` SSE event. |
| `run_image_analysis` | 3623 | Execute server-side `analyzeImage`: resolve cached image ids, call vision backend bounded by `request_timeout` + cancellable, redact vision text, inject description as tool result into `current_messages`. No public output items emitted. |
| `extract_web_search_query` | 4430 | Extract the query string from a `WebSearchAction` or `arguments` Value. Returns `AppResult<String>`. |

## E1 Repair

| Function | Line | Description |
|-|-|-|
| `is_hidden_tool_name` | 3723 | Classify a streamed tool name as hidden from the client: unoffered (hallucinated) tools and `analyzeImage` are hidden; offered client tools are visible. |
| `record_unknown_tool_outcome` | 754 | Record one unknown-tool-call outcome into the bounded always-on counter `{provider, served_model, outcome}`. Folds into overflow key at 256 distinct keys. |
| `unknown_tool_call_counts` | 779 | Snapshot of the unknown-tool-call counter for tests/introspection. |
| `relax_tool_choice_after_stripping_tool` | 3784 | If web_search tool was stripped (no Brave key), relax a forced `tool_choice` to `"auto"` to prevent the model from being forced into a nonexistent tool. |

## G3 Budget

| Function | Line | Description |
|-|-|-|
| `estimate_input_tokens` | 474 | Coarse deterministic estimate: serialize the lowered + sanitized chat request, compute `ceil(bytes / 4)`. Seeds the C3 `response.created` input_tokens estimate and feeds G3 pre-flight budgeting. Returns `i64`. |
| `estimate_request_from_lowered` | 430 | Build the G3 estimate request from the lowered payload using `build_upstream_chat_request` with lower-bound-safe additives, then `sanitize_chat_request`. |
| `build_upstream_chat_request` | 392 | The ONE first-upstream-request builder shared by G3 estimate and run_turn dispatch. Common base (`messages`/`tools`/`response_format`/`tool_choice`/`stream`) identical for both; `UpstreamRequestAdditives` parameterizes the seam. |
| `budget_explicit_max_output_tokens` | 507 | Cap requested `max_output_tokens` to `min(requested, available)` where `available = context_limit - estimated_input - margin(128)`. Returns `Err(ContextBudgetError)` if input+margin already exhausts context. Pure, unit-testable. |
| `ContextBudgetError` | 304 | Marker error: the local size heuristic found no remaining context budget. Call site defers to upstream provider tokenizer. |

## G4 Image Agent

| Function | Line | Description |
|-|-|-|
| `activate_image_agent` | 1704 | G4 gating + strip: gate on `image_agent_enabled`, non-none `vision_url`, latest user message has images, non-native-vision backend, `tool_choice != "none"`. If active, calls `strip_and_cache_images` and returns a per-turn session id. Returns `Option<String>`. |
| `backend_is_native_vision` | 1769 | Native-vision gating decision. Decision table: empty candidate set => strip; request override attaches only to genuinely-resolved primary candidate; else per-candidate profile/name detection (Kimi). Returns `bool`. |
| `candidate_is_native_vision` | 1821 | Per-candidate native-vision: profile `native_vision` or name sniff (Kimi). `candidate_model` is already the final backend model. Returns `bool`. |

## E2b Residual Safety

| Function | Line | Description |
|-|-|-|
| *(inline in `stream_responses_with_api_call_id`)* | 1334-1385 | E2b residual-image pass: runs unconditionally after G4 strip. On non-native-vision backends, rejects (400) or degrades residual images to text placeholders. Bypasses replay cache on degradation. |

## Replay

| Function | Line | Description |
|-|-|-|
| `find_replay_baseline` | 1828 | Look up a replay record by `longest_prefix_match` on model, instructions, and input. Returns `(Option<ReplayRecord>, prefix_len)`. Gated on `request.store`. |
| *(replay store insert)* | 2942-2951 | Inside `run_turn`: insert a `ReplayRecord` (model, instructions, visible_history, internal_messages) after the turn completes, gated on `request.store`. |

## Dashboard Telemetry

| Function | Line | Description |
|-|-|-|
| `record_terminal_metrics` | 835 | D5: record terminal response into metrics rings at the D3 finalize seam. No-op when metrics disabled. Sources served model, endpoint, upstream, final usage from the guard's own evict-safe inputs (not `detail()` re-read). |
| `send_event` | 1082 | Forward one SSE event to the client + raw output mirror. Cancellable via abort_token and tx.closed(). Returns `AppResult<()>`. D3: tx.send failure => cancelled (499). D6: composes kill token with send. |
| `emit_function_call_delta` | 1106 | Forward one gated `function_call_arguments` delta: mirror to monitor hub and stream as SSE event. |
| `drive_delta_decision` | 1141 | Drive a `ToolDeltaGate` decision (None/One/Flush) to the wire via `emit_function_call_delta`. |
| `flow_store` | 892 | Access the dashboard FlowStore. No-op when disabled. |
| `abort_hub` | 898 | Access the AbortHub (live-cancellation registry). No-op when disabled. |
| `abort` | 912 | Cancel the live server-side stream for `api_call_id`. Returns `true` if a live token was found and cancelled. |
| `spawn_provider_health_publisher` | 984 | D4: spawn topology-health publication task (1s tick + cooldown-deadline wake). Returns `JoinHandle`. |
| `next_upstream_chunk` | 3187 | Read next chunk from upstream stream, cancellable via tx.closed() + abort_token. Returns `Option<ChatCompletionChunk>`. |

## Helper Functions

| Function | Line | Description |
|-|-|-|
| `next_cooldown_wake` | 689 | Nearest future cooldown deadline from health vector as `Duration` from now. `None` when no provider is cooling. |
| `flow_status_artifact_str` | 555 | Map `FlowStatus` to turn-capture artifact status string. |
| `chat_request_requested_reasoning` | 570 | Whether a Chat request asked for reasoning (via `reasoning_effort` or thinking knobs in `chat_template_kwargs`). |
| `chat_reasoning_suppressed` | 1042 | Public gate: suppress `reasoning_content` in Chat output when the client did not request reasoning. |
| `build_upstream_extra_body` | 585 | Build the per-turn upstream `extra_body` from defaults, normalized request fields, and merged `request.extra_body`. |
| `remove_defaults_for_explicit_request_fields` | 605 | Remove config-default keys from extra_body when the request explicitly sets the corresponding typed field. |
| `remove_defaults_shadowed_by_request_extra` | 637 | Remove config-default keys from extra_body when the request's explicit `extra_body` contains an alias. |
| `remove_keys` | 648 | Remove multiple keys from a `BTreeMap`. |
| `merge_request_extra_value` | 654 | Merge one key into extra_body, deep-merging `chat_template_kwargs` objects. |
| `merge_json_value_prefer_source` | 664 | Deep-merge two JSON Values, preferring the source on conflict. |
| `emit_completed_public_items` | 3206 | Emit terminal `done` events for reasoning, message, and refusal items at the end of an upstream turn. |
| `preview_json` | 3808 | Serialize a value to pretty JSON, limited to 4000 chars. |
| `preview_json_limited` | 3815 | Serialize to pretty JSON with a custom char limit (text only, no images). |
| `preview_json_limited_with_images` | 3828 | Serialize to pretty JSON with char limit, collecting image metadata cards and redacting image URIs. |
| `collect_data_image_cards` | 3869 | Recursively collect debug-UI image metadata (mime/size/path) from a JSON value — no raw bytes. |
| `extract_data_image` | 3890 | Extract a single `data:image/...` metadata card from a string value. Case-insensitive prefix. |
| `estimate_base64_payload_bytes` | 3925 | Estimate decoded byte count of a base64-encoded string. |
| `summarize_response_item` | 3954 | One-line text summary of a `ResponseItem` for monitor/debug events. |
| `summarize_content` | 4013 | Summarize content items (text, images, files) into a preview string. |
| `trailing_tool_output_items` | 4047 | Reverse-walk the request tail to collect trailing tool output items. |
| `is_tool_output_item` | 4057 | Whether a `ResponseItem` is a tool output (FunctionCallOutput, CustomToolCallOutput, ToolSearchOutput). |
| `preview_text` | 4066 | Truncate text to 1024 chars on a char boundary, appending `...` if truncated. |
| `accumulate_optional` | 4692 | Add an optional reported sub-count into an accumulator slot that starts `None` (gap 07). |
| `response_item_event_id` | 4654 | Extract the event id from a `ResponseItem` for output indexing. |

## Internal Types

| Type | Line | Description |
|-|-|-|
| `Gateway` | 134 | Main gateway struct: holds config, replay store, upstream/search/vision clients, monitor, flow store, abort hub, provider health, metrics, turn capture, model catalog, tokenize capability. |
| `UpstreamRequestAdditives` | 351 | Per-upstream-request additives for `build_upstream_chat_request`. `for_estimate()` returns lower-bound-safe empties for G3. |
| `CachedUpstreamModelCatalog` | 214 | TTL-cached upstream model catalog with `fetched_at` timestamp. |
| `UpstreamModelCatalog` | 220 | Parsed `/v1/models` catalog: id list, canonical-key index, per-model context limits. Methods: `exact_id`, `canonical_unique`, `default_id`. |
| `TurnCompletion` | 544 | F1c: `Completed` (genuine stop) or `Incomplete` (length truncation). |
| `TokenizeCapability` | 72 | Process-wide negative cache for `/tokenize` endpoint support: Unknown, Supported, Unsupported. |
| `UnknownToolOutcome` | 111 | E1: `Repaired` or `Exhausted` — outcome of bounded unknown-tool-call repair. |
| `ResponseEventState` | 4605 | Tracks output index allocation and active message/reasoning targets for SSE event emission. |
| `AccumulatedUsage` | 4673 | Per-turn running token accumulator with gap-07 optional cached/reasoning tracking. `snapshot()` produces `FlowUsage`; `into_response_usage()` produces client-facing `ResponseUsage`. |
| `OutputTarget` | 4598 | Pair of `item_id` and `output_index` for SSE event targeting. |
| `SseEvent` | 208 | Server-sent event with event type string and JSON data payload. |
| `JsonPreview` | 3823 | Preview text + collected image metadata cards for debug UI. |

## Constants

| Constant | Line | Value | Description |
|-|-|-|-|
| `UPSTREAM_MODEL_CATALOG_TTL_SECS` | 69 | 300 | TTL for the upstream model catalog cache. |
| `UNKNOWN_TOOL_REPAIR_CEILING` | 82 | 1 | Max in-gateway repair rounds for hallucinated tool calls. Must not exceed 2. |
| `MAX_UNKNOWN_TOOL_COUNTER_KEYS` | 90 | 256 | Max distinct `{provider, served_model}` keys in the bounded unknown-tool counter. |
| `UNKNOWN_TOOL_COUNTER_OVERFLOW_KEY` | 94 | `"__other__"` | Catch-all key when counter map is full. |
| `CONTEXT_BUDGET_MARGIN_TOKENS` | 298 | 128 | Fixed reserve subtracted from context window when capping output budget. |
| `CLOSED_TOOL_SET_NOTE` | 103 | *(text constant)* | System message injected in repair rounds telling the model not to invent tool names. |
| `TAINTED_TOOL_RESULT` | 98 | *(text constant)* | Synthetic tool result for valid-but-tainted calls (sibling hallucinated). |
| `WEB_SEARCH_ROUNDS_HARD_CEILING` | 2909 | 25 | Absolute ceiling on web search tool rounds. |

## SSE Event Builders

| Function | Line | Description |
|-|-|-|
| `created_event` | 4457 | `response.created` with stub id and C3 input_tokens estimate. |
| `in_progress_event` | 4878 | `response.in_progress` for the response id. |
| `completed_event` | 4480 | `response.completed` with full `ResponseResource`. |
| `incomplete_event` | 4490 | `response.incomplete` with resource and `IncompleteDetails`. |
| `failure_event` | 4855 | `response.failed` with `FailedResponse` payload. |
| `output_item_added_event` | 4786 | `response.output_item.added` with the item and output index. |
| `output_item_done_event` | 4796 | `response.output_item.done` with the item and output index. |
| `output_text_delta_event` | 4806 | `response.output_text.delta` delta chunk. |
| `output_text_done_event` | 4895 | `response.output_text.done` with final text. |
| `content_part_added_event` | 4500 | `response.content_part.added` for output content parts. |
| `content_part_done_event` | 4519 | `response.content_part.done` with final text. |
| `reasoning_text_delta_event` | 4821 | `response.reasoning.delta` for reasoning text chunks. |
| `reasoning_signature_delta_event` | 4836 | `response.reasoning.signature_delta` for signature chunks. |
| `reasoning_summary_part_added_event` | 4538 | `response.reasoning.summary_part.added`. |
| `reasoning_summary_part_done_event` | 4556 | `response.reasoning.summary_part.done` with final text. |
| `refusal_delta_event` | 4578 | `response.refusal.delta` delta chunk. |
| `refusal_done_event` | 4588 | `response.refusal.done` with final refusal text. |
| `function_call_args_delta_event` | 4910 | `response.function_call_arguments.delta` with call_id, name, delta. |
| `function_call_args_done_event` | 4928 | `response.function_call_arguments.done` with final arguments. |
| `json_event` | 4942 | Generic SSE event builder: wraps a payload under `{event, data: {type, ...payload}}`. |

## Dashboard Telemetry — FlowStore/L1 Guard Seam

The terminal finalize seam in the `tokio::spawn` closure (lines 1596-1663 of `stream_responses_with_api_call_id`) classifies `run_turn`'s `Result` into `FlowStatus::Completed` / `Cancelled` / `Failed`, then:

1. Finalizes the L1 telemetry guard (D3) with the status and reason.
2. Records terminal metrics (D5) from the guard's evict-safe inputs.
3. Finalizes the turn-capture `CaptureGuard` (F1c) with split status for incomplete-vs-completed.
4. Emits an SSE `response.failed` event on error (with a fresh never-cancelled token).
