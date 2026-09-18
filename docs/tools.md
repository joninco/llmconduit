# Server-Side Tools

## Available Tools

| Tool | File | Line | Invocation | Limits |
|-|-|-|-|-|
| web_search | engine.rs | 5584 | handle_tool_calls (engine.rs:5391) | configurable (default 5), hard ceiling 25 |
| image_analysis | engine.rs | 5779 | handle_tool_calls (engine.rs:5391) | hard ceiling 8 |
| E1 repair | engine.rs | 4661 | tool loop in run_turn (engine.rs:4076) | 1 repair round (UNKNOWN_TOOL_REPAIR_CEILING, engine.rs:80) |

## web_search (Brave Search)

**Trait.** `SearchClient` (`src/search.rs:26`) — `async fn search(&self, query: &str) -> AppResult<SearchOutcome>`.

**Implementation.** `BraveSearchClient` (`src/search.rs:31`) hits `{brave_base_url}/web/search` with `X-Subscription-Token`. Returns `SearchOutcome { formatted: String, sources: Vec<SearchSource> }`. Sources are collected by `collect_sources` (`src/search.rs:168`) and rendered by `format_search_results` (`src/search.rs:205`).

**Invocation.** `handle_tool_calls` (`engine.rs:5391`) classifies each `ResolvedToolCall` — `ToolKind::WebSearch` dispatches to `run_web_search` (`engine.rs:5584`, only when `brave_api_key` is configured). That method:

1. Emits `response.output_item.added` with `web_search_call { status: "in_progress" }` (`engine.rs:5619-5629`).
2. Extracts the query via `extract_web_search_query` (`engine.rs:6991`): reads `WebSearchAction::Search.query` first, falls back to JSON `arguments["query"]`. Rejects `OpenPage` / `FindInPage` / `Other`.
3. Runs `self.search.search(&query)` bounded by `request_timeout` + cancellable via `tx.closed()` / `abort_token` (`engine.rs:5644-5663`). A failure or timeout degrades to a model-visible tool message tagged by `server_tool_failure_taxonomy` (`engine.rs:5886`) — the turn still completes.
4. Redacts credentials from the outcome via `redact_search_outcome_credentials` (`src/search.rs:149`).
5. Emits `response.output_item.done` with `web_search_call { status: "completed" }` (`engine.rs:5687-5706`). When the client asked for `web_search_call.action.sources` (`engine.rs:4004`), the completed action carries url-only sources.
6. Emits `response.web_search_results` SSE event with `tool_use_id`, `query`, `results[]` for Anthropic clients (`engine.rs:5729-5742`). The `AnthropicStreamConverter` (`responses_to_anthropic/mod.rs:86`) renders this as `server_tool_use` + `web_search_tool_result` blocks. Non-Anthropic clients ignore the unknown event.
7. Injects `ChatMessage { role: "tool" }` with the formatted text back into `current_messages` (`engine.rs:5749-5761`).

**Arguments (via tool dispatch).**
- `query` — from `WebSearchAction::Search { query }` or `arguments["query"]` (JSON string).
- Also: `count` from `brave_max_results` config, `text_decorations=false`, `spellcheck=false`.

**Limits.**

- `max_web_search_rounds` configurable in config (default 5). `0` = unlimited, but always capped by `WEB_SEARCH_ROUNDS_HARD_CEILING = 25` (`engine.rs:4823`).
- Checked after each server-tool round (`engine.rs:4816-4833`): `web_search_rounds >= effective_limit` errors the turn.

## image_analysis (analyzeImage / G4)

**Trait.** `VisionClient` (`src/vision/client.rs:93`).

**Implementation.** `ReqwestVisionClient` (`src/vision/client.rs:105`) sends cached images to a vision-capable backend (`vision_url`, `vision_model`). Returns `VisionOutcome` (`src/vision/client.rs:88`) with description text.

**Tool spec.** Defined in `src/vision/strip.rs:73` (`analyze_image_tool_spec`): `Function { name: "analyzeImage", ... }`. Parameters (`analyze_image_tool_parameters`, `src/vision/strip.rs:48`):

- `imageId` (array of strings, required) — IDs extracted from `[Image #N]` placeholders.
- `task` (string, required) — what to look for.
- `context` (string, optional) — conversation context.

**Image stripping.** `ImageCache::strip_and_cache_images` (`src/vision/strip.rs:107`) runs before replay/lowering: strips image bytes from user messages, caches them per session (`src/vision/cache.rs:48`), replaces with `[Image #N]` placeholders. Injects the `analyzeImage` tool via `inject_analyze_image_tool` (`src/vision/strip.rs:209`) + system-prompt instruction. Dedups any caller-supplied `analyzeImage` (`tool_is_analyze_image`, `src/vision/strip.rs:234`).

**Invocation.** `handle_tool_calls` (`engine.rs:5391`) — `ToolKind::ImageAnalysis` dispatches to `run_image_analysis` (`engine.rs:5779`).

1. Resolves `VisionRequest::from_arguments` (`src/vision/client.rs:45`, called at `engine.rs:5806`): parses `imageId`, `task`, `context`; resolves cached images via `ImageCache`.
2. If no images resolved, injects a model-visible "no cached image found" message so the model can recover (`engine.rs:5819-5826`).
3. Otherwise runs `self.vision.analyze(&vision_request)` bounded by `request_timeout` + cancellable (`engine.rs:5828-5852`). Result text is redacted via `redact_vision_text_with_literals` (`src/redaction.rs:180`) before injection; failures/timeouts degrade to model-visible tool text via `server_tool_failure_taxonomy`.
4. Injects `ChatMessage { role: "tool" }` with the description into `current_messages` (`engine.rs:5865-5878`).
5. Emits NO public `output_item` events and pushes nothing to `public_history`/`response_output` — `analyzeImage` is an internal server tool that never surfaces to the client.

**Limits.**

- `IMAGE_ANALYSIS_ROUNDS_HARD_CEILING = 8` (`engine.rs:4840`). Independent of the web-search ceiling.
- Checked after each server-tool round (`engine.rs:4834-4844`): `image_analysis_rounds >= 8` errors the turn.

**Activation.** `activate_image_agent` (`engine.rs:3441`) gates on config `image_agent_enabled` + `vision_url` + `tool_choice != "none"` (`engine.rs:3447-3450`). Tool registered only when active via `build_tool_registry` (`responses_to_chat.rs:2303`) — `analyzeImage` classifies as `ToolKind::ImageAnalysis` only on active image-agent turns; otherwise it is a normal client `Function`.

## E1 Repair (hallucinated tool call recovery)

**Not a user-facing tool.** An engine-level mechanism that soft-rejects unoffered (hallucinated) tool calls and runs a bounded in-gateway repair round.

**Trigger.** After stream finalization, `finalized.rejected_tool_calls` is non-empty (`engine.rs:4661`). A tool name NOT in the offered `ToolRegistry` is classified as rejected during `StreamAccumulator::finalize` (`chat_to_responses.rs:643`, classification at `chat_to_responses.rs:697-735`) — a recoverable generation error, not a hard stream error. Rejected arguments are capped at `REJECTED_TOOL_ARGS_CAP_BYTES = 2048` (`chat_to_responses.rs:24`).

**Flow** (`engine.rs:4661-4756`):

1. WARN log + monitor emit `unknown_tool_rejected` (`engine.rs:4668-4685`).
2. Check `UNKNOWN_TOOL_REPAIR_CEILING = 1` (`engine.rs:80`, checked at `engine.rs:4687`). Exhausted → emit structured terminal failure `AppError::unknown_tool_repair_exhausted()` (rendered as `response.failed` with code `invalid_tool_call`).
3. Under ceiling: inject synthetic `tool`-role result per valid call (`TAINTED_TOOL_RESULT = "not_executed: ..."`, `engine.rs:107`, injected at `engine.rs:4723-4731` via `synthetic_tool_result`, `engine.rs:5921`) and per rejected call (`"tool_unavailable: ..."`, `engine.rs:4732-4744`).
4. Inject `CLOSED_TOOL_SET_NOTE` (`engine.rs:112`) as a system message via `closed_tool_set_note` (`engine.rs:5935`, pushed at `engine.rs:4751`): *"You may only call tools that are explicitly provided in this request. Do not invent or guess tool names..."* It goes through the same profile role mapping as any interleaved system message.
5. Relax `tool_choice` to `"auto"` (`engine.rs:4755`).
6. `continue` the loop — re-sends history (with synthetic results) to the same provider. A subsequent clean round records a `Repaired` outcome exactly once (`engine.rs:4758-4772`).

**Delta gating.** `ToolDeltaGate` (`src/tool_delta_gate.rs:101`) quarantines EVERY raw `function_call_arguments` delta per `call_id` until the batch is finalized (`engine.rs:4434-4443`): a resolved name is deliberately classified hidden so the raw wrapper is dropped, and a later unoffered call in the same batch can never taint an already-exposed valid one. Clean ordinary client functions are re-emitted AFTER finalization from their validated canonical arguments in `handle_tool_calls` (`engine.rs:5431-5545`, chunked at `PUBLIC_TOOL_ARGUMENT_DELTA_MAX_BYTES = 64 KiB`, `engine.rs:103`); name-late fragments still in the gate are discarded (`engine.rs:4613-4621`). The client never sees a hallucinated tool's argument stream. Buffer caps (`src/tool_delta_gate.rs:36,40`): 256 KiB per call, 1 MiB total per upstream turn — overflow errors the turn.

**Observability.** Always-on bounded counter `unknown_tool_call_total{provider, served_model, outcome ∈ {repaired, exhausted}}` (`UnknownToolOutcome`, `engine.rs:120`; recorded via `record_unknown_tool_outcome`, `engine.rs:1218`). Capped at `MAX_UNKNOWN_TOOL_COUNTER_KEYS = 256` distinct keys (`engine.rs:88`); overflow folds into `__other__` (`engine.rs:92`).

## Tool Loop Flow

The engine's tool loop lives in `run_turn` (`engine.rs:3608`), entered per upstream dispatch; the round loop starts at `engine.rs:4076`:

```
loop {
    1. Build upstream chat request with current messages + tools + tool_choice
    2. Stream upstream SSE chunks through ToolDeltaGate (all argument deltas quarantined)
    3. Finalize the turn (FinalizedAssistantTurn: tool_calls + rejected_tool_calls)
    4. Emit completed public items (client function args re-emitted from canonical arguments)
    5. If rejected_tool_calls non-empty → E1 repair (continue loop)
    6. If tool_calls empty → break (turn complete)
    7. Classify calls via handle_tool_calls (server vs client split)
    8. If client tools → handoff + break
    9. If server tools → execute sequentially (web_search / image_analysis), inject results, continue loop
       - Web search rounds incremented + ceiling checked (engine.rs:4816-4833)
       - Image analysis rounds incremented + ceiling checked (engine.rs:4834-4844)
       - tool_choice relaxed to "auto" after first server-tool round (engine.rs:4847)
}
```

Server and client tools cannot mix in a single turn — a mixed batch is rejected up front (`engine.rs:5426-5430`). Server tools run SEQUENTIALLY in a single batch (`engine.rs:5546-5579`) — a turn may mix `web_search` and `analyzeImage` but they execute one at a time in order.

## Client-Facing Tool Classification

| Tool | Method | Rendered To Client |
|-|-|-|
| web_search | `response.web_search_results` SSE event | Anthropic: `server_tool_use` + `web_search_tool_result`. OpenAI: ignored. |
| analyzeImage | Never emitted | Invisible to all clients (internal tool result injected directly into chat history). |
| Client tools | `function_call_arguments.delta/done` + `response.output_item.done` | Re-emitted from validated canonical arguments via `handle_tool_calls` client-branch (`engine.rs:5431-5545`). |

The `is_hidden_server_tool` function (`responses_to_anthropic/mod.rs:76`) hides `web_search` from the normal Anthropic tool block stream — it is instead rendered via the additive `response.web_search_results` event. Engine-side, the `ToolDeltaGate` hidden classification (`engine.rs:4434`) drops raw argument deltas for `analyzeImage` AND any hallucinated/unoffered tool from the client's stream; only post-finalization canonical re-emission reaches the client.
