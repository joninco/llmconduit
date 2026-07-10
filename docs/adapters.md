# Adapters

## Module structure (from mod.rs)

```rust
pub mod anthropic_to_responses;    // line 1
pub mod chat_completions;          // line 2
pub mod chat_to_responses;         // line 3
pub mod responses_to_anthropic;    // line 4
pub mod responses_to_chat;         // line 5
```

---

## anthropic_to_responses

Converts an inbound **Anthropic Messages** request to the canonical `ResponsesRequest`.

### Entry points called by http.rs

| Function | Definition line | Call site in http.rs |
|-|-|-|
| `anthropic_to_responses::convert_request` | 25 | lines 1358, 1470 |

### Full public API

```rust
pub fn convert_request(request: AnthropicRequest) -> AppResult<ResponsesRequest>  // line 25
```

Called in two routes:
- **line 1358** — `/v1/tokenize` path (Anthropic route, lowered to Chat backend via `responses_to_chat::lower_request_with_image_agent_and_roles`)
- **line 1470** — `/v1/messages` streaming Anthropic route (passed directly to `gateway.stream_responses_with_api_call_id`)

---

## chat_completions

This is the actual inbound **Chat Completions** adapter at the HTTP layer (OpenAI-compatible `/v1/chat/completions`).

### Entry points called by http.rs

| Function / method | Definition line | Call site in http.rs |
|-|-|-|
| `chat_completions::convert_request` | 23 | line 1434 |
| `ChatCompletionStreamConverter::with_reasoning_suppression` | 388 | line 1669 |
| `ChatCompletionStreamConverter::convert` | 405 | line 1676 |
| `ChatSseEvent::to_sse_data` | 360 | line 1686 |
| `ChatCompletionCollector::with_reasoning_suppression` | 592 | line 1873 |
| `ChatCompletionCollector::process` | 601 | line 1876 |
| `ChatCompletionCollector::into_response` | 613 | line 1878 |

### Full public API

```rust
// Free function — request conversion
pub fn convert_request(request: ChatCompletionRequest) -> AppResult<ResponsesRequest>  // line 23

// SSE event type (serialised into the SSE data field)
pub enum ChatSseEvent { ... }  // line 354
impl ChatSseEvent {
    pub fn to_sse_data(&self) -> String  // line 360
}

// Streaming converter — converts engine SseEvent → Vec<ChatSseEvent>
pub struct ChatCompletionStreamConverter { ... }  // line 368
impl ChatCompletionStreamConverter {
    pub fn new(model: String, include_usage: bool) -> Self                              // line 384
    pub fn with_reasoning_suppression(model: String, include_usage: bool, suppress_reasoning: bool) -> Self  // line 388
    pub fn convert(&mut self, event: &SseEvent) -> Vec<ChatSseEvent>                    // line 405
}

// Non-streaming collector — accumulates engine SseEvents into a final JSON response
pub struct ChatCompletionCollector { ... }  // line 577
impl ChatCompletionCollector {
    pub fn new(model: String) -> Self                                                        // line 588
    pub fn with_reasoning_suppression(model: String, suppress_reasoning: bool) -> Self       // line 592
    pub fn process(&mut self, event: &SseEvent)                                              // line 601
    pub fn into_response(self) -> AppResult<Value>                                           // line 613
}
```

### Usage in http.rs

- **line 1434** — `chat_completions::convert_request(request)` converts the inbound Chat request
- **lines 1669-1681** — `ChatCompletionStreamConverter` drives SSE streaming
- **lines 1872-1878** — `ChatCompletionCollector` drives non-streaming collection

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
pub enum StreamEmission { ... }              // line 27
pub struct ResolvedToolCall { ... }           // line 50
pub enum ToolRejectionReason { ... }          // line 59
pub struct RejectedToolCall { ... }           // line 76
pub struct FinalizedAssistantTurn { ... }     // line 84
pub struct StreamState { ... }                // line 101

impl StreamState {
    pub fn apply_chunk(&mut self, chunk: &ChatCompletionChunk) -> Vec<StreamEmission>  // line 136
    pub fn finalize(self, registry: &ToolRegistry) -> AppResult<FinalizedAssistantTurn>  // line 276
}
```

`StreamState::apply_chunk` processes each streaming chunk from the Chat provider and emits `StreamEmission` events. `StreamState::finalize` produces a `FinalizedAssistantTurn` (including repair-round synthesis for rejected or unknown tool calls).

---

## responses_to_anthropic

Converts canonical engine `SseEvent` streams back to **Anthropic Messages** wire format (SSE events and non-streaming response).

### Entry points called by http.rs

| Function / method | Definition line | Call site in http.rs |
|-|-|-|
| `AnthropicStreamConverter::new` | 94 (mod.rs) | line 1712 |
| `AnthropicStreamConverter::convert` | 121 (mod.rs) | line 1715 |
| `AnthropicStreamConverter::finalize` | 455 (mod.rs) | line 1727 |
| `AnthropicStreamCollector::new` | 57 (collector.rs) | line 1885 |
| `AnthropicStreamCollector::process` | 73 (collector.rs) | line 1888 |
| `AnthropicStreamCollector::into_response` | 178 (collector.rs) | used at line 1890 |

### Full public API (mod.rs)

```rust
// Streaming converter
pub struct AnthropicStreamConverter { ... }  // line 67
impl AnthropicStreamConverter {
    pub fn new(model: String) -> Self                                // line 94
    pub fn convert(&mut self, event: &SseEvent) -> Vec<AnthropicStreamEvent>  // line 121
    pub fn finalize(&mut self) -> Vec<AnthropicStreamEvent>          // line 455
}

// Re-exported from collector.rs
pub use collector::AnthropicStreamCollector;  // line 28
```

### Full public API (collector.rs)

```rust
pub struct AnthropicStreamCollector { ... }  // line 42 (private struct)
impl AnthropicStreamCollector {
    pub fn new(model: String) -> Self                                     // line 57
    pub fn process(&mut self, event: &SseEvent)                           // line 73
    pub fn into_response(self) -> Result<AnthropicMessageResponse, AnthropicErrorBody>  // line 178
}
```

### Test utilities (conformance.rs)

Not called from http.rs or engine.rs — used in tests.

```rust
pub fn assert_stream_conformant(events: &[AnthropicStreamEvent], surface: Surface)   // line 73
pub fn check_stream_conformant(events: &[AnthropicStreamEvent], surface: Surface) -> Result<(), String>  // line 82
pub fn assert_sse_conformant(events: &[Value], surface: Surface)                      // line 93
pub fn check_sse_conformant(events: &[Value], surface: Surface) -> Result<(), String> // line 100
```

### Usage in http.rs

- **lines 1712-1731** — `AnthropicStreamConverter` drives SSE streaming for `/v1/messages?stream=true`
- **lines 1885-1891** — `AnthropicStreamCollector` drives non-streaming response for `/v1/messages`

---

## responses_to_chat

Lowers a canonical `ResponsesRequest` to a Chat-compatible form (`ChatMessage`, `ChatTool`, `ToolRegistry`). Used by the `/v1/tokenize` Anthropic route and by the engine for repair-round re-injection.

### Entry points called by http.rs

| Function | Definition line | Call site in http.rs |
|-|-|-|
| `responses_to_chat::lower_request_with_image_agent_and_roles` | 138 | line 1364 |

### Full public API

```rust
// Types
pub enum ToolKind { ... }                                           // line 23
pub struct ToolRegistry { ... }                                     // line 42
pub struct LoweredTurn {                                            // line 72
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ChatTool>,
    pub tool_registry: ToolRegistry,
    pub response_format: Option<Value>,
    pub reasoning_effort: Option<String>,
    pub frequency_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
}

impl ToolRegistry {
    pub fn get(&self, name: &str) -> Option<&ToolKind>    // line 47
    pub fn from_map(by_name: HashMap<String, ToolKind>) -> Self  // line 66 (cfg(test))
}

// Free functions
pub fn lower_request(                            // line 114
    request: &ResponsesRequest,
    extra_messages: Vec<ChatMessage>,
) -> AppResult<LoweredTurn>

pub fn lower_request_with_image_agent(           // line 127
    request: &ResponsesRequest,
    extra_messages: Vec<ChatMessage>,
    image_agent_active: bool,
) -> AppResult<LoweredTurn>

pub fn lower_request_with_image_agent_and_roles( // line 138
    request: &ResponsesRequest,
    extra_messages: Vec<ChatMessage>,
    image_agent_active: bool,
    roles: Option<&RolesConfig>,
) -> AppResult<LoweredTurn>

pub fn stringify_tool_output(value: &Value) -> String  // line 1135
pub fn tool_call_arguments_object(arguments: &Option<Value>) -> Value  // line 1142
```

### Usage in http.rs

- **line 1364** — `responses_to_chat::lower_request_with_image_agent_and_roles(...)` lowers the Anthropic-derived `ResponsesRequest` into Chat messages for tokenization
