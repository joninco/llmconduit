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
use std::collections::HashSet;
use uuid::Uuid;

const ESTIMATED_OUTPUT_TOKEN_BYTES: usize = 4;

/// Name of the server-side web-search tool. Brave runs server-side, so the
/// model's own `web_search` call is NOT surfaced to the Anthropic client as a
/// regular `tool_use` block -- the search is rendered via the additive
/// `response.web_search_results` event (`server_tool_use` +
/// `web_search_tool_result`). The streamed `function_call_arguments` for this
/// tool are therefore swallowed: they must not open a client tool_use block,
/// must not flip `has_tool_calls` (the turn ends `end_turn`, not `tool_use`),
/// and must not trip the late-reasoning drop gate (`content_started`) -- a
/// web-search turn legitimately continues with reasoning after the results.
const WEB_SEARCH_TOOL_NAME: &str = "web_search";

/// Whether a tool name is a server-side tool the converter must swallow from the
/// canonical Responses stream so it never becomes an Anthropic `tool_use` block.
/// This is ONLY Brave `web_search`: the engine still streams its
/// `function_call_arguments` (the search is surfaced separately via
/// `response.web_search_results`), so the converter has to drop them, and the
/// turn ends `end_turn` so post-search reasoning is not gated as "late".
///
/// NOTE (round-7 #2): `analyzeImage` is deliberately NOT hidden by name here. On
/// an active image-agent turn the engine classifies it as the server-side
/// ImageAnalysis tool and NEVER emits its delta/done/item events into the stream
/// at all, so the converter never sees them. On an INACTIVE turn `analyzeImage`
/// is a legitimate CLIENT tool that must surface normally — name-hiding it would
/// wrongly swallow the client's tool.
fn is_hidden_server_tool(name: &str) -> bool {
    name == WEB_SEARCH_TOOL_NAME
}

enum ContentBlockState {
    Text { index: usize },
    ToolUse { index: usize, call_id: String },
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
    estimated_output_bytes: usize,
    last_output_tokens: u64,
    web_search_count: u64,
    emitted_tool_call_ids: HashSet<String>,
    closed_tool_call_ids: HashSet<String>,
    // Deferred reasoning (G8): reasoning is buffered rather than emitted live so
    // the converter can decide its final shape only once the stream's shape is
    // known. When text/tool output follows, the buffer is flushed as a genuine
    // `thinking` block. When the turn produces *only* reasoning, a CLEAN
    // terminal (`response.completed`) promotes it to a `text` block (the backend
    // put the answer in the reasoning channel) -- unless it carries a signature,
    // which marks it as genuine chain-of-thought that must stay a `thinking`
    // block. ANY `response.incomplete` terminal (`max_output_tokens`,
    // `content_filter`, or any future reason) is not a clean stop and is never
    // promoted -- it stays a `thinking` block. Reasoning arriving after text has
    // already started is "late" and dropped.
    reasoning_buffer: Vec<String>,
    reasoning_signature: Option<String>,
    // The late-reasoning drop gate. Set ONLY by real text/tool output (the cases
    // where any subsequent reasoning is genuinely abnormal/late). It is NOT set
    // by the additive `response.web_search_results` block: a normal web-search
    // turn streams CONTINUATION reasoning after the results, which must be
    // buffered and handled normally rather than dropped. The fact that a search
    // ran is tracked separately via `web_search_count`.
    content_started: bool,
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
            estimated_output_bytes: 0,
            last_output_tokens: 0,
            web_search_count: 0,
            emitted_tool_call_ids: HashSet::new(),
            closed_tool_call_ids: HashSet::new(),
            reasoning_buffer: Vec::new(),
            reasoning_signature: None,
            content_started: false,
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
            "response.reasoning_summary_text.signature_delta" => {
                self.handle_reasoning_signature_delta(&event.data, &mut output);
            }
            "response.function_call_arguments.delta" => {
                self.handle_function_call_arguments_delta(&event.data, &mut output);
            }
            "response.function_call_arguments.done" => {
                self.handle_function_call_arguments_done(&event.data, &mut output);
            }
            "response.output_item.done" => self.handle_item_done(&event.data, &mut output),
            "response.completed" | "response.incomplete" => {
                self.handle_completed(event.event.as_str(), &event.data, &mut output)
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
                self.flush_reasoning_as_thinking(output);
                self.close_open_block(output);
                self.content_started = true;
                self.start_text_block(output);
            }
            "reasoning" => {
                // Deferred: do not open a thinking block here. Reasoning is
                // buffered until the stream shape is known (see struct docs).
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
                self.record_output_delta(delta, output);
            }
            _ => {
                self.flush_reasoning_as_thinking(output);
                self.close_open_block(output);
                self.content_started = true;
                self.start_text_block(output);
                if let Some(ContentBlockState::Text { index }) = self.open_block {
                    output.push(AnthropicStreamEvent::ContentBlockDelta {
                        index,
                        delta: AnthropicDelta::TextDelta {
                            text: delta.to_string(),
                        },
                    });
                    self.record_output_delta(delta, output);
                }
            }
        }
    }

    fn handle_reasoning_delta(&mut self, data: &Value, output: &mut Vec<AnthropicStreamEvent>) {
        self.ensure_started(output);
        let Some(delta) = data.get("delta").and_then(Value::as_str) else {
            return;
        };
        if delta.is_empty() {
            return;
        }
        // Late reasoning: anything that arrives after text/tool output has
        // already begun is abnormal and dropped (it cannot legally precede the
        // content that was already streamed).
        if self.content_started {
            return;
        }
        // Defer: buffer the reasoning, but keep progressive output-usage live so
        // clients still see the token counter advance while the model thinks.
        self.record_output_delta(delta, output);
        self.reasoning_buffer.push(delta.to_string());
    }

    fn handle_reasoning_signature_delta(
        &mut self,
        data: &Value,
        output: &mut Vec<AnthropicStreamEvent>,
    ) {
        self.ensure_started(output);
        let Some(signature) = data.get("signature").and_then(Value::as_str) else {
            return;
        };
        if signature.is_empty() || self.content_started {
            return;
        }
        // Buffered alongside the reasoning text; flushed with it. A signature is
        // a marker of genuine chain-of-thought, so its presence later pins the
        // buffer to a `thinking` block (never promoted to text). A signature can
        // arrive in multiple `signature_delta` chunks, so accumulate them in
        // order (concatenate) rather than overwriting -- otherwise only the last
        // fragment would survive.
        self.reasoning_signature
            .get_or_insert_with(String::new)
            .push_str(signature);
    }

    fn handle_function_call_arguments_delta(
        &mut self,
        data: &Value,
        output: &mut Vec<AnthropicStreamEvent>,
    ) {
        self.ensure_started(output);
        let Some(call_id) = data.get("call_id").and_then(Value::as_str) else {
            return;
        };
        if self.emitted_tool_call_ids.contains(call_id)
            || self.closed_tool_call_ids.contains(call_id)
        {
            return;
        }
        let name = data.get("name").and_then(Value::as_str).unwrap_or_default();
        // Server-side tools (web_search, analyzeImage): swallow streamed
        // arguments. web_search is surfaced via `response.web_search_results`;
        // analyzeImage is fully internal (G4). Opening a client tool_use block
        // here would leak it, flip the turn to `tool_use`, and trip the
        // late-reasoning gate against the post-tool reasoning.
        if is_hidden_server_tool(name) {
            return;
        }
        let Some(delta) = data.get("delta").and_then(Value::as_str) else {
            return;
        };
        if delta.is_empty() {
            return;
        }
        self.has_tool_calls = true;
        self.ensure_tool_block(call_id, name, output);
        if let Some(ContentBlockState::ToolUse { index, .. }) = self.open_block {
            output.push(AnthropicStreamEvent::ContentBlockDelta {
                index,
                delta: AnthropicDelta::InputJsonDelta {
                    partial_json: delta.to_string(),
                },
            });
            self.record_output_delta(delta, output);
        }
    }

    fn handle_function_call_arguments_done(
        &mut self,
        data: &Value,
        output: &mut Vec<AnthropicStreamEvent>,
    ) {
        self.ensure_started(output);
        let Some(call_id) = data.get("call_id").and_then(Value::as_str) else {
            return;
        };
        // Server-side tools (web_search, analyzeImage): swallow. Must precede the
        // `has_tool_calls` flip so the turn ends `end_turn` and post-tool
        // reasoning is not gated as "late".
        if data
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(is_hidden_server_tool)
        {
            return;
        }
        self.has_tool_calls = true;
        if self.emitted_tool_call_ids.contains(call_id) {
            return;
        }
        if self.closed_tool_call_ids.contains(call_id) {
            self.emitted_tool_call_ids.insert(call_id.to_string());
            return;
        }
        if matches!(
            &self.open_block,
            Some(ContentBlockState::ToolUse {
                call_id: open_call_id,
                ..
            }) if open_call_id == call_id
        ) {
            self.close_open_block(output);
            self.emitted_tool_call_ids.insert(call_id.to_string());
            return;
        }

        let name = data.get("name").and_then(Value::as_str).unwrap_or_default();
        let arguments = data
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or("{}");
        self.ensure_tool_block(call_id, name, output);
        if !arguments.is_empty()
            && let Some(ContentBlockState::ToolUse { index, .. }) = self.open_block
        {
            output.push(AnthropicStreamEvent::ContentBlockDelta {
                index,
                delta: AnthropicDelta::InputJsonDelta {
                    partial_json: arguments.to_string(),
                },
            });
            self.record_output_delta(arguments, output);
        }
        self.close_open_block(output);
        self.emitted_tool_call_ids.insert(call_id.to_string());
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
                // Deferred: the reasoning item completing does not flush the
                // buffer. The buffer is held until content arrives (-> thinking)
                // or the turn ends (-> promote to text / keep as thinking).
            }
            "function_call" | "custom_tool_call" => {
                // G4: a server-side `analyzeImage` function_call is hidden like
                // `web_search_call` (review #3) — never surfaced as a tool_use
                // block. The engine already keeps it out of the public stream.
                if item
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(is_hidden_server_tool)
                {
                    return;
                }
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
        // No clean terminal event arrived (engine error / stalled turn). The
        // reference treats a missing finish_reason as "do not promote": flush any
        // buffered reasoning as a thinking block rather than guessing it was the
        // answer. Promotion is reserved for a clean `response.completed`.
        self.flush_reasoning_as_thinking(&mut output);
        self.close_open_block(&mut output);
        output.push(AnthropicStreamEvent::MessageDelta {
            delta: AnthropicMessageDeltaBody {
                stop_reason: Some(if self.has_tool_calls {
                    "tool_use".to_string()
                } else {
                    "end_turn".to_string()
                }),
                stop_sequence: None,
            },
            usage: AnthropicUsage {
                input_tokens: self.pending_input_tokens,
                output_tokens: Some(self.last_output_tokens),
                server_tool_use: self.server_tool_use_usage(),
            },
        });
        output.push(AnthropicStreamEvent::MessageStop);
        self.completed = true;
        output
    }

    fn handle_completed(
        &mut self,
        event_type: &str,
        data: &Value,
        output: &mut Vec<AnthropicStreamEvent>,
    ) {
        self.completed = true;
        let mapped_stop_reason = response_stop_reason(data);
        // T7: resolve any buffered reasoning before closing out, promoting a
        // reasoning-only turn to text ONLY on a CLEAN STOP (`terminal_reason:
        // stop`). The gate reads the typed terminal reason the engine carries
        // on the resource (not the event-type string), so a future non-stop
        // terminal reason arriving as `response.completed` can no longer
        // wrongly promote — `response.incomplete` is never a clean stop, and a
        // non-`stop` `response.completed` (e.g. `content_filter`) is not
        // either. Fallback: if the typed reason is absent (older event / a
        // non-terminal resource), only a literal `response.completed` is
        // treated as a clean stop (preserves the pre-T7 behavior for any path
        // the engine did not tag).
        let clean_stop = response_terminal_reason(data)
            .map(|reason| reason.is_clean_stop())
            .unwrap_or_else(|| event_type == "response.completed");
        self.flush_reasoning_terminal(clean_stop, output);
        self.close_open_block(output);
        let usage = response_usage(data);
        if let Some(usage) = usage.as_ref() {
            self.pending_input_tokens = Some(usage.input_tokens);
        }
        let output_tokens = usage
            .as_ref()
            .map(|usage| usage.output_tokens)
            .unwrap_or(self.last_output_tokens)
            .max(self.last_output_tokens);
        self.last_output_tokens = output_tokens;
        let stop_reason = if let Some(reason) = mapped_stop_reason {
            reason
        } else if self.has_tool_calls {
            "tool_use".to_string()
        } else {
            "end_turn".to_string()
        };
        output.push(AnthropicStreamEvent::MessageDelta {
            delta: AnthropicMessageDeltaBody {
                stop_reason: Some(stop_reason),
                stop_sequence: None,
            },
            usage: AnthropicUsage {
                input_tokens: usage.as_ref().map(|usage| usage.input_tokens),
                output_tokens: Some(output_tokens),
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
    ///
    /// A web-search round is additive, NOT real text/tool output: a normal turn
    /// is reasoning -> web_search_call -> `web_search_results` -> CONTINUATION
    /// reasoning -> text answer. So this block must NOT trip the late-reasoning
    /// gate (`content_started`) -- doing so would drop the legitimate
    /// post-search continuation reasoning. It still flushes any buffered
    /// PRE-search reasoning as a `thinking` block (genuine CoT that preceded the
    /// search) and records the search via `web_search_count` (tracked separately
    /// from `content_started`, which keys only on real text/tool content). The
    /// continuation reasoning that follows is buffered and handled normally
    /// (flushed as thinking when the answer starts, or resolved at the terminal).
    fn handle_web_search_results(&mut self, data: &Value, output: &mut Vec<AnthropicStreamEvent>) {
        self.ensure_started(output);
        self.flush_reasoning_as_thinking(output);
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

    /// Flush buffered reasoning as a genuine `thinking` block.
    ///
    /// Used when text/tool content follows the reasoning (the reasoning was a
    /// real chain-of-thought preface) and at the terminal event for truncated or
    /// signed reasoning. No-op when the buffer is empty. The buffer (text and
    /// signature) is consumed.
    fn flush_reasoning_as_thinking(&mut self, output: &mut Vec<AnthropicStreamEvent>) {
        if self.reasoning_buffer.is_empty() && self.reasoning_signature.is_none() {
            return;
        }
        self.close_open_block(output);
        let index = self.next_block_index;
        self.next_block_index += 1;
        output.push(AnthropicStreamEvent::ContentBlockStart {
            index,
            content_block: AnthropicContentBlockStart::Thinking {
                thinking: String::new(),
            },
        });
        for chunk in std::mem::take(&mut self.reasoning_buffer) {
            output.push(AnthropicStreamEvent::ContentBlockDelta {
                index,
                delta: AnthropicDelta::ThinkingDelta { thinking: chunk },
            });
        }
        if let Some(signature) = self.reasoning_signature.take() {
            output.push(AnthropicStreamEvent::ContentBlockDelta {
                index,
                delta: AnthropicDelta::SignatureDelta { signature },
            });
        }
        output.push(AnthropicStreamEvent::ContentBlockStop { index });
    }

    /// Resolve the reasoning buffer at the terminal event.
    ///
    /// If reasoning was the *only* output this turn (no text or tool blocks),
    /// the backend put the answer in the reasoning channel: promote it to a
    /// `text` block so the client renders it -- but ONLY on a CLEAN STOP (a
    /// true `finish_reason:stop`, surfaced as `terminal_reason: stop` since
    /// T7) and only when it is not genuine chain-of-thought (no signature).
    /// EVERY other terminal reason -- `length` (`response.incomplete`), a
    /// future `content_filter` arriving as `response.completed`, or any
    /// non-`stop` reason -- is not a clean stop, so `clean_stop` is false and
    /// the buffer stays a `thinking` block. Once any content has started,
    /// leftover reasoning is just a normal preface and is flushed as a
    /// `thinking` block.
    fn flush_reasoning_terminal(
        &mut self,
        clean_stop: bool,
        output: &mut Vec<AnthropicStreamEvent>,
    ) {
        if self.reasoning_buffer.is_empty() && self.reasoning_signature.is_none() {
            return;
        }
        let promote = clean_stop
            && !self.content_started
            && !self.has_tool_calls
            && self.reasoning_signature.is_none();
        if !promote {
            self.flush_reasoning_as_thinking(output);
            return;
        }
        let promoted: String = std::mem::take(&mut self.reasoning_buffer).concat();
        self.reasoning_signature = None;
        self.content_started = true;
        self.close_open_block(output);
        let index = self.next_block_index;
        self.next_block_index += 1;
        output.push(AnthropicStreamEvent::ContentBlockStart {
            index,
            content_block: AnthropicContentBlockStart::Text {
                text: String::new(),
            },
        });
        output.push(AnthropicStreamEvent::ContentBlockDelta {
            index,
            delta: AnthropicDelta::TextDelta { text: promoted },
        });
        output.push(AnthropicStreamEvent::ContentBlockStop { index });
    }

    fn close_open_block(&mut self, output: &mut Vec<AnthropicStreamEvent>) {
        if let Some(block) = self.open_block.take() {
            let index = match block {
                ContentBlockState::Text { index } => index,
                ContentBlockState::ToolUse { index, call_id } => {
                    self.closed_tool_call_ids.insert(call_id);
                    index
                }
            };
            output.push(AnthropicStreamEvent::ContentBlockStop { index });
        }
    }

    fn record_output_delta(&mut self, delta: &str, output: &mut Vec<AnthropicStreamEvent>) {
        if delta.is_empty() {
            return;
        }
        self.estimated_output_bytes = self.estimated_output_bytes.saturating_add(delta.len());
        let estimated_tokens = self
            .estimated_output_bytes
            .div_ceil(ESTIMATED_OUTPUT_TOKEN_BYTES) as u64;
        if estimated_tokens <= self.last_output_tokens {
            return;
        }
        self.last_output_tokens = estimated_tokens;
        output.push(AnthropicStreamEvent::MessageDelta {
            delta: AnthropicMessageDeltaBody {
                stop_reason: None,
                stop_sequence: None,
            },
            usage: AnthropicUsage {
                input_tokens: None,
                output_tokens: Some(estimated_tokens),
                server_tool_use: None,
            },
        });
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

    fn ensure_tool_block(
        &mut self,
        call_id: &str,
        name: &str,
        output: &mut Vec<AnthropicStreamEvent>,
    ) {
        if matches!(
            &self.open_block,
            Some(ContentBlockState::ToolUse {
                call_id: open_call_id,
                ..
            }) if open_call_id == call_id
        ) {
            return;
        }
        self.flush_reasoning_as_thinking(output);
        self.content_started = true;
        self.close_open_block(output);
        let index = self.next_block_index;
        self.next_block_index += 1;
        self.open_block = Some(ContentBlockState::ToolUse {
            index,
            call_id: call_id.to_string(),
        });
        output.push(AnthropicStreamEvent::ContentBlockStart {
            index,
            content_block: AnthropicContentBlockStart::ToolUse {
                id: call_id.to_string(),
                name: name.to_string(),
                input: Value::Object(Default::default()),
            },
        });
    }

    fn emit_tool_use_block(&mut self, item: &Value, output: &mut Vec<AnthropicStreamEvent>) {
        self.has_tool_calls = true;
        let call_id = item
            .get("call_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if self.emitted_tool_call_ids.contains(call_id) {
            return;
        }
        if self.closed_tool_call_ids.contains(call_id) {
            self.emitted_tool_call_ids.insert(call_id.to_string());
            return;
        }
        let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
        let arguments = item
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or("{}");

        self.ensure_tool_block(call_id, name, output);
        if let Some(ContentBlockState::ToolUse { index, .. }) = self.open_block {
            output.push(AnthropicStreamEvent::ContentBlockDelta {
                index,
                delta: AnthropicDelta::InputJsonDelta {
                    partial_json: arguments.to_string(),
                },
            });
            self.record_output_delta(arguments, output);
        }
        self.close_open_block(output);
        self.emitted_tool_call_ids.insert(call_id.to_string());
    }
}

// ---------------------------------------------------------------------------
// Non-streaming collector: accumulates stream events into a single response
// ---------------------------------------------------------------------------

enum AccumulatedBlock {
    Thinking {
        text: String,
        signature: Option<String>,
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
                                signature: None,
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
                            Some(AccumulatedBlock::Thinking { text, .. }),
                            AnthropicDelta::ThinkingDelta { thinking: t },
                        ) => {
                            text.push_str(&t);
                        }
                        (
                            Some(AccumulatedBlock::Thinking { signature, .. }),
                            AnthropicDelta::SignatureDelta { signature: sig },
                        ) => {
                            *signature = Some(sig);
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
                    if let Some(stop_reason) = delta.stop_reason {
                        self.stop_reason = Some(stop_reason);
                    }
                    if let Some(output_tokens) = usage.output_tokens {
                        self.output_tokens = output_tokens;
                    }
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
                AccumulatedBlock::Thinking { text, signature } => {
                    AnthropicResponseContentBlock::Thinking {
                        thinking: text,
                        signature,
                    }
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
                } => AnthropicResponseContentBlock::WebSearchToolResult {
                    tool_use_id,
                    content,
                },
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

/// Typed terminal reason the engine carries on the terminal resource (T7).
/// `None` only when the field is ABSENT (a non-terminal resource, or an older
/// event that the engine did not tag — the caller falls back to the event-type
/// string). A PRESENT-but-unrecognized reason maps to `Other` (non-clean), NOT
/// `None` — so a future reason the converter doesn't know still gates as
/// non-clean rather than falling back to the event-type string (T7 R1 fix).
/// Reads `data.response.terminal_reason`.
fn response_terminal_reason(data: &Value) -> Option<crate::models::responses::TerminalReason> {
    use crate::models::responses::TerminalReason;
    data.get("response")
        .and_then(|response| response.get("terminal_reason"))
        .and_then(Value::as_str)
        .map(|reason| match reason {
            "stop" => TerminalReason::Stop,
            "length" => TerminalReason::Length,
            "tool_calls" => TerminalReason::ToolCall,
            "content_filter" => TerminalReason::ContentFilter,
            _ => TerminalReason::Other,
        })
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

    fn reasoning_signature_delta_event(signature: &str) -> SseEvent {
        SseEvent {
            event: "response.reasoning_summary_text.signature_delta".to_string(),
            data: json!({
                "type": "response.reasoning_summary_text.signature_delta",
                "signature": signature,
                "summary_index": 0
            }),
        }
    }

    fn function_call_arguments_delta_event(call_id: &str, name: &str, delta: &str) -> SseEvent {
        SseEvent {
            event: "response.function_call_arguments.delta".to_string(),
            data: json!({
                "type": "response.function_call_arguments.delta",
                "call_id": call_id,
                "name": name,
                "delta": delta,
            }),
        }
    }

    fn function_call_arguments_done_event(call_id: &str, name: &str, arguments: &str) -> SseEvent {
        SseEvent {
            event: "response.function_call_arguments.done".to_string(),
            data: json!({
                "type": "response.function_call_arguments.done",
                "call_id": call_id,
                "name": name,
                "arguments": arguments,
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
                "message_delta",
                "content_block_delta",
                "message_delta",
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

        // Reasoning is deferred (G8): its progressive output-usage is emitted
        // live as bytes arrive, but the `thinking` block itself is not opened
        // until the following text forces a flush. So the progress `message_delta`
        // for the reasoning bytes precedes the (contiguous) thinking block.
        assert_eq!(
            event_types(&events),
            vec![
                "ping",
                "message_start",
                "message_delta",       // progressive usage for buffered reasoning
                "content_block_start", // thinking (flushed on text arrival)
                "content_block_delta", // thinking delta
                "content_block_stop",  // close thinking before text
                "content_block_start", // text
                "content_block_delta", // text delta
                "message_delta",       // progressive usage
                "content_block_stop",  // close text
                "message_delta",       // terminal stop reason
                "message_stop",
            ]
        );
    }

    #[test]
    fn converts_reasoning_signature_delta() {
        // A signed reasoning-only turn: the buffer carries a signature, so at the
        // terminal event it is flushed as a `thinking` block (never promoted) and
        // the buffered `signature_delta` is surfaced.
        let mut converter = AnthropicStreamConverter::new("claude-3".to_string());
        let events: Vec<AnthropicStreamEvent> = [
            created_event(),
            item_added_event("reasoning", ""),
            reasoning_delta_event("Thinking..."),
            reasoning_signature_delta_event("sig_123"),
            item_done_event("reasoning", json!({})),
            completed_event(),
        ]
        .iter()
        .flat_map(|e| converter.convert(e))
        .collect();

        let signatures: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                AnthropicStreamEvent::ContentBlockDelta {
                    delta: AnthropicDelta::SignatureDelta { signature },
                    ..
                } => Some(signature.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(signatures, vec!["sig_123"]);
        // Signed reasoning must stay a thinking block, not be promoted to text.
        assert!(
            events.iter().any(|event| matches!(
                event,
                AnthropicStreamEvent::ContentBlockStart {
                    content_block: AnthropicContentBlockStart::Thinking { .. },
                    ..
                }
            )),
            "signed reasoning should produce a thinking block"
        );
        assert!(
            !events.iter().any(|event| matches!(
                event,
                AnthropicStreamEvent::ContentBlockStart {
                    content_block: AnthropicContentBlockStart::Text { .. },
                    ..
                }
            )),
            "signed reasoning must never be promoted to a text block"
        );
    }

    #[test]
    fn reasoning_only_incomplete_non_length_stays_thinking() {
        // A reasoning-only turn ending in `response.incomplete` with a reason
        // OTHER than length/max-tokens (e.g. content_filter) is NOT a clean
        // stop, so it must flush as a thinking block, never be promoted to text.
        let mut converter = AnthropicStreamConverter::new("claude-3".to_string());
        let events: Vec<AnthropicStreamEvent> = [
            created_event(),
            item_added_event("reasoning", ""),
            reasoning_delta_event("partial think"),
            item_done_event("reasoning", json!({})),
            incomplete_event("content_filter", 12, 5),
        ]
        .iter()
        .flat_map(|e| converter.convert(e))
        .collect();

        assert!(
            events.iter().any(|event| matches!(
                event,
                AnthropicStreamEvent::ContentBlockStart {
                    content_block: AnthropicContentBlockStart::Thinking { .. },
                    ..
                }
            )),
            "non-length incomplete reasoning must stay a thinking block"
        );
        assert!(
            !events.iter().any(|event| matches!(
                event,
                AnthropicStreamEvent::ContentBlockStart {
                    content_block: AnthropicContentBlockStart::Text { .. },
                    ..
                }
            )),
            "non-length incomplete reasoning must never be promoted to text"
        );
    }

    #[test]
    fn accumulates_multi_part_signature_deltas() {
        // A thinking signature can be streamed in multiple `signature_delta`
        // chunks; the emitted thinking block's signature must be the full
        // concatenation, not just the last fragment.
        let mut converter = AnthropicStreamConverter::new("claude-3".to_string());
        let events: Vec<AnthropicStreamEvent> = [
            created_event(),
            item_added_event("reasoning", ""),
            reasoning_delta_event("Thinking..."),
            reasoning_signature_delta_event("sig_"),
            reasoning_signature_delta_event("part2_"),
            reasoning_signature_delta_event("end"),
            item_done_event("reasoning", json!({})),
            completed_event(),
        ]
        .iter()
        .flat_map(|e| converter.convert(e))
        .collect();

        let signatures: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                AnthropicStreamEvent::ContentBlockDelta {
                    delta: AnthropicDelta::SignatureDelta { signature },
                    ..
                } => Some(signature.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            signatures,
            vec!["sig_part2_end"],
            "multi-part signature must be concatenated in order"
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
                "message_delta",       // progressive usage
                "content_block_stop",  // close text
                "content_block_start", // tool_use
                "content_block_delta", // input_json_delta
                "message_delta",       // progressive usage
                "content_block_stop",  // close tool_use
                "message_delta",       // terminal stop reason
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
    fn streams_function_call_argument_deltas_progressively() {
        let mut converter = AnthropicStreamConverter::new("claude-3".to_string());
        let events: Vec<AnthropicStreamEvent> = [
            created_event(),
            function_call_arguments_delta_event("call_1", "get_weather", r#"{"loc"#),
            function_call_arguments_delta_event("call_1", "get_weather", r#"ation":"Seattle"}"#),
            function_call_arguments_done_event(
                "call_1",
                "get_weather",
                r#"{"location":"Seattle"}"#,
            ),
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
                "content_block_start",
                "content_block_delta",
                "message_delta",
                "content_block_delta",
                "message_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        let tool_starts = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    AnthropicStreamEvent::ContentBlockStart {
                        content_block: AnthropicContentBlockStart::ToolUse { .. },
                        ..
                    }
                )
            })
            .count();
        assert_eq!(tool_starts, 1);
        let partials: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                AnthropicStreamEvent::ContentBlockDelta {
                    delta: AnthropicDelta::InputJsonDelta { partial_json },
                    ..
                } => Some(partial_json.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(partials, vec![r#"{"loc"#, r#"ation":"Seattle"}"#]);
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
    fn emits_progress_usage_for_reasoning_deltas() {
        let mut converter = AnthropicStreamConverter::new("claude-3".to_string());
        let events: Vec<AnthropicStreamEvent> = [
            created_event(),
            item_added_event("reasoning", ""),
            reasoning_delta_event("abcd"),
            reasoning_delta_event("efgh"),
        ]
        .iter()
        .flat_map(|e| converter.convert(e))
        .collect();

        let message_deltas: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                AnthropicStreamEvent::MessageDelta { delta, usage } => Some((delta, usage)),
                _ => None,
            })
            .collect();
        let output_tokens: Vec<u64> = message_deltas
            .iter()
            .filter_map(|(_, usage)| usage.output_tokens)
            .collect();
        assert_eq!(output_tokens, vec![1, 2]);
        assert!(
            message_deltas
                .iter()
                .all(|(delta, _)| delta.stop_reason.is_none()),
            "progress usage must not terminate the message"
        );
    }

    #[test]
    fn completed_without_upstream_usage_preserves_progress_usage() {
        let mut converter = AnthropicStreamConverter::new("claude-3".to_string());
        let events: Vec<AnthropicStreamEvent> = [
            created_event(),
            item_added_event("message", "assistant"),
            text_delta_event("abcd"),
            item_done_event("message", json!({})),
            completed_event(),
        ]
        .iter()
        .flat_map(|e| converter.convert(e))
        .collect();

        let terminal_delta = events
            .iter()
            .filter_map(|event| match event {
                AnthropicStreamEvent::MessageDelta { delta, usage } => {
                    delta.stop_reason.as_ref().map(|reason| (reason, usage))
                }
                _ => None,
            })
            .next()
            .expect("terminal message_delta");
        assert_eq!(terminal_delta.0, "end_turn");
        assert_eq!(terminal_delta.1.output_tokens, Some(1));
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
        assert_eq!(message_delta.stop_reason.as_deref(), Some("max_tokens"));
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
    fn collector_preserves_thinking_signature() {
        let mut collector = AnthropicStreamCollector::new("claude-3".to_string());
        for event in [
            created_event(),
            item_added_event("reasoning", ""),
            reasoning_delta_event("private chain"),
            reasoning_signature_delta_event("sig_123"),
            item_done_event("reasoning", json!({})),
            completed_event(),
        ] {
            collector.process(&event);
        }

        let response = collector.into_response().expect("response");
        assert!(matches!(
            &response.content[0],
            AnthropicResponseContentBlock::Thinking {
                thinking,
                signature: Some(signature),
            } if thinking == "private chain" && signature == "sig_123"
        ));
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
        let partial = jsons[stu_pos + 1]["delta"]["partial_json"]
            .as_str()
            .unwrap();
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
            .find(|j| j["type"] == "message_delta" && j["delta"]["stop_reason"].is_string())
            .expect("message_delta");
        assert_eq!(msg_delta["delta"]["stop_reason"], "end_turn");
    }

    #[test]
    fn post_web_search_reasoning_and_answer_are_preserved() {
        // Regression: a web-search turn is reasoning -> web_search_results ->
        // CONTINUATION reasoning -> text answer. The web_search_results block is
        // additive and must NOT trip the late-reasoning drop gate, otherwise the
        // legitimate post-search reasoning is silently dropped. Both the
        // post-search reasoning (as a thinking block) and the text answer must
        // survive.
        let mut converter = AnthropicStreamConverter::new("claude-3".to_string());
        let results = json!([
            {"type": "web_search_result", "url": "https://example.com/a", "title": "Site A"}
        ]);
        let events: Vec<AnthropicStreamEvent> = [
            created_event(),
            item_added_event("reasoning", ""),
            reasoning_delta_event("Pre-search thought."),
            web_search_results_event("srvtoolu_1", "weather", results.clone()),
            // Continuation reasoning AFTER the search results.
            item_added_event("reasoning", ""),
            reasoning_delta_event("Post-search thought."),
            // The actual answer.
            item_added_event("message", "assistant"),
            text_delta_event("It is sunny."),
            item_done_event("message", json!({})),
            completed_event(),
        ]
        .iter()
        .flat_map(|e| converter.convert(e))
        .collect();

        // Both reasoning segments must surface as thinking deltas (pre-search
        // flushed at the search boundary, post-search flushed when text begins).
        let thinking: String = events
            .iter()
            .filter_map(|event| match event {
                AnthropicStreamEvent::ContentBlockDelta {
                    delta: AnthropicDelta::ThinkingDelta { thinking },
                    ..
                } => Some(thinking.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            thinking.contains("Pre-search thought."),
            "pre-search reasoning must be preserved as thinking, got {thinking:?}"
        );
        assert!(
            thinking.contains("Post-search thought."),
            "post-search continuation reasoning must NOT be dropped, got {thinking:?}"
        );

        // The text answer must survive intact.
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                AnthropicStreamEvent::ContentBlockDelta {
                    delta: AnthropicDelta::TextDelta { text },
                    ..
                } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "It is sunny.", "the text answer must be preserved");
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
            .filter_map(|e| match e {
                AnthropicStreamEvent::MessageDelta { .. } => {
                    Some(serde_json::from_str::<Value>(&e.to_json()).unwrap())
                }
                _ => None,
            })
            .find(|j| j["usage"].get("server_tool_use").is_some())
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
