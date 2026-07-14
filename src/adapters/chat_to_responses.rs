use crate::adapters::responses_to_chat::ToolKind;
use crate::adapters::responses_to_chat::ToolRegistry;
use crate::adapters::responses_to_chat::tool_call_arguments_object;
use crate::error::AppError;
use crate::error::AppResult;
use crate::models::chat::ChatCompletionChunk;
use crate::models::chat::ChatMessage;
use crate::models::chat::ChatThinking;
use crate::models::chat::ChatToolCall;
use crate::models::responses::ContentItem;
use crate::models::responses::LocalShellAction;
use crate::models::responses::LocalShellExecAction;
use crate::models::responses::ReasoningContentItem;
use crate::models::responses::ResponseItem;
use crate::models::responses::WebSearchAction;
use serde_json::Value;
use std::borrow::Cow;
use uuid::Uuid;

/// E1: cap on the unknown-tool argument text carried on a [`RejectedToolCall`].
/// The args were never executable, so they are kept only for the operator log +
/// the synthetic repair-round echo; cap them so a hostile/huge argument blob
/// cannot bloat the log line or the repair prompt.
const REJECTED_TOOL_ARGS_CAP_BYTES: usize = 2048;

/// Hard aggregate bound on model-controlled bytes retained while one upstream
/// Chat stream is converted into canonical Responses items. The per-SSE-frame
/// guard bounds one frame; this independently prevents many individually-small
/// frames from growing `StreamState` without limit.
const MAX_STREAM_STATE_RETAINED_BYTES: usize = 64 * 1024 * 1024;

/// Charge each retained accumulator for its fixed in-memory structure as well as
/// its separately-counted strings. This prevents an unbounded sequence of empty,
/// index-only tool calls from bypassing the aggregate byte ceiling.
const TOOL_CALL_ACCUMULATOR_RETAINED_BYTES: usize = std::mem::size_of::<ToolCallAccumulator>();

#[derive(Debug, Clone)]
pub enum StreamEmission {
    OutputItemAdded(ResponseItem),
    OutputTextDelta {
        delta: String,
        content_index: usize,
    },
    ContentPartAdded {
        content_index: usize,
    },
    ContentPartDone {
        text: String,
    },
    ReasoningItemAdded(ResponseItem),
    /// Provider-private reasoning text retained for internal converters only.
    ReasoningTextDelta(String),
    /// Provider-explicit safe summary text eligible for raw Responses egress.
    ReasoningSummaryTextDelta(String),
    ReasoningSignatureDelta(String),
    ReasoningSummaryPartAdded,
    ReasoningSummaryPartDone {
        text: String,
    },
    FunctionCallArgumentsDelta {
        call_id: String,
        name: Option<String>,
        delta: String,
    },
    RefusalPartAdded {
        content_index: usize,
    },
    RefusalDelta {
        delta: String,
        content_index: usize,
    },
}

#[derive(Debug, Clone)]
pub struct ResolvedToolCall {
    pub kind: ToolKind,
    pub arguments: Value,
    pub public_item: ResponseItem,
    pub internal_call: ChatToolCall,
    /// The provider-supplied identity before llmconduit minted or normalized a
    /// public call id. This lives only for the duration of the turn so the
    /// bounded diagnostics can compare identities without retaining the raw id.
    pub raw_upstream_call_id: Option<String>,
}

/// Why a streamed upstream tool call was soft-rejected instead of executed (E1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolRejectionReason {
    /// The tool name was NOT in the offered tool set for this turn — a
    /// hallucinated/unoffered tool (e.g. a Claude Code tool the client DEFERS
    /// behind `ToolSearch` and did not offer). NOT executable, NOT handed off.
    UnknownTool,
}

/// A streamed upstream tool call that [`StreamState::finalize`] classified as
/// non-executable (E1). It is NOT in [`FinalizedAssistantTurn::tool_calls`] (so
/// it is never executed or handed to the client); the engine taints the whole
/// batch, injects a synthetic tool result for it, and runs a bounded in-gateway
/// repair round so the model can self-correct.
///
/// `raw_arguments` carries the model's argument text capped for LOGGING ONLY —
/// it is never JSON-parsed (the call was never executable, and parsing untrusted
/// args of an unknown tool buys nothing but a second failure mode).
#[derive(Debug, Clone)]
pub struct RejectedToolCall {
    pub call_id: String,
    pub name: String,
    pub raw_arguments: String,
    pub reason: ToolRejectionReason,
}

#[derive(Debug, Clone)]
pub struct FinalizedAssistantTurn {
    pub message_item: Option<ResponseItem>,
    pub reasoning_item: Option<ResponseItem>,
    pub tool_calls: Vec<ResolvedToolCall>,
    /// E1: tool calls whose name was not in the offered set. Empty on a normal
    /// turn; non-empty taints the WHOLE batch (no `tool_calls` are executed or
    /// handed off) and triggers the engine's bounded repair round.
    pub rejected_tool_calls: Vec<RejectedToolCall>,
    pub internal_assistant_message: Option<ChatMessage>,
    pub content_part_emitted: bool,
    pub output_content_index: Option<usize>,
    pub refusal_content_index: Option<usize>,
    pub reasoning_part_emitted: bool,
    pub refusal_text: String,
    pub finish_reason: Option<String>,
    pub stop_sequence: Option<String>,
}

#[derive(Debug)]
pub struct StreamState {
    message_id: Option<String>,
    reasoning_id: Option<String>,
    output_text: String,
    reasoning_text: String,
    reasoning_summary_text: String,
    reasoning_signature: Option<String>,
    tool_calls: Vec<ToolCallAccumulator>,
    content_part_emitted: bool,
    refusal_part_emitted: bool,
    output_content_index: Option<usize>,
    refusal_content_index: Option<usize>,
    reasoning_part_emitted: bool,
    refusal_text: String,
    finish_reason: Option<String>,
    stop_sequence: Option<String>,
    retained_bytes: usize,
    max_retained_bytes: usize,
}

impl Default for StreamState {
    fn default() -> Self {
        Self {
            message_id: None,
            reasoning_id: None,
            output_text: String::new(),
            reasoning_text: String::new(),
            reasoning_summary_text: String::new(),
            reasoning_signature: None,
            tool_calls: Vec::new(),
            content_part_emitted: false,
            refusal_part_emitted: false,
            output_content_index: None,
            refusal_content_index: None,
            reasoning_part_emitted: false,
            refusal_text: String::new(),
            finish_reason: None,
            stop_sequence: None,
            retained_bytes: 0,
            max_retained_bytes: MAX_STREAM_STATE_RETAINED_BYTES,
        }
    }
}

#[derive(Debug, Default, Clone)]
struct ToolCallAccumulator {
    upstream_index: Option<usize>,
    upstream_id: Option<String>,
    served_call_id: Option<String>,
    name: Option<String>,
    arguments_text: String,
}

impl StreamState {
    #[cfg(test)]
    fn with_retained_byte_limit(max_retained_bytes: usize) -> Self {
        Self {
            max_retained_bytes,
            ..Self::default()
        }
    }

    fn reserve_retained_bytes(&mut self, additional: usize) -> AppResult<()> {
        self.replace_retained_bytes(0, additional)
    }

    fn replace_retained_bytes(&mut self, old_len: usize, new_len: usize) -> AppResult<()> {
        let without_old = self.retained_bytes.saturating_sub(old_len);
        let Some(next) = without_old.checked_add(new_len) else {
            return Err(self.retained_byte_limit_error());
        };
        if next > self.max_retained_bytes {
            return Err(self.retained_byte_limit_error());
        }
        self.retained_bytes = next;
        Ok(())
    }

    fn retained_byte_limit_error(&self) -> AppError {
        AppError::upstream(format!(
            "upstream response exceeded the {}-byte retained streaming-state limit",
            self.max_retained_bytes
        ))
        .with_code("upstream_response_too_large")
    }

    pub fn try_apply_chunk(
        &mut self,
        chunk: &ChatCompletionChunk,
    ) -> AppResult<Vec<StreamEmission>> {
        let mut emissions = Vec::new();
        for choice in &chunk.choices {
            if let Some(reasoning_delta) = choice
                .delta
                .reasoning_delta()
                .filter(|delta| !delta.is_empty())
            {
                self.reserve_retained_bytes(reasoning_delta.len())?;
                self.ensure_reasoning_item(&mut emissions);
                self.reasoning_text.push_str(reasoning_delta);
                emissions.push(StreamEmission::ReasoningTextDelta(
                    reasoning_delta.to_string(),
                ));
            }
            if let Some(summary_delta) = choice
                .delta
                .reasoning_summary_delta()
                .filter(|delta| !delta.is_empty())
            {
                self.reserve_retained_bytes(summary_delta.len())?;
                self.ensure_reasoning_item(&mut emissions);
                if !self.reasoning_part_emitted {
                    emissions.push(StreamEmission::ReasoningSummaryPartAdded);
                    self.reasoning_part_emitted = true;
                }
                self.reasoning_summary_text.push_str(summary_delta);
                emissions.push(StreamEmission::ReasoningSummaryTextDelta(
                    summary_delta.to_string(),
                ));
            }
            if let Some(signature) = choice
                .delta
                .reasoning_signature_delta()
                .filter(|signature| !signature.is_empty())
            {
                // Some providers stream the opaque reasoning signature before
                // (or without) a reasoning-text delta. Allocate the canonical
                // reasoning item first so the signature event always has a
                // stable item/output target and the signature survives into the
                // terminal internal resource.
                let old_len = self
                    .reasoning_signature
                    .as_ref()
                    .map(String::len)
                    .unwrap_or(0);
                self.replace_retained_bytes(old_len, signature.len())?;
                self.ensure_reasoning_item(&mut emissions);
                self.reasoning_signature = Some(signature.to_string());
                emissions.push(StreamEmission::ReasoningSignatureDelta(
                    signature.to_string(),
                ));
            }
            if let Some(content_delta) = choice
                .delta
                .content
                .as_deref()
                .filter(|delta| !delta.is_empty())
            {
                self.reserve_retained_bytes(content_delta.len())?;
                if self.message_id.is_none() {
                    let item = ResponseItem::Message {
                        id: Some(new_item_id("msg")),
                        role: "assistant".to_string(),
                        content: Vec::new(),
                        phase: None,
                    };
                    self.message_id = item_message_id(&item);
                    self.content_part_emitted = false;
                    emissions.push(StreamEmission::OutputItemAdded(item));
                }
                if !self.content_part_emitted {
                    let content_index = usize::from(self.refusal_content_index.is_some());
                    self.output_content_index = Some(content_index);
                    emissions.push(StreamEmission::ContentPartAdded { content_index });
                    self.content_part_emitted = true;
                }
                self.output_text.push_str(content_delta);
                emissions.push(StreamEmission::OutputTextDelta {
                    delta: content_delta.to_string(),
                    content_index: self.output_content_index.unwrap_or(0),
                });
            }
            if let Some(refusal) = choice
                .delta
                .refusal
                .as_deref()
                .filter(|delta| !delta.is_empty())
            {
                self.reserve_retained_bytes(refusal.len())?;
                if self.message_id.is_none() {
                    let item = ResponseItem::Message {
                        id: Some(new_item_id("msg")),
                        role: "assistant".to_string(),
                        content: Vec::new(),
                        phase: None,
                    };
                    self.message_id = item_message_id(&item);
                    emissions.push(StreamEmission::OutputItemAdded(item));
                }
                if !self.refusal_part_emitted {
                    let content_index = usize::from(self.output_content_index.is_some());
                    self.refusal_content_index = Some(content_index);
                    emissions.push(StreamEmission::RefusalPartAdded { content_index });
                    self.refusal_part_emitted = true;
                }
                self.refusal_text.push_str(refusal);
                emissions.push(StreamEmission::RefusalDelta {
                    delta: refusal.to_string(),
                    content_index: self.refusal_content_index.unwrap_or(0),
                });
            }
            if let Some(reason) = &choice.finish_reason {
                let old_len = self.finish_reason.as_ref().map(String::len).unwrap_or(0);
                self.replace_retained_bytes(old_len, reason.len())?;
                self.finish_reason = Some(reason.clone());
            }
            // vLLM's `stop_reason` carries the matched stop token: a string when
            // a stop string fired, an integer token id (incl. EOS) otherwise.
            // Only a string is a real stop-sequence match we can surface.
            if let Some(Value::String(stop)) = &choice.stop_reason
                && !stop.is_empty()
            {
                let old_len = self.stop_sequence.as_ref().map(String::len).unwrap_or(0);
                self.replace_retained_bytes(old_len, stop.len())?;
                self.stop_sequence = Some(stop.clone());
            }
            if let Some(tool_calls) = &choice.delta.tool_calls {
                for tool_call in tool_calls {
                    self.apply_tool_call_delta(
                        tool_call.index,
                        tool_call.id.as_deref(),
                        tool_call.function.name.as_deref(),
                        tool_call.function.arguments.as_ref(),
                        &mut emissions,
                    )?;
                }
            }
            if let Some(function_call) = &choice.delta.function_call {
                self.apply_tool_call_delta(
                    None,
                    None,
                    function_call.name.as_deref(),
                    function_call.arguments.as_ref(),
                    &mut emissions,
                )?;
            }
        }
        Ok(emissions)
    }

    #[cfg(test)]
    fn apply_chunk(&mut self, chunk: &ChatCompletionChunk) -> Vec<StreamEmission> {
        self.try_apply_chunk(chunk).expect("valid test chunk")
    }

    fn ensure_reasoning_item(&mut self, emissions: &mut Vec<StreamEmission>) {
        if self.reasoning_id.is_some() {
            return;
        }
        let item = ResponseItem::Reasoning {
            id: new_item_id("rsn"),
            summary: Vec::new(),
            content: Some(Vec::new()),
            encrypted_content: None,
        };
        self.reasoning_id = item_reasoning_id(&item);
        self.reasoning_part_emitted = false;
        emissions.push(StreamEmission::ReasoningItemAdded(item));
    }

    /// A streamed turn is complete only after the upstream supplies a terminal
    /// finish reason. EOF by itself is malformed and must not cause the engine
    /// to synthesize item/content/function `done` events.
    pub fn has_terminal_finish_reason(&self) -> bool {
        self.finish_reason.is_some()
    }

    fn apply_tool_call_delta(
        &mut self,
        upstream_index: Option<usize>,
        upstream_id: Option<&str>,
        name: Option<&str>,
        arguments: Option<&Value>,
        emissions: &mut Vec<StreamEmission>,
    ) -> AppResult<()> {
        let position = self.resolve_tool_call(upstream_index, upstream_id)?;
        let call_id = self.ensure_tool_call_id(position, upstream_id)?;
        if let Some(name) = name {
            let old_len = self.tool_calls[position]
                .name
                .as_ref()
                .map(String::len)
                .unwrap_or(0);
            self.replace_retained_bytes(old_len, name.len())?;
            self.tool_calls[position].name = Some(name.to_string());
        }
        if let Some(arguments) = arguments {
            let fragment = argument_fragment(arguments);
            self.reserve_retained_bytes(fragment.len())?;
            let entry = &mut self.tool_calls[position];
            let before_len = entry.arguments_text.len();
            entry.arguments_text.push_str(&fragment);
            let delta = entry.arguments_text[before_len..].to_string();
            if !delta.is_empty() {
                emissions.push(StreamEmission::FunctionCallArgumentsDelta {
                    call_id,
                    name: entry.name.clone(),
                    delta,
                });
            }
        }
        Ok(())
    }

    fn ensure_tool_call_id(
        &mut self,
        position: usize,
        upstream_id: Option<&str>,
    ) -> AppResult<String> {
        if let Some(call_id) = self.tool_calls[position].served_call_id.clone() {
            return Ok(call_id);
        }
        let call_id = self.tool_calls[position]
            .upstream_id
            .as_deref()
            .or(upstream_id)
            .map(ToString::to_string)
            .unwrap_or_else(|| new_item_id("call"));
        self.reserve_retained_bytes(call_id.len())?;
        self.tool_calls[position].served_call_id = Some(call_id.clone());
        Ok(call_id)
    }

    fn resolve_tool_call(
        &mut self,
        upstream_index: Option<usize>,
        upstream_id: Option<&str>,
    ) -> AppResult<usize> {
        if let Some(index) = upstream_index {
            if let Some(position) = self
                .tool_calls
                .iter()
                .position(|call| call.upstream_index == Some(index))
            {
                if let (Some(existing), Some(incoming)) = (
                    self.tool_calls[position].upstream_id.as_deref(),
                    upstream_id,
                ) && existing != incoming
                {
                    return Err(AppError::upstream(format!(
                        "upstream reused tool call index {index} for multiple call ids"
                    )));
                }
                if self.tool_calls[position].upstream_id.is_none() {
                    if let Some(incoming) = upstream_id {
                        self.reserve_retained_bytes(incoming.len())?;
                    }
                    self.tool_calls[position].upstream_id = upstream_id.map(ToString::to_string);
                }
                return Ok(position);
            }
            if let Some(id) = upstream_id
                && let Some(position) = self
                    .tool_calls
                    .iter()
                    .position(|call| call.upstream_id.as_deref() == Some(id))
            {
                if self.tool_calls[position]
                    .upstream_index
                    .is_some_and(|existing| existing != index)
                {
                    return Err(AppError::upstream(format!(
                        "upstream reused tool call id {id} at multiple indexes"
                    )));
                }
                self.tool_calls[position].upstream_index = Some(index);
                return Ok(position);
            }
            let retained = TOOL_CALL_ACCUMULATOR_RETAINED_BYTES
                .checked_add(upstream_id.map(str::len).unwrap_or(0))
                .ok_or_else(|| self.retained_byte_limit_error())?;
            self.reserve_retained_bytes(retained)?;
            self.tool_calls.push(ToolCallAccumulator {
                upstream_index: Some(index),
                upstream_id: upstream_id.map(ToString::to_string),
                ..Default::default()
            });
            return Ok(self.tool_calls.len() - 1);
        }

        if let Some(id) = upstream_id {
            if let Some(position) = self
                .tool_calls
                .iter()
                .position(|call| call.upstream_id.as_deref() == Some(id))
            {
                return Ok(position);
            }
            let anonymous = self
                .tool_calls
                .iter()
                .enumerate()
                .filter_map(|(position, call)| call.upstream_id.is_none().then_some(position))
                .collect::<Vec<_>>();
            if anonymous.len() == 1 {
                let position = anonymous[0];
                self.reserve_retained_bytes(id.len())?;
                self.tool_calls[position].upstream_id = Some(id.to_string());
                return Ok(position);
            }
            if anonymous.len() > 1 {
                return Err(AppError::upstream(
                    "ambiguous upstream tool call id without an index",
                ));
            }
            let retained = TOOL_CALL_ACCUMULATOR_RETAINED_BYTES
                .checked_add(id.len())
                .ok_or_else(|| self.retained_byte_limit_error())?;
            self.reserve_retained_bytes(retained)?;
            self.tool_calls.push(ToolCallAccumulator {
                upstream_id: Some(id.to_string()),
                ..Default::default()
            });
            return Ok(self.tool_calls.len() - 1);
        }

        match self.tool_calls.len() {
            0 => {
                self.reserve_retained_bytes(TOOL_CALL_ACCUMULATOR_RETAINED_BYTES)?;
                self.tool_calls.push(ToolCallAccumulator::default());
                Ok(0)
            }
            1 => Ok(0),
            _ => Err(AppError::upstream(
                "ambiguous upstream tool call fragment without index or call id",
            )),
        }
    }

    /// Best-effort live output snapshot used only when terminalizing a failed
    /// response. It never marks a malformed/incomplete function call executable.
    pub fn partial_output_items(&self, registry: &ToolRegistry) -> Vec<ResponseItem> {
        let mut items = Vec::new();
        if let Some(id) = self.reasoning_id.clone() {
            items.push(ResponseItem::Reasoning {
                id,
                summary: if self.reasoning_summary_text.is_empty() {
                    Vec::new()
                } else {
                    vec![
                        crate::models::responses::ReasoningSummaryItem::SummaryText {
                            text: self.reasoning_summary_text.clone(),
                        },
                    ]
                },
                content: if self.reasoning_text.is_empty() {
                    None
                } else {
                    Some(vec![ReasoningContentItem::ReasoningText {
                        text: self.reasoning_text.clone(),
                    }])
                },
                encrypted_content: self.reasoning_signature.clone(),
            });
        }
        if let Some(id) = self.message_id.clone() {
            let mut content = Vec::new();
            if !self.output_text.is_empty() {
                content.push((
                    self.output_content_index.unwrap_or(0),
                    ContentItem::OutputText {
                        text: self.output_text.clone(),
                    },
                ));
            }
            if !self.refusal_text.is_empty() {
                content.push((
                    self.refusal_content_index.unwrap_or(0),
                    ContentItem::Refusal {
                        refusal: self.refusal_text.clone(),
                    },
                ));
            }
            content.sort_by_key(|(index, _)| *index);
            items.push(ResponseItem::Message {
                id: Some(id),
                role: "assistant".to_string(),
                content: content.into_iter().map(|(_, part)| part).collect(),
                phase: None,
            });
        }
        // Preserve client-visible function calls that were already introduced
        // on the stream. Keep the raw accumulated argument text: on a failed
        // turn it may intentionally be incomplete/malformed and must never be
        // normalized into an executable call. The engine reconciles these
        // items with its registered public item ids and drops calls that were
        // never exposed (gateway-owned or still-unidentified calls).
        items.extend(self.tool_calls.iter().filter_map(|call| {
            let internal_name = call.name.as_deref()?;
            let call_id = call
                .served_call_id
                .as_deref()
                .or(call.upstream_id.as_deref())?;
            let ToolKind::Function {
                public_name,
                namespace,
            } = registry.get(&internal_name.to_ascii_lowercase())?
            else {
                return None;
            };
            Some(ResponseItem::FunctionCall {
                id: None,
                name: public_name.clone(),
                namespace: namespace.clone(),
                arguments: call.arguments_text.clone(),
                call_id: call_id.to_string(),
            })
        }));
        items
    }

    pub fn finalize(self, registry: &ToolRegistry) -> AppResult<FinalizedAssistantTurn> {
        let message_item = if self.message_id.is_some() {
            let mut content = Vec::new();
            if let Some(index) = self.output_content_index {
                content.push((
                    index,
                    ContentItem::OutputText {
                        text: self.output_text.clone(),
                    },
                ));
            }
            if let Some(index) = self.refusal_content_index {
                content.push((
                    index,
                    ContentItem::Refusal {
                        refusal: self.refusal_text.clone(),
                    },
                ));
            }
            content.sort_by_key(|(index, _)| *index);
            Some(ResponseItem::Message {
                id: self.message_id,
                role: "assistant".to_string(),
                content: content.into_iter().map(|(_, part)| part).collect(),
                phase: None,
            })
        } else {
            None
        };
        let reasoning_item = if self.reasoning_id.is_some() {
            Some(ResponseItem::Reasoning {
                id: self.reasoning_id.unwrap_or_else(|| new_item_id("rsn")),
                summary: if self.reasoning_summary_text.is_empty() {
                    Vec::new()
                } else {
                    vec![
                        crate::models::responses::ReasoningSummaryItem::SummaryText {
                            text: self.reasoning_summary_text.clone(),
                        },
                    ]
                },
                content: if self.reasoning_text.is_empty() {
                    None
                } else {
                    Some(vec![ReasoningContentItem::ReasoningText {
                        text: self.reasoning_text.clone(),
                    }])
                },
                encrypted_content: self.reasoning_signature.clone(),
            })
        } else {
            None
        };
        let mut resolved_tool_calls = Vec::new();
        let mut rejected_tool_calls = Vec::new();
        let mut internal_tool_calls = Vec::new();
        for (position, mut accumulator) in self.tool_calls.into_iter().enumerate() {
            // A missing function name is still a HARD error: the chunk stream is
            // malformed, not a recoverable unoffered-tool generation.
            let raw_upstream_call_id = accumulator.upstream_id.clone();
            let call_id = accumulator.served_call_id.take().ok_or_else(|| {
                AppError::internal("tool call accumulator missing its served call id")
            })?;
            let upstream_index = accumulator.upstream_index.unwrap_or(position);
            let name = accumulator.name.take().ok_or_else(|| {
                AppError::upstream("upstream tool call chunk missing function name")
            })?;
            let name_lc = name.to_ascii_lowercase();
            // E1: an unknown/unoffered tool name is a RECOVERABLE upstream
            // generation error, NOT a hard error (the old `?` aborted the SSE
            // stream mid-flight). Classify it into `rejected_tool_calls` WITHOUT
            // JSON-parsing its arguments (never executable), and replay the
            // attempted call into the assistant message so the engine's synthetic
            // repair-round tool result lines up by `call_id`. Malformed KNOWN-tool
            // args / invalid `local_shell` below STILL hard-error (unchanged).
            let Some(tool_kind) = registry.get(&name_lc).cloned() else {
                let raw_arguments =
                    cap_str(&accumulator.arguments_text, REJECTED_TOOL_ARGS_CAP_BYTES);
                internal_tool_calls.push(ChatToolCall {
                    id: Some(call_id.clone()),
                    index: Some(upstream_index),
                    kind: "function".to_string(),
                    function: crate::models::chat::ChatFunctionCall {
                        name: Some(name.clone()),
                        arguments: Some(Value::String(raw_arguments.clone())),
                    },
                });
                rejected_tool_calls.push(RejectedToolCall {
                    call_id,
                    name,
                    raw_arguments,
                    reason: ToolRejectionReason::UnknownTool,
                });
                continue;
            };
            let arguments = if accumulator.arguments_text.trim().is_empty() {
                Value::Object(Default::default())
            } else {
                let cleaned = extract_json_arguments(&accumulator.arguments_text).ok_or_else(|| {
                    AppError::upstream(format!(
                        "failed to parse upstream tool arguments for {name}: unexpected data outside JSON payload"
                    ))
                })?;
                serde_json::from_str(cleaned).map_err(|err| {
                    AppError::upstream(format!(
                        "failed to parse upstream tool arguments for {name}: {err}"
                    ))
                })?
            };
            registry.validate_function_arguments(&name_lc, &arguments)?;
            let public_item = match &tool_kind {
                ToolKind::Function {
                    public_name,
                    namespace,
                } => ResponseItem::FunctionCall {
                    id: None,
                    name: public_name.clone(),
                    namespace: namespace.clone(),
                    arguments: serde_json::to_string(&arguments).map_err(|err| {
                        AppError::internal(format!("failed to serialize function arguments: {err}"))
                    })?,
                    call_id: call_id.clone(),
                },
                ToolKind::Custom { public_name } => ResponseItem::CustomToolCall {
                    id: Some(new_item_id("ctc")),
                    status: None,
                    call_id: call_id.clone(),
                    name: public_name.clone(),
                    input: arguments
                        .get("input")
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                        .ok_or_else(|| {
                            AppError::upstream(format!(
                                "invalid upstream custom tool input for {name}: expected a string input field"
                            ))
                            .with_code("invalid_tool_call")
                        })?,
                },
                ToolKind::LocalShell => ResponseItem::LocalShellCall {
                    id: None,
                    call_id: Some(call_id.clone()),
                    status: "completed".to_string(),
                    action: LocalShellAction::Exec(
                        serde_json::from_value::<LocalShellExecAction>(arguments.clone()).map_err(
                            |err| {
                                AppError::upstream(format!(
                                    "invalid upstream local_shell arguments: {err}"
                                ))
                            },
                        )?,
                    ),
                },
                ToolKind::ToolSearch => ResponseItem::ToolSearchCall {
                    id: None,
                    call_id: Some(call_id.clone()),
                    status: None,
                    execution: "client".to_string(),
                    arguments: arguments.clone(),
                },
                ToolKind::WebSearch => ResponseItem::WebSearchCall {
                    id: Some(call_id.clone()),
                    status: Some("completed".to_string()),
                    action: Some(WebSearchAction::Search {
                        query: arguments
                            .get("query")
                            .and_then(Value::as_str)
                            .map(ToString::to_string),
                        queries: None,
                        sources: None,
                    }),
                },
                // G4: the `analyzeImage` call is a server-side tool. We carry it
                // as a FunctionCall public_item so the engine's executor can read
                // its arguments and `internal_call`, but the engine keeps it OUT
                // of `response_output`/`public_history` and suppresses its
                // streamed deltas — exactly like `web_search` is never surfaced
                // to the client.
                ToolKind::ImageAnalysis => ResponseItem::FunctionCall {
                    id: None,
                    name: crate::vision::ANALYZE_IMAGE_TOOL_NAME.to_string(),
                    namespace: None,
                    arguments: serde_json::to_string(&arguments).map_err(|err| {
                        AppError::internal(format!(
                            "failed to serialize analyzeImage arguments: {err}"
                        ))
                    })?,
                    call_id: call_id.clone(),
                },
            };
            let internal_call = ChatToolCall {
                id: Some(call_id.clone()),
                index: Some(upstream_index),
                kind: "function".to_string(),
                function: crate::models::chat::ChatFunctionCall {
                    name: Some(name),
                    arguments: Some(tool_call_arguments_object(&Some(arguments.clone()))),
                },
            };
            internal_tool_calls.push(internal_call.clone());
            resolved_tool_calls.push(ResolvedToolCall {
                kind: tool_kind,
                arguments,
                public_item,
                internal_call,
                raw_upstream_call_id,
            });
        }
        let internal_assistant_message = if message_item.is_some()
            || reasoning_item.is_some()
            || !internal_tool_calls.is_empty()
        {
            Some(ChatMessage {
                role: "assistant".to_string(),
                content: message_item.as_ref().map(|item| match item {
                    ResponseItem::Message { content, .. } => Value::String(output_text(content)),
                    _ => Value::Null,
                }),
                tool_call_id: None,
                name: None,
                reasoning_content: reasoning_item.as_ref().map(|item| match item {
                    ResponseItem::Reasoning { content, .. } => content
                        .as_ref()
                        .and_then(|items| items.first())
                        .map(reasoning_content_text)
                        .unwrap_or_default(),
                    _ => String::new(),
                }),
                thinking: reasoning_item.as_ref().and_then(|item| match item {
                    ResponseItem::Reasoning {
                        content,
                        encrypted_content,
                        ..
                    } => encrypted_content.as_ref().map(|signature| ChatThinking {
                        content: content
                            .as_ref()
                            .and_then(|items| items.first())
                            .map(reasoning_content_text)
                            .unwrap_or_default(),
                        signature: Some(signature.clone()),
                    }),
                    _ => None,
                }),
                tool_calls: (!internal_tool_calls.is_empty()).then_some(internal_tool_calls),
            })
        } else {
            None
        };
        Ok(FinalizedAssistantTurn {
            message_item,
            reasoning_item,
            tool_calls: resolved_tool_calls,
            rejected_tool_calls,
            internal_assistant_message,
            content_part_emitted: self.content_part_emitted,
            output_content_index: self.output_content_index,
            refusal_content_index: self.refusal_content_index,
            reasoning_part_emitted: self.reasoning_part_emitted,
            refusal_text: self.refusal_text,
            finish_reason: self.finish_reason,
            stop_sequence: self.stop_sequence,
        })
    }
}

/// Truncate `s` to at most `cap` bytes on a char boundary, appending a short
/// elision marker when it was cut. Used to bound unknown-tool argument text on a
/// [`RejectedToolCall`] (E1) — kept for logging / the synthetic repair echo, not
/// for execution.
fn cap_str(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated {} bytes]", &s[..end], s.len() - end)
}

#[cfg(test)]
fn append_argument_fragment(buffer: &mut String, value: &Value) {
    buffer.push_str(&argument_fragment(value));
}

fn argument_fragment(value: &Value) -> Cow<'_, str> {
    match value {
        Value::String(fragment) => Cow::Borrowed(fragment),
        other => Cow::Owned(serde_json::to_string(other).unwrap_or_else(|_| "null".to_string())),
    }
}

/// Extract the JSON payload from an upstream tool-call `arguments` string.
///
/// vLLM's Kimi/Moonshot tool-call parser can leak the model's internal
/// tool-call sentinel tokens (e.g. `<|tool_calls_section_begin|>`) into
/// `function.arguments`, particularly when `tool_choice` forces a specific
/// function — which is exactly what an Anthropic `web_search` server tool
/// produces. The leaked prefix/suffix makes an otherwise-valid object fail
/// strict JSON parsing ("expected value at line 1 column 2"). Strip only the
/// narrowly-recognized Kimi wrapper tokens and otherwise require the balanced
/// JSON object/array to occupy the complete trimmed payload. This prevents
/// arbitrary prefix/suffix garbage from being repaired into an executable call.
fn extract_json_arguments(raw: &str) -> Option<&str> {
    let mut candidate = raw.trim();
    for prefix in ["<|tool_calls_section_begin|>", "<|tool_call_begin|>"] {
        if let Some(rest) = candidate.strip_prefix(prefix) {
            candidate = rest.trim_start();
            break;
        }
    }
    let open = candidate.chars().next()?;
    if !matches!(open, '{' | '[') {
        return None;
    }
    let close = if open == '{' { '}' } else { ']' };
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (idx, ch) in candidate.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            c if c == open => depth += 1,
            c if c == close => {
                depth -= 1;
                if depth == 0 {
                    let end = idx + ch.len_utf8();
                    let suffix = candidate[end..].trim();
                    if suffix.is_empty()
                        || matches!(suffix, "<|tool_calls_section_end|>" | "<|tool_call_end|>")
                    {
                        return Some(&candidate[..end]);
                    }
                    return None;
                }
            }
            _ => {}
        }
    }
    None
}

fn new_item_id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

fn item_message_id(item: &ResponseItem) -> Option<String> {
    match item {
        ResponseItem::Message { id, .. } => id.clone(),
        _ => None,
    }
}

fn item_reasoning_id(item: &ResponseItem) -> Option<String> {
    match item {
        ResponseItem::Reasoning { id, .. } => Some(id.clone()),
        _ => None,
    }
}

fn output_text(content: &[ContentItem]) -> String {
    content
        .iter()
        .filter_map(|item| match item {
            ContentItem::OutputText { text } | ContentItem::InputText { text } => {
                Some(text.clone())
            }
            ContentItem::InputImage { .. }
            | ContentItem::InputFile { .. }
            | ContentItem::Refusal { .. }
            | ContentItem::Other(_) => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn reasoning_content_text(item: &ReasoningContentItem) -> String {
    match item {
        ReasoningContentItem::ReasoningText { text } | ReasoningContentItem::Text { text } => {
            text.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::responses_to_chat::{ToolKind, ToolRegistry};
    use crate::models::chat::*;
    fn content_chunk(id: &str, text: &str) -> ChatCompletionChunk {
        ChatCompletionChunk {
            service_tier: None,
            id: id.to_string(),
            choices: vec![ChatChunkChoice {
                index: 0,
                delta: ChatDelta {
                    content: Some(text.to_string()),
                    reasoning_content: None,
                    tool_calls: None,
                    function_call: None,
                    refusal: None,
                    extra: Default::default(),
                },
                finish_reason: None,
                stop_reason: None,
            }],
            usage: None,
        }
    }

    fn reasoning_chunk(id: &str, text: &str) -> ChatCompletionChunk {
        ChatCompletionChunk {
            service_tier: None,
            id: id.to_string(),
            choices: vec![ChatChunkChoice {
                index: 0,
                delta: ChatDelta {
                    content: None,
                    reasoning_content: Some(text.to_string()),
                    tool_calls: None,
                    function_call: None,
                    refusal: None,
                    extra: Default::default(),
                },
                finish_reason: None,
                stop_reason: None,
            }],
            usage: None,
        }
    }

    fn tool_call_chunk(
        id: &str,
        call_id: Option<&str>,
        index: usize,
        name: Option<&str>,
        arguments: Option<&str>,
    ) -> ChatCompletionChunk {
        ChatCompletionChunk {
            service_tier: None,
            id: id.to_string(),
            choices: vec![ChatChunkChoice {
                index: 0,
                delta: ChatDelta {
                    content: None,
                    reasoning_content: None,
                    tool_calls: Some(vec![ChatToolCall {
                        id: call_id.map(str::to_string),
                        index: Some(index),
                        kind: "function".to_string(),
                        function: ChatFunctionCall {
                            name: name.map(str::to_string),
                            arguments: arguments.map(|s| serde_json::Value::String(s.to_string())),
                        },
                    }]),
                    function_call: None,
                    refusal: None,
                    extra: Default::default(),
                },
                finish_reason: None,
                stop_reason: None,
            }],
            usage: None,
        }
    }

    fn legacy_function_call_chunk(
        id: &str,
        name: Option<&str>,
        arguments: Option<&str>,
    ) -> ChatCompletionChunk {
        ChatCompletionChunk {
            service_tier: None,
            id: id.to_string(),
            choices: vec![ChatChunkChoice {
                index: 0,
                delta: ChatDelta {
                    content: None,
                    reasoning_content: None,
                    tool_calls: None,
                    function_call: Some(ChatFunctionCall {
                        name: name.map(str::to_string),
                        arguments: arguments.map(|s| serde_json::Value::String(s.to_string())),
                    }),
                    refusal: None,
                    extra: Default::default(),
                },
                finish_reason: None,
                stop_reason: None,
            }],
            usage: None,
        }
    }

    fn simple_registry(entries: Vec<(&str, ToolKind)>) -> ToolRegistry {
        ToolRegistry::from_map(
            entries
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }

    #[test]
    fn apply_chunk_content_delta() {
        let mut state = StreamState::default();
        let emissions = state.apply_chunk(&content_chunk("c1", "hello"));
        assert_eq!(emissions.len(), 3);
        assert!(matches!(&emissions[0], StreamEmission::OutputItemAdded(_)));
        assert!(matches!(
            &emissions[1],
            StreamEmission::ContentPartAdded { content_index: 0 }
        ));
        assert!(
            matches!(&emissions[2], StreamEmission::OutputTextDelta { delta, content_index: 0 } if delta == "hello")
        );
    }

    #[test]
    fn empty_content_delta_is_silent() {
        let mut state = StreamState::default();
        let emissions = state.apply_chunk(&content_chunk("c1", ""));
        assert!(emissions.is_empty());

        let emissions = state.apply_chunk(&content_chunk("c1", "hello"));
        assert_eq!(emissions.len(), 3);
        assert!(matches!(&emissions[0], StreamEmission::OutputItemAdded(_)));
        assert!(matches!(
            &emissions[1],
            StreamEmission::ContentPartAdded { content_index: 0 }
        ));
        assert!(
            matches!(&emissions[2], StreamEmission::OutputTextDelta { delta, content_index: 0 } if delta == "hello")
        );
    }

    #[test]
    fn empty_reasoning_delta_is_silent() {
        let mut state = StreamState::default();
        let emissions = state.apply_chunk(&reasoning_chunk("c1", ""));
        assert!(emissions.is_empty());

        let emissions = state.apply_chunk(&reasoning_chunk("c1", "thinking"));
        assert_eq!(emissions.len(), 2);
        assert!(matches!(
            &emissions[0],
            StreamEmission::ReasoningItemAdded(_)
        ));
        assert!(matches!(&emissions[1], StreamEmission::ReasoningTextDelta(d) if d == "thinking"));
    }

    #[test]
    fn retained_byte_limit_rejects_many_individually_small_output_frames() {
        let mut state = StreamState::with_retained_byte_limit(12);

        for _ in 0..3 {
            state
                .try_apply_chunk(&content_chunk("c1", "abcd"))
                .expect("each frame is below the aggregate limit");
        }
        assert_eq!(state.output_text, "abcdabcdabcd");
        assert_eq!(state.retained_bytes, 12);

        let error = state
            .try_apply_chunk(&content_chunk("c1", "x"))
            .expect_err("the aggregate retained-state limit must be enforced");
        assert_eq!(error.code.as_deref(), Some("upstream_response_too_large"));
        assert!(error.message.contains("retained streaming-state limit"));
        assert_eq!(state.output_text, "abcdabcdabcd");
        assert_eq!(state.retained_bytes, 12);
    }

    #[test]
    fn retained_byte_limit_is_shared_across_reasoning_summary_signature_and_refusal() {
        let mut state = StreamState::with_retained_byte_limit(16);

        state
            .try_apply_chunk(&reasoning_chunk("c1", "abc"))
            .unwrap();

        let mut summary = reasoning_chunk("c1", "");
        summary.choices[0]
            .delta
            .extra
            .insert("reasoning_summary".to_string(), serde_json::json!("defg"));
        state.try_apply_chunk(&summary).unwrap();

        let mut signature = reasoning_chunk("c1", "");
        signature.choices[0]
            .delta
            .extra
            .insert("signature".to_string(), serde_json::json!("1234"));
        state.try_apply_chunk(&signature).unwrap();
        assert_eq!(state.retained_bytes, 11);

        // Signatures are snapshots rather than append-only deltas, so replacing
        // one must release the bytes retained by the prior value.
        signature.choices[0]
            .delta
            .extra
            .insert("signature".to_string(), serde_json::json!("x"));
        state.try_apply_chunk(&signature).unwrap();
        assert_eq!(state.retained_bytes, 8);

        let mut refusal = content_chunk("c1", "");
        refusal.choices[0].delta.content = None;
        refusal.choices[0].delta.refusal = Some("12345678".to_string());
        state.try_apply_chunk(&refusal).unwrap();
        assert_eq!(state.retained_bytes, 16);

        let error = state
            .try_apply_chunk(&content_chunk("c1", "z"))
            .expect_err("all retained payload categories must share one ceiling");
        assert_eq!(error.code.as_deref(), Some("upstream_response_too_large"));
        assert!(state.output_text.is_empty());
        assert_eq!(state.reasoning_text, "abc");
        assert_eq!(state.reasoning_summary_text, "defg");
        assert_eq!(state.reasoning_signature.as_deref(), Some("x"));
        assert_eq!(state.refusal_text, "12345678");
        assert_eq!(state.retained_bytes, 16);
    }

    #[test]
    fn apply_chunk_interleaved_reasoning_and_content() {
        let mut state = StreamState::default();
        let e1 = state.apply_chunk(&reasoning_chunk("c1", "thinking"));
        assert_eq!(e1.len(), 2);
        assert!(matches!(&e1[0], StreamEmission::ReasoningItemAdded(_)));
        assert!(matches!(&e1[1], StreamEmission::ReasoningTextDelta(d) if d == "thinking"));
        let e2 = state.apply_chunk(&content_chunk("c1", "answer"));
        assert_eq!(e2.len(), 3);
        assert!(matches!(&e2[0], StreamEmission::OutputItemAdded(_)));
        assert!(matches!(
            &e2[1],
            StreamEmission::ContentPartAdded { content_index: 0 }
        ));
        assert!(
            matches!(&e2[2], StreamEmission::OutputTextDelta { delta, content_index: 0 } if delta == "answer")
        );
    }

    #[test]
    fn apply_chunk_multi_index_tool_calls() {
        let mut state = StreamState::default();
        state.apply_chunk(&tool_call_chunk(
            "c1",
            Some("call_1"),
            0,
            Some("fn_a"),
            Some("{}"),
        ));
        state.apply_chunk(&tool_call_chunk(
            "c1",
            Some("call_2"),
            1,
            Some("fn_b"),
            Some("{}"),
        ));
        let registry = simple_registry(vec![
            (
                "fn_a",
                ToolKind::Function {
                    public_name: "fn_a".to_string(),
                    namespace: None,
                },
            ),
            (
                "fn_b",
                ToolKind::Function {
                    public_name: "fn_b".to_string(),
                    namespace: None,
                },
            ),
        ]);
        let finalized = state.finalize(&registry).unwrap();
        assert_eq!(finalized.tool_calls.len(), 2);
    }

    #[test]
    fn retained_byte_limit_rejects_incremental_tool_arguments_before_growth() {
        let retained_before_arguments =
            TOOL_CALL_ACCUMULATOR_RETAINED_BYTES + (2 * "call_1".len()) + "fn".len();
        let mut state = StreamState::with_retained_byte_limit(retained_before_arguments + 9);

        state
            .try_apply_chunk(&tool_call_chunk(
                "c1",
                Some("call_1"),
                0,
                Some("fn"),
                Some("aaa"),
            ))
            .unwrap();
        for _ in 0..2 {
            state
                .try_apply_chunk(&tool_call_chunk("c1", None, 0, None, Some("aaa")))
                .unwrap();
        }

        assert_eq!(state.tool_calls[0].arguments_text, "aaaaaaaaa");
        assert_eq!(
            state.retained_bytes, state.max_retained_bytes,
            "the test must reach the injected ceiling exactly"
        );

        let error = state
            .try_apply_chunk(&tool_call_chunk("c1", None, 0, None, Some("x")))
            .expect_err("another sub-limit argument frame must exceed the aggregate limit");
        assert_eq!(error.code.as_deref(), Some("upstream_response_too_large"));
        assert_eq!(state.tool_calls[0].arguments_text, "aaaaaaaaa");
        assert_eq!(state.retained_bytes, state.max_retained_bytes);
    }

    #[test]
    fn indexless_parallel_calls_with_distinct_ids_remain_distinct() {
        let mut state = StreamState::default();
        let mut first = tool_call_chunk("c1", Some("call_a"), 0, Some("fn_a"), Some("{}"));
        first.choices[0].delta.tool_calls.as_mut().unwrap()[0].index = None;
        let mut second = tool_call_chunk("c1", Some("call_b"), 0, Some("fn_b"), Some("{}"));
        second.choices[0].delta.tool_calls.as_mut().unwrap()[0].index = None;
        state.try_apply_chunk(&first).unwrap();
        state.try_apply_chunk(&second).unwrap();
        let finalized = state
            .finalize(&simple_registry(vec![
                (
                    "fn_a",
                    ToolKind::Function {
                        public_name: "fn_a".into(),
                        namespace: None,
                    },
                ),
                (
                    "fn_b",
                    ToolKind::Function {
                        public_name: "fn_b".into(),
                        namespace: None,
                    },
                ),
            ]))
            .unwrap();
        assert_eq!(finalized.tool_calls.len(), 2);
        assert_eq!(
            finalized.tool_calls[0].internal_call.id.as_deref(),
            Some("call_a")
        );
        assert_eq!(
            finalized.tool_calls[1].internal_call.id.as_deref(),
            Some("call_b")
        );
    }

    #[test]
    fn identity_free_parallel_fragment_is_rejected_as_ambiguous() {
        let mut state = StreamState::default();
        state
            .try_apply_chunk(&tool_call_chunk(
                "c1",
                Some("call_a"),
                0,
                Some("fn_a"),
                Some("{"),
            ))
            .unwrap();
        state
            .try_apply_chunk(&tool_call_chunk(
                "c1",
                Some("call_b"),
                1,
                Some("fn_b"),
                Some("{"),
            ))
            .unwrap();
        let mut fragment = tool_call_chunk("c1", None, 0, None, Some("}"));
        fragment.choices[0].delta.tool_calls.as_mut().unwrap()[0].index = None;
        let error = state.try_apply_chunk(&fragment).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("ambiguous upstream tool call fragment")
        );
    }

    #[test]
    fn reused_tool_index_with_different_ids_is_rejected() {
        let mut state = StreamState::default();
        state
            .try_apply_chunk(&tool_call_chunk(
                "c1",
                Some("call_a"),
                0,
                Some("fn_a"),
                Some("{}"),
            ))
            .unwrap();
        let error = state
            .try_apply_chunk(&tool_call_chunk(
                "c1",
                Some("call_b"),
                0,
                Some("fn_b"),
                Some("{}"),
            ))
            .unwrap_err();
        assert!(error.to_string().contains("reused tool call index"));
    }

    #[test]
    fn finalize_missing_tool_name() {
        let mut state = StreamState::default();
        state.apply_chunk(&tool_call_chunk("c1", Some("call_1"), 0, None, Some("{}")));
        let registry = simple_registry(vec![]);
        let result = state.finalize(&registry);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("missing function name")
        );
    }

    #[test]
    fn finalize_unknown_tool_is_rejected_not_errored() {
        // E1: an unoffered tool name no longer hard-errors (which aborted the SSE
        // stream); it is classified into `rejected_tool_calls`, kept OUT of the
        // executable `tool_calls`, and replayed into the assistant message so the
        // repair round's synthetic tool result lines up by call_id.
        let mut state = StreamState::default();
        state.apply_chunk(&tool_call_chunk(
            "c1",
            Some("call_1"),
            0,
            Some("Grep"),
            Some(r#"{"pattern":"foo"}"#),
        ));
        let registry = simple_registry(vec![]);
        let finalized = state
            .finalize(&registry)
            .expect("unknown tool must not error");
        assert!(finalized.tool_calls.is_empty());
        assert_eq!(finalized.rejected_tool_calls.len(), 1);
        let rejected = &finalized.rejected_tool_calls[0];
        assert_eq!(rejected.name, "Grep");
        assert_eq!(rejected.call_id, "call_1");
        assert_eq!(rejected.reason, ToolRejectionReason::UnknownTool);
        assert_eq!(rejected.raw_arguments, r#"{"pattern":"foo"}"#);
        // The attempted call is replayed into the assistant message verbatim.
        let internal = finalized
            .internal_assistant_message
            .expect("assistant message present for a lone rejected call");
        let tool_calls = internal.tool_calls.expect("rejected call replayed");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].function.name.as_deref(), Some("Grep"));
    }

    #[test]
    fn finalize_mixed_valid_and_unknown_splits_into_tool_calls_and_rejected() {
        // A batch with one offered tool and one hallucinated tool: the valid call
        // resolves into `tool_calls`, the unknown into `rejected_tool_calls`, and
        // BOTH are replayed into the assistant message (so the engine can supply a
        // synthetic result per call_id when it taints the batch).
        let mut state = StreamState::default();
        state.apply_chunk(&tool_call_chunk(
            "c1",
            Some("call_ok"),
            0,
            Some("echo"),
            Some(r#"{"value":"hi"}"#),
        ));
        state.apply_chunk(&tool_call_chunk(
            "c1",
            Some("call_bad"),
            1,
            Some("Grep"),
            Some(r#"{"pattern":"x"}"#),
        ));
        let registry = simple_registry(vec![(
            "echo",
            ToolKind::Function {
                public_name: "echo".to_string(),
                namespace: None,
            },
        )]);
        let finalized = state
            .finalize(&registry)
            .expect("mixed batch must not error");
        assert_eq!(finalized.tool_calls.len(), 1);
        assert_eq!(finalized.rejected_tool_calls.len(), 1);
        assert_eq!(finalized.rejected_tool_calls[0].name, "Grep");
        // The assistant message carries BOTH attempted calls.
        let tool_calls = finalized
            .internal_assistant_message
            .expect("assistant message present")
            .tool_calls
            .expect("both calls replayed");
        assert_eq!(tool_calls.len(), 2);
    }

    #[test]
    fn finalize_does_not_json_parse_unknown_tool_arguments() {
        // E1: unknown-tool args are NEVER JSON-parsed (the call was never
        // executable). Even syntactically-broken args must classify as rejected,
        // not surface a parse error, and the raw text is carried as-is (capped).
        let mut state = StreamState::default();
        state.apply_chunk(&tool_call_chunk(
            "c1",
            Some("call_1"),
            0,
            Some("Grep"),
            Some("not json at all {{{"),
        ));
        let registry = simple_registry(vec![]);
        let finalized = state
            .finalize(&registry)
            .expect("must not parse / not error");
        assert_eq!(finalized.rejected_tool_calls.len(), 1);
        assert_eq!(
            finalized.rejected_tool_calls[0].raw_arguments,
            "not json at all {{{"
        );
    }

    #[test]
    fn cap_str_truncates_on_char_boundary() {
        assert_eq!(cap_str("hello", 10), "hello");
        let capped = cap_str("abcdefghij", 4);
        assert!(capped.starts_with("abcd"));
        assert!(capped.contains("truncated"));
        // Multi-byte boundary: cap mid-char must not panic and stays valid UTF-8.
        let s = "héllo wörld"; // non-ASCII at byte 1
        let capped = cap_str(s, 2);
        assert!(capped.is_char_boundary(0));
    }

    #[test]
    fn finalize_empty_arguments() {
        let mut state = StreamState::default();
        let emissions = state.apply_chunk(&tool_call_chunk(
            "c1",
            Some("call_1"),
            0,
            Some("echo"),
            Some(""),
        ));
        assert!(emissions.is_empty());
        let registry = simple_registry(vec![(
            "echo",
            ToolKind::Function {
                public_name: "echo".to_string(),
                namespace: None,
            },
        )]);
        let finalized = state.finalize(&registry).unwrap();
        assert_eq!(finalized.tool_calls.len(), 1);
        assert_eq!(finalized.tool_calls[0].arguments, serde_json::json!({}));
    }

    #[test]
    fn finalize_invalid_json_arguments() {
        let mut state = StreamState::default();
        state.apply_chunk(&tool_call_chunk(
            "c1",
            Some("call_1"),
            0,
            Some("echo"),
            Some("not json{"),
        ));
        let registry = simple_registry(vec![(
            "echo",
            ToolKind::Function {
                public_name: "echo".to_string(),
                namespace: None,
            },
        )]);
        let result = state.finalize(&registry);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("failed to parse"));
    }

    #[test]
    fn finalize_invalid_local_shell_arguments() {
        let mut state = StreamState::default();
        state.apply_chunk(&tool_call_chunk(
            "c1",
            Some("call_1"),
            0,
            Some("local_shell"),
            Some(r#"{"bad":"schema"}"#),
        ));
        let registry = simple_registry(vec![("local_shell", ToolKind::LocalShell)]);
        let result = state.finalize(&registry);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("invalid upstream local_shell")
        );
    }

    #[test]
    fn finalize_custom_tool_input_extraction() {
        let mut state = StreamState::default();
        state.apply_chunk(&tool_call_chunk(
            "c1",
            Some("call_1"),
            0,
            Some("my_tool"),
            Some(r#"{"input":"code here"}"#),
        ));
        let registry = simple_registry(vec![(
            "my_tool",
            ToolKind::Custom {
                public_name: "my_tool".to_string(),
            },
        )]);
        let finalized = state.finalize(&registry).unwrap();
        assert_eq!(finalized.tool_calls.len(), 1);
        match &finalized.tool_calls[0].public_item {
            crate::models::responses::ResponseItem::CustomToolCall { input, .. } => {
                assert_eq!(input, "code here");
            }
            other => panic!("expected CustomToolCall, got {other:?}"),
        }
    }

    #[test]
    fn apply_chunk_emits_function_call_arguments_delta() {
        let mut state = StreamState::default();
        let emissions = state.apply_chunk(&tool_call_chunk(
            "c1",
            Some("call_1"),
            0,
            Some("echo"),
            Some(r#"{"val"#),
        ));
        let deltas: Vec<_> = emissions
            .iter()
            .filter_map(|e| match e {
                StreamEmission::FunctionCallArgumentsDelta { call_id, delta, .. } => {
                    Some((call_id.clone(), delta.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].0, "call_1");
        assert_eq!(deltas[0].1, r#"{"val"#);

        let emissions2 =
            state.apply_chunk(&tool_call_chunk("c1", None, 0, None, Some(r#"ue":"hi"}"#)));
        let deltas2: Vec<_> = emissions2
            .iter()
            .filter_map(|e| match e {
                StreamEmission::FunctionCallArgumentsDelta { call_id, delta, .. } => {
                    Some((call_id.clone(), delta.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(deltas2.len(), 1);
        assert_eq!(deltas2[0].1, r#"ue":"hi"}"#);
    }

    #[test]
    fn tool_call_arguments_before_id_are_emitted() {
        let mut state = StreamState::default();
        let emissions = state.apply_chunk(&tool_call_chunk(
            "c1",
            None,
            0,
            Some("echo"),
            Some(r#"{"value":"hi"}"#),
        ));
        let deltas: Vec<_> = emissions
            .iter()
            .filter_map(|e| match e {
                StreamEmission::FunctionCallArgumentsDelta { call_id, delta, .. } => {
                    Some((call_id.clone(), delta.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(deltas.len(), 1);
        assert!(deltas[0].0.starts_with("call_"));
        assert_eq!(deltas[0].1, r#"{"value":"hi"}"#);

        let registry = simple_registry(vec![(
            "echo",
            ToolKind::Function {
                public_name: "echo".to_string(),
                namespace: None,
            },
        )]);
        let finalized = state.finalize(&registry).unwrap();
        assert!(matches!(
            &finalized.tool_calls[0].public_item,
            ResponseItem::FunctionCall { call_id, .. } if call_id == &deltas[0].0
        ));
        assert_eq!(
            finalized.tool_calls[0].arguments,
            serde_json::json!({"value": "hi"})
        );
    }

    #[test]
    fn legacy_function_call_arguments_are_emitted() {
        let mut state = StreamState::default();
        state.apply_chunk(&legacy_function_call_chunk("c1", Some("echo"), None));
        let emissions =
            state.apply_chunk(&legacy_function_call_chunk("c1", None, Some(r#"{"val"#)));
        let deltas: Vec<_> = emissions
            .iter()
            .filter_map(|e| match e {
                StreamEmission::FunctionCallArgumentsDelta { call_id, delta, .. } => {
                    Some((call_id.clone(), delta.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(deltas.len(), 1);
        assert!(deltas[0].0.starts_with("call_"));
        assert_eq!(deltas[0].1, r#"{"val"#);

        let emissions2 = state.apply_chunk(&legacy_function_call_chunk(
            "c1",
            None,
            Some(r#"ue":"hi"}"#),
        ));
        let deltas2: Vec<_> = emissions2
            .iter()
            .filter_map(|e| match e {
                StreamEmission::FunctionCallArgumentsDelta { call_id, delta, .. } => {
                    Some((call_id.clone(), delta.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(deltas2.len(), 1);
        assert_eq!(deltas2[0].0, deltas[0].0);
        assert_eq!(deltas2[0].1, r#"ue":"hi"}"#);

        let registry = simple_registry(vec![(
            "echo",
            ToolKind::Function {
                public_name: "echo".to_string(),
                namespace: None,
            },
        )]);
        let finalized = state.finalize(&registry).unwrap();
        assert!(matches!(
            &finalized.tool_calls[0].public_item,
            ResponseItem::FunctionCall { call_id, .. } if call_id == &deltas[0].0
        ));
        assert_eq!(
            finalized.tool_calls[0].arguments,
            serde_json::json!({"value": "hi"})
        );
    }

    #[test]
    fn append_argument_fragment_non_string() {
        let mut buffer = String::new();
        append_argument_fragment(&mut buffer, &serde_json::json!(42));
        assert_eq!(buffer, "42");

        let mut buffer2 = String::new();
        append_argument_fragment(&mut buffer2, &serde_json::json!(true));
        assert_eq!(buffer2, "true");
    }

    #[test]
    fn test_content_part_added_before_first_delta() {
        let mut state = StreamState::default();
        let emissions = state.apply_chunk(&content_chunk("c1", "hello"));
        assert!(emissions.len() >= 3);
        assert!(matches!(&emissions[0], StreamEmission::OutputItemAdded(_)));
        assert!(matches!(
            &emissions[1],
            StreamEmission::ContentPartAdded { content_index: 0 }
        ));
        assert!(
            matches!(&emissions[2], StreamEmission::OutputTextDelta { delta, content_index: 0 } if delta == "hello")
        );
    }

    #[test]
    fn test_content_part_not_duplicated() {
        let mut state = StreamState::default();
        let e1 = state.apply_chunk(&content_chunk("c1", "hello"));
        let e2 = state.apply_chunk(&content_chunk("c1", " world"));
        let part_added_count = e1
            .iter()
            .chain(e2.iter())
            .filter(|e| matches!(e, StreamEmission::ContentPartAdded { .. }))
            .count();
        assert_eq!(part_added_count, 1);
    }

    #[test]
    fn test_safe_reasoning_summary_part_added_before_first_delta() {
        let mut state = StreamState::default();
        let mut chunk = reasoning_chunk("c1", "");
        chunk.choices[0].delta.extra.insert(
            "reasoning_summary".to_string(),
            serde_json::json!("brief summary"),
        );
        let emissions = state.apply_chunk(&chunk);
        assert!(emissions.len() >= 3);
        assert!(matches!(
            &emissions[0],
            StreamEmission::ReasoningItemAdded(_)
        ));
        assert!(matches!(
            &emissions[1],
            StreamEmission::ReasoningSummaryPartAdded
        ));
        assert!(
            matches!(&emissions[2], StreamEmission::ReasoningSummaryTextDelta(d) if d == "brief summary")
        );
    }

    #[test]
    fn test_reasoning_delta_alias_emitted() {
        let mut state = StreamState::default();
        let chunk = ChatCompletionChunk {
            service_tier: None,
            id: "c1".to_string(),
            choices: vec![ChatChunkChoice {
                index: 0,
                delta: ChatDelta {
                    content: None,
                    reasoning_content: None,
                    tool_calls: None,
                    function_call: None,
                    refusal: None,
                    extra: std::collections::BTreeMap::from([(
                        "reasoning".to_string(),
                        serde_json::json!("hidden step"),
                    )]),
                },
                finish_reason: None,
                stop_reason: None,
            }],
            usage: None,
        };

        let emissions = state.apply_chunk(&chunk);

        assert!(
            emissions
                .iter()
                .any(|emission| matches!(emission, StreamEmission::ReasoningTextDelta(delta) if delta == "hidden step"))
        );
    }

    #[test]
    fn nested_thinking_object_emits_reasoning_and_signature_delta() {
        let mut state = StreamState::default();
        let chunk = ChatCompletionChunk {
            service_tier: None,
            id: "c1".to_string(),
            choices: vec![ChatChunkChoice {
                index: 0,
                delta: ChatDelta {
                    content: None,
                    reasoning_content: None,
                    tool_calls: None,
                    function_call: None,
                    refusal: None,
                    extra: std::collections::BTreeMap::from([(
                        "thinking".to_string(),
                        serde_json::json!({
                            "content": "hidden step",
                            "signature": "sig_123"
                        }),
                    )]),
                },
                finish_reason: None,
                stop_reason: None,
            }],
            usage: None,
        };

        let emissions = state.apply_chunk(&chunk);

        assert!(
            emissions
                .iter()
                .any(|emission| matches!(emission, StreamEmission::ReasoningTextDelta(delta) if delta == "hidden step"))
        );
        assert!(
            emissions
                .iter()
                .any(|emission| matches!(emission, StreamEmission::ReasoningSignatureDelta(signature) if signature == "sig_123"))
        );
        let finalized = state.finalize(&simple_registry(vec![])).unwrap();
        assert!(matches!(
            finalized.reasoning_item,
            Some(ResponseItem::Reasoning {
                encrypted_content: Some(ref signature),
                ..
            }) if signature == "sig_123"
        ));
        let internal = finalized
            .internal_assistant_message
            .expect("internal assistant message");
        assert_eq!(internal.reasoning_content.as_deref(), Some("hidden step"));
        let thinking = internal.thinking.as_ref().expect("signed thinking");
        assert_eq!(thinking.content, "hidden step");
        assert_eq!(thinking.signature.as_deref(), Some("sig_123"));
    }

    #[test]
    fn signature_only_and_signature_first_chunks_allocate_reasoning_item() {
        let signature_chunk = ChatCompletionChunk {
            service_tier: None,
            id: "c1".to_string(),
            choices: vec![ChatChunkChoice {
                index: 0,
                delta: ChatDelta {
                    extra: std::collections::BTreeMap::from([(
                        "signature".to_string(),
                        serde_json::json!("sig_first"),
                    )]),
                    ..Default::default()
                },
                finish_reason: None,
                stop_reason: None,
            }],
            usage: None,
        };
        let mut signature_only = StreamState::default();
        let signature_emissions = signature_only.apply_chunk(&signature_chunk);
        assert!(matches!(
            &signature_emissions[..],
            [
                StreamEmission::ReasoningItemAdded(_),
                StreamEmission::ReasoningSignatureDelta(signature)
            ] if signature == "sig_first"
        ));
        let signature_only = signature_only.finalize(&simple_registry(vec![])).unwrap();
        assert!(matches!(
            signature_only.reasoning_item,
            Some(ResponseItem::Reasoning {
                content: None,
                encrypted_content: Some(ref signature),
                ..
            }) if signature == "sig_first"
        ));

        let mut state = StreamState::default();
        state.apply_chunk(&signature_chunk);
        let text_emissions = state.apply_chunk(&reasoning_chunk("c1", "later reasoning"));
        assert!(
            !text_emissions
                .iter()
                .any(|emission| matches!(emission, StreamEmission::ReasoningItemAdded(_)))
        );
        assert!(text_emissions.iter().any(
            |emission| matches!(emission, StreamEmission::ReasoningTextDelta(text) if text == "later reasoning")
        ));

        let finalized = state.finalize(&simple_registry(vec![])).unwrap();
        assert!(matches!(
            finalized.reasoning_item,
            Some(ResponseItem::Reasoning {
                content: Some(ref content),
                encrypted_content: Some(ref signature),
                ..
            }) if signature == "sig_first"
                && matches!(&content[..], [ReasoningContentItem::ReasoningText { text }] if text == "later reasoning")
        ));
    }

    #[test]
    fn test_refusal_delta_emitted() {
        let mut state = StreamState::default();
        let chunk = ChatCompletionChunk {
            service_tier: None,
            id: "c1".to_string(),
            choices: vec![ChatChunkChoice {
                index: 0,
                delta: ChatDelta {
                    content: None,
                    reasoning_content: None,
                    tool_calls: None,
                    function_call: None,
                    refusal: Some("I cannot help".to_string()),
                    extra: Default::default(),
                },
                finish_reason: None,
                stop_reason: None,
            }],
            usage: None,
        };
        let emissions = state.apply_chunk(&chunk);
        assert_eq!(emissions.len(), 3);
        assert!(matches!(&emissions[0], StreamEmission::OutputItemAdded(_)));
        assert!(matches!(
            &emissions[1],
            StreamEmission::RefusalPartAdded { content_index: 0 }
        ));
        assert!(matches!(
            &emissions[2],
            StreamEmission::RefusalDelta { delta, content_index: 0 } if delta == "I cannot help"
        ));
        assert_eq!(state.refusal_text, "I cannot help");
    }

    #[test]
    fn test_finish_reason_captured() {
        let mut state = StreamState::default();
        let chunk = ChatCompletionChunk {
            service_tier: None,
            id: "c1".to_string(),
            choices: vec![ChatChunkChoice {
                index: 0,
                delta: ChatDelta {
                    content: Some("hi".to_string()),
                    reasoning_content: None,
                    tool_calls: None,
                    function_call: None,
                    refusal: None,
                    extra: Default::default(),
                },
                finish_reason: Some("length".to_string()),
                stop_reason: None,
            }],
            usage: None,
        };
        state.apply_chunk(&chunk);
        let registry = simple_registry(vec![]);
        let finalized = state.finalize(&registry).unwrap();
        assert_eq!(finalized.finish_reason, Some("length".to_string()));
    }

    #[test]
    fn test_stop_sequence_captured_from_string_stop_reason() {
        let mut state = StreamState::default();
        let chunk = ChatCompletionChunk {
            service_tier: None,
            id: "c1".to_string(),
            choices: vec![ChatChunkChoice {
                index: 0,
                delta: ChatDelta {
                    content: Some("hi".to_string()),
                    reasoning_content: None,
                    tool_calls: None,
                    function_call: None,
                    refusal: None,
                    extra: Default::default(),
                },
                finish_reason: Some("stop".to_string()),
                stop_reason: Some(serde_json::json!("</block>")),
            }],
            usage: None,
        };
        state.apply_chunk(&chunk);
        let registry = simple_registry(vec![]);
        let finalized = state.finalize(&registry).unwrap();
        assert_eq!(finalized.stop_sequence, Some("</block>".to_string()));
    }

    #[test]
    fn test_integer_stop_reason_does_not_become_stop_sequence() {
        let mut state = StreamState::default();
        let chunk = ChatCompletionChunk {
            service_tier: None,
            id: "c1".to_string(),
            choices: vec![ChatChunkChoice {
                index: 0,
                delta: ChatDelta {
                    content: Some("hi".to_string()),
                    reasoning_content: None,
                    tool_calls: None,
                    function_call: None,
                    refusal: None,
                    extra: Default::default(),
                },
                finish_reason: Some("stop".to_string()),
                stop_reason: Some(serde_json::json!(163586)),
            }],
            usage: None,
        };
        state.apply_chunk(&chunk);
        let registry = simple_registry(vec![]);
        let finalized = state.finalize(&registry).unwrap();
        assert_eq!(finalized.stop_sequence, None);
    }

    #[test]
    fn extract_json_arguments_strips_kimi_tool_call_sentinel() {
        // Exact bytes observed from vLLM Kimi-K2.6 under forced tool_choice.
        let raw = " <|tool_calls_section_begin|> {\"query\":\"current weather Boppard Germany\"}";
        let cleaned = extract_json_arguments(raw).expect("known sentinel wrapper");
        let v: Value = serde_json::from_str(cleaned).expect("must parse after cleaning");
        assert_eq!(v["query"], "current weather Boppard Germany");
    }

    #[test]
    fn extract_json_arguments_handles_clean_and_padded_input() {
        assert_eq!(extract_json_arguments("{\"a\":1}"), Some("{\"a\":1}"));
        assert_eq!(extract_json_arguments("  {\"a\":1}  "), Some("{\"a\":1}"));
        // Trailing sentinel after the object is ignored too.
        assert_eq!(
            extract_json_arguments("{\"a\":1} <|tool_calls_section_end|>"),
            Some("{\"a\":1}")
        );
    }

    #[test]
    fn extract_json_arguments_is_string_aware() {
        // Braces inside string literals must not end the scan early.
        let raw = "<|tool_call_begin|> {\"q\":\"a{b}c \\\" }\"}";
        let cleaned = extract_json_arguments(raw).expect("known sentinel wrapper");
        let v: Value = serde_json::from_str(cleaned).expect("string-aware parse");
        assert_eq!(v["q"], "a{b}c \" }");
    }

    #[test]
    fn extract_json_arguments_supports_array_payloads() {
        let raw = "<|tool_call_begin|> [{\"a\":1},{\"b\":2}] <|tool_call_end|>";
        let cleaned = extract_json_arguments(raw).expect("known sentinel wrapper");
        let v: Value = serde_json::from_str(cleaned).expect("array parse");
        assert!(v.is_array() && v.as_array().unwrap().len() == 2);
    }

    #[test]
    fn extract_json_arguments_rejects_non_json_and_arbitrary_garbage() {
        assert_eq!(extract_json_arguments("  not json  "), None);
        assert_eq!(extract_json_arguments("junk {\"a\":1}"), None);
        assert_eq!(extract_json_arguments("{\"a\":1} junk"), None);
    }

    #[test]
    fn finalize_tolerates_kimi_sentinel_in_web_search_arguments() {
        // End-to-end through finalize(): the leaked sentinel must no longer
        // produce `failed to parse upstream tool arguments`.
        let mut state = StreamState::default();
        state.apply_chunk(&tool_call_chunk(
            "c1",
            Some("call_ws"),
            0,
            Some("web_search"),
            Some(" <|tool_calls_section_begin|> {\"query\":\"boppard weather\"}"),
        ));
        let registry = simple_registry(vec![("web_search", ToolKind::WebSearch)]);
        let finalized = state
            .finalize(&registry)
            .expect("kimi sentinel must be tolerated");
        let call = &finalized.tool_calls[0];
        match &call.public_item {
            ResponseItem::WebSearchCall {
                action: Some(WebSearchAction::Search { query, .. }),
                ..
            } => assert_eq!(query.as_deref(), Some("boppard weather")),
            other => panic!("expected web_search_call, got {other:?}"),
        }
    }
}
