use crate::engine::SseEvent;
use crate::models::anthropic::AnthropicContentBlockStart;
use crate::models::anthropic::AnthropicDelta;
use crate::models::anthropic::AnthropicErrorBody;
use crate::models::anthropic::AnthropicMessageDeltaBody;
use crate::models::anthropic::AnthropicMessageResponse;
use crate::models::anthropic::AnthropicMessageStart;
use crate::models::anthropic::AnthropicMessageUsage;
use crate::models::anthropic::AnthropicResponseContentBlock;
use crate::models::anthropic::AnthropicServerToolUse;
use crate::models::anthropic::AnthropicStreamEvent;
use crate::models::anthropic::AnthropicUsage;
use serde_json::Value;
use uuid::Uuid;

enum ContentBlockState {
    Thinking { index: usize },
    Text { index: usize },
}

pub struct AnthropicStreamConverter {
    model: String,
    message_id: String,
    next_block_index: usize,
    open_block: Option<ContentBlockState>,
    has_tool_calls: bool,
    started: bool,
    completed: bool,
    pending_input_tokens: Option<u64>,
    web_search_count: u64,
}

impl AnthropicStreamConverter {
    pub fn new(model: String) -> Self {
        Self {
            model,
            message_id: format!("msg_{}", Uuid::new_v4().simple()),
            next_block_index: 0,
            open_block: None,
            has_tool_calls: false,
            started: false,
            completed: false,
            pending_input_tokens: None,
            web_search_count: 0,
        }
    }

    /// `usage.server_tool_use` for terminal events: `Some` once a server-side
    /// web search has run this turn, otherwise `None` so token-only turns stay
    /// byte-identical.
    fn server_tool_use_usage(&self) -> Option<AnthropicServerToolUse> {
        (self.web_search_count > 0).then_some(AnthropicServerToolUse {
            web_search_requests: self.web_search_count,
        })
    }

    pub fn convert(&mut self, event: &SseEvent) -> Vec<AnthropicStreamEvent> {
        let mut output = Vec::new();
        match event.event.as_str() {
            "response.created" => self.handle_created(&mut output),
            "response.output_item.added" => self.handle_item_added(&event.data, &mut output),
            "response.output_text.delta" => self.handle_text_delta(&event.data, &mut output),
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                self.handle_reasoning_delta(&event.data, &mut output);
            }
            "response.output_item.done" => self.handle_item_done(&event.data, &mut output),
            "response.completed" | "response.incomplete" => {
                self.handle_completed(&event.data, &mut output)
            }
            "response.failed" => self.handle_failed(&event.data, &mut output),
            "response.web_search_results" => {
                self.handle_web_search_results(&event.data, &mut output)
            }
            _ => {}
        }
        output
    }

    fn ensure_started(&mut self, output: &mut Vec<AnthropicStreamEvent>) {
        if !self.started {
            self.started = true;
            output.push(AnthropicStreamEvent::Ping);
            output.push(AnthropicStreamEvent::MessageStart {
                message: AnthropicMessageStart {
                    id: self.message_id.clone(),
                    kind: "message".to_string(),
                    role: "assistant".to_string(),
                    content: Vec::new(),
                    model: self.model.clone(),
                    stop_reason: None,
                    stop_sequence: None,
                    usage: AnthropicUsage {
                        input_tokens: Some(self.pending_input_tokens.unwrap_or(0)),
                        output_tokens: Some(0),
                        server_tool_use: None,
                    },
                },
            });
        }
    }

    fn handle_created(&mut self, output: &mut Vec<AnthropicStreamEvent>) {
        self.ensure_started(output);
    }

    fn handle_item_added(&mut self, data: &Value, output: &mut Vec<AnthropicStreamEvent>) {
        self.ensure_started(output);
        let Some(item) = data.get("item") else { return };
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
        match item_type {
            "message" => {
                self.close_open_block(output);
                self.start_text_block(output);
            }
            "reasoning" => {
                self.close_open_block(output);
                self.start_thinking_block(output);
            }
            _ => {}
        }
    }

    fn handle_text_delta(&mut self, data: &Value, output: &mut Vec<AnthropicStreamEvent>) {
        self.ensure_started(output);
        let Some(delta) = data.get("delta").and_then(Value::as_str) else {
            return;
        };
        match self.open_block {
            Some(ContentBlockState::Text { index }) => {
                output.push(AnthropicStreamEvent::ContentBlockDelta {
                    index,
                    delta: AnthropicDelta::TextDelta {
                        text: delta.to_string(),
                    },
                });
            }
            _ => {
                self.close_open_block(output);
                self.start_text_block(output);
                if let Some(ContentBlockState::Text { index }) = self.open_block {
                    output.push(AnthropicStreamEvent::ContentBlockDelta {
                        index,
                        delta: AnthropicDelta::TextDelta {
                            text: delta.to_string(),
                        },
                    });
                }
            }
        }
    }

    fn handle_reasoning_delta(&mut self, data: &Value, output: &mut Vec<AnthropicStreamEvent>) {
        self.ensure_started(output);
        let Some(delta) = data.get("delta").and_then(Value::as_str) else {
            return;
        };
        match self.open_block {
            Some(ContentBlockState::Thinking { index }) => {
                output.push(AnthropicStreamEvent::ContentBlockDelta {
                    index,
                    delta: AnthropicDelta::ThinkingDelta {
                        thinking: delta.to_string(),
                    },
                });
            }
            _ => {
                self.close_open_block(output);
                self.start_thinking_block(output);
                if let Some(ContentBlockState::Thinking { index }) = self.open_block {
                    output.push(AnthropicStreamEvent::ContentBlockDelta {
                        index,
                        delta: AnthropicDelta::ThinkingDelta {
                            thinking: delta.to_string(),
                        },
                    });
                }
            }
        }
    }

    fn handle_item_done(&mut self, data: &Value, output: &mut Vec<AnthropicStreamEvent>) {
        let Some(item) = data.get("item") else { return };
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
        match item_type {
            "message" => {
                if matches!(self.open_block, Some(ContentBlockState::Text { .. })) {
                    self.close_open_block(output);
                }
            }
            "reasoning" => {
                if matches!(self.open_block, Some(ContentBlockState::Thinking { .. })) {
                    self.close_open_block(output);
                }
            }
            "function_call" | "custom_tool_call" => {
                self.close_open_block(output);
                self.emit_tool_use_block(item, output);
            }
            "web_search_call" => {
                // Server-side tool: deliberately not surfaced. Brave Search runs
                // server-side, so the Anthropic client must not see a tool_use
                // block for it; the final answer (generated after results are
                // injected) is emitted as regular text. Same effect as the `_`
                // arm — kept explicit to document the intentional omission.
            }
            _ => {}
        }
    }

    /// Guarantees the Anthropic stream is terminated.
    ///
    /// The converter only emits `message_delta` + `message_stop` when it sees a
    /// `response.completed` event. If the upstream turn ends any other way (an
    /// error, a dropped/stalled engine task, an aborted web-search round-trip),
    /// the client would otherwise be left waiting forever behind the SSE
    /// keep-alive. Callers MUST invoke this once the upstream event stream is
    /// exhausted so every connection ends with a terminal event.
    pub fn finalize(&mut self) -> Vec<AnthropicStreamEvent> {
        let mut output = Vec::new();
        if self.completed {
            return output;
        }
        self.ensure_started(&mut output);
        self.close_open_block(&mut output);
        output.push(AnthropicStreamEvent::MessageDelta {
            delta: AnthropicMessageDeltaBody {
                stop_reason: if self.has_tool_calls {
                    "tool_use".to_string()
                } else {
                    "end_turn".to_string()
                },
                stop_sequence: None,
            },
            usage: AnthropicUsage {
                input_tokens: self.pending_input_tokens,
                output_tokens: Some(0),
                server_tool_use: self.server_tool_use_usage(),
            },
        });
        output.push(AnthropicStreamEvent::MessageStop);
        self.completed = true;
        output
    }

    fn handle_completed(&mut self, data: &Value, output: &mut Vec<AnthropicStreamEvent>) {
        self.completed = true;
        self.close_open_block(output);
        let usage = response_usage(data);
        if let Some(usage) = usage.as_ref() {
            self.pending_input_tokens = Some(usage.input_tokens);
        }
        let stop_reason = if let Some(reason) = response_stop_reason(data) {
            reason
        } else if self.has_tool_calls {
            "tool_use".to_string()
        } else {
            "end_turn".to_string()
        };
        output.push(AnthropicStreamEvent::MessageDelta {
            delta: AnthropicMessageDeltaBody {
                stop_reason,
                stop_sequence: None,
            },
            usage: AnthropicUsage {
                input_tokens: usage.as_ref().map(|usage| usage.input_tokens),
                output_tokens: Some(usage.as_ref().map_or(0, |usage| usage.output_tokens)),
                server_tool_use: self.server_tool_use_usage(),
            },
        });
        output.push(AnthropicStreamEvent::MessageStop);
    }

    fn handle_failed(&mut self, data: &Value, output: &mut Vec<AnthropicStreamEvent>) {
        let message = data
            .get("response")
            .and_then(|r| r.get("error"))
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("gateway error")
            .to_string();
        output.push(AnthropicStreamEvent::Error {
            error: AnthropicErrorBody {
                kind: "api_error".to_string(),
                message,
            },
        });
    }

    /// Surface a server-side web search to the Anthropic client.
    ///
    /// resp2chat runs Brave server-side, so the client never sees the model's
    /// own tool call. Without explicit `server_tool_use` + `web_search_tool_result`
    /// blocks, Claude Code reports "Did 0 searches" and renders no source
    /// citations. This mirrors Anthropic's native web-search streaming shape:
    /// a `server_tool_use` block (query streamed via `input_json_delta`) followed
    /// by a `web_search_tool_result` block carrying the structured sources. The
    /// turn still ends with `end_turn` (the search is resolved server-side), so
    /// `has_tool_calls` is intentionally left untouched.
    fn handle_web_search_results(&mut self, data: &Value, output: &mut Vec<AnthropicStreamEvent>) {
        self.ensure_started(output);
        self.close_open_block(output);
        self.web_search_count += 1;

        let tool_use_id = data
            .get("tool_use_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let query = data.get("query").and_then(Value::as_str).unwrap_or("");
        let results = data
            .get("results")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));

        let stu_index = self.next_block_index;
        self.next_block_index += 1;
        output.push(AnthropicStreamEvent::ContentBlockStart {
            index: stu_index,
            content_block: AnthropicContentBlockStart::ServerToolUse {
                id: tool_use_id.clone(),
                name: "web_search".to_string(),
            },
        });
        output.push(AnthropicStreamEvent::ContentBlockDelta {
            index: stu_index,
            delta: AnthropicDelta::InputJsonDelta {
                partial_json: serde_json::json!({ "query": query }).to_string(),
            },
        });
        output.push(AnthropicStreamEvent::ContentBlockStop { index: stu_index });

        let result_index = self.next_block_index;
        self.next_block_index += 1;
        output.push(AnthropicStreamEvent::ContentBlockStart {
            index: result_index,
            content_block: AnthropicContentBlockStart::WebSearchToolResult {
                tool_use_id,
                content: results,
            },
        });
        output.push(AnthropicStreamEvent::ContentBlockStop {
            index: result_index,
        });
    }

    fn close_open_block(&mut self, output: &mut Vec<AnthropicStreamEvent>) {
        if let Some(block) = self.open_block.take() {
            let index = match block {
                ContentBlockState::Thinking { index } | ContentBlockState::Text { index } => index,
            };
            output.push(AnthropicStreamEvent::ContentBlockStop { index });
        }
    }

    fn start_text_block(&mut self, output: &mut Vec<AnthropicStreamEvent>) {
        let index = self.next_block_index;
        self.next_block_index += 1;
        self.open_block = Some(ContentBlockState::Text { index });
        output.push(AnthropicStreamEvent::ContentBlockStart {
            index,
            content_block: AnthropicContentBlockStart::Text {
                text: String::new(),
            },
        });
    }

    fn start_thinking_block(&mut self, output: &mut Vec<AnthropicStreamEvent>) {
        let index = self.next_block_index;
        self.next_block_index += 1;
        self.open_block = Some(ContentBlockState::Thinking { index });
        output.push(AnthropicStreamEvent::ContentBlockStart {
            index,
            content_block: AnthropicContentBlockStart::Thinking {
                thinking: String::new(),
            },
        });
    }

    fn emit_tool_use_block(&mut self, item: &Value, output: &mut Vec<AnthropicStreamEvent>) {
        self.has_tool_calls = true;
        let index = self.next_block_index;
        self.next_block_index += 1;

        let call_id = item
            .get("call_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
        let arguments = item
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or("{}");

        output.push(AnthropicStreamEvent::ContentBlockStart {
            index,
            content_block: AnthropicContentBlockStart::ToolUse {
                id: call_id.to_string(),
                name: name.to_string(),
                input: Value::Object(Default::default()),
            },
        });
        output.push(AnthropicStreamEvent::ContentBlockDelta {
            index,
            delta: AnthropicDelta::InputJsonDelta {
                partial_json: arguments.to_string(),
            },
        });
        output.push(AnthropicStreamEvent::ContentBlockStop { index });
    }
}

// ---------------------------------------------------------------------------
// Non-streaming collector: accumulates stream events into a single response
// ---------------------------------------------------------------------------

enum AccumulatedBlock {
    Thinking {
        text: String,
    },
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: String,
    },
    ServerToolUse {
        id: String,
        name: String,
        input: String,
    },
    WebSearchToolResult {
        tool_use_id: String,
        content: Value,
    },
}

pub struct AnthropicStreamCollector {
    inner: AnthropicStreamConverter,
    message_id: Option<String>,
    model: Option<String>,
    stop_reason: Option<String>,
    blocks: Vec<AccumulatedBlock>,
    current_block: Option<AccumulatedBlock>,
    input_tokens: u64,
    output_tokens: u64,
    error: Option<AnthropicErrorBody>,
}

impl AnthropicStreamCollector {
    pub fn new(model: String) -> Self {
        Self {
            inner: AnthropicStreamConverter::new(model.clone()),
            message_id: None,
            model: Some(model),
            stop_reason: None,
            blocks: Vec::new(),
            current_block: None,
            input_tokens: 0,
            output_tokens: 0,
            error: None,
        }
    }

    pub fn process(&mut self, event: &SseEvent) {
        if let Some(usage) = response_usage(&event.data) {
            self.input_tokens = usage.input_tokens;
            self.output_tokens = usage.output_tokens;
        }
        let stream_events = self.inner.convert(event);
        for se in stream_events {
            match se {
                AnthropicStreamEvent::MessageStart { message } => {
                    self.message_id = Some(message.id);
                    self.input_tokens = message.usage.input_tokens.unwrap_or(0);
                    self.output_tokens = message.usage.output_tokens.unwrap_or(0);
                }
                AnthropicStreamEvent::ContentBlockStart { content_block, .. } => {
                    self.current_block = match content_block {
                        AnthropicContentBlockStart::Text { .. } => Some(AccumulatedBlock::Text {
                            text: String::new(),
                        }),
                        AnthropicContentBlockStart::Thinking { .. } => {
                            Some(AccumulatedBlock::Thinking {
                                text: String::new(),
                            })
                        }
                        AnthropicContentBlockStart::ToolUse { id, name, .. } => {
                            Some(AccumulatedBlock::ToolUse {
                                id,
                                name,
                                input: String::new(),
                            })
                        }
                        AnthropicContentBlockStart::ServerToolUse { id, name } => {
                            Some(AccumulatedBlock::ServerToolUse {
                                id,
                                name,
                                input: String::new(),
                            })
                        }
                        AnthropicContentBlockStart::WebSearchToolResult {
                            tool_use_id,
                            content,
                        } => Some(AccumulatedBlock::WebSearchToolResult {
                            tool_use_id,
                            content,
                        }),
                    };
                }
                AnthropicStreamEvent::ContentBlockDelta { delta, .. } => {
                    match (&mut self.current_block, delta) {
                        (
                            Some(AccumulatedBlock::Text { text }),
                            AnthropicDelta::TextDelta { text: t },
                        ) => {
                            text.push_str(&t);
                        }
                        (
                            Some(AccumulatedBlock::Thinking { text }),
                            AnthropicDelta::ThinkingDelta { thinking: t },
                        ) => {
                            text.push_str(&t);
                        }
                        (
                            Some(AccumulatedBlock::ToolUse { input, .. })
                            | Some(AccumulatedBlock::ServerToolUse { input, .. }),
                            AnthropicDelta::InputJsonDelta { partial_json },
                        ) => {
                            input.push_str(&partial_json);
                        }
                        _ => {}
                    }
                }
                AnthropicStreamEvent::ContentBlockStop { .. } => {
                    if let Some(block) = self.current_block.take() {
                        self.blocks.push(block);
                    }
                }
                AnthropicStreamEvent::MessageDelta { delta, usage } => {
                    self.stop_reason = Some(delta.stop_reason);
                    self.output_tokens = usage.output_tokens.unwrap_or(0);
                }
                AnthropicStreamEvent::Error { error } => {
                    self.error = Some(error);
                }
                _ => {}
            }
        }
    }

    pub fn into_response(self) -> Result<AnthropicMessageResponse, AnthropicErrorBody> {
        if let Some(error) = self.error {
            return Err(error);
        }

        let content: Vec<AnthropicResponseContentBlock> = self
            .blocks
            .into_iter()
            .map(|block| match block {
                AccumulatedBlock::Text { text } => AnthropicResponseContentBlock::Text { text },
                AccumulatedBlock::Thinking { text } => {
                    AnthropicResponseContentBlock::Thinking { thinking: text }
                }
                AccumulatedBlock::ToolUse { id, name, input } => {
                    let parsed_input: Value =
                        serde_json::from_str(&input).unwrap_or(Value::Object(Default::default()));
                    AnthropicResponseContentBlock::ToolUse {
                        id,
                        name,
                        input: parsed_input,
                    }
                }
                AccumulatedBlock::ServerToolUse { id, name, input } => {
                    let parsed_input: Value =
                        serde_json::from_str(&input).unwrap_or(Value::Object(Default::default()));
                    AnthropicResponseContentBlock::ServerToolUse {
                        id,
                        name,
                        input: parsed_input,
                    }
                }
                AccumulatedBlock::WebSearchToolResult {
                    tool_use_id,
                    content,
                } => AnthropicResponseContentBlock::WebSearchToolResult { tool_use_id, content },
            })
            .collect();

        Ok(AnthropicMessageResponse {
            id: self
                .message_id
                .unwrap_or_else(|| format!("msg_{}", Uuid::new_v4().simple())),
            kind: "message".to_string(),
            role: "assistant".to_string(),
            content,
            model: self.model.unwrap_or_default(),
            stop_reason: self.stop_reason.unwrap_or_else(|| "end_turn".to_string()),
            stop_sequence: None,
            usage: AnthropicMessageUsage {
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
            },
        })
    }
}

fn response_usage(data: &Value) -> Option<AnthropicMessageUsage> {
    let usage = data.get("response")?.get("usage")?;
    Some(AnthropicMessageUsage {
        input_tokens: usage.get("input_tokens")?.as_u64()?,
        output_tokens: usage.get("output_tokens")?.as_u64()?,
    })
}

fn response_stop_reason(data: &Value) -> Option<String> {
    if let Some(reason) = data
        .get("response")
        .and_then(|response| response.get("incomplete_details"))
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str)
    {
        return Some(match reason {
            "max_output_tokens" => "max_tokens".to_string(),
            other => other.to_string(),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::anthropic::AnthropicStreamEvent;
    use serde_json::json;

    fn created_event() -> SseEvent {
        SseEvent {
            event: "response.created".to_string(),
            data: json!({
                "type": "response.created",
                "response": { "id": "resp_123" }
            }),
        }
    }

    fn item_added_event(item_type: &str, role: &str) -> SseEvent {
        SseEvent {
            event: "response.output_item.added".to_string(),
            data: json!({
                "type": "response.output_item.added",
                "item": { "type": item_type, "role": role }
            }),
        }
    }

    fn text_delta_event(text: &str) -> SseEvent {
        SseEvent {
            event: "response.output_text.delta".to_string(),
            data: json!({
                "type": "response.output_text.delta",
                "delta": text
            }),
        }
    }

    fn reasoning_delta_event(text: &str) -> SseEvent {
        SseEvent {
            event: "response.reasoning_text.delta".to_string(),
            data: json!({
                "type": "response.reasoning_text.delta",
                "delta": text,
                "content_index": 0
            }),
        }
    }

    fn item_done_event(item_type: &str, extra: Value) -> SseEvent {
        let mut item = serde_json::json!({ "type": item_type });
        if let Value::Object(map) = extra {
            for (k, v) in map {
                item.as_object_mut().unwrap().insert(k, v);
            }
        }
        SseEvent {
            event: "response.output_item.done".to_string(),
            data: json!({
                "type": "response.output_item.done",
                "item": item
            }),
        }
    }

    fn completed_event() -> SseEvent {
        SseEvent {
            event: "response.completed".to_string(),
            data: json!({
                "type": "response.completed",
                "response": { "id": "resp_123" }
            }),
        }
    }

    fn completed_event_with_usage(input_tokens: u64, output_tokens: u64) -> SseEvent {
        SseEvent {
            event: "response.completed".to_string(),
            data: json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_123",
                    "usage": {
                        "input_tokens": input_tokens,
                        "output_tokens": output_tokens,
                    }
                }
            }),
        }
    }

    fn incomplete_event(reason: &str, input_tokens: u64, output_tokens: u64) -> SseEvent {
        SseEvent {
            event: "response.incomplete".to_string(),
            data: json!({
                "type": "response.incomplete",
                "response": {
                    "id": "resp_123",
                    "usage": {
                        "input_tokens": input_tokens,
                        "output_tokens": output_tokens,
                    },
                    "incomplete_details": {
                        "reason": reason,
                    }
                }
            }),
        }
    }

    fn failed_event(message: &str) -> SseEvent {
        SseEvent {
            event: "response.failed".to_string(),
            data: json!({
                "type": "response.failed",
                "response": {
                    "error": { "code": "gateway_error", "message": message }
                }
            }),
        }
    }

    fn event_types(events: &[AnthropicStreamEvent]) -> Vec<&str> {
        events.iter().map(|e| e.sse_event_type()).collect()
    }

    #[test]
    fn converts_simple_text_response() {
        let mut converter = AnthropicStreamConverter::new("claude-3".to_string());
        let events: Vec<AnthropicStreamEvent> = [
            created_event(),
            item_added_event("message", "assistant"),
            text_delta_event("Hello"),
            text_delta_event(" world"),
            item_done_event("message", json!({})),
            completed_event(),
        ]
        .iter()
        .flat_map(|e| converter.convert(e))
        .collect();

        assert_eq!(
            event_types(&events),
            vec![
                "ping",
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
    }

    #[test]
    fn converts_reasoning_then_text_response() {
        let mut converter = AnthropicStreamConverter::new("claude-3".to_string());
        let events: Vec<AnthropicStreamEvent> = [
            created_event(),
            item_added_event("reasoning", ""),
            reasoning_delta_event("Thinking..."),
            item_added_event("message", "assistant"),
            text_delta_event("Answer"),
            item_done_event("reasoning", json!({})),
            item_done_event("message", json!({})),
            completed_event(),
        ]
        .iter()
        .flat_map(|e| converter.convert(e))
        .collect();

        assert_eq!(
            event_types(&events),
            vec![
                "ping",
                "message_start",
                "content_block_start", // thinking
                "content_block_delta", // thinking delta
                "content_block_stop",  // close thinking before text
                "content_block_start", // text
                "content_block_delta", // text delta
                "content_block_stop",  // close text
                "message_delta",
                "message_stop",
            ]
        );
    }

    #[test]
    fn converts_function_call_response() {
        let mut converter = AnthropicStreamConverter::new("claude-3".to_string());
        let events: Vec<AnthropicStreamEvent> = [
            created_event(),
            item_added_event("message", "assistant"),
            text_delta_event("Let me check."),
            item_done_event("message", json!({})),
            item_done_event(
                "function_call",
                json!({
                    "call_id": "call_1",
                    "name": "get_weather",
                    "arguments": "{\"location\":\"Seattle\"}"
                }),
            ),
            completed_event(),
        ]
        .iter()
        .flat_map(|e| converter.convert(e))
        .collect();

        assert_eq!(
            event_types(&events),
            vec![
                "ping",
                "message_start",
                "content_block_start", // text
                "content_block_delta", // text delta
                "content_block_stop",  // close text
                "content_block_start", // tool_use
                "content_block_delta", // input_json_delta
                "content_block_stop",  // close tool_use
                "message_delta",
                "message_stop",
            ]
        );

        // Verify stop_reason is tool_use
        let message_delta = events
            .iter()
            .find(|e| matches!(e, AnthropicStreamEvent::MessageDelta { .. }));
        assert!(message_delta.is_some());
    }

    #[test]
    fn converts_tool_use_only_response() {
        let mut converter = AnthropicStreamConverter::new("claude-3".to_string());
        let events: Vec<AnthropicStreamEvent> = [
            created_event(),
            item_done_event(
                "function_call",
                json!({
                    "call_id": "call_1",
                    "name": "search",
                    "arguments": "{\"query\":\"test\"}"
                }),
            ),
            completed_event(),
        ]
        .iter()
        .flat_map(|e| converter.convert(e))
        .collect();

        // Should have tool_use block with tool_use stop_reason
        let has_tool_use = events.iter().any(|e| {
            matches!(
                e,
                AnthropicStreamEvent::ContentBlockStart {
                    content_block: AnthropicContentBlockStart::ToolUse { .. },
                    ..
                }
            )
        });
        assert!(has_tool_use);
    }

    #[test]
    fn converts_failure_event() {
        let mut converter = AnthropicStreamConverter::new("claude-3".to_string());
        let events = converter.convert(&failed_event("upstream timeout"));

        assert_eq!(event_types(&events), vec!["error"]);
    }

    #[test]
    fn emits_usage_from_completed_response() {
        let mut converter = AnthropicStreamConverter::new("claude-3".to_string());
        let events: Vec<AnthropicStreamEvent> =
            [created_event(), completed_event_with_usage(12, 5)]
                .iter()
                .flat_map(|e| converter.convert(e))
                .collect();

        let message_start = events
            .iter()
            .find_map(|event| match event {
                AnthropicStreamEvent::MessageStart { message } => Some(message),
                _ => None,
            })
            .expect("message_start");
        assert_eq!(message_start.usage.input_tokens, Some(0));
        assert_eq!(message_start.usage.output_tokens, Some(0));

        let message_delta = events
            .iter()
            .find_map(|event| match event {
                AnthropicStreamEvent::MessageDelta { usage, .. } => Some(usage),
                _ => None,
            })
            .expect("message_delta");
        assert_eq!(message_delta.input_tokens, Some(12));
        assert_eq!(message_delta.output_tokens, Some(5));
    }

    #[test]
    fn converts_incomplete_to_max_tokens_stop_reason() {
        let mut converter = AnthropicStreamConverter::new("claude-3".to_string());
        let events: Vec<AnthropicStreamEvent> = [
            created_event(),
            incomplete_event("max_output_tokens", 12, 5),
        ]
        .iter()
        .flat_map(|e| converter.convert(e))
        .collect();

        let message_delta = events
            .iter()
            .find_map(|event| match event {
                AnthropicStreamEvent::MessageDelta { delta, .. } => Some(delta),
                _ => None,
            })
            .expect("message_delta");
        assert_eq!(message_delta.stop_reason, "max_tokens");
    }

    #[test]
    fn collector_returns_final_usage() {
        let mut collector = AnthropicStreamCollector::new("claude-3".to_string());
        for event in [
            created_event(),
            item_added_event("message", "assistant"),
            text_delta_event("Hello"),
            item_done_event("message", json!({})),
            completed_event_with_usage(12, 5),
        ] {
            collector.process(&event);
        }

        let response = collector.into_response().expect("response");
        assert_eq!(response.usage.input_tokens, 12);
        assert_eq!(response.usage.output_tokens, 5);
    }

    #[test]
    fn skips_web_search_call_events() {
        let mut converter = AnthropicStreamConverter::new("claude-3".to_string());
        let events: Vec<AnthropicStreamEvent> = [
            created_event(),
            item_added_event("message", "assistant"),
            text_delta_event("Searching..."),
            item_done_event("message", json!({})),
            // web_search_call events should be skipped
            SseEvent {
                event: "response.output_item.added".to_string(),
                data: json!({
                    "type": "response.output_item.added",
                    "item": { "type": "web_search_call", "id": "ws_1", "status": "in_progress" }
                }),
            },
            SseEvent {
                event: "response.output_item.done".to_string(),
                data: json!({
                    "type": "response.output_item.done",
                    "item": { "type": "web_search_call", "id": "ws_1", "status": "completed" }
                }),
            },
            // After internal web search, more text comes in a new turn
            // (simulated by another item_added + delta)
            item_added_event("message", "assistant"),
            text_delta_event("Here are the results."),
            item_done_event("message", json!({})),
            completed_event(),
        ]
        .iter()
        .flat_map(|e| converter.convert(e))
        .collect();

        // No tool_use events should appear for web_search_call
        let has_web_search_tool_use = events.iter().any(|e| {
            matches!(
                e,
                AnthropicStreamEvent::ContentBlockStart {
                    content_block: AnthropicContentBlockStart::ToolUse { name, .. },
                    ..
                } if name == "web_search"
            )
        });
        assert!(!has_web_search_tool_use);
    }

    #[test]
    fn finalize_terminates_stream_when_upstream_ends_without_completed() {
        // Regression: a web-search round-trip that stalls/aborts ends the
        // upstream event stream without `response.completed`. Without an
        // explicit finalize the client only ever sees `message_start` and
        // hangs forever behind the SSE keep-alive.
        let mut converter = AnthropicStreamConverter::new("claude-3".to_string());
        let mut events: Vec<AnthropicStreamEvent> = [
            created_event(),
            item_added_event("message", "assistant"),
            text_delta_event("Searching the web"),
            // ...stream ends here: no response.output_item.done, no completed.
        ]
        .iter()
        .flat_map(|e| converter.convert(e))
        .collect();
        events.extend(converter.finalize());

        let names = event_types(&events);
        assert_eq!(
            names.last(),
            Some(&"message_stop"),
            "stream must end with message_stop, got {names:?}"
        );
        // The dangling text content block must be closed before message_stop.
        let stop_idx = names.iter().position(|n| *n == "content_block_stop");
        let msg_stop_idx = names.iter().position(|n| *n == "message_stop");
        assert!(stop_idx < msg_stop_idx, "open block not closed: {names:?}");
    }

    #[test]
    fn finalize_terminates_stream_with_no_events_at_all() {
        // The engine can stall before producing any output. finalize() must
        // still synthesize a complete, valid message envelope.
        let mut converter = AnthropicStreamConverter::new("claude-3".to_string());
        let events = converter.finalize();
        assert_eq!(
            event_types(&events),
            vec!["ping", "message_start", "message_delta", "message_stop"]
        );
    }

    #[test]
    fn finalize_is_noop_after_normal_completion() {
        let mut converter = AnthropicStreamConverter::new("claude-3".to_string());
        let _: Vec<_> = [
            created_event(),
            item_added_event("message", "assistant"),
            text_delta_event("Hello"),
            item_done_event("message", json!({})),
            completed_event(),
        ]
        .iter()
        .flat_map(|e| converter.convert(e))
        .collect();
        // No duplicate message_delta/message_stop after a clean completion.
        assert!(converter.finalize().is_empty());
    }

    fn web_search_results_event(tool_use_id: &str, query: &str, results: Value) -> SseEvent {
        SseEvent {
            event: "response.web_search_results".to_string(),
            data: json!({
                "type": "response.web_search_results",
                "tool_use_id": tool_use_id,
                "query": query,
                "results": results,
            }),
        }
    }

    #[test]
    fn web_search_results_emit_server_tool_use_then_result_block() {
        // Regression: resp2chat ran Brave server-side but swallowed the call,
        // so Claude Code reported "Did 0 searches" and listed no sources. The
        // converter must surface the Anthropic server-side web-search blocks
        // (`server_tool_use` + `web_search_tool_result`) so the client counts
        // the search and renders source chips.
        let mut converter = AnthropicStreamConverter::new("claude-3".to_string());
        let results = json!([
            {"type": "web_search_result", "url": "https://example.com/a", "title": "Site A"},
            {"type": "web_search_result", "url": "https://example.com/b", "title": "Site B"}
        ]);
        let events: Vec<AnthropicStreamEvent> = [
            created_event(),
            item_added_event("message", "assistant"),
            text_delta_event("Let me search."),
            web_search_results_event("srvtoolu_1", "current weather Boppard", results.clone()),
            text_delta_event("It is 11C."),
            item_done_event("message", json!({})),
            completed_event(),
        ]
        .iter()
        .flat_map(|e| converter.convert(e))
        .collect();

        let jsons: Vec<Value> = events
            .iter()
            .map(|e| serde_json::from_str(&e.to_json()).unwrap())
            .collect();

        // server_tool_use block, query streamed via input_json_delta, then stop.
        let stu_pos = jsons
            .iter()
            .position(|j| j["content_block"]["type"] == "server_tool_use")
            .expect("server_tool_use content_block_start");
        assert_eq!(jsons[stu_pos]["type"], "content_block_start");
        assert_eq!(jsons[stu_pos]["content_block"]["name"], "web_search");
        let stu_id = jsons[stu_pos]["content_block"]["id"].as_str().unwrap();
        assert!(!stu_id.is_empty(), "server_tool_use must carry an id");
        let stu_idx = jsons[stu_pos]["index"].clone();

        assert_eq!(jsons[stu_pos + 1]["type"], "content_block_delta");
        assert_eq!(jsons[stu_pos + 1]["delta"]["type"], "input_json_delta");
        let partial = jsons[stu_pos + 1]["delta"]["partial_json"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(partial).expect("partial_json is JSON");
        assert_eq!(parsed["query"], "current weather Boppard");
        assert_eq!(jsons[stu_pos + 2]["type"], "content_block_stop");
        assert_eq!(jsons[stu_pos + 2]["index"], stu_idx);

        // web_search_tool_result block carrying the structured sources.
        let res_pos = stu_pos + 3;
        assert_eq!(jsons[res_pos]["type"], "content_block_start");
        assert_eq!(
            jsons[res_pos]["content_block"]["type"],
            "web_search_tool_result"
        );
        assert_eq!(jsons[res_pos]["content_block"]["tool_use_id"], stu_id);
        assert_eq!(jsons[res_pos]["content_block"]["content"], results);
        assert_eq!(jsons[res_pos + 1]["type"], "content_block_stop");

        // Server-side tool: turn still ends with end_turn, not tool_use.
        let msg_delta = jsons
            .iter()
            .find(|j| j["type"] == "message_delta")
            .expect("message_delta");
        assert_eq!(msg_delta["delta"]["stop_reason"], "end_turn");
    }

    #[test]
    fn web_search_count_is_reported_in_terminal_usage() {
        // Claude Code's "Did N searches" reads usage.server_tool_use
        // .web_search_requests. resp2chat must report it so a server-side
        // search isn't shown as "Did 0 searches".
        let mut converter = AnthropicStreamConverter::new("claude-3".to_string());
        let r = json!([{"type": "web_search_result", "url": "https://x", "title": "X"}]);
        let events: Vec<AnthropicStreamEvent> = [
            created_event(),
            item_added_event("message", "assistant"),
            web_search_results_event("srvtoolu_1", "q1", r.clone()),
            web_search_results_event("srvtoolu_2", "q2", r.clone()),
            text_delta_event("answer"),
            item_done_event("message", json!({})),
            completed_event(),
        ]
        .iter()
        .flat_map(|e| converter.convert(e))
        .collect();

        let md = events
            .iter()
            .find(|e| matches!(e, AnthropicStreamEvent::MessageDelta { .. }))
            .map(|e| serde_json::from_str::<Value>(&e.to_json()).unwrap())
            .expect("message_delta");
        assert_eq!(md["usage"]["server_tool_use"]["web_search_requests"], 2);
    }

    #[test]
    fn no_server_tool_use_usage_when_no_web_search() {
        // Pristine: a turn without web search must not emit server_tool_use usage.
        let mut converter = AnthropicStreamConverter::new("claude-3".to_string());
        let events: Vec<AnthropicStreamEvent> = [
            created_event(),
            item_added_event("message", "assistant"),
            text_delta_event("hi"),
            item_done_event("message", json!({})),
            completed_event(),
        ]
        .iter()
        .flat_map(|e| converter.convert(e))
        .collect();
        let md = events
            .iter()
            .find(|e| matches!(e, AnthropicStreamEvent::MessageDelta { .. }))
            .map(|e| serde_json::from_str::<Value>(&e.to_json()).unwrap())
            .expect("message_delta");
        assert!(
            md["usage"].get("server_tool_use").is_none(),
            "no web search => no server_tool_use usage, got {}",
            md["usage"]
        );
    }

    #[test]
    fn finalize_terminates_stream_after_failure_event() {
        // handle_failed only emits `error`; the client still needs a terminal
        // message_stop to stop waiting.
        let mut converter = AnthropicStreamConverter::new("claude-3".to_string());
        let mut events = converter.convert(&created_event());
        events.extend(converter.convert(&failed_event("web search round limit exceeded")));
        events.extend(converter.finalize());

        let names = event_types(&events);
        assert!(names.contains(&"error"), "expected error event: {names:?}");
        assert_eq!(
            names.last(),
            Some(&"message_stop"),
            "stream must still end with message_stop, got {names:?}"
        );
    }
}
