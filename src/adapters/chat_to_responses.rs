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
    ContentPartAdded,
    ContentPartDone { text: String },
    ReasoningItemAdded(ResponseItem),
    ReasoningTextDelta(String),
    ReasoningSummaryPartAdded,
    ReasoningSummaryPartDone { text: String },
    FunctionCallArgumentsDelta { call_id: String, delta: String },
    RefusalDelta(String),
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
    pub content_part_emitted: bool,
    pub reasoning_part_emitted: bool,
    pub refusal_text: String,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Default)]
pub struct StreamState {
    message_id: Option<String>,
    reasoning_id: Option<String>,
    output_text: String,
    reasoning_text: String,
    tool_calls: BTreeMap<usize, ToolCallAccumulator>,
    content_part_emitted: bool,
    reasoning_part_emitted: bool,
    refusal_text: String,
    finish_reason: Option<String>,
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
                    self.reasoning_part_emitted = false;
                    emissions.push(StreamEmission::ReasoningItemAdded(item));
                }
                if !self.reasoning_part_emitted {
                    emissions.push(StreamEmission::ReasoningSummaryPartAdded);
                    self.reasoning_part_emitted = true;
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
                    self.content_part_emitted = false;
                    emissions.push(StreamEmission::OutputItemAdded(item));
                }
                if !self.content_part_emitted {
                    emissions.push(StreamEmission::ContentPartAdded);
                    self.content_part_emitted = true;
                }
                self.output_text.push_str(content_delta);
                emissions.push(StreamEmission::OutputTextDelta(content_delta.clone()));
            }
            if let Some(refusal) = &choice.delta.refusal {
                self.refusal_text.push_str(refusal);
                emissions.push(StreamEmission::RefusalDelta(refusal.clone()));
            }
            if let Some(reason) = &choice.finish_reason {
                self.finish_reason = Some(reason.clone());
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
                        let before_len = entry.arguments_text.len();
                        append_argument_fragment(&mut entry.arguments_text, arguments);
                        if let Some(call_id) = &entry.id {
                            let delta = entry.arguments_text[before_len..].to_string();
                            emissions.push(StreamEmission::FunctionCallArgumentsDelta {
                                call_id: call_id.clone(),
                                delta,
                            });
                        }
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
            content_part_emitted: self.content_part_emitted,
            reasoning_part_emitted: self.reasoning_part_emitted,
            refusal_text: self.refusal_text,
            finish_reason: self.finish_reason,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::responses_to_chat::{ToolKind, ToolRegistry};
    use crate::models::chat::*;
    fn content_chunk(id: &str, text: &str) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: id.to_string(),
            choices: vec![ChatChunkChoice {
                index: 0,
                delta: ChatDelta {
                    content: Some(text.to_string()),
                    reasoning_content: None,
                    tool_calls: None,
                    refusal: None,
                },
                finish_reason: None,
            }],
            usage: None,
        }
    }

    fn reasoning_chunk(id: &str, text: &str) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: id.to_string(),
            choices: vec![ChatChunkChoice {
                index: 0,
                delta: ChatDelta {
                    content: None,
                    reasoning_content: Some(text.to_string()),
                    tool_calls: None,
                    refusal: None,
                },
                finish_reason: None,
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
                            arguments: arguments
                                .map(|s| serde_json::Value::String(s.to_string())),
                        },
                    }]),
                    refusal: None,
                },
                finish_reason: None,
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
        assert!(matches!(&emissions[1], StreamEmission::ContentPartAdded));
        assert!(
            matches!(&emissions[2], StreamEmission::OutputTextDelta(d) if d == "hello")
        );
    }

    #[test]
    fn apply_chunk_interleaved_reasoning_and_content() {
        let mut state = StreamState::default();
        let e1 = state.apply_chunk(&reasoning_chunk("c1", "thinking"));
        assert_eq!(e1.len(), 3);
        assert!(matches!(&e1[0], StreamEmission::ReasoningItemAdded(_)));
        assert!(matches!(&e1[1], StreamEmission::ReasoningSummaryPartAdded));
        assert!(
            matches!(&e1[2], StreamEmission::ReasoningTextDelta(d) if d == "thinking")
        );
        let e2 = state.apply_chunk(&content_chunk("c1", "answer"));
        assert_eq!(e2.len(), 3);
        assert!(matches!(&e2[0], StreamEmission::OutputItemAdded(_)));
        assert!(matches!(&e2[1], StreamEmission::ContentPartAdded));
        assert!(
            matches!(&e2[2], StreamEmission::OutputTextDelta(d) if d == "answer")
        );
    }

    #[test]
    fn apply_chunk_multi_index_tool_calls() {
        let mut state = StreamState::default();
        state.apply_chunk(&tool_call_chunk("c1", Some("call_1"), 0, Some("fn_a"), Some("{}")));
        state.apply_chunk(&tool_call_chunk("c1", Some("call_2"), 1, Some("fn_b"), Some("{}")));
        let registry = simple_registry(vec![
            ("fn_a", ToolKind::Function { public_name: "fn_a".to_string(), namespace: None }),
            ("fn_b", ToolKind::Function { public_name: "fn_b".to_string(), namespace: None }),
        ]);
        let finalized = state.finalize(&registry).unwrap();
        assert_eq!(finalized.tool_calls.len(), 2);
    }

    #[test]
    fn finalize_missing_tool_name() {
        let mut state = StreamState::default();
        state.apply_chunk(&tool_call_chunk("c1", Some("call_1"), 0, None, Some("{}")));
        let registry = simple_registry(vec![]);
        let result = state.finalize(&registry);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing function name"));
    }

    #[test]
    fn finalize_unknown_tool() {
        let mut state = StreamState::default();
        state.apply_chunk(&tool_call_chunk(
            "c1",
            Some("call_1"),
            0,
            Some("unknown_fn"),
            Some("{}"),
        ));
        let registry = simple_registry(vec![]);
        let result = state.finalize(&registry);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown tool"));
    }

    #[test]
    fn finalize_empty_arguments() {
        let mut state = StreamState::default();
        state.apply_chunk(&tool_call_chunk("c1", Some("call_1"), 0, Some("echo"), Some("")));
        let registry = simple_registry(vec![(
            "echo",
            ToolKind::Function { public_name: "echo".to_string(), namespace: None },
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
            ToolKind::Function { public_name: "echo".to_string(), namespace: None },
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
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid upstream local_shell"));
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
            ToolKind::Custom { public_name: "my_tool".to_string() },
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
                StreamEmission::FunctionCallArgumentsDelta { call_id, delta } => {
                    Some((call_id.clone(), delta.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].0, "call_1");
        assert_eq!(deltas[0].1, r#"{"val"#);

        let emissions2 = state.apply_chunk(&tool_call_chunk("c1", None, 0, None, Some(r#"ue":"hi"}"#)));
        let deltas2: Vec<_> = emissions2
            .iter()
            .filter_map(|e| match e {
                StreamEmission::FunctionCallArgumentsDelta { call_id, delta } => {
                    Some((call_id.clone(), delta.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(deltas2.len(), 1);
        assert_eq!(deltas2[0].1, r#"ue":"hi"}"#);
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
        assert!(matches!(&emissions[1], StreamEmission::ContentPartAdded));
        assert!(matches!(&emissions[2], StreamEmission::OutputTextDelta(d) if d == "hello"));
    }

    #[test]
    fn test_content_part_not_duplicated() {
        let mut state = StreamState::default();
        let e1 = state.apply_chunk(&content_chunk("c1", "hello"));
        let e2 = state.apply_chunk(&content_chunk("c1", " world"));
        let part_added_count = e1
            .iter()
            .chain(e2.iter())
            .filter(|e| matches!(e, StreamEmission::ContentPartAdded))
            .count();
        assert_eq!(part_added_count, 1);
    }

    #[test]
    fn test_reasoning_part_added_before_first_delta() {
        let mut state = StreamState::default();
        let emissions = state.apply_chunk(&reasoning_chunk("c1", "thinking"));
        assert!(emissions.len() >= 3);
        assert!(matches!(&emissions[0], StreamEmission::ReasoningItemAdded(_)));
        assert!(matches!(&emissions[1], StreamEmission::ReasoningSummaryPartAdded));
        assert!(matches!(&emissions[2], StreamEmission::ReasoningTextDelta(d) if d == "thinking"));
    }

    #[test]
    fn test_refusal_delta_emitted() {
        let mut state = StreamState::default();
        let chunk = ChatCompletionChunk {
            id: "c1".to_string(),
            choices: vec![ChatChunkChoice {
                index: 0,
                delta: ChatDelta {
                    content: None,
                    reasoning_content: None,
                    tool_calls: None,
                    refusal: Some("I cannot help".to_string()),
                },
                finish_reason: None,
            }],
            usage: None,
        };
        let emissions = state.apply_chunk(&chunk);
        assert_eq!(emissions.len(), 1);
        assert!(matches!(&emissions[0], StreamEmission::RefusalDelta(t) if t == "I cannot help"));
        assert_eq!(state.refusal_text, "I cannot help");
    }

    #[test]
    fn test_finish_reason_captured() {
        let mut state = StreamState::default();
        let chunk = ChatCompletionChunk {
            id: "c1".to_string(),
            choices: vec![ChatChunkChoice {
                index: 0,
                delta: ChatDelta {
                    content: Some("hi".to_string()),
                    reasoning_content: None,
                    tool_calls: None,
                    refusal: None,
                },
                finish_reason: Some("length".to_string()),
            }],
            usage: None,
        };
        state.apply_chunk(&chunk);
        let registry = simple_registry(vec![]);
        let finalized = state.finalize(&registry).unwrap();
        assert_eq!(finalized.finish_reason, Some("length".to_string()));
    }
}
