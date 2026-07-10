# Server-Side Tools

## Available Tools

| Tool | File | Line | Invocation | Limits |
|-|-|-|-|-|
| web_search | engine.rs | 3454 | handle_tool_calls | configurable (default 5), hard ceiling 25 |
| image_analysis | engine.rs | 3623 | handle_tool_calls | hard ceiling 8 |
| E1 repair | engine.rs | 2734 | main tool loop | 1 repair round (UNKNOWN_TOOL_REPAIR_CEILING) |

## web_search (Brave Search)

**Trait.** `SearchClient` (`src/search.rs:26`) — `async fn search(&self, query: &str) -> AppResult<SearchOutcome>`.

**Implementation.** `BraveSearchClient` (`src/search.rs:31`) hits `{brave_base_url}/web/search` with `X-Subscription-Token`. Returns `SearchOutcome { formatted: String, sources: Vec<SearchSource> }`.

**Invocation.** `handle_tool_calls` (`engine.rs:3335`) classifies each `ResolvedToolCall` — `ToolKind::WebSearch` dispatches to `run_web_search` (`engine.rs:3435`). That method:

1. Emits `response.output_item.added` with `web_search_call { status: "in_progress" }` (`engine.rs:3476`).
2. Extracts the query via `extract_web_search_query` (`engine.rs:4430`): reads `WebSearchAction::Search.query` first, falls back to JSON `arguments["query"]`. Rejects `OpenPage` / `FindInPage` / `Other`.
3. Runs `self.search.search(&query)` bounded by `request_timeout` + cancellable via `tx.closed()` / `abort_token` (`engine.rs:3513-3529`).
4. Emits `response.output_item.done` with `web_search_call { status: "completed" }` (`engine.rs:3531-3550`).
5. Emits `response.web_search_results` SSE event with `tool_use_id`, `query`, `results[]` for Anthropic clients (`engine.rs:3573-3586`). The `AnthropicStreamConverter` (`responses_to_anthropic/mod.rs:144`) renders this as `server_tool_use` + `web_search_tool_result` blocks. Non-Anthropic clients ignore the unknown event.
6. Injects `ChatMessage { role: "tool" }` with the formatted text back into `current_messages` (`engine.rs:3593-3605`).

**Arguments (via tool dispatch).**
- `query` — from `WebSearchAction::Search { query }` or `arguments["query"]` (JSON string).
- Also: `count` from `brave_max_results` config, `text_decorations=false`, `spellcheck=false`.

**Limits.**

- `max_web_search_rounds` configurable in config (default 5). `0` = unlimited, but always capped by `WEB_SEARCH_ROUNDS_HARD_CEILING = 25` (`engine.rs:2909`).
- Checked after each server-tool round (`engine.rs:2902-2918`): `web_search_rounds >= effective_limit` errors the turn.

## image_analysis (analyzeImage / G4)

**Trait.** `VisionClient` (`src/vision/client.rs`).

**Implementation.** `ReqwestVisionClient` — sends cached images to a vision-capable backend (`vision_url`, `vision_model`). Returns description text.

**Tool spec.** Defined in `src/vision/strip.rs:72` (`analyze_image_tool_spec`): `Function { name: "analyzeImage", ... }`. Parameters:

- `imageId` (array of strings, required) — IDs extracted from `[Image #N]` placeholders.
- `task` (string, required) — what to look for.
- `context` (string, optional) — conversation context.

**Image stripping.** `strip_and_cache_images` (`src/vision/strip.rs:106`) runs before replay/lowering: strips image bytes from user messages, caches them by session, replaces with `[Image #N]` placeholders. Injects `analyzeImage` tool + system-prompt instruction. Dedups any caller-supplied `analyzeImage`.

**Invocation.** `handle_tool_calls` (`engine.rs:3422`) — `ToolKind::ImageAnalysis` dispatches to `run_image_analysis` (`engine.rs:3623`).

1. Resolves `VisionRequest::from_arguments` (`src/vision/client.rs:45`): parses `imageId`, `task`, `context`; resolves cached images via `ImageCache`.
2. If no images resolved, injects a model-visible "no cached image" message (`engine.rs:3661-3668`).
3. Otherwise runs `self.vision.analyze(&vision_request)` bounded by `request_timeout` + cancellable (`engine.rs:3672-3689`). Result text is redacted (`redact_vision_text`) before injection.
4. Injects `ChatMessage { role: "tool" }` with the description into `current_messages` (`engine.rs:3700-3712`).
5. Emits NO public `output_item` events — `analyzeImage` is an internal server tool that never surfaces to the client.

**Limits.**

- `IMAGE_ANALYSIS_ROUNDS_HARD_CEILING = 8` (`engine.rs:2926`). Independent of web-search ceiling.
- Checked after each server-tool round (`engine.rs:2920-2930`): `image_analysis_rounds >= 8` errors the turn.

**Activation.** Gated by config `image_agent_enabled` + `vision_url` + `vision_model`. Tool registered only when active via `build_tool_registry` (`responses_to_chat.rs:827`) — `analyzeImage` classifies as `ToolKind::ImageAnalysis` only on active image-agent turns; otherwise it is a normal client `Function`.

## E1 Repair (hallucinated tool call recovery)

**Not a user-facing tool.** An engine-level mechanism that soft-rejects unoffered (hallucinated) tool calls and runs a bounded in-gateway repair round.

**Trigger.** After stream finalization, `finalized.rejected_tool_calls` is non-empty (`engine.rs:2743`). A tool name NOT in the offered `ToolRegistry` is classified as rejected during `apply_tool_call_delta` (`chat_to_responses.rs:314`).

**Flow.**

1. WARN log + monitor emit `unknown_tool_rejected` (`engine.rs:2758-2775`).
2. Check `UNKNOWN_TOOL_REPAIR_CEILING = 1` (`engine.rs:2777`). Exhausted → emit structured terminal failure `AppError::unknown_tool_repair_exhausted()` with machine code `invalid_tool_call`.
3. Under ceiling: inject synthetic `tool`-role result per valid call (`TAINTED_TOOL_RESULT = "not_executed: ..."`, `engine.rs:2813-2821`) and per rejected call (`"tool_unavailable: ..."`, `engine.rs:2822-2833`).
4. Inject `CLOSED_TOOL_SET_NOTE` as system message (`engine.rs:2841`): *"You may only call tools that are explicitly provided..."*
5. Relax `tool_choice` to `"auto"` (`engine.rs:2845`).
6. `continue` the loop — re-sends history (with synthetic results) to the same provider.

**Delta gating.** `ToolDeltaGate` (`src/tool_delta_gate.rs`) buffers leading `function_call_arguments` deltas per `call_id` until the engine classifies the resolved name via `is_hidden_tool_name` (`engine.rs:3723`). Unoffered names are HIDDEN (`is_hidden_tool_name` returns `true` for names not in registry) — the client never sees the hallucinated tool's argument stream. Per-call buffer cap: 256 KiB; total cap: 1 MiB.

**Observability.** Always-on bounded counter (`engine.rs:192,754`): `{provider, served_model, outcome ∈ {repaired, exhausted}}`. Capped at 256 distinct keys; overflow folds into `__other__`.

## Tool Loop Flow

The engine's tool loop lives in `stream_responses_with_api_call_id` (`engine.rs:2283`):

```
loop {
    1. Build upstream chat request with current messages + tools + tool_choice
    2. Stream upstream SSE chunks through ToolDeltaGate (delta buffering + classification)
    3. Finalize the turn (FinalizedAssistantTurn: tool_calls + rejected_tool_calls)
    4. Emit completed public items (text deltas already emitted inline)
    5. If rejected_tool_calls non-empty → E1 repair (continue loop)
    6. If tool_calls empty → break (turn complete)
    7. Classify calls via handle_tool_calls (server vs client split)
    8. If client tools → handoff + break
    9. If server tools → execute sequentially (web_search / image_analysis), inject results, continue loop
       - Web search rounds incremented + ceiling checked
       - Image analysis rounds incremented + ceiling checked
       - tool_choice relaxed to "auto" after first server-tool round
}
```

Server and client tools cannot mix in a single turn (`engine.rs:3369-3373`). Server tools run SEQUENTIALLY in a single batch — a turn may mix `web_search` and `analyzeImage` but they execute one at a time in order.

## Client-Facing Tool Classification

| Tool | Method | Rendered To Client |
|-|-|-|
| web_search | `response.web_search_results` SSE event | Anthropic: `server_tool_use` + `web_search_tool_result`. OpenAI: ignored. |
| analyzeImage | Never emitted | Invisible to all clients (internal tool result injected directly into chat history). |
| Client tools | `function_call_arguments.delta/done` + `response.output_item.done` | Forwarded as-is to client via `handle_tool_calls` client-branch (`engine.rs:3374-3416`). |

The `is_hidden_server_tool` function (`responses_to_anthropic/mod.rs:58`) hides `web_search` from the normal Anthropic tool block stream — it is instead rendered via the additive `response.web_search_results` event. The engine-side `is_hidden_tool_name` (`engine.rs:3723`) hides BOTH `analyzeImage` AND any hallucinated/unoffered tool from the client's delta stream.
