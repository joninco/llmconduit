use crate::error::AppError;
use crate::error::AppResult;
use crate::models::anthropic::AnthropicContent;
use crate::models::anthropic::AnthropicContentBlock;
use crate::models::anthropic::AnthropicImageSource;
use crate::models::anthropic::AnthropicRequest;
use crate::models::anthropic::AnthropicSystemContent;
use crate::models::anthropic::AnthropicThinking;
use crate::models::anthropic::AnthropicTool;
use crate::models::responses::ContentItem;
use crate::models::responses::ReasoningContentItem;
use crate::models::responses::ReasoningRequest;
use crate::models::responses::ResponseItem;
use crate::models::responses::ResponsesRequest;
use crate::models::responses::TextControls;
use crate::models::responses::TextFormat;
use crate::models::responses::ToolSpec;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use uuid::Uuid;

pub fn convert_request(request: AnthropicRequest) -> AppResult<ResponsesRequest> {
    if request.top_k.is_some() {
        return Err(AppError::bad_request(
            "Anthropic top_k is not supported by this gateway",
        ));
    }
    if request
        .stop_sequences
        .as_ref()
        .is_some_and(|sequences| !sequences.is_empty())
    {
        return Err(AppError::bad_request(
            "Anthropic stop_sequences are not supported by this gateway",
        ));
    }
    let instructions = extract_system_text(&request.system);
    let input = convert_messages(&request.messages)?;
    let tools = convert_tools(&request.tools);
    let reasoning = convert_thinking(&request.thinking);
    let tool_choice = convert_tool_choice(request.tool_choice);
    let metadata = convert_metadata(request.metadata)?;
    let text = convert_output_config(request.output_config)?;
    let max_output_tokens = request.max_tokens.map(|value| {
        i64::try_from(value)
            .map_err(|_| AppError::bad_request("Anthropic max_tokens exceeds supported range"))
    });

    Ok(ResponsesRequest {
        model: request.model,
        instructions,
        input,
        tools,
        tool_choice,
        parallel_tool_calls: false,
        reasoning,
        store: false,
        stream: true,
        include: Vec::new(),
        service_tier: None,
        prompt_cache_key: None,
        text,
        client_metadata: None,
        previous_response_id: None,
        temperature: request.temperature,
        top_p: request.top_p,
        max_output_tokens: max_output_tokens.transpose()?,
        frequency_penalty: None,
        presence_penalty: None,
        truncation: None,
        metadata,
        extra_body: Default::default(),
    })
}

fn convert_output_config(output_config: Option<Value>) -> AppResult<Option<TextControls>> {
    let Some(output_config) = output_config else {
        return Ok(None);
    };
    let Value::Object(config) = output_config else {
        return Err(AppError::bad_request(
            "Anthropic output_config must be a JSON object",
        ));
    };
    let Some(format) = config.get("format") else {
        return Ok(None);
    };
    if format.is_null() {
        return Ok(None);
    }
    let Some(format) = format.as_object() else {
        return Err(AppError::bad_request(
            "Anthropic output_config.format must be a JSON object",
        ));
    };
    let kind = format
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::bad_request("Anthropic output_config.format is missing type"))?;
    if kind != "json_schema" {
        return Err(AppError::bad_request(format!(
            "Anthropic output_config.format type \"{kind}\" is not supported by this gateway"
        )));
    }
    let schema = format.get("schema").cloned().ok_or_else(|| {
        AppError::bad_request("Anthropic output_config.format json_schema is missing schema")
    })?;
    let name = format
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("response")
        .to_string();
    let strict = format
        .get("strict")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    Ok(Some(TextControls {
        verbosity: None,
        format: Some(TextFormat {
            kind: "json_schema".to_string(),
            strict,
            schema,
            name,
        }),
    }))
}

fn extract_system_text(system: &Option<AnthropicSystemContent>) -> String {
    match system {
        Some(AnthropicSystemContent::Text(text)) => strip_billing_nonce(text),
        Some(AnthropicSystemContent::Blocks(blocks)) => blocks
            .iter()
            .filter_map(|block| match block {
                crate::models::anthropic::AnthropicTextBlock::Text { text } => {
                    Some(strip_billing_nonce(text))
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        None => String::new(),
    }
}

fn convert_thinking(thinking: &Option<AnthropicThinking>) -> Option<ReasoningRequest> {
    thinking.as_ref().and_then(|t| match t {
        AnthropicThinking::Enabled { .. } => Some(ReasoningRequest {
            effort: Some("medium".to_string()),
            summary: None,
        }),
        AnthropicThinking::Disabled => None,
        AnthropicThinking::Adaptive { .. } => Some(ReasoningRequest {
            effort: None,
            summary: None,
        }),
    })
}

fn convert_messages(
    messages: &[crate::models::anthropic::AnthropicMessage],
) -> AppResult<Vec<ResponseItem>> {
    let mut items = Vec::new();
    for message in messages {
        convert_message(&message.role, &message.content, &mut items)?;
    }
    Ok(items)
}

fn convert_message(
    role: &str,
    content: &AnthropicContent,
    items: &mut Vec<ResponseItem>,
) -> AppResult<()> {
    match content {
        AnthropicContent::Text(text) => {
            let text = normalize_message_text(role, text);
            if !text.is_empty() {
                items.push(text_message_item(role, &text));
            }
        }
        AnthropicContent::Blocks(blocks) => {
            // Collect text/image content into a single message item,
            // then emit tool_use / tool_result items separately.
            let mut content_items = Vec::new();
            for block in blocks {
                match block {
                    AnthropicContentBlock::Text { text } => {
                        let text = normalize_message_text(role, text);
                        if role == "user" {
                            content_items.push(ContentItem::InputText { text });
                        } else {
                            content_items.push(ContentItem::OutputText { text });
                        }
                    }
                    AnthropicContentBlock::Image { source } => {
                        content_items.push(image_source_to_content_item(source)?);
                    }
                    _ => {}
                }
            }
            if !content_items.is_empty() {
                items.push(ResponseItem::Message {
                    id: None,
                    role: role.to_string(),
                    content: content_items,
                    phase: None,
                });
            }
            for block in blocks {
                match block {
                    AnthropicContentBlock::ToolUse { id, name, input } => {
                        let arguments = serde_json::to_string(input).map_err(|err| {
                            AppError::bad_request(format!(
                                "failed to serialize tool_use input: {err}"
                            ))
                        })?;
                        items.push(ResponseItem::FunctionCall {
                            id: None,
                            name: name.clone(),
                            namespace: None,
                            arguments,
                            call_id: id.clone(),
                        });
                    }
                    AnthropicContentBlock::ToolResult {
                        tool_use_id,
                        content: result_content,
                        ..
                    } => {
                        items.push(ResponseItem::FunctionCallOutput {
                            call_id: tool_use_id.clone(),
                            output: extract_tool_result_content(result_content),
                        });
                    }
                    AnthropicContentBlock::Thinking { thinking } => {
                        items.push(ResponseItem::Reasoning {
                            id: format!("rsn_{}", Uuid::new_v4().simple()),
                            summary: Vec::new(),
                            content: Some(vec![ReasoningContentItem::ReasoningText {
                                text: thinking.clone(),
                            }]),
                            encrypted_content: None,
                        });
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

fn convert_tool_choice(
    tool_choice: Option<crate::models::anthropic::AnthropicToolChoice>,
) -> Value {
    match tool_choice {
        Some(crate::models::anthropic::AnthropicToolChoice::Auto) | None => {
            Value::String("auto".to_string())
        }
        Some(crate::models::anthropic::AnthropicToolChoice::Any) => {
            Value::String("required".to_string())
        }
        Some(crate::models::anthropic::AnthropicToolChoice::None) => {
            Value::String("none".to_string())
        }
        Some(crate::models::anthropic::AnthropicToolChoice::Tool { name }) => {
            json!({"type": "function", "function": {"name": name}})
        }
    }
}

fn convert_metadata(metadata: Option<Value>) -> AppResult<Option<HashMap<String, Value>>> {
    match metadata {
        None => Ok(None),
        Some(Value::Object(map)) => Ok(Some(map.into_iter().collect())),
        Some(_) => Err(AppError::bad_request(
            "Anthropic metadata must be a JSON object",
        )),
    }
}

fn normalize_message_text(role: &str, text: &str) -> String {
    if role == "user" {
        strip_date_injection(text)
    } else {
        text.to_string()
    }
}

fn strip_billing_nonce(text: &str) -> String {
    join_filtered_lines(text, |line| {
        !line.starts_with("x-anthropic-billing-header:")
    })
}

fn strip_date_injection(text: &str) -> String {
    join_filtered_lines(text, |line| !is_date_injection_line(line))
}

fn join_filtered_lines(text: &str, keep: impl Fn(&str) -> bool) -> String {
    let trailing_newline = text.ends_with('\n');
    let lines: Vec<&str> = text.lines().filter(|line| keep(line)).collect();
    if lines.is_empty() {
        return String::new();
    }
    let mut normalized = lines.join("\n");
    if trailing_newline {
        normalized.push('\n');
    }
    normalized
}

fn is_date_injection_line(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("Today's date is ") else {
        return false;
    };
    let Some(date) = rest.strip_suffix('.') else {
        return false;
    };
    let bytes = date.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
}

fn text_message_item(role: &str, text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: role.to_string(),
        content: if role == "user" {
            vec![ContentItem::InputText {
                text: text.to_string(),
            }]
        } else {
            vec![ContentItem::OutputText {
                text: text.to_string(),
            }]
        },
        phase: None,
    }
}

fn image_source_to_content_item(source: &AnthropicImageSource) -> AppResult<ContentItem> {
    match source.kind.as_str() {
        "base64" => {
            let media_type = source.media_type.as_deref().unwrap_or("image/png");
            let data = source
                .data
                .as_deref()
                .filter(|data| !data.is_empty())
                .ok_or_else(|| {
                    AppError::bad_request("Anthropic base64 image source is missing data")
                })?;
            Ok(ContentItem::InputImage {
                image_url: Some(format!("data:{media_type};base64,{data}")),
                file_id: None,
                detail: None,
            })
        }
        "url" => {
            let image_url = source
                .url
                .clone()
                .filter(|url| !url.is_empty())
                .ok_or_else(|| AppError::bad_request("Anthropic image source is missing url"))?;
            Ok(ContentItem::InputImage {
                image_url: Some(image_url),
                file_id: None,
                detail: None,
            })
        }
        "file" => {
            let file_id = source
                .file_id
                .clone()
                .filter(|file_id| !file_id.is_empty())
                .ok_or_else(|| {
                    AppError::bad_request("Anthropic file image source is missing file_id")
                })?;
            Ok(ContentItem::InputImage {
                image_url: None,
                file_id: Some(file_id),
                detail: None,
            })
        }
        other => Err(AppError::bad_request(format!(
            "unsupported Anthropic image source type \"{other}\""
        ))),
    }
}

fn extract_tool_result_content(content: &Option<AnthropicContent>) -> Value {
    match content {
        Some(AnthropicContent::Text(text)) => Value::String(text.clone()),
        Some(AnthropicContent::Blocks(blocks)) => {
            let text_parts: Vec<String> = blocks
                .iter()
                .filter_map(|block| match block {
                    AnthropicContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect();
            if text_parts.len() == 1 {
                Value::String(text_parts.into_iter().next().unwrap())
            } else if text_parts.is_empty() {
                Value::Null
            } else {
                Value::String(text_parts.join("\n"))
            }
        }
        None => Value::Null,
    }
}

fn convert_tools(tools: &Option<Vec<AnthropicTool>>) -> Vec<ToolSpec> {
    match tools {
        Some(tools) => {
            let mut sorted_tools = tools.clone();
            sorted_tools.sort_by(|a, b| a.name.cmp(&b.name));
            sorted_tools
                .into_iter()
                // The Anthropic `web_search` server tool (type
                // `web_search_20250305`, which Claude Code always sends and
                // forces via `tool_choice`) must run server-side via Brave, so
                // it maps to `ToolSpec::WebSearch` rather than a client
                // `Function`. The `type` field is dropped at deserialization,
                // so the tool name is the only signal available here.
                .map(|tool| match tool.name.as_str() {
                    "web_search" => ToolSpec::WebSearch {
                        external_web_access: None,
                        filters: None,
                        user_location: None,
                        search_context_size: None,
                        search_content_types: None,
                    },
                    _ => ToolSpec::Function {
                        name: tool.name,
                        description: tool.description.unwrap_or_default(),
                        strict: false,
                        parameters: tool.input_schema,
                    },
                })
                .collect()
        }
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::anthropic::*;
    use crate::models::responses::ResponseItem;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn converts_simple_text_request() {
        let request = AnthropicRequest {
            model: "claude-3-5-sonnet-20241022".to_string(),
            max_tokens: Some(1024),
            system: Some(AnthropicSystemContent::Text(
                "x-anthropic-billing-header: nonce\nBe helpful".to_string(),
            )),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicContent::Text("Today's date is 2026-04-21.\nHello".to_string()),
            }],
            tools: None,
            tool_choice: None,
            stream: true,
            temperature: Some(0.2),
            top_p: Some(0.7),
            top_k: None,
            stop_sequences: None,
            metadata: Some(json!({"source": "anthropic"})),
            thinking: None,
            output_config: None,
        };

        let result = convert_request(request).expect("convert");
        assert_eq!(result.model, "claude-3-5-sonnet-20241022");
        assert_eq!(result.instructions, "Be helpful");
        assert_eq!(result.max_output_tokens, Some(1024));
        assert_eq!(result.temperature, Some(0.2));
        assert_eq!(result.top_p, Some(0.7));
        assert_eq!(
            result
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("source")),
            Some(&json!("anthropic"))
        );
        assert_eq!(result.input.len(), 1);
        assert!(matches!(
            &result.input[0],
            ResponseItem::Message { role, content, .. }
                if role == "user"
                    && matches!(&content[0], ContentItem::InputText { text } if text == "Hello")
        ));
        assert_eq!(result.stream, true);
    }

    #[test]
    fn converts_output_config_json_schema_to_text_format() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "title": { "type": "string" }
            },
            "required": ["title"]
        });
        let request = AnthropicRequest {
            model: "claude-haiku-4-5-20251001".to_string(),
            max_tokens: Some(32000),
            system: Some(AnthropicSystemContent::Blocks(vec![
                AnthropicTextBlock::Text {
                    text: "Return JSON with a single \"title\" field.".to_string(),
                },
            ])),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicContent::Text("Build a web app.".to_string()),
            }],
            tools: Some(Vec::new()),
            tool_choice: None,
            stream: true,
            temperature: Some(1.0),
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
            thinking: None,
            output_config: Some(json!({
                "format": {
                    "type": "json_schema",
                    "schema": schema
                }
            })),
        };

        let result = convert_request(request).expect("convert");
        let format = result
            .text
            .as_ref()
            .and_then(|text| text.format.as_ref())
            .expect("text format");
        assert_eq!(format.kind, "json_schema");
        assert_eq!(format.name, "response");
        assert!(format.strict);
        assert_eq!(format.schema, schema);
    }

    #[test]
    fn converts_tool_use_and_result_history() {
        let request = AnthropicRequest {
            model: "claude-3-5-sonnet-20241022".to_string(),
            max_tokens: Some(1024),
            system: None,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: AnthropicContent::Text("What is the weather?".to_string()),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: AnthropicContent::Blocks(vec![
                        AnthropicContentBlock::Text {
                            text: "Let me check.".to_string(),
                        },
                        AnthropicContentBlock::ToolUse {
                            id: "toolu_1".to_string(),
                            name: "get_weather".to_string(),
                            input: json!({"location": "Seattle"}),
                        },
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: AnthropicContent::Blocks(vec![AnthropicContentBlock::ToolResult {
                        tool_use_id: "toolu_1".to_string(),
                        content: Some(AnthropicContent::Text("72°F sunny".to_string())),
                        is_error: None,
                    }]),
                },
            ],
            tools: None,
            tool_choice: None,
            stream: true,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
            thinking: None,
            output_config: None,
        };

        let result = convert_request(request).expect("convert");
        // user text, assistant text, function_call, function_call_output
        assert_eq!(result.input.len(), 4);

        let user_msg = &result.input[0];
        assert!(matches!(user_msg, ResponseItem::Message { role, .. } if role == "user"));

        let asst_text = &result.input[1];
        assert!(matches!(asst_text, ResponseItem::Message { role, .. } if role == "assistant"));

        let fn_call = &result.input[2];
        assert!(
            matches!(fn_call, ResponseItem::FunctionCall { name, call_id, .. }
            if name == "get_weather" && call_id == "toolu_1")
        );

        let fn_output = &result.input[3];
        assert!(
            matches!(fn_output, ResponseItem::FunctionCallOutput { call_id, .. } if call_id == "toolu_1")
        );
    }

    #[test]
    fn converts_tools_to_function_specs() {
        let request = AnthropicRequest {
            model: "claude-3-5-sonnet-20241022".to_string(),
            max_tokens: Some(1024),
            system: None,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicContent::Text("Hi".to_string()),
            }],
            tools: Some(vec![
                AnthropicTool {
                    name: "zulu".to_string(),
                    description: Some("Z".to_string()),
                    input_schema: json!({"type": "object"}),
                },
                AnthropicTool {
                    name: "alpha".to_string(),
                    description: Some("A".to_string()),
                    input_schema: json!({
                        "type": "object",
                        "properties": { "location": { "type": "string" } },
                        "required": ["location"]
                    }),
                },
            ]),
            tool_choice: None,
            stream: true,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
            thinking: None,
            output_config: None,
        };

        let result = convert_request(request).expect("convert");
        assert_eq!(result.tools.len(), 2);
        assert!(matches!(&result.tools[0], ToolSpec::Function { name, .. } if name == "alpha"));
    }

    #[test]
    fn converts_thinking_enabled_to_reasoning() {
        let request = AnthropicRequest {
            model: "claude-3-5-sonnet-20241022".to_string(),
            max_tokens: Some(16000),
            system: None,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicContent::Text("Think".to_string()),
            }],
            tools: None,
            tool_choice: None,
            stream: true,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
            thinking: Some(AnthropicThinking::Enabled {
                budget_tokens: Some(10000),
            }),
            output_config: None,
        };

        let result = convert_request(request).expect("convert");
        assert!(result.reasoning.is_some());
        assert_eq!(
            result.reasoning.as_ref().unwrap().effort.as_deref(),
            Some("medium")
        );
    }

    #[test]
    fn accepts_non_streaming_request() {
        let request = AnthropicRequest {
            model: "claude-3-5-sonnet-20241022".to_string(),
            max_tokens: Some(1024),
            system: None,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicContent::Text("Hello".to_string()),
            }],
            tools: None,
            tool_choice: None,
            stream: false,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
            thinking: None,
            output_config: None,
        };

        let result = convert_request(request);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().stream, true);
    }

    #[test]
    fn converts_anthropic_tool_choice_variants() {
        let request = AnthropicRequest {
            model: "claude-3-5-sonnet-20241022".to_string(),
            max_tokens: Some(1024),
            system: None,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicContent::Text("Hello".to_string()),
            }],
            tools: Some(vec![AnthropicTool {
                name: "echo".to_string(),
                description: Some("Echo".to_string()),
                input_schema: json!({"type": "object"}),
            }]),
            tool_choice: Some(AnthropicToolChoice::Tool {
                name: "echo".to_string(),
            }),
            stream: true,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
            thinking: None,
            output_config: None,
        };

        let tool_specific = convert_request(request).expect("convert");
        assert_eq!(
            tool_specific.tool_choice,
            json!({"type": "function", "function": {"name": "echo"}})
        );

        assert_eq!(
            convert_tool_choice(Some(AnthropicToolChoice::Any)),
            Value::String("required".to_string())
        );
        assert_eq!(
            convert_tool_choice(Some(AnthropicToolChoice::None)),
            Value::String("none".to_string())
        );
    }

    #[test]
    fn rejects_unsupported_anthropic_fields() {
        let request = AnthropicRequest {
            model: "claude-3-5-sonnet-20241022".to_string(),
            max_tokens: Some(1024),
            system: None,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicContent::Text("Hello".to_string()),
            }],
            tools: None,
            tool_choice: None,
            stream: true,
            temperature: None,
            top_p: None,
            top_k: Some(5),
            stop_sequences: None,
            metadata: None,
            thinking: None,
            output_config: None,
        };
        assert!(convert_request(request).is_err());

        let request = AnthropicRequest {
            model: "claude-3-5-sonnet-20241022".to_string(),
            max_tokens: Some(1024),
            system: None,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: AnthropicContent::Text("Hello".to_string()),
            }],
            tools: None,
            tool_choice: None,
            stream: true,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: Some(vec!["stop".to_string()]),
            metadata: None,
            thinking: None,
            output_config: None,
        };
        assert!(convert_request(request).is_err());
    }

    #[test]
    fn converts_base64_image_to_input_image() {
        let source = AnthropicImageSource {
            kind: "base64".to_string(),
            media_type: Some("image/png".to_string()),
            data: Some("iVBORw0KGgo=".to_string()),
            url: None,
            file_id: None,
        };
        let item = image_source_to_content_item(&source).expect("image item");
        assert!(matches!(
            item,
            ContentItem::InputImage {
                image_url: Some(ref image_url),
                file_id: None,
                ..
            } if image_url == "data:image/png;base64,iVBORw0KGgo="
        ));
    }

    #[test]
    fn converts_url_image_source() {
        let source = AnthropicImageSource {
            kind: "url".to_string(),
            media_type: None,
            data: None,
            url: Some("https://example.com/img.png".to_string()),
            file_id: None,
        };
        let item = image_source_to_content_item(&source).expect("image item");
        assert!(matches!(
            item,
            ContentItem::InputImage {
                image_url: Some(ref image_url),
                file_id: None,
                ..
            } if image_url == "https://example.com/img.png"
        ));
    }

    #[test]
    fn converts_file_image_source_to_file_id() {
        let source = AnthropicImageSource {
            kind: "file".to_string(),
            media_type: None,
            data: None,
            url: None,
            file_id: Some("file_123".to_string()),
        };
        let item = image_source_to_content_item(&source).expect("image item");
        assert!(matches!(
            item,
            ContentItem::InputImage {
                image_url: None,
                file_id: Some(ref file_id),
                ..
            } if file_id == "file_123"
        ));
    }
}
