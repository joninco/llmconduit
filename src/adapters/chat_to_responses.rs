use crate::adapters::responses_to_chat::ToolKind;
use crate::adapters::responses_to_chat::ToolRegistry;
use crate::adapters::responses_to_chat::tool_call_arguments_object;
use crate::error::AppError;
use crate::error::AppResult;
use crate::models::chat::ChatCompletionChunk;
use crate::models::chat::ChatMessage;
use crate::models::chat::ChatToolCall;
use crate::models::responses::ContentItem;
use crate::models::responses::LocalShellAction;
use crate::models::responses::LocalShellExecAction;
use crate::models::responses::ReasoningContentItem;
use crate::models::responses::ResponseItem;
use crate::models::responses::WebSearchAction;
use serde_json::Value;
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub enum StreamEmission {
    OutputItemAdded(ResponseItem),
    OutputTextDelta(String),
    ReasoningItemAdded(ResponseItem),
    ReasoningTextDelta(String),
}

#[derive(Debug, Clone)]
pub struct ResolvedToolCall {
    pub kind: ToolKind,
    pub arguments: Value,
    pub public_item: ResponseItem,
    pub internal_call: ChatToolCall,
}

#[derive(Debug, Clone)]
pub struct FinalizedAssistantTurn {
    pub message_item: Option<ResponseItem>,
    pub reasoning_item: Option<ResponseItem>,
    pub tool_calls: Vec<ResolvedToolCall>,
    pub internal_assistant_message: Option<ChatMessage>,
}

#[derive(Debug, Default)]
pub struct StreamState {
    message_id: Option<String>,
    reasoning_id: Option<String>,
    output_text: String,
    reasoning_text: String,
    tool_calls: BTreeMap<usize, ToolCallAccumulator>,
}

#[derive(Debug, Default, Clone)]
struct ToolCallAccumulator {
    id: Option<String>,
    name: Option<String>,
    arguments_text: String,
}

impl StreamState {
    pub fn apply_chunk(&mut self, chunk: &ChatCompletionChunk) -> Vec<StreamEmission> {
        let mut emissions = Vec::new();
        for choice in &chunk.choices {
            if let Some(reasoning_delta) = &choice.delta.reasoning_content {
                if self.reasoning_id.is_none() {
                    let item = ResponseItem::Reasoning {
                        id: new_item_id("rsn"),
                        summary: Vec::new(),
                        content: Some(Vec::new()),
                        encrypted_content: None,
                    };
                    self.reasoning_id = item_reasoning_id(&item);
                    emissions.push(StreamEmission::ReasoningItemAdded(item));
                }
                self.reasoning_text.push_str(reasoning_delta);
                emissions.push(StreamEmission::ReasoningTextDelta(reasoning_delta.clone()));
            }
            if let Some(content_delta) = &choice.delta.content {
                if self.message_id.is_none() {
                    let item = ResponseItem::Message {
                        id: Some(new_item_id("msg")),
                        role: "assistant".to_string(),
                        content: vec![ContentItem::OutputText {
                            text: String::new(),
                        }],
                        phase: None,
                    };
                    self.message_id = item_message_id(&item);
                    emissions.push(StreamEmission::OutputItemAdded(item));
                }
                self.output_text.push_str(content_delta);
                emissions.push(StreamEmission::OutputTextDelta(content_delta.clone()));
            }
            if let Some(tool_calls) = &choice.delta.tool_calls {
                for tool_call in tool_calls {
                    let index = tool_call.index.unwrap_or(0);
                    let entry = self.tool_calls.entry(index).or_default();
                    if let Some(id) = &tool_call.id {
                        entry.id = Some(id.clone());
                    }
                    if let Some(name) = &tool_call.function.name {
                        entry.name = Some(name.clone());
                    }
                    if let Some(arguments) = &tool_call.function.arguments {
                        append_argument_fragment(&mut entry.arguments_text, arguments);
                    }
                }
            }
        }
        emissions
    }

    pub fn finalize(self, registry: &ToolRegistry) -> AppResult<FinalizedAssistantTurn> {
        let message_item = if self.message_id.is_some() {
            Some(ResponseItem::Message {
                id: self.message_id,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: self.output_text.clone(),
                }],
                phase: None,
            })
        } else {
            None
        };
        let reasoning_item = if self.reasoning_id.is_some() {
            Some(ResponseItem::Reasoning {
                id: self.reasoning_id.unwrap_or_else(|| new_item_id("rsn")),
                summary: Vec::new(),
                content: Some(vec![ReasoningContentItem::ReasoningText {
                    text: self.reasoning_text.clone(),
                }]),
                encrypted_content: None,
            })
        } else {
            None
        };
        let mut resolved_tool_calls = Vec::new();
        let mut internal_tool_calls = Vec::new();
        for accumulator in self.tool_calls.into_values() {
            let name = accumulator.name.ok_or_else(|| {
                AppError::upstream("upstream tool call chunk missing function name")
            })?;
            let tool_kind = registry.get(&name).cloned().ok_or_else(|| {
                AppError::upstream(format!("unknown tool returned by upstream: {name}"))
            })?;
            let arguments = if accumulator.arguments_text.trim().is_empty() {
                Value::Object(Default::default())
            } else {
                serde_json::from_str(&accumulator.arguments_text).map_err(|err| {
                    AppError::upstream(format!(
                        "failed to parse upstream tool arguments for {name}: {err}"
                    ))
                })?
            };
            let call_id = accumulator
                .id
                .unwrap_or_else(|| format!("call_{}", Uuid::new_v4().simple()));
            let public_item = match &tool_kind {
                ToolKind::Function { public_name } => ResponseItem::FunctionCall {
                    id: None,
                    name: public_name.clone(),
                    namespace: None,
                    arguments: serde_json::to_string(&arguments).map_err(|err| {
                        AppError::internal(format!("failed to serialize function arguments: {err}"))
                    })?,
                    call_id: call_id.clone(),
                },
                ToolKind::Custom { public_name } => ResponseItem::CustomToolCall {
                    status: None,
                    call_id: call_id.clone(),
                    name: public_name.clone(),
                    input: arguments
                        .get("input")
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                        .unwrap_or_else(|| arguments.to_string()),
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
                    }),
                },
            };
            let internal_call = ChatToolCall {
                id: Some(call_id.clone()),
                index: Some(0),
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
                tool_calls: (!internal_tool_calls.is_empty()).then_some(internal_tool_calls),
            })
        } else {
            None
        };
        Ok(FinalizedAssistantTurn {
            message_item,
            reasoning_item,
            tool_calls: resolved_tool_calls,
            internal_assistant_message,
        })
    }
}

fn append_argument_fragment(buffer: &mut String, value: &Value) {
    match value {
        Value::String(fragment) => buffer.push_str(fragment),
        other => {
            buffer.push_str(&serde_json::to_string(other).unwrap_or_else(|_| "null".to_string()))
        }
    }
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
            ContentItem::InputImage { .. } => None,
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
