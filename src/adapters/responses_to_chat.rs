use crate::error::AppError;
use crate::error::AppResult;
use crate::models::chat::ChatFunctionCall;
use crate::models::chat::ChatMessage;
use crate::models::chat::ChatTool;
use crate::models::chat::ChatToolCall;
use crate::models::chat::ChatToolDefinition;
use crate::models::responses::ContentItem;
use crate::models::responses::LocalShellAction;
use crate::models::responses::ResponseItem;
use crate::models::responses::ResponsesRequest;
use crate::models::responses::ToolSpec;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum ToolKind {
    Function { public_name: String },
    Custom { public_name: String },
    LocalShell,
    ToolSearch,
    WebSearch,
}

#[derive(Debug, Clone)]
pub struct ToolRegistry {
    by_name: HashMap<String, ToolKind>,
}

impl ToolRegistry {
    pub fn get(&self, name: &str) -> Option<&ToolKind> {
        self.by_name.get(name)
    }
}

#[derive(Debug, Clone)]
pub struct LoweredTurn {
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ChatTool>,
    pub tool_registry: ToolRegistry,
    pub response_format: Option<Value>,
    pub reasoning_effort: Option<String>,
}

pub fn lower_request(
    request: &ResponsesRequest,
    baseline_messages: Vec<ChatMessage>,
) -> AppResult<LoweredTurn> {
    validate_request(request)?;
    let mut messages = baseline_messages;
    if messages.is_empty() && !request.instructions.is_empty() {
        messages.push(ChatMessage {
            role: "system".to_string(),
            content: Some(Value::String(request.instructions.clone())),
            tool_call_id: None,
            name: None,
            reasoning_content: None,
            tool_calls: None,
        });
    }
    let tools = lower_tools(&request.tools)?;
    let registry = build_tool_registry(&request.tools)?;
    let mut pending_reasoning: Option<String> = None;
    for item in &request.input {
        match item {
            ResponseItem::Message { role, content, .. } => {
                let text = message_content_to_chat_value(content)?;
                let reasoning_content = if role == "assistant" {
                    pending_reasoning.take()
                } else {
                    None
                };
                messages.push(ChatMessage {
                    role: role.clone(),
                    content: Some(text),
                    tool_call_id: None,
                    name: None,
                    reasoning_content,
                    tool_calls: None,
                });
            }
            ResponseItem::Reasoning {
                summary, content, ..
            } => {
                let text = reasoning_item_text(summary, content);
                pending_reasoning = match pending_reasoning.take() {
                    Some(existing) if !existing.is_empty() && !text.is_empty() => {
                        Some(format!("{existing}\n\n{text}"))
                    }
                    Some(existing) => Some(existing),
                    None => Some(text),
                };
            }
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => messages.push(assistant_tool_call_message(
                call_id.clone(),
                name.clone(),
                parse_json_string(arguments)?,
                pending_reasoning.take(),
            )),
            ResponseItem::CustomToolCall {
                call_id,
                name,
                input,
                ..
            } => messages.push(assistant_tool_call_message(
                call_id.clone(),
                name.clone(),
                json!({ "input": input }),
                pending_reasoning.take(),
            )),
            ResponseItem::ToolSearchCall {
                call_id,
                arguments,
                execution,
                ..
            } => {
                if execution != "client" {
                    return Err(AppError::bad_request(
                        "only tool_search calls with execution=client are supported",
                    ));
                }
                messages.push(assistant_tool_call_message(
                    call_id
                        .clone()
                        .unwrap_or_else(|| "tool_search_missing_call_id".to_string()),
                    "tool_search".to_string(),
                    arguments.clone(),
                    pending_reasoning.take(),
                ));
            }
            ResponseItem::LocalShellCall {
                call_id,
                id,
                action,
                ..
            } => {
                let call_id = call_id
                    .clone()
                    .or_else(|| id.clone())
                    .ok_or_else(|| AppError::bad_request("local_shell_call missing call_id"))?;
                let arguments = match action {
                    LocalShellAction::Exec(exec) => serde_json::to_value(exec).map_err(|err| {
                        AppError::bad_request(format!(
                            "failed to serialize local_shell_call action: {err}"
                        ))
                    })?,
                };
                messages.push(assistant_tool_call_message(
                    call_id,
                    "local_shell".to_string(),
                    arguments,
                    pending_reasoning.take(),
                ));
            }
            ResponseItem::FunctionCallOutput { call_id, output }
            | ResponseItem::CustomToolCallOutput {
                call_id, output, ..
            } => messages.push(ChatMessage {
                role: "tool".to_string(),
                content: Some(Value::String(stringify_tool_output(output))),
                tool_call_id: Some(call_id.clone()),
                name: None,
                reasoning_content: None,
                tool_calls: None,
            }),
            ResponseItem::ToolSearchOutput {
                call_id,
                status,
                execution,
                tools,
            } => messages.push(ChatMessage {
                role: "tool".to_string(),
                content: Some(Value::String(
                    serde_json::to_string(&json!({
                        "status": status,
                        "execution": execution,
                        "tools": tools,
                    }))
                    .map_err(|err| {
                        AppError::bad_request(format!(
                            "failed to serialize tool_search_output: {err}"
                        ))
                    })?,
                )),
                tool_call_id: call_id.clone(),
                name: None,
                reasoning_content: None,
                tool_calls: None,
            }),
            ResponseItem::WebSearchCall { id, action, .. } => {
                let call_id = id
                    .clone()
                    .unwrap_or_else(|| format!("web_search_missing_replay_{}", messages.len()));
                messages.push(assistant_tool_call_message(
                    call_id.clone(),
                    "web_search".to_string(),
                    web_search_arguments(action),
                    pending_reasoning.take(),
                ));
                messages.push(ChatMessage {
                    role: "tool".to_string(),
                    content: Some(Value::String(web_search_placeholder_result(action))),
                    tool_call_id: Some(call_id),
                    name: None,
                    reasoning_content: None,
                    tool_calls: None,
                });
            }
            ResponseItem::ImageGenerationCall { .. } => {
                return Err(AppError::bad_request(
                    "image_generation history is not supported in v1",
                ));
            }
        }
    }
    if let Some(reasoning) = pending_reasoning.take() {
        messages.push(ChatMessage {
            role: "assistant".to_string(),
            content: None,
            tool_call_id: None,
            name: None,
            reasoning_content: Some(reasoning),
            tool_calls: None,
        });
    }
    let response_format = request
        .text
        .as_ref()
        .and_then(|text| text.format.as_ref())
        .map(|format| {
            json!({
                "type": format.kind,
                "json_schema": {
                    "name": format.name,
                    "schema": format.schema,
                    "strict": format.strict,
                }
            })
        });
    let reasoning_effort = request
        .reasoning
        .as_ref()
        .and_then(|reasoning| reasoning.effort.clone());
    Ok(LoweredTurn {
        messages,
        tools,
        tool_registry: registry,
        response_format,
        reasoning_effort,
    })
}

fn validate_request(request: &ResponsesRequest) -> AppResult<()> {
    if !request.stream {
        return Err(AppError::bad_request("only stream=true is supported"));
    }
    if request.previous_response_id.is_some() {
        return Err(AppError::bad_request(
            "previous_response_id is not supported in v1",
        ));
    }
    if request.tool_choice != "auto" {
        return Err(AppError::bad_request(
            "only tool_choice=auto is supported in v1",
        ));
    }
    if request
        .tools
        .iter()
        .any(|tool| matches!(tool, ToolSpec::ImageGeneration { .. }))
    {
        return Err(AppError::bad_request(
            "image_generation is not supported in v1",
        ));
    }
    Ok(())
}

fn lower_tools(specs: &[ToolSpec]) -> AppResult<Vec<ChatTool>> {
    let mut tools = Vec::with_capacity(specs.len());
    let mut seen_names = HashMap::new();
    for spec in specs {
        let tool = match spec {
            ToolSpec::Function {
                name,
                description,
                strict,
                parameters,
            } => ChatTool {
                kind: "function".to_string(),
                function: ChatToolDefinition {
                    name: name.clone(),
                    description: description.clone(),
                    parameters: Some(parameters.clone()),
                    strict: *strict,
                },
            },
            ToolSpec::ToolSearch {
                description,
                parameters,
                ..
            } => ChatTool {
                kind: "function".to_string(),
                function: ChatToolDefinition {
                    name: "tool_search".to_string(),
                    description: description.clone(),
                    parameters: Some(parameters.clone()),
                    strict: false,
                },
            },
            ToolSpec::LocalShell {} => ChatTool {
                kind: "function".to_string(),
                function: ChatToolDefinition {
                    name: "local_shell".to_string(),
                    description: "Execute a shell command locally.".to_string(),
                    parameters: Some(json!({
                        "type": "object",
                        "properties": {
                            "command": {
                                "type": "array",
                                "items": { "type": "string" }
                            },
                            "timeout_ms": { "type": "integer" },
                            "working_directory": { "type": "string" },
                            "env": {
                                "type": "object",
                                "additionalProperties": { "type": "string" }
                            },
                            "user": { "type": "string" }
                        },
                        "required": ["command"]
                    })),
                    strict: false,
                },
            },
            ToolSpec::WebSearch { .. } => ChatTool {
                kind: "function".to_string(),
                function: ChatToolDefinition {
                    name: "web_search".to_string(),
                    description: "Search the web and return relevant result snippets.".to_string(),
                    parameters: Some(json!({
                        "type": "object",
                        "properties": {
                            "query": { "type": "string" }
                        },
                        "required": ["query"]
                    })),
                    strict: false,
                },
            },
            ToolSpec::Custom {
                name,
                description,
                format,
            } => ChatTool {
                kind: "function".to_string(),
                function: ChatToolDefinition {
                    name: name.clone(),
                    description: format!(
                        "{description}\n\nReturn the raw tool input as a string matching the {} {} format.",
                        format.kind, format.syntax
                    ),
                    parameters: Some(json!({
                        "type": "object",
                        "properties": {
                            "input": { "type": "string" }
                        },
                        "required": ["input"]
                    })),
                    strict: false,
                },
            },
            ToolSpec::ImageGeneration { .. } => continue,
        };
        let name = tool.function.name.clone();
        if seen_names.insert(name.clone(), ()).is_some() {
            return Err(AppError::bad_request(format!(
                "duplicate tool name is not supported: {name}"
            )));
        }
        tools.push(tool);
    }
    Ok(tools)
}

fn build_tool_registry(specs: &[ToolSpec]) -> AppResult<ToolRegistry> {
    let mut by_name = HashMap::new();
    for spec in specs {
        let (name, kind) = match spec {
            ToolSpec::Function { name, .. } => (
                name.clone(),
                ToolKind::Function {
                    public_name: name.clone(),
                },
            ),
            ToolSpec::ToolSearch { .. } => ("tool_search".to_string(), ToolKind::ToolSearch),
            ToolSpec::LocalShell {} => ("local_shell".to_string(), ToolKind::LocalShell),
            ToolSpec::WebSearch { .. } => ("web_search".to_string(), ToolKind::WebSearch),
            ToolSpec::Custom { name, .. } => (
                name.clone(),
                ToolKind::Custom {
                    public_name: name.clone(),
                },
            ),
            ToolSpec::ImageGeneration { .. } => continue,
        };
        if by_name.insert(name.clone(), kind).is_some() {
            return Err(AppError::bad_request(format!(
                "duplicate tool name is not supported: {name}"
            )));
        }
    }
    Ok(ToolRegistry { by_name })
}

fn assistant_tool_call_message(
    call_id: String,
    name: String,
    arguments: Value,
    reasoning_content: Option<String>,
) -> ChatMessage {
    ChatMessage {
        role: "assistant".to_string(),
        content: None,
        tool_call_id: None,
        name: None,
        reasoning_content,
        tool_calls: Some(vec![ChatToolCall {
            id: Some(call_id),
            index: Some(0),
            kind: "function".to_string(),
            function: ChatFunctionCall {
                name: Some(name),
                arguments: Some(arguments),
            },
        }]),
    }
}

fn parse_json_string(raw: &str) -> AppResult<Value> {
    serde_json::from_str(raw)
        .map_err(|err| AppError::bad_request(format!("invalid JSON tool arguments: {err}")))
}

fn web_search_arguments(action: &Option<crate::models::responses::WebSearchAction>) -> Value {
    match action {
        Some(crate::models::responses::WebSearchAction::Search { query, queries }) => {
            if let Some(query) = query {
                json!({ "query": query })
            } else if let Some(query) = queries.as_ref().and_then(|queries| queries.first()) {
                json!({ "query": query })
            } else {
                json!({})
            }
        }
        Some(crate::models::responses::WebSearchAction::OpenPage { url }) => {
            json!({ "url": url })
        }
        Some(crate::models::responses::WebSearchAction::FindInPage { url, pattern }) => {
            json!({ "url": url, "pattern": pattern })
        }
        Some(crate::models::responses::WebSearchAction::Other) | None => json!({}),
    }
}

fn web_search_placeholder_result(
    action: &Option<crate::models::responses::WebSearchAction>,
) -> String {
    match action {
        Some(crate::models::responses::WebSearchAction::Search { query, queries }) => {
            let query = query
                .clone()
                .or_else(|| queries.as_ref().and_then(|queries| queries.first().cloned()));
            match query {
                Some(query) => format!(
                    "Previous web_search completed in an earlier turn, but the original tool result is unavailable because replay state was missing. Query: {query}"
                ),
                None => "Previous web_search completed in an earlier turn, but the original tool result is unavailable because replay state was missing.".to_string(),
            }
        }
        Some(crate::models::responses::WebSearchAction::OpenPage { url }) => match url {
            Some(url) => format!(
                "Previous web_search open_page completed in an earlier turn, but the original tool result is unavailable because replay state was missing. URL: {url}"
            ),
            None => "Previous web_search open_page completed in an earlier turn, but the original tool result is unavailable because replay state was missing.".to_string(),
        },
        Some(crate::models::responses::WebSearchAction::FindInPage { url, pattern }) => {
            match (url, pattern) {
                (Some(url), Some(pattern)) => format!(
                    "Previous web_search find_in_page completed in an earlier turn, but the original tool result is unavailable because replay state was missing. URL: {url}. Pattern: {pattern}"
                ),
                (Some(url), None) => format!(
                    "Previous web_search find_in_page completed in an earlier turn, but the original tool result is unavailable because replay state was missing. URL: {url}"
                ),
                (None, Some(pattern)) => format!(
                    "Previous web_search find_in_page completed in an earlier turn, but the original tool result is unavailable because replay state was missing. Pattern: {pattern}"
                ),
                (None, None) => "Previous web_search find_in_page completed in an earlier turn, but the original tool result is unavailable because replay state was missing.".to_string(),
            }
        }
        Some(crate::models::responses::WebSearchAction::Other) | None => {
            "Previous web_search completed in an earlier turn, but the original tool result is unavailable because replay state was missing.".to_string()
        }
    }
}

fn reasoning_item_text(
    summary: &[crate::models::responses::ReasoningSummaryItem],
    content: &Option<Vec<crate::models::responses::ReasoningContentItem>>,
) -> String {
    let mut pieces = Vec::new();
    for entry in summary {
        let crate::models::responses::ReasoningSummaryItem::SummaryText { text } = entry;
        if !text.is_empty() {
            pieces.push(text.clone());
        }
    }
    if let Some(content) = content {
        for entry in content {
            match entry {
                crate::models::responses::ReasoningContentItem::ReasoningText { text }
                | crate::models::responses::ReasoningContentItem::Text { text }
                    if !text.is_empty() =>
                {
                    pieces.push(text.clone());
                }
                crate::models::responses::ReasoningContentItem::ReasoningText { .. }
                | crate::models::responses::ReasoningContentItem::Text { .. } => {}
            }
        }
    }
    pieces.join("\n")
}

fn message_content_to_chat_value(content: &[ContentItem]) -> AppResult<Value> {
    if content.is_empty() {
        return Ok(Value::String(String::new()));
    }
    if content.len() == 1 {
        return content_item_to_chat_value(&content[0]);
    }
    let mut parts = Vec::with_capacity(content.len());
    for item in content {
        let value = match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => json!({
                "type": "text",
                "text": text,
            }),
            ContentItem::InputImage { image_url } => json!({
                "type": "image_url",
                "image_url": { "url": image_url }
            }),
        };
        parts.push(value);
    }
    Ok(Value::Array(parts))
}

fn content_item_to_chat_value(item: &ContentItem) -> AppResult<Value> {
    match item {
        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
            Ok(Value::String(text.clone()))
        }
        ContentItem::InputImage { image_url } => Ok(json!([{
            "type": "image_url",
            "image_url": { "url": image_url }
        }])),
    }
}

pub fn stringify_tool_output(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        _ => serde_json::to_string(value).unwrap_or_else(|_| "null".to_string()),
    }
}

pub fn tool_call_arguments_object(arguments: &Option<Value>) -> Value {
    match arguments {
        Some(Value::Object(map)) => Value::Object(map.clone()),
        Some(other) => other.clone(),
        None => Value::Object(Map::new()),
    }
}
