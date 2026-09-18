# Adapters

## Module structure (from mod.rs)

```rust
pub mod anthropic_to_responses;    // line 1
pub mod chat_completions;          // line 2
pub mod chat_to_responses;         // line 3
pub mod codex_private_responses;   // line 4
pub mod responses_to_anthropic;    // line 5
pub mod responses_to_chat;         // line 6
```

`mod.rs` also carries gateway-only error metadata used across adapters:

```rust
pub(crate) struct CanonicalErrorMetadata { ... }              // line 16
pub(crate) fn canonical_error_metadata(data: &Value) -> CanonicalErrorMetadata  // line 23
```

`CanonicalErrorMetadata` extracts a bounded HTTP status, JSON-parameter path, error code, and `retry_after_secs` from a canonical `response.failed` payload; every field is length- and character-checked so an internal error cannot create unbounded collector state or leak arbitrary provider text.

---

## anthropic_to_responses

Converts an inbound **Anthropic Messages** request to the canonical `ResponsesRequest`. Also translates Anthropic tool `strict` schemas into the canonical `StrictSchemaDialect` form consumed by Responses-native backends.

### Entry points called by http.rs

| Function | Definition line | Call site in http.rs |
|-|-|-|
| `anthropic_to_responses::convert_request` | 26 | lines 1813, 2003 |

### Full public API

```rust
pub fn convert_request(request: AnthropicRequest) -> AppResult<ResponsesRequest>  // line 26
```

Called in two routes:
- **line 1813** — `handle_count_tokens` (behind `post_count_tokens`, `/v1/messages/count_tokens`): the Anthropic request is lowered for tokenization; on Codex-Responses wire backends the canonical request instead feeds `estimate_native_responses_input_tokens` (http.rs line 1884)
- **line 2003** — `post_messages` (`/v1/messages`): the canonical request is passed directly to `gateway.stream_responses_with_api_call_id`. The adapter-recorded `thinking` flag drives whether egress uses a live thinking block (`stream_anthropic_response` / `collect_anthropic_response` take a `live_thinking` parameter)

---

## chat_completions

This is the actual inbound **Chat Completions** adapter at the HTTP layer (OpenAI-compatible `/v1/chat/completions`).

### Entry points called by http.rs

| Function / method | Definition line | Call site in http.rs |
|-|-|-|
| `chat_completions::convert_request` | 26 | line 1941 |
| `ChatCompletionStreamConverter::with_reasoning_suppression` | 397 | line 2393 |
| `ChatCompletionStreamConverter::convert` | 414 | line 2400 |
| `ChatSseEvent::to_sse_data` | 369 | line 2410 |
| `ChatCompletionCollector::with_reasoning_suppression` | 604 | line 2941 |
| `ChatCompletionCollector::process` | 613 | line 2944 |
| `ChatCompletionCollector::into_response` | 629 | line 2946 |

### Full public API

```rust
// Free function — request conversion
pub fn convert_request(request: ChatCompletionRequest) -> AppResult<ResponsesRequest>  // line 26

// SSE event type (serialised into the SSE data field)
pub enum ChatSseEvent { ... }  // line 363
impl ChatSseEvent {
    pub fn to_sse_data(&self) -> String  // line 369
}

// Streaming converter — converts engine SseEvent → Vec<ChatSseEvent>
pub struct ChatCompletionStreamConverter { ... }  // line 377
impl ChatCompletionStreamConverter {
    pub fn new(model: String, include_usage: bool) -> Self                              // line 393
    pub fn with_reasoning_suppression(model: String, include_usage: bool, suppress_reasoning: bool) -> Self  // line 397
    pub fn convert(&mut self, event: &SseEvent) -> Vec<ChatSseEvent>                    // line 414
}

// Non-streaming collector — accumulates engine SseEvents into a final JSON response
pub struct ChatCompletionCollector { ... }  // line 586
impl ChatCompletionCollector {
    pub fn new(model: String) -> Self                                                        // line 600
    pub fn with_reasoning_suppression(model: String, suppress_reasoning: bool) -> Self       // line 604
    pub fn process(&mut self, event: &SseEvent)                                              // line 613
    pub fn into_response(self) -> AppResult<Value>                                           // line 629
}
```

### Usage in http.rs

- **line 1941** — `chat_completions::convert_request(request)` converts the inbound Chat request in `post_chat_completions` (line 1924)
- **lines 2393-2410** — `stream_chat_completions_response` (line 2385): `ChatCompletionStreamConverter` drives SSE streaming
- **lines 2941-2946** — `collect_chat_completions_response` (line 2935): `ChatCompletionCollector` drives non-streaming collection

---

## chat_to_responses

**Engine-internal.** Not called from `http.rs` directly. Used by `engine.rs` to consume `ChatCompletionChunk` events yielded by the upstream vLLM provider.

Import sites in engine.rs:
```rust
use crate::adapters::chat_to_responses::FinalizedAssistantTurn;  // line 1
use crate::adapters::chat_to_responses::ResolvedToolCall;        // line 2
use crate::adapters::chat_to_responses::StreamEmission;          // line 3
use crate::adapters::chat_to_responses::StreamState;             // line 4
```

### Full public API

```rust
// Types
pub enum StreamEmission { ... }              // line 38
pub struct ResolvedToolCall { ... }           // line 75
pub enum ToolRejectionReason { ... }          // line 88
pub struct RejectedToolCall { ... }           // line 105
pub struct FinalizedAssistantTurn { ... }     // line 113
pub struct StreamState { ... }                // line 132

impl StreamState {
    pub fn try_apply_chunk(&mut self, chunk: &ChatCompletionChunk) -> AppResult<Vec<StreamEmission>>  // line 218
    pub fn has_terminal_finish_reason(&self) -> bool                                                  // line 395
    pub fn partial_output_items(&self, registry: &ToolRegistry) -> Vec<ResponseItem>                   // line 563
    pub fn finalize(self, registry: &ToolRegistry) -> AppResult<FinalizedAssistantTurn>                // line 643
}
```

`StreamState::try_apply_chunk` processes each streaming chunk from the Chat provider and emits `StreamEmission` events; it is fallible because the retained streaming state is bounded (a `max_retained_bytes` ceiling rejects oversized upstream turns with `upstream_response_too_large` instead of buffering without limit). `has_terminal_finish_reason` gates terminalization: EOF without a terminal finish reason is malformed and must not synthesize `done` events. `partial_output_items` is a best-effort live snapshot used only when terminalizing a failed response; it never marks a malformed/incomplete function call executable. `StreamState::finalize` produces a `FinalizedAssistantTurn` (including repair-round synthesis for rejected or unknown tool calls).

---

## codex_private_responses

**Engine-internal.** Not called from `http.rs`. Projection from Codex's private Responses-Lite stream into the public Responses lifecycle used inside llmconduit. The private stream omits fields that are mandatory on the public wire (text deltas without item/content indexes, function argument deltas without a call id/output index, completed items without ids); this module owns the state to fill those fields once, consistently, so downstream converters never see the private dialect.

Used by `engine.rs` (line 3065) inside the native Responses streaming loop: each upstream event passes through `CodexPrivateResponsesProjector::project` before entering the canonical event flow, and `finish` gates terminalization.

### Full public API

```rust
// Memory and cardinality ceilings for one private stream (defaults:
// 8 MiB per function argument, 16k function events, 32 MiB total function
// bytes, 4096 open items, 32 MiB text)
pub struct ProjectionLimits { ... }  // line 27

// One normalized canonical event (event name + JSON data)
pub struct ProjectedEvent { ... }    // line 50

// A completed canonical output item plus its stable output index
pub struct CompletedOutputItem { ... }  // line 67

// Result of projecting one private event; a single private event can expand
// into a complete public lifecycle (added -> deltas -> done)
pub struct Projection { ... }        // line 76

// Stateful projector for one upstream response stream
pub struct CodexPrivateResponsesProjector { ... }  // line 142
impl CodexPrivateResponsesProjector {
    pub fn with_limits(limits: ProjectionLimits) -> Self                                     // line 161
    pub fn project(&mut self, event: impl Into<String>, data: Value) -> AppResult<Projection>  // line 178
    pub fn finish(&self) -> AppResult<()>                                                    // line 203
}
```

`project` normalizes one private event; unknown non-lifecycle events pass through unchanged (the caller keeps its surface allowlist and terminal-resource policy). `finish` rejects a terminal stream while any quarantined function call remains unresolved — a partial executable call must never escape as a completed turn.

---

## responses_to_anthropic

Converts canonical engine `SseEvent` streams back to **Anthropic Messages** wire format (SSE events and non-streaming response).

### Entry points called by http.rs

| Function / method | Definition line | Call site in http.rs |
|-|-|-|
| `AnthropicStreamConverter::new` | 118 (mod.rs) | line 2440 |
| `AnthropicStreamConverter::with_live_thinking` | 140 (mod.rs) | line 2438 |
| `AnthropicStreamConverter::convert` | 156 (mod.rs) | line 2444 |
| `AnthropicStreamConverter::finalize` | 508 (mod.rs) | line 2456 |
| `AnthropicStreamCollector::new` | 57 (collector.rs) | line 2957 |
| `AnthropicStreamCollector::with_live_thinking` | 64 (collector.rs) | line 2955 |
| `AnthropicStreamCollector::process` | 87 (collector.rs) | line 2961 |
| `AnthropicStreamCollector::into_response` | 192 (collector.rs) | line 2963 |
| `anthropic_error_type_for_status` | 35 (mod.rs, `pub(crate)`) | line 3006 |

### Full public API (mod.rs)

```rust
// Re-exported from collector.rs
pub use collector::AnthropicStreamCollector;  // line 29

// Anthropic wire error-type mapping for HTTP error responses
pub(crate) fn anthropic_error_type_for_status(status: u16) -> &'static str  // line 35

// Streaming converter
pub struct AnthropicStreamConverter { ... }  // line 86
impl AnthropicStreamConverter {
    pub fn new(model: String) -> Self                                             // line 118
    pub fn with_live_thinking(model: String) -> Self                              // line 140
    pub fn convert(&mut self, event: &SseEvent) -> Vec<AnthropicStreamEvent>      // line 156
    pub fn finalize(&mut self) -> Vec<AnthropicStreamEvent>                       // line 508
}
```

### Full public API (collector.rs)

```rust
pub struct AnthropicStreamCollector { ... }  // line 42
impl AnthropicStreamCollector {
    pub fn new(model: String) -> Self                                     // line 57
    pub fn with_live_thinking(model: String) -> Self                      // line 64
    pub fn process(&mut self, event: &SseEvent)                           // line 87
    pub fn into_response(self) -> Result<AnthropicMessageResponse, AnthropicErrorBody>  // line 192
}
```

`with_live_thinking` selects the egress shape in which explicitly requested reasoning is committed to a live `thinking` block; `new` retains the deferred/promotion compatibility path for unrequested backend reasoning. `post_messages` (http.rs line 2003) picks between them from the adapter-recorded `thinking` flag.

### Test utilities (conformance.rs)

Not called from http.rs or engine.rs — used by unit tests and external integration-test crates (which is why these are `pub`, not `#[cfg(test)]`).

```rust
pub enum Surface { TextOnly, ReasoningText, ClientToolUse, WebSearch, Error }  // line 54

pub fn assert_stream_conformant(events: &[AnthropicStreamEvent], surface: Surface)   // line 73
pub fn check_stream_conformant(events: &[AnthropicStreamEvent], surface: Surface) -> Result<(), String>  // line 82
pub fn assert_sse_conformant(events: &[Value], surface: Surface)                      // line 93
pub fn check_sse_conformant(events: &[Value], surface: Surface) -> Result<(), String> // line 100
```

Both entry points normalize to one shared invariant walk (`check_shapes`), so live converter output and parsed-JSON-over-the-wire can never drift apart. Invariants: exactly one `message_delta` with non-null `stop_reason` (1), no `message_delta` before the first `content_block_start` (2), none while any block is open (3), a `thinking` block emits a non-empty `signature_delta` before closing (4), the last two events are `message_delta` then `message_stop` (5), and `Surface::Error` instead ends with an `error` event (6).

### Usage in http.rs

- **lines 2438-2456** — `stream_anthropic_response` (line 2430): `AnthropicStreamConverter` drives SSE streaming for `/v1/messages` with `stream=true`; `finalize` emits a terminal `message_delta` + `message_stop` if the upstream ended without `response.completed`
- **lines 2955-2963** — `collect_anthropic_response` (line 2949): `AnthropicStreamCollector` drives the non-streaming response
- **line 3006** — `anthropic_error_type_for_status` maps an HTTP status to the Anthropic error `type` field

---

## responses_to_chat

Lowers a canonical `ResponsesRequest` to a Chat-compatible form (`ChatMessage`, `ChatTool`, `ToolRegistry`). Used by the `/v1/messages/count_tokens` route and by the engine for repair-round re-injection. Also owns the shared tool-schema validation that native Responses backends reuse.

### Entry points called by http.rs

| Function | Definition line | Call site in http.rs |
|-|-|-|
| `responses_to_chat::lower_request_with_image_agent_and_roles` | 181 | line 1831 |

### Full public API

```rust
// Types
pub enum ToolKind { ... }                                           // line 27
pub struct ToolRegistry { ... }                                     // line 46

pub struct LoweredTurn {                                            // line 96
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ChatTool>,
    pub tool_registry: ToolRegistry,
    pub response_format: Option<Value>,
    pub reasoning_effort: Option<String>,
    pub frequency_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
}

impl ToolRegistry {
    pub fn get(&self, name: &str) -> Option<&ToolKind>    // line 52
    pub fn from_map(by_name: HashMap<String, ToolKind>) -> Self  // line 87 (cfg(test))
}

// Free functions — request lowering
pub fn lower_request(                            // line 157
    request: &ResponsesRequest,
    extra_messages: Vec<ChatMessage>,
) -> AppResult<LoweredTurn>

pub fn lower_request_with_image_agent(           // line 170
    request: &ResponsesRequest,
    extra_messages: Vec<ChatMessage>,
    image_agent_active: bool,
) -> AppResult<LoweredTurn>

pub fn lower_request_with_image_agent_and_roles( // line 181
    request: &ResponsesRequest,
    extra_messages: Vec<ChatMessage>,
    image_agent_active: bool,
    roles: Option<&RolesConfig>,
) -> AppResult<LoweredTurn>

// Shared tool-contract validation (engine-facing)
pub(crate) fn validate_and_build_native_tool_registry(   // line 2398
    specs: &[ToolSpec],
    strict_schema_dialect: StrictSchemaDialect,
) -> AppResult<ToolRegistry>

pub(crate) fn validate_json_schema_value(                // line 2413
    schema: &Value,
    value: &Value,
    path: &str,
    depth: usize,
) -> Result<(), String>

// Helpers
pub fn stringify_tool_output(value: &Value) -> String      // line 3026
pub fn tool_call_arguments_object(arguments: &Option<Value>) -> Value  // line 3033
```

`validate_and_build_native_tool_registry` validates the complete public function-tool contract (names, argument JSON, strict schemas) and builds the same registry Chat lowering uses, without lowering the request; native Responses backends call it (engine.rs line 2357) to quarantine provider tool events until the checks pass.

### Usage in http.rs and engine.rs

- **http.rs line 1831** — `handle_count_tokens` (`/v1/messages/count_tokens`): lowers the Anthropic-derived `ResponsesRequest` into Chat messages for tokenization
- **engine.rs line 2441** — `lower_request_with_image_agent_and_roles` lowers the repair-round request for re-injection
