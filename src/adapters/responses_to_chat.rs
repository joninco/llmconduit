use crate::config::{Action, RolesConfig, When};
use crate::error::AppError;
use crate::error::AppResult;
use crate::models::chat::ChatFunctionCall;
use crate::models::chat::ChatMessage;
use crate::models::chat::ChatThinking;
use crate::models::chat::ChatTool;
use crate::models::chat::ChatToolCall;
use crate::models::chat::ChatToolDefinition;
use crate::models::responses::ContentItem;
use crate::models::responses::CustomToolFormat;
use crate::models::responses::FunctionCallOutputContent;
use crate::models::responses::LocalShellAction;
use crate::models::responses::NamespaceToolSpec;
use crate::models::responses::ResponseItem;
use crate::models::responses::ResponsesRequest;
use crate::models::responses::ToolSpec;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum ToolKind {
    Function {
        public_name: String,
        namespace: Option<String>,
    },
    Custom {
        public_name: String,
    },
    LocalShell,
    ToolSearch,
    WebSearch,
    /// G4 server-side image analysis (`analyzeImage`). Registered ONLY when the
    /// image agent is active for the turn (see `lower_request`'s
    /// `image_agent_active`), so a client-supplied `analyzeImage` tool on a
    /// non-image turn still classifies as a normal client `Function`.
    ImageAnalysis,
}

#[derive(Debug, Clone)]
pub struct ToolRegistry {
    by_name: HashMap<String, ToolKind>,
    strict_function_schemas: HashMap<String, Value>,
}

impl ToolRegistry {
    pub fn get(&self, name: &str) -> Option<&ToolKind> {
        self.by_name.get(name)
    }

    pub(crate) fn has_active_server_tool(
        &self,
        web_search_active: bool,
        image_agent_active: bool,
    ) -> bool {
        self.by_name.values().any(|kind| match kind {
            ToolKind::WebSearch => web_search_active,
            ToolKind::ImageAnalysis => image_agent_active,
            _ => false,
        })
    }

    pub(crate) fn validate_function_arguments(
        &self,
        name: &str,
        arguments: &Value,
    ) -> AppResult<()> {
        let Some(schema) = self.strict_function_schemas.get(&name.to_ascii_lowercase()) else {
            return Ok(());
        };
        validate_json_schema_value(schema, arguments, "$", 0).map_err(|detail| {
            AppError::upstream(format!(
                "upstream arguments for function {name} do not match its strict schema: {detail}"
            ))
            .with_code("invalid_tool_call")
        })
    }
}

#[cfg(test)]
impl ToolRegistry {
    pub fn from_map(by_name: HashMap<String, ToolKind>) -> Self {
        Self {
            by_name,
            strict_function_schemas: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoweredTurn {
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ChatTool>,
    pub tool_registry: ToolRegistry,
    pub response_format: Option<Value>,
    pub reasoning_effort: Option<String>,
    pub frequency_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
}

#[derive(Debug, Clone)]
struct PendingReasoning {
    text: String,
    signature: Option<String>,
}

impl PendingReasoning {
    fn from_parts(text: String, signature: Option<String>) -> Self {
        Self { text, signature }
    }

    fn append(&mut self, text: String, signature: Option<String>) {
        if !self.text.is_empty() && !text.is_empty() {
            self.text.push_str("\n\n");
            self.text.push_str(&text);
        } else if self.text.is_empty() {
            self.text = text;
        }
        if self.signature.is_none() {
            self.signature = signature;
        }
    }

    fn into_chat_parts(self) -> (Option<String>, Option<ChatThinking>) {
        let thinking = self.signature.clone().map(|signature| ChatThinking {
            content: self.text.clone(),
            signature: Some(signature),
        });
        (Some(self.text), thinking)
    }
}

pub fn lower_request(
    request: &ResponsesRequest,
    baseline_messages: Vec<ChatMessage>,
) -> AppResult<LoweredTurn> {
    lower_request_with_image_agent_and_roles(request, baseline_messages, false, None)
}

/// Like [`lower_request`], but `image_agent_active` decides whether an
/// `analyzeImage` tool classifies as the server-side [`ToolKind::ImageAnalysis`]
/// (true, the gateway runs it) or a plain client `Function` (false). The engine
/// passes `true` only on turns where G4 gating activated the image agent, so a
/// caller that happens to define its own `analyzeImage` tool on a text turn is
/// unaffected.
pub fn lower_request_with_image_agent(
    request: &ResponsesRequest,
    baseline_messages: Vec<ChatMessage>,
    image_agent_active: bool,
) -> AppResult<LoweredTurn> {
    lower_request_with_image_agent_and_roles(request, baseline_messages, image_agent_active, None)
}

/// Lower a turn while applying an optional model-profile role policy. Role
/// rules apply only to the new tail, never replayed baseline messages, so tags
/// and rewrites are not applied twice on follow-up turns.
pub fn lower_request_with_image_agent_and_roles(
    request: &ResponsesRequest,
    baseline_messages: Vec<ChatMessage>,
    image_agent_active: bool,
    roles: Option<&RolesConfig>,
) -> AppResult<LoweredTurn> {
    validate_request(request)?;
    let mut messages = baseline_messages;
    let baseline_len = messages.len();
    if messages.is_empty()
        && let Some(instructions) = request.instructions.text()
        && !instructions.is_empty()
    {
        messages.push(ChatMessage {
            role: "system".to_string(),
            content: Some(Value::String(instructions.to_string())),
            tool_call_id: None,
            name: None,
            reasoning_content: None,
            thinking: None,
            tool_calls: None,
        });
    }
    let tools = lower_tools(&request.tools)?;
    let registry = build_tool_registry(&request.tools, image_agent_active)?;
    let mut pending_reasoning: Option<PendingReasoning> = None;
    let instruction_items = if messages.is_empty() {
        request.instructions.items().unwrap_or_default()
    } else {
        &[]
    };
    for item in instruction_items.iter().chain(request.input.iter()) {
        match item {
            ResponseItem::ItemReference { .. } => {
                return Err(AppError::internal(
                    "unresolved item_reference reached upstream lowering",
                ));
            }
            ResponseItem::Message { role, content, .. } => {
                let text = message_content_to_chat_value(content)?;
                // Responses `developer` is a first-class role. Keep it intact
                // through canonical lowering; a model profile may still opt
                // into an explicit role rewrite through `roles` below.
                let normalized_role = role.to_string();
                let (reasoning_content, thinking) = if normalized_role == "assistant" {
                    pending_reasoning
                        .take()
                        .map(PendingReasoning::into_chat_parts)
                        .unwrap_or((None, None))
                } else {
                    (None, None)
                };
                messages.push(ChatMessage {
                    role: normalized_role,
                    content: Some(text),
                    tool_call_id: None,
                    name: None,
                    reasoning_content,
                    thinking,
                    tool_calls: None,
                });
            }
            ResponseItem::Reasoning {
                summary,
                content,
                encrypted_content,
                ..
            } => {
                let text = reasoning_item_text(summary, content);
                let signature = encrypted_content
                    .as_ref()
                    .filter(|signature| !signature.is_empty())
                    .cloned();
                if let Some(existing) = pending_reasoning.as_mut() {
                    existing.append(text, signature);
                } else {
                    pending_reasoning = Some(PendingReasoning::from_parts(text, signature));
                }
            }
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => append_tool_call(
                &mut messages,
                call_id.clone(),
                name.clone(),
                parse_json_string(arguments)?,
                pending_reasoning.take(),
            ),
            ResponseItem::CustomToolCall {
                call_id,
                name,
                input,
                ..
            } => append_tool_call(
                &mut messages,
                call_id.clone(),
                name.clone(),
                json!({ "input": input }),
                pending_reasoning.take(),
            ),
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
                append_tool_call(
                    &mut messages,
                    call_id
                        .clone()
                        .unwrap_or_else(|| "tool_search_missing_call_id".to_string()),
                    "tool_search".to_string(),
                    arguments.clone(),
                    pending_reasoning.take(),
                );
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
                append_tool_call(
                    &mut messages,
                    call_id,
                    "local_shell".to_string(),
                    arguments,
                    pending_reasoning.take(),
                );
            }
            ResponseItem::FunctionCallOutput { call_id, output } => messages.push(ChatMessage {
                role: "tool".to_string(),
                content: Some(function_call_output_to_chat_value(output)?),
                tool_call_id: Some(call_id.clone()),
                name: None,
                reasoning_content: None,
                thinking: None,
                tool_calls: None,
            }),
            ResponseItem::CustomToolCallOutput {
                call_id, output, ..
            } => messages.push(ChatMessage {
                role: "tool".to_string(),
                content: Some(function_call_output_to_chat_value(output)?),
                tool_call_id: Some(call_id.clone()),
                name: None,
                reasoning_content: None,
                thinking: None,
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
                thinking: None,
                tool_calls: None,
            }),
            ResponseItem::WebSearchCall { id, action, .. } => {
                let call_id = id
                    .clone()
                    .unwrap_or_else(|| format!("web_search_missing_replay_{}", messages.len()));
                append_tool_call(
                    &mut messages,
                    call_id.clone(),
                    "web_search".to_string(),
                    web_search_arguments(action),
                    pending_reasoning.take(),
                );
                messages.push(ChatMessage {
                    role: "tool".to_string(),
                    content: Some(Value::String(web_search_placeholder_result(action))),
                    tool_call_id: Some(call_id),
                    name: None,
                    reasoning_content: None,
                    thinking: None,
                    tool_calls: None,
                });
            }
            ResponseItem::ImageGenerationCall { .. } => {}
        }
    }
    if let Some(reasoning) = pending_reasoning.take() {
        let (reasoning_content, thinking) = reasoning.into_chat_parts();
        messages.push(ChatMessage {
            role: "assistant".to_string(),
            content: None,
            tool_call_id: None,
            name: None,
            reasoning_content,
            thinking,
            tool_calls: None,
        });
    }
    if roles.is_some() {
        let mut new_messages = messages.split_off(baseline_len);
        apply_role_rules(&mut new_messages, roles)?;
        messages.append(&mut new_messages);
    } else {
        // Preserve the fork's historical compatibility behavior unless a
        // profile explicitly opts into the more general role-policy system.
        hoist_system_messages(&mut messages);
    }
    let response_format = request
        .text
        .as_ref()
        .and_then(|text| text.format.as_ref())
        .map(|format| match format.kind.as_str() {
            "text" | "json_object" => json!({ "type": format.kind }),
            "json_schema" => {
                let mut json_schema = json!({
                    "name": format.name,
                    "schema": format.schema,
                    "strict": format.strict,
                });
                if let Some(description) = &format.description {
                    json_schema
                        .as_object_mut()
                        .expect("json_schema literal is an object")
                        .insert(
                            "description".to_string(),
                            Value::String(description.clone()),
                        );
                }
                json!({
                    "type": "json_schema",
                    "json_schema": json_schema,
                })
            }
            _ => unreachable!("TextFormat deserialization validates the variant"),
        });
    let reasoning_effort = normalize_reasoning_effort(
        request
            .reasoning
            .as_ref()
            .and_then(|reasoning| reasoning.effort.as_deref()),
    )?;
    Ok(LoweredTurn {
        messages,
        tools,
        tool_registry: registry,
        response_format,
        reasoning_effort,
        frequency_penalty: request.frequency_penalty,
        presence_penalty: request.presence_penalty,
    })
}

pub(crate) fn apply_role_rules(
    messages: &mut Vec<ChatMessage>,
    roles: Option<&RolesConfig>,
) -> AppResult<()> {
    // Two decoupled concerns: (1) per-message role rewrite + tag wrap, which is
    // NOT idempotent (`wrap_role_tag` double-wraps) and so runs exactly once per
    // message; (2) adjacency merge, which IS idempotent and may re-run before
    // any upstream send. Keeping them separate lets the engine re-normalize
    // adjacency after injecting repair-round messages without re-wrapping tags.
    assign_roles(messages, roles)?;
    merge_adjacent_if_configured(messages, roles);
    Ok(())
}

/// Per-message role rewrite + tag wrap (the non-idempotent half of the role
/// policy). Runs once per message; `When::Leading`/`Inline` key off the
/// message's index within `messages` (`0` == leading, `> 0` == inline).
fn assign_roles(messages: &mut Vec<ChatMessage>, roles: Option<&RolesConfig>) -> AppResult<()> {
    let Some(roles) = roles else {
        return Ok(());
    };
    let mut out = Vec::with_capacity(messages.len());
    for (index, mut message) in messages.drain(..).enumerate() {
        let Some(rules) = roles.rules_for(&message.role) else {
            return Err(AppError::bad_request(format!(
                "role \"{}\" is not permitted by the model profile roles config",
                message.role
            )));
        };
        let Some(rule) = rules.iter().find(|rule| match rule.when {
            None | Some(When::Always) => true,
            Some(When::Leading) => index == 0,
            Some(When::Inline) => index > 0,
        }) else {
            return Err(AppError::bad_request(format!(
                "role \"{}\" at index {} matches no rule in the model profile roles config",
                message.role, index
            )));
        };
        match rule.action {
            Action::Reject => {
                return Err(AppError::bad_request(format!(
                    "role \"{}\" is rejected by the model profile roles config",
                    message.role
                )));
            }
            Action::Drop => continue,
            Action::Accept => {
                wrap_role_tag(&mut message, &rule.tag, &rule.tag_attributes);
                out.push(message);
            }
            Action::Rewrite => {
                if let Some(target) = &rule.target_role {
                    message.role.clone_from(target);
                }
                wrap_role_tag(&mut message, &rule.tag, &rule.tag_attributes);
                out.push(message);
            }
        }
    }
    *messages = out;
    Ok(())
}

/// Collapse adjacent same-role runs for the configured roles. IDEMPOTENT (a
/// collapsed run is a lone message that re-collapses to itself; `\n\n` joins do
/// not accumulate), so it is safe to re-run before every upstream send. Only
/// content-only roles may be merged (enforced by `RolesConfig::validate`) since
/// the merge discards non-content fields (tool_call_id/tool_calls/reasoning).
pub(crate) fn merge_adjacent_if_configured(
    messages: &mut Vec<ChatMessage>,
    roles: Option<&RolesConfig>,
) {
    let Some(roles) = roles else {
        return;
    };
    if !roles.merge_adjacent.is_empty() {
        merge_adjacent_role_runs(messages, &roles.merge_adjacent);
    }
}

/// Shape ONE message being appended at the TAIL of an already-lowered chat
/// history. It is always "inline" (never index `0`/leading), so an engine-side
/// repair injection honors the SAME profile role mapping + tag wrap as the main
/// lowering pass would have applied to an interleaved message of that role
/// (e.g. an inline `system` note is rewritten to `developer`). Returns
/// `Ok(None)` when the profile drops the role. Adjacency cleanup is left to a
/// subsequent `merge_adjacent_if_configured` pass — this shapes one message.
pub(crate) fn shape_tail_message(
    mut message: ChatMessage,
    roles: Option<&RolesConfig>,
) -> AppResult<Option<ChatMessage>> {
    let Some(roles) = roles else {
        return Ok(Some(message));
    };
    let Some(rules) = roles.rules_for(&message.role) else {
        return Err(AppError::bad_request(format!(
            "role \"{}\" is not permitted by the model profile roles config",
            message.role
        )));
    };
    // Tail append ⇒ inline position (index > 0); never matches `Leading`.
    let Some(rule) = rules
        .iter()
        .find(|rule| matches!(rule.when, None | Some(When::Always) | Some(When::Inline)))
    else {
        return Err(AppError::bad_request(format!(
            "role \"{}\" (inline) matches no rule in the model profile roles config",
            message.role
        )));
    };
    match rule.action {
        Action::Reject => Err(AppError::bad_request(format!(
            "role \"{}\" is rejected by the model profile roles config",
            message.role
        ))),
        Action::Drop => Ok(None),
        Action::Accept => {
            wrap_role_tag(&mut message, &rule.tag, &rule.tag_attributes);
            Ok(Some(message))
        }
        Action::Rewrite => {
            if let Some(target) = &rule.target_role {
                message.role.clone_from(target);
            }
            wrap_role_tag(&mut message, &rule.tag, &rule.tag_attributes);
            Ok(Some(message))
        }
    }
}

fn wrap_role_tag(
    message: &mut ChatMessage,
    tag: &Option<String>,
    attributes: &BTreeMap<String, String>,
) {
    let Some(tag) = tag else {
        return;
    };
    let Some(content) = message.content.take() else {
        return;
    };
    let text = match content {
        Value::String(text) => text,
        other => other.to_string(),
    };
    let mut open = format!("<{tag}");
    for (key, value) in attributes {
        open.push(' ');
        open.push_str(key);
        open.push_str("=\"");
        for ch in value.chars() {
            match ch {
                '&' => open.push_str("&amp;"),
                '"' => open.push_str("&quot;"),
                '<' => open.push_str("&lt;"),
                _ => open.push(ch),
            }
        }
        open.push('"');
    }
    open.push('>');
    message.content = Some(Value::String(format!("{open}{text}</{tag}>")));
}

fn merge_adjacent_role_runs(messages: &mut Vec<ChatMessage>, roles: &[String]) {
    let mut out = Vec::with_capacity(messages.len());
    let mut index = 0;
    while index < messages.len() {
        let role = messages[index].role.clone();
        if !roles.contains(&role) {
            out.push(messages[index].clone());
            index += 1;
            continue;
        }
        let mut parts = Vec::new();
        while index < messages.len() && messages[index].role == role {
            if let Some(content) = &messages[index].content {
                let text = match content {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                if !text.is_empty() {
                    parts.push(text);
                }
            }
            index += 1;
        }
        if !parts.is_empty() {
            out.push(ChatMessage {
                role,
                content: Some(Value::String(parts.join("\n\n"))),
                tool_call_id: None,
                name: None,
                reasoning_content: None,
                thinking: None,
                tool_calls: None,
            });
        }
    }
    *messages = out;
}

fn normalize_reasoning_effort(effort: Option<&str>) -> AppResult<Option<String>> {
    // Carry the RAW canonical level through (trimmed + lowercased) so the upstream
    // leaf — the single point that knows the FINAL provider model — can apply a
    // per-model `reasoning_effort_map` (which needs `xhigh`/`max` kept distinct)
    // or clamp it to a backend's vocabulary. The clamp lives at the leaf
    // (`upstream::clamp_reasoning_effort`), not here.
    Ok(effort
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase()))
}

fn hoist_system_messages(messages: &mut Vec<ChatMessage>) {
    // Find the end of the initial contiguous block of system messages.
    let prefix_end = messages
        .iter()
        .position(|m| m.role != "system")
        .unwrap_or(messages.len());
    if prefix_end == 0 {
        return;
    }
    let mut system_texts: Vec<String> = Vec::new();
    for msg in &messages[..prefix_end] {
        if let Some(content) = &msg.content {
            let text = match content {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            if !text.is_empty() {
                system_texts.push(text);
            }
        }
    }
    let rest: Vec<ChatMessage> = messages.drain(prefix_end..).collect();
    messages.clear();
    if !system_texts.is_empty() {
        messages.push(ChatMessage {
            role: "system".to_string(),
            content: Some(Value::String(system_texts.join("\n\n"))),
            tool_call_id: None,
            name: None,
            reasoning_content: None,
            thinking: None,
            tool_calls: None,
        });
    }
    messages.extend(rest);
}

fn validate_request(request: &ResponsesRequest) -> AppResult<()> {
    if request
        .temperature
        .is_some_and(|value| !(0.0..=2.0).contains(&value))
    {
        return Err(AppError::bad_request("temperature must be between 0 and 2")
            .with_code("invalid_value")
            .with_param("temperature"));
    }
    if request
        .top_p
        .is_some_and(|value| !(0.0..=1.0).contains(&value))
    {
        return Err(AppError::bad_request("top_p must be between 0 and 1")
            .with_code("invalid_value")
            .with_param("top_p"));
    }
    if request.max_output_tokens.is_some_and(|value| value <= 0) {
        return Err(
            AppError::bad_request("max_output_tokens must be greater than zero")
                .with_code("invalid_value")
                .with_param("max_output_tokens"),
        );
    }
    if let Some(reasoning) = &request.reasoning {
        if let Some(effort) = reasoning.effort.as_deref()
            && !matches!(
                effort.trim().to_ascii_lowercase().as_str(),
                "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
            )
        {
            return Err(AppError::bad_request("unsupported reasoning effort")
                .with_code("invalid_value")
                .with_param("reasoning.effort"));
        }
        if let Some(summary) = reasoning.summary.as_deref()
            && !matches!(summary, "none" | "auto" | "concise" | "detailed")
        {
            return Err(AppError::bad_request("unsupported reasoning summary mode")
                .with_code("invalid_value")
                .with_param("reasoning.summary"));
        }
    }
    if let Some(format) = request.text.as_ref().and_then(|text| text.format.as_ref())
        && format.kind == "json_schema"
    {
        validate_function_schema_at(
            &format.name,
            format.strict,
            &format.schema,
            "text.format.name",
            "text.format.schema",
        )?;
    }
    validate_tool_schemas(&request.tools)?;
    if let Some(metadata) = &request.metadata {
        if metadata.len() > 16 {
            return Err(
                AppError::bad_request("metadata supports at most 16 entries")
                    .with_code("invalid_value")
                    .with_param("metadata"),
            );
        }
        for (key, value) in metadata {
            if key.is_empty() || key.len() > 64 {
                return Err(AppError::bad_request(
                    "metadata keys must contain between 1 and 64 bytes",
                )
                .with_code("invalid_value")
                .with_param(format!("metadata.{key}")));
            }
            if value.as_str().is_none_or(|value| value.len() > 512) {
                return Err(AppError::bad_request(
                    "metadata values must be strings no longer than 512 bytes",
                )
                .with_code("invalid_type")
                .with_param(format!("metadata.{key}")));
            }
        }
    }
    if request
        .prompt_cache_key
        .as_ref()
        .is_some_and(|key| key.is_empty() || key.len() > 64)
    {
        return Err(
            AppError::bad_request("prompt_cache_key must contain between 1 and 64 bytes")
                .with_code("invalid_value")
                .with_param("prompt_cache_key"),
        );
    }
    if let Some(verbosity) = request
        .text
        .as_ref()
        .and_then(|text| text.verbosity.as_deref())
        && !matches!(verbosity, "low" | "medium" | "high")
    {
        return Err(
            AppError::bad_request("text.verbosity must be low, medium, or high")
                .with_code("invalid_value")
                .with_param("text.verbosity"),
        );
    }
    if let Some(truncation) = &request.truncation
        && !matches!(truncation.as_str(), Some("auto" | "disabled"))
    {
        return Err(AppError::bad_request("truncation must be auto or disabled")
            .with_code("invalid_value")
            .with_param("truncation"));
    }
    for (base, items) in request
        .instructions
        .items()
        .into_iter()
        .map(|items| ("instructions", items))
        .chain(std::iter::once(("input", request.input.as_slice())))
    {
        validate_response_items(items, base)?;
    }
    // Validate tool_choice
    match &request.tool_choice {
        Value::String(s) => match s.as_str() {
            "auto" | "none" => {}
            "required" => {
                if request.tools.is_empty() {
                    return Err(AppError::bad_request(
                        "tool_choice is \"required\" but no tools are provided",
                    ));
                }
            }
            _ => {
                return Err(AppError::bad_request(
                    "invalid tool_choice string; expected auto, none, or required",
                )
                .with_code("invalid_value")
                .with_param("tool_choice"));
            }
        },
        Value::Object(map) => {
            let Some(kind) = map.get("type").and_then(Value::as_str) else {
                return Err(AppError::bad_request("tool_choice.type must be a string")
                    .with_code("invalid_type")
                    .with_param("tool_choice.type"));
            };
            match kind {
                "function" => {
                    let selected = map
                        .get("function")
                        .and_then(Value::as_object)
                        .and_then(|function| function.get("name"))
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .ok_or_else(|| {
                            AppError::bad_request("tool_choice function name is required")
                                .with_code("invalid_value")
                                .with_param("tool_choice.name")
                        })?;
                    if !request.tools.iter().any(|tool| match tool {
                        ToolSpec::Function { name, .. } => name == selected,
                        ToolSpec::Namespace { tools, .. } => tools.iter().any(|tool| match tool {
                            NamespaceToolSpec::Function { name, .. } => name == selected,
                        }),
                        ToolSpec::ToolSearch { .. } => selected == "tool_search",
                        ToolSpec::LocalShell { .. } => selected == "local_shell",
                        ToolSpec::WebSearch { .. } => selected == "web_search",
                        ToolSpec::Custom { .. } => false,
                        ToolSpec::ImageGeneration { .. } => false,
                    }) {
                        return Err(AppError::bad_request(
                            "tool_choice names an unavailable function",
                        )
                        .with_code("invalid_value")
                        .with_param("tool_choice"));
                    }
                }
                "custom" => {
                    let selected = map
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .ok_or_else(|| {
                            AppError::bad_request("tool_choice custom name is required")
                                .with_code("invalid_value")
                                .with_param("tool_choice.name")
                        })?;
                    if !request.tools.iter().any(
                        |tool| matches!(tool, ToolSpec::Custom { name, .. } if name == selected),
                    ) {
                        return Err(AppError::bad_request(
                            "tool_choice names an unavailable custom tool",
                        )
                        .with_code("invalid_value")
                        .with_param("tool_choice"));
                    }
                }
                "web_search" => {
                    if !request
                        .tools
                        .iter()
                        .any(|tool| matches!(tool, ToolSpec::WebSearch { .. }))
                    {
                        return Err(AppError::bad_request(
                            "tool_choice selects web_search but no web_search tool is provided",
                        )
                        .with_code("invalid_value")
                        .with_param("tool_choice"));
                    }
                }
                _ => return Err(AppError::unsupported_parameter("tool_choice.type")),
            }
        }
        _ => {
            return Err(AppError::bad_request(
                "invalid tool_choice: expected a string (\"auto\", \"none\", \"required\") or a function object",
            ));
        }
    }
    Ok(())
}

fn validate_response_items(items: &[ResponseItem], base: &str) -> AppResult<()> {
    for (item_index, item) in items.iter().enumerate() {
        match item {
            ResponseItem::Message { role, content, .. } => {
                if !matches!(role.as_str(), "developer" | "system" | "user" | "assistant") {
                    return Err(AppError::bad_request("unsupported message role")
                        .with_code("invalid_value")
                        .with_param(format!("{base}[{item_index}].role")));
                }
                validate_multimodal_content(content, &format!("{base}[{item_index}].content"))?;
            }
            ResponseItem::FunctionCallOutput { call_id, output } => {
                if call_id.is_empty() {
                    return Err(AppError::bad_request(
                        "tool history items require a non-empty call_id",
                    )
                    .with_code("invalid_value")
                    .with_param(format!("{base}[{item_index}].call_id")));
                }
                if let FunctionCallOutputContent::Content(content) = output {
                    validate_multimodal_content(content, &format!("{base}[{item_index}].output"))?;
                }
            }
            ResponseItem::CustomToolCallOutput {
                call_id, output, ..
            } => {
                if call_id.is_empty() {
                    return Err(AppError::bad_request(
                        "tool history items require a non-empty call_id",
                    )
                    .with_code("invalid_value")
                    .with_param(format!("{base}[{item_index}].call_id")));
                }
                if let FunctionCallOutputContent::Content(content) = output {
                    validate_multimodal_content(content, &format!("{base}[{item_index}].output"))?;
                }
            }
            ResponseItem::FunctionCall { call_id, .. }
            | ResponseItem::CustomToolCall { call_id, .. }
                if call_id.is_empty() =>
            {
                return Err(AppError::bad_request(
                    "tool history items require a non-empty call_id",
                )
                .with_code("invalid_value")
                .with_param(format!("{base}[{item_index}].call_id")));
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_multimodal_content(content: &[ContentItem], base: &str) -> AppResult<()> {
    for (content_index, part) in content.iter().enumerate() {
        let param = format!("{base}[{content_index}]");
        match part {
            ContentItem::InputImage {
                image_url,
                file_id,
                detail,
            } => {
                if usize::from(image_url.is_some()) + usize::from(file_id.is_some()) != 1 {
                    return Err(AppError::bad_request(
                        "input_image requires exactly one of image_url or file_id",
                    )
                    .with_code("invalid_value")
                    .with_param(param));
                }
                if detail
                    .as_deref()
                    .is_some_and(|detail| !matches!(detail, "auto" | "low" | "high" | "original"))
                {
                    return Err(AppError::bad_request(
                        "input_image detail must be auto, low, high, or original",
                    )
                    .with_code("invalid_value")
                    .with_param(format!("{param}.detail")));
                }
            }
            ContentItem::InputFile {
                file_id,
                file_url,
                file_data,
                ..
            } => {
                let sources = usize::from(file_id.is_some())
                    + usize::from(file_url.is_some())
                    + usize::from(file_data.is_some());
                if sources != 1 {
                    return Err(AppError::bad_request(
                        "input_file requires exactly one of file_id, file_url, or file_data",
                    )
                    .with_code("invalid_value")
                    .with_param(param));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn lower_tools(specs: &[ToolSpec]) -> AppResult<Vec<ChatTool>> {
    let mut tools = Vec::new();
    for spec in specs {
        let lowered_tools = match spec {
            ToolSpec::Function {
                name,
                description,
                strict,
                parameters,
            } => {
                validate_function_schema(name, *strict, parameters)?;
                vec![ChatTool {
                    kind: "function".to_string(),
                    function: ChatToolDefinition {
                        name: name.clone(),
                        description: description.clone(),
                        parameters: Some(parameters.clone()),
                        strict: *strict,
                    },
                }]
            }
            ToolSpec::Namespace {
                tools: namespace_tools,
                ..
            } => namespace_tools
                .iter()
                .map(|tool| -> AppResult<ChatTool> {
                    Ok(match tool {
                        NamespaceToolSpec::Function {
                            name,
                            description,
                            strict,
                            parameters,
                        } => {
                            validate_function_schema(name, *strict, parameters)?;
                            ChatTool {
                                kind: "function".to_string(),
                                function: ChatToolDefinition {
                                    name: name.clone(),
                                    description: description.clone(),
                                    parameters: Some(parameters.clone()),
                                    strict: *strict,
                                },
                            }
                        }
                    })
                })
                .collect::<AppResult<Vec<_>>>()?,
            ToolSpec::ToolSearch {
                description,
                parameters,
                ..
            } => vec![ChatTool {
                kind: "function".to_string(),
                function: ChatToolDefinition {
                    name: "tool_search".to_string(),
                    description: description.clone(),
                    parameters: Some(parameters.clone()),
                    strict: false,
                },
            }],
            ToolSpec::LocalShell {} => vec![ChatTool {
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
            }],
            ToolSpec::WebSearch { .. } => vec![ChatTool {
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
            }],
            ToolSpec::Custom {
                name,
                description,
                format,
            } => {
                let format_instruction = match format {
                    CustomToolFormat::Text => {
                        "Return the raw tool input as an unconstrained string.".to_string()
                    }
                    CustomToolFormat::Grammar { syntax, definition } => format!(
                        "Return the raw tool input as a string matching this {syntax} grammar:\n\n{definition}"
                    ),
                };
                let description = if description.is_empty() {
                    format_instruction
                } else {
                    format!("{description}\n\n{format_instruction}")
                };
                vec![ChatTool {
                    kind: "function".to_string(),
                    function: ChatToolDefinition {
                        name: name.clone(),
                        description,
                        parameters: Some(json!({
                            "type": "object",
                            "properties": {
                                "input": { "type": "string" }
                            },
                            "required": ["input"]
                        })),
                        strict: false,
                    },
                }]
            }
            ToolSpec::ImageGeneration { .. } => Vec::new(),
        };
        // Duplicate-tool-name rejection lives solely in `build_tool_registry`
        // (case-insensitive), which `lower_request` always calls on the same
        // tool slice. `lower_tools` only builds and sorts the chat tools.
        tools.extend(lowered_tools);
    }
    tools.sort_by(|a, b| a.function.name.cmp(&b.function.name));
    Ok(tools)
}

fn validate_function_schema(name: &str, strict: bool, parameters: &Value) -> AppResult<()> {
    validate_function_schema_at(name, strict, parameters, "tools", "tools")
}

fn validate_tool_schemas(specs: &[ToolSpec]) -> AppResult<()> {
    for (tool_index, spec) in specs.iter().enumerate() {
        match spec {
            ToolSpec::Function {
                name,
                strict,
                parameters,
                ..
            } => validate_function_schema_at(
                name,
                *strict,
                parameters,
                &format!("tools[{tool_index}].name"),
                &format!("tools[{tool_index}].parameters"),
            )?,
            ToolSpec::Namespace { tools, .. } => {
                for (nested_index, tool) in tools.iter().enumerate() {
                    let NamespaceToolSpec::Function {
                        name,
                        strict,
                        parameters,
                        ..
                    } = tool;
                    let prefix = format!("tools[{tool_index}].tools[{nested_index}]");
                    validate_function_schema_at(
                        name,
                        *strict,
                        parameters,
                        &format!("{prefix}.name"),
                        &format!("{prefix}.parameters"),
                    )?;
                }
            }
            ToolSpec::Custom { name, format, .. } => {
                validate_tool_name(name, &format!("tools[{tool_index}].name"))?;
                if let CustomToolFormat::Grammar { syntax, definition } = format {
                    if !matches!(syntax.as_str(), "lark" | "regex") {
                        return Err(AppError::bad_request(
                            "custom tool grammar syntax must be lark or regex",
                        )
                        .with_code("invalid_value")
                        .with_param(format!("tools[{tool_index}].format.syntax")));
                    }
                    if definition.trim().is_empty() {
                        return Err(AppError::bad_request(
                            "custom tool grammar definition must not be empty",
                        )
                        .with_code("invalid_value")
                        .with_param(format!("tools[{tool_index}].format.definition")));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_function_schema_at(
    name: &str,
    strict: bool,
    parameters: &Value,
    name_path: &str,
    schema_path: &str,
) -> AppResult<()> {
    validate_tool_name(name, name_path)?;

    if !parameters.is_object() {
        return Err(
            AppError::bad_request("function parameters must be a JSON Schema object")
                .with_code("invalid_json_schema")
                .with_param(schema_path),
        );
    }

    // Validate the supported JSON Schema vocabulary for every function, not
    // only `strict:true` functions. Passing malformed/unsupported non-strict
    // schemas through to different providers produces provider-dependent 4xxs
    // and makes capability behavior nondeterministic.
    validate_schema_node(parameters, schema_path, 0, strict)?;

    // Function arguments are always JSON objects. Keep `{}` as the permissive
    // non-strict shorthand used by existing clients, but reject an explicit
    // contradictory root type. Strict schemas must declare the object type.
    let root_types = effective_schema_types(parameters, parameters, schema_path, 0)?;
    if root_types
        .as_ref()
        .is_some_and(|types| types.as_slice() != ["object"])
        || strict && root_types.is_none()
    {
        return Err(AppError::bad_request(
            "function parameters must declare a top-level object schema",
        )
        .with_code("invalid_json_schema")
        .with_param(format!("{schema_path}.type")));
    }
    Ok(())
}

fn validate_tool_name(name: &str, name_path: &str) -> AppResult<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(AppError::bad_request(
            "tool names must contain 1-64 ASCII letters, digits, underscores, or hyphens",
        )
        .with_code("invalid_value")
        .with_param(name_path));
    }
    Ok(())
}

fn validate_schema_node(schema: &Value, path: &str, depth: usize, strict: bool) -> AppResult<()> {
    validate_schema_node_with_root(schema, schema, path, depth, strict)
}

fn validate_schema_node_with_root(
    root: &Value,
    schema: &Value,
    path: &str,
    depth: usize,
    strict: bool,
) -> AppResult<()> {
    if depth > 64 {
        return Err(invalid_schema(
            "JSON Schema exceeds the supported nesting depth",
            path,
        ));
    }
    let object = schema
        .as_object()
        .ok_or_else(|| invalid_schema("schema nodes must be JSON objects", path))?;

    // Every assertion keyword accepted here is also enforced by
    // `validate_json_schema_value`. Keeping the two sides in lock-step avoids
    // accepting a schema locally and then advertising an unchecked result.
    const SUPPORTED_KEYWORDS: &[&str] = &[
        "$defs",
        "$ref",
        "type",
        "anyOf",
        "properties",
        "required",
        "additionalProperties",
        "items",
        "enum",
        "const",
        "minLength",
        "maxLength",
        "pattern",
        "format",
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
        "minItems",
        "maxItems",
        "uniqueItems",
        "minProperties",
        "maxProperties",
        "description",
        "title",
        "default",
        "examples",
    ];
    for keyword in object.keys() {
        if !SUPPORTED_KEYWORDS.contains(&keyword.as_str()) {
            return Err(invalid_schema(
                format!("unsupported JSON Schema keyword: {keyword}"),
                format!("{path}.{keyword}"),
            ));
        }
    }

    let schema_types = declared_schema_types(object, path)?;
    let allows_type = |kind: &str| {
        schema_types
            .as_ref()
            .is_none_or(|types| types.contains(&kind))
    };

    if let Some(reference) = object.get("$ref") {
        let reference = reference
            .as_str()
            .ok_or_else(|| invalid_schema("$ref must be a string", format!("{path}.$ref")))?;
        let target = resolve_local_ref(root, reference)
            .map_err(|message| invalid_schema(message, format!("{path}.$ref")))?;
        if !target.is_object() {
            return Err(invalid_schema(
                "$ref must resolve to a schema object",
                format!("{path}.$ref"),
            ));
        }
        if std::ptr::eq(target, schema) {
            return Err(invalid_schema(
                "$ref forms a reference cycle without a constraining schema",
                format!("{path}.$ref"),
            ));
        }
    }

    if let Some(definitions) = object.get("$defs") {
        let definitions = definitions
            .as_object()
            .ok_or_else(|| invalid_schema("$defs must be an object", format!("{path}.$defs")))?;
        for (name, definition) in definitions {
            validate_schema_node_with_root(
                root,
                definition,
                &format!("{path}.$defs.{name}"),
                depth + 1,
                strict,
            )?;
        }
    }

    if let Some(any_of) = object.get("anyOf") {
        let branches = any_of
            .as_array()
            .ok_or_else(|| invalid_schema("anyOf must be an array", format!("{path}.anyOf")))?;
        if branches.is_empty() {
            return Err(invalid_schema(
                "anyOf must contain at least one schema",
                format!("{path}.anyOf"),
            ));
        }
        for (index, branch) in branches.iter().enumerate() {
            validate_schema_node_with_root(
                root,
                branch,
                &format!("{path}.anyOf[{index}]"),
                depth + 1,
                strict,
            )?;
        }
    }

    for annotation in ["description", "title"] {
        if object
            .get(annotation)
            .is_some_and(|value| !value.is_string())
        {
            return Err(invalid_schema(
                format!("{annotation} must be a string"),
                format!("{path}.{annotation}"),
            ));
        }
    }
    if object
        .get("examples")
        .is_some_and(|value| !value.is_array())
    {
        return Err(invalid_schema(
            "examples must be an array",
            format!("{path}.examples"),
        ));
    }
    if let Some(value) = object.get("enum") {
        let values = value
            .as_array()
            .ok_or_else(|| invalid_schema("enum must be an array", format!("{path}.enum")))?;
        if values.is_empty() {
            return Err(invalid_schema(
                "enum must contain at least one value",
                format!("{path}.enum"),
            ));
        }
        for (index, value) in values.iter().enumerate() {
            if values[..index].contains(value) {
                return Err(invalid_schema(
                    "enum values must be unique",
                    format!("{path}.enum[{index}]"),
                ));
            }
        }
    }

    for keyword in [
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
        "minProperties",
        "maxProperties",
    ] {
        if let Some(value) = object.get(keyword)
            && value.as_u64().is_none()
        {
            return Err(invalid_schema(
                format!("{keyword} must be a non-negative integer"),
                format!("{path}.{keyword}"),
            ));
        }
    }
    for (minimum_keyword, maximum_keyword) in [
        ("minLength", "maxLength"),
        ("minItems", "maxItems"),
        ("minProperties", "maxProperties"),
    ] {
        if let (Some(minimum), Some(maximum)) = (
            object.get(minimum_keyword).and_then(Value::as_u64),
            object.get(maximum_keyword).and_then(Value::as_u64),
        ) && minimum > maximum
        {
            return Err(invalid_schema(
                "minimum constraint must not exceed maximum constraint",
                format!("{path}.{maximum_keyword}"),
            ));
        }
    }

    if let Some(pattern) = object.get("pattern") {
        let pattern = pattern
            .as_str()
            .ok_or_else(|| invalid_schema("pattern must be a string", format!("{path}.pattern")))?;
        regex::Regex::new(pattern).map_err(|error| {
            invalid_schema(
                format!("pattern is not a valid regular expression: {error}"),
                format!("{path}.pattern"),
            )
        })?;
    }
    if let Some(format) = object.get("format") {
        let format = format
            .as_str()
            .ok_or_else(|| invalid_schema("format must be a string", format!("{path}.format")))?;
        if !matches!(
            format,
            "date-time"
                | "date"
                | "time"
                | "duration"
                | "email"
                | "hostname"
                | "ipv4"
                | "ipv6"
                | "uuid"
                | "uri"
        ) {
            return Err(invalid_schema(
                format!("unsupported JSON Schema format: {format}"),
                format!("{path}.format"),
            ));
        }
    }
    for keyword in [
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
    ] {
        if let Some(value) = object.get(keyword)
            && value.as_f64().is_none()
        {
            return Err(invalid_schema(
                format!("{keyword} must be a number"),
                format!("{path}.{keyword}"),
            ));
        }
    }
    if object
        .get("multipleOf")
        .and_then(Value::as_f64)
        .is_some_and(|value| value <= 0.0)
    {
        return Err(invalid_schema(
            "multipleOf must be greater than zero",
            format!("{path}.multipleOf"),
        ));
    }
    if let (Some(minimum), Some(maximum)) = (
        object.get("minimum").and_then(Value::as_f64),
        object.get("maximum").and_then(Value::as_f64),
    ) && minimum > maximum
    {
        return Err(invalid_schema(
            "minimum must not exceed maximum",
            format!("{path}.maximum"),
        ));
    }
    if object
        .get("uniqueItems")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err(invalid_schema(
            "uniqueItems must be a boolean",
            format!("{path}.uniqueItems"),
        ));
    }

    for (keyword, allowed) in [
        ("minLength", &["string"][..]),
        ("maxLength", &["string"][..]),
        ("pattern", &["string"][..]),
        ("format", &["string"][..]),
        ("minimum", &["integer", "number"][..]),
        ("maximum", &["integer", "number"][..]),
        ("exclusiveMinimum", &["integer", "number"][..]),
        ("exclusiveMaximum", &["integer", "number"][..]),
        ("multipleOf", &["integer", "number"][..]),
        ("minItems", &["array"][..]),
        ("maxItems", &["array"][..]),
        ("uniqueItems", &["array"][..]),
        ("minProperties", &["object"][..]),
        ("maxProperties", &["object"][..]),
    ] {
        if object.contains_key(keyword)
            && (schema_types
                .as_ref()
                .is_some_and(|types| !types.iter().any(|kind| allowed.contains(kind)))
                || strict && schema_types.is_none())
        {
            return Err(invalid_schema(
                format!("{keyword} is incompatible with the declared schema type"),
                format!("{path}.{keyword}"),
            ));
        }
    }

    let properties = match object.get("properties") {
        Some(Value::Object(properties)) => Some(properties),
        Some(_) => {
            return Err(invalid_schema(
                "properties must be an object",
                format!("{path}.properties"),
            ));
        }
        None => None,
    };
    if properties.is_some() && (!allows_type("object") || strict && schema_types.is_none()) {
        return Err(invalid_schema(
            "properties requires type object",
            format!("{path}.type"),
        ));
    }

    if let Some(properties) = properties {
        for (name, child_schema) in properties {
            validate_schema_node_with_root(
                root,
                child_schema,
                &format!("{path}.properties.{name}"),
                depth + 1,
                strict,
            )?;
        }
    }

    let required = match object.get("required") {
        Some(Value::Array(required)) => {
            let mut required_names = std::collections::HashSet::new();
            for (index, required_name) in required.iter().enumerate() {
                let Some(required_name) = required_name.as_str() else {
                    return Err(invalid_schema(
                        "required entries must be strings",
                        format!("{path}.required[{index}]"),
                    ));
                };
                if !required_names.insert(required_name) {
                    return Err(invalid_schema(
                        "required entries must be unique",
                        format!("{path}.required[{index}]"),
                    ));
                }
            }
            Some(required_names)
        }
        Some(_) => {
            return Err(invalid_schema(
                "required must be an array of unique strings",
                format!("{path}.required"),
            ));
        }
        None => None,
    };
    if required.is_some() && (!allows_type("object") || strict && schema_types.is_none()) {
        return Err(invalid_schema(
            "required is only supported for object schemas",
            format!("{path}.required"),
        ));
    }

    let additional_properties = match object.get("additionalProperties") {
        Some(value @ Value::Bool(_)) => Some(value),
        Some(value @ Value::Object(_)) => {
            validate_schema_node_with_root(
                root,
                value,
                &format!("{path}.additionalProperties"),
                depth + 1,
                false,
            )?;
            Some(value)
        }
        Some(_) => {
            return Err(invalid_schema(
                "additionalProperties must be a boolean or schema object",
                format!("{path}.additionalProperties"),
            ));
        }
        None => None,
    };
    if additional_properties.is_some()
        && (!allows_type("object") || strict && schema_types.is_none())
    {
        return Err(invalid_schema(
            "additionalProperties is only supported for object schemas",
            format!("{path}.additionalProperties"),
        ));
    }

    if strict && allows_type("object") && schema_types.is_some() {
        let properties = properties.ok_or_else(|| {
            invalid_schema(
                "strict object schemas must define properties",
                format!("{path}.properties"),
            )
        })?;
        if object.get("additionalProperties").and_then(Value::as_bool) != Some(false) {
            return Err(invalid_schema(
                "strict object schemas must set additionalProperties to false",
                format!("{path}.additionalProperties"),
            ));
        }
        let required_names = required.as_ref().ok_or_else(|| {
            invalid_schema(
                "strict object schemas must list every property in required",
                format!("{path}.required"),
            )
        })?;
        if required_names.len() != properties.len()
            || !properties
                .keys()
                .all(|key| required_names.contains(key.as_str()))
        {
            return Err(invalid_schema(
                "strict object schemas must list every property exactly once in required",
                format!("{path}.required"),
            ));
        }
    }

    match object.get("items") {
        Some(items) if allows_type("array") && (schema_types.is_some() || !strict) => {
            validate_schema_node_with_root(
                root,
                items,
                &format!("{path}.items"),
                depth + 1,
                strict,
            )?;
        }
        Some(_) => {
            return Err(invalid_schema(
                "items is only supported for array schemas",
                format!("{path}.items"),
            ));
        }
        None if strict && allows_type("array") && schema_types.is_some() => {
            return Err(invalid_schema(
                "strict array schemas must define items",
                format!("{path}.items"),
            ));
        }
        None => {}
    }

    Ok(())
}

fn declared_schema_types<'a>(
    object: &'a Map<String, Value>,
    path: &str,
) -> AppResult<Option<Vec<&'a str>>> {
    let Some(schema_type) = object.get("type") else {
        return Ok(None);
    };
    let values: Vec<&str> = match schema_type {
        Value::String(kind) => vec![kind.as_str()],
        Value::Array(kinds) if !kinds.is_empty() => {
            let mut values = Vec::with_capacity(kinds.len());
            for (index, kind) in kinds.iter().enumerate() {
                let kind = kind.as_str().ok_or_else(|| {
                    invalid_schema(
                        "schema type array entries must be strings",
                        format!("{path}.type[{index}]"),
                    )
                })?;
                if values.contains(&kind) {
                    return Err(invalid_schema(
                        "schema type array entries must be unique",
                        format!("{path}.type[{index}]"),
                    ));
                }
                values.push(kind);
            }
            values
        }
        Value::Array(_) => {
            return Err(invalid_schema(
                "schema type array must not be empty",
                format!("{path}.type"),
            ));
        }
        _ => {
            return Err(invalid_schema(
                "schema type must be a string or non-empty string array",
                format!("{path}.type"),
            ));
        }
    };
    for kind in &values {
        if !matches!(
            *kind,
            "object" | "array" | "string" | "integer" | "number" | "boolean" | "null"
        ) {
            return Err(invalid_schema(
                format!("unsupported JSON Schema type: {kind}"),
                format!("{path}.type"),
            ));
        }
    }
    Ok(Some(values))
}

fn effective_schema_types(
    root: &Value,
    schema: &Value,
    path: &str,
    depth: usize,
) -> AppResult<Option<Vec<String>>> {
    if depth > 64 {
        return Err(invalid_schema(
            "JSON Schema reference chain exceeds the supported nesting depth",
            format!("{path}.$ref"),
        ));
    }
    let object = schema
        .as_object()
        .ok_or_else(|| invalid_schema("schema nodes must be JSON objects", path))?;
    if let Some(types) = declared_schema_types(object, path)? {
        return Ok(Some(types.into_iter().map(str::to_string).collect()));
    }
    let Some(reference) = object.get("$ref").and_then(Value::as_str) else {
        return Ok(None);
    };
    let target = resolve_local_ref(root, reference)
        .map_err(|message| invalid_schema(message, format!("{path}.$ref")))?;
    if std::ptr::eq(target, schema) {
        return Err(invalid_schema(
            "$ref forms a reference cycle without declaring a root type",
            format!("{path}.$ref"),
        ));
    }
    effective_schema_types(root, target, path, depth + 1)
}

fn resolve_local_ref<'a>(root: &'a Value, reference: &str) -> Result<&'a Value, String> {
    let pointer = reference.strip_prefix('#').ok_or_else(|| {
        "only local JSON Schema references beginning with # are supported".to_string()
    })?;
    if !pointer.is_empty() && !pointer.starts_with('/') {
        return Err("local JSON Schema references must use JSON Pointer syntax".to_string());
    }
    root.pointer(pointer)
        .ok_or_else(|| format!("unresolved local JSON Schema reference: {reference}"))
}

fn invalid_schema(message: impl Into<String>, param: impl Into<String>) -> AppError {
    AppError::bad_request(message)
        .with_code("invalid_json_schema")
        .with_param(param)
}

fn build_tool_registry(specs: &[ToolSpec], image_agent_active: bool) -> AppResult<ToolRegistry> {
    let mut by_name = HashMap::new();
    let mut strict_function_schemas = HashMap::new();
    for spec in specs {
        match spec {
            ToolSpec::Function {
                name,
                strict: true,
                parameters,
                ..
            } => {
                strict_function_schemas.insert(name.to_ascii_lowercase(), parameters.clone());
            }
            ToolSpec::Namespace { tools, .. } => {
                for tool in tools {
                    let NamespaceToolSpec::Function {
                        name,
                        strict,
                        parameters,
                        ..
                    } = tool;
                    if *strict {
                        strict_function_schemas
                            .insert(name.to_ascii_lowercase(), parameters.clone());
                    }
                }
            }
            _ => {}
        }
        let lowered_kinds: Vec<(String, ToolKind)> = match spec {
            // G4: on an active image-agent turn, classify the injected (or
            // caller-supplied) `analyzeImage` function as the server-side
            // ImageAnalysis tool so the engine runs it instead of handing it to
            // the client. On a non-image turn it stays a normal client Function.
            ToolSpec::Function { name, .. }
                if image_agent_active
                    && name.eq_ignore_ascii_case(crate::vision::ANALYZE_IMAGE_TOOL_NAME) =>
            {
                vec![(name.clone(), ToolKind::ImageAnalysis)]
            }
            ToolSpec::Function { name, .. } => vec![(
                name.clone(),
                ToolKind::Function {
                    public_name: name.clone(),
                    namespace: None,
                },
            )],
            ToolSpec::Namespace {
                name: namespace,
                tools,
                ..
            } => tools
                .iter()
                .map(|tool| match tool {
                    NamespaceToolSpec::Function { name, .. } => (
                        name.clone(),
                        ToolKind::Function {
                            public_name: name.clone(),
                            namespace: Some(namespace.clone()),
                        },
                    ),
                })
                .collect(),
            ToolSpec::ToolSearch { .. } => {
                vec![("tool_search".to_string(), ToolKind::ToolSearch)]
            }
            ToolSpec::LocalShell {} => vec![("local_shell".to_string(), ToolKind::LocalShell)],
            ToolSpec::WebSearch { .. } => vec![("web_search".to_string(), ToolKind::WebSearch)],
            ToolSpec::Custom { name, .. } => vec![(
                name.clone(),
                ToolKind::Custom {
                    public_name: name.clone(),
                },
            )],
            ToolSpec::ImageGeneration { .. } => Vec::new(),
        };
        for (name, kind) in lowered_kinds {
            let name_lc = name.to_ascii_lowercase();
            if by_name.insert(name_lc.clone(), kind).is_some() {
                return Err(AppError::bad_request(
                    "duplicate tool names are not supported",
                ));
            }
        }
    }
    Ok(ToolRegistry {
        by_name,
        strict_function_schemas,
    })
}

pub(crate) fn validate_json_schema_value(
    schema: &Value,
    value: &Value,
    path: &str,
    depth: usize,
) -> Result<(), String> {
    validate_json_schema_value_with_root(schema, schema, value, path, depth)
}

fn validate_json_schema_value_with_root(
    root: &Value,
    schema: &Value,
    value: &Value,
    path: &str,
    depth: usize,
) -> Result<(), String> {
    if depth > 64 {
        return Err(format!("{path} exceeds the supported schema nesting depth"));
    }
    let object = schema
        .as_object()
        .ok_or_else(|| format!("{path} has a non-object schema"))?;

    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        let target = resolve_local_ref(root, reference)
            .map_err(|message| format!("{path} has invalid $ref: {message}"))?;
        if std::ptr::eq(target, schema) {
            return Err(format!("{path} contains an unproductive $ref cycle"));
        }
        validate_json_schema_value_with_root(root, target, value, path, depth + 1)?;
    }

    if let Some(branches) = object.get("anyOf").and_then(Value::as_array)
        && !branches.iter().any(|branch| {
            validate_json_schema_value_with_root(root, branch, value, path, depth + 1).is_ok()
        })
    {
        return Err(format!("{path} does not match any anyOf branch"));
    }

    let expected_types: Vec<&str> = match object.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        Some(_) => return Err(format!("{path} has an invalid schema type declaration")),
        None => Vec::new(),
    };
    if !expected_types.is_empty()
        && !expected_types
            .iter()
            .any(|expected| value_matches_schema_type(value, expected))
    {
        return Err(format!("{path} must be {}", expected_types.join(" or ")));
    }

    if let Some(constant) = object.get("const")
        && value != constant
    {
        return Err(format!("{path} does not match const"));
    }
    if let Some(choices) = object.get("enum").and_then(Value::as_array)
        && !choices.contains(value)
    {
        return Err(format!("{path} is not one of the allowed enum values"));
    }

    if let Some(map) = value.as_object() {
        let properties = object.get("properties").and_then(Value::as_object);
        if let Some(required) = object.get("required").and_then(Value::as_array) {
            for key in required.iter().filter_map(Value::as_str) {
                if !map.contains_key(key) {
                    return Err(format!("{path}.{key} is required"));
                }
            }
        }
        if let Some(minimum) = object.get("minProperties").and_then(Value::as_u64)
            && map.len() < minimum as usize
        {
            return Err(format!("{path} must contain at least {minimum} properties"));
        }
        if let Some(maximum) = object.get("maxProperties").and_then(Value::as_u64)
            && map.len() > maximum as usize
        {
            return Err(format!("{path} must contain at most {maximum} properties"));
        }
        for (key, child) in map {
            if let Some(child_schema) = properties.and_then(|properties| properties.get(key)) {
                validate_json_schema_value_with_root(
                    root,
                    child_schema,
                    child,
                    &format!("{path}.{key}"),
                    depth + 1,
                )?;
                continue;
            }
            match object.get("additionalProperties") {
                Some(Value::Bool(false)) => {
                    return Err(format!("{path}.{key} is not allowed"));
                }
                Some(additional_schema @ Value::Object(_)) => {
                    validate_json_schema_value_with_root(
                        root,
                        additional_schema,
                        child,
                        &format!("{path}.{key}"),
                        depth + 1,
                    )?;
                }
                _ => {}
            }
        }
    }

    if let Some(values) = value.as_array() {
        if let Some(minimum) = object.get("minItems").and_then(Value::as_u64)
            && values.len() < minimum as usize
        {
            return Err(format!("{path} must contain at least {minimum} items"));
        }
        if let Some(maximum) = object.get("maxItems").and_then(Value::as_u64)
            && values.len() > maximum as usize
        {
            return Err(format!("{path} must contain at most {maximum} items"));
        }
        if object.get("uniqueItems").and_then(Value::as_bool) == Some(true) {
            for (index, value) in values.iter().enumerate() {
                if values[..index].contains(value) {
                    return Err(format!("{path}[{index}] duplicates an earlier item"));
                }
            }
        }
        if let Some(items) = object.get("items") {
            for (index, child) in values.iter().enumerate() {
                validate_json_schema_value_with_root(
                    root,
                    items,
                    child,
                    &format!("{path}[{index}]"),
                    depth + 1,
                )?;
            }
        }
    }

    if let Some(text) = value.as_str() {
        let length = text.chars().count();
        if let Some(minimum) = object.get("minLength").and_then(Value::as_u64)
            && length < minimum as usize
        {
            return Err(format!("{path} must contain at least {minimum} characters"));
        }
        if let Some(maximum) = object.get("maxLength").and_then(Value::as_u64)
            && length > maximum as usize
        {
            return Err(format!("{path} must contain at most {maximum} characters"));
        }
        if let Some(pattern) = object.get("pattern").and_then(Value::as_str)
            && !regex::Regex::new(pattern)
                .map_err(|error| format!("{path} has an invalid pattern: {error}"))?
                .is_match(text)
        {
            return Err(format!("{path} does not match the required pattern"));
        }
        if let Some(format) = object.get("format").and_then(Value::as_str)
            && !string_matches_schema_format(text, format)
        {
            return Err(format!("{path} is not a valid {format}"));
        }
    }

    if let Some(number) = value.as_f64() {
        if let Some(minimum) = object.get("minimum").and_then(Value::as_f64)
            && number < minimum
        {
            return Err(format!("{path} must be at least {minimum}"));
        }
        if let Some(maximum) = object.get("maximum").and_then(Value::as_f64)
            && number > maximum
        {
            return Err(format!("{path} must be at most {maximum}"));
        }
        if let Some(minimum) = object.get("exclusiveMinimum").and_then(Value::as_f64)
            && number <= minimum
        {
            return Err(format!("{path} must be greater than {minimum}"));
        }
        if let Some(maximum) = object.get("exclusiveMaximum").and_then(Value::as_f64)
            && number >= maximum
        {
            return Err(format!("{path} must be less than {maximum}"));
        }
        if let Some(multiple) = object.get("multipleOf").and_then(Value::as_f64) {
            let quotient = number / multiple;
            let tolerance = 1e-9_f64 * quotient.abs().max(1.0);
            if (quotient - quotient.round()).abs() > tolerance {
                return Err(format!("{path} must be a multiple of {multiple}"));
            }
        }
    }

    Ok(())
}

fn value_matches_schema_type(value: &Value, expected: &str) -> bool {
    match expected {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value
            .as_f64()
            .is_some_and(|number| number.is_finite() && number.fract() == 0.0),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

fn string_matches_schema_format(value: &str, format: &str) -> bool {
    match format {
        "date-time" => chrono::DateTime::parse_from_rfc3339(value).is_ok(),
        "date" => chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok(),
        "time" => chrono::DateTime::parse_from_rfc3339(&format!("1970-01-01T{value}")).is_ok(),
        "duration" => {
            value != "P"
                && value != "PT"
                && regex::Regex::new(
                    r"^P(?:\d+(?:[.,]\d+)?Y)?(?:\d+(?:[.,]\d+)?M)?(?:\d+(?:[.,]\d+)?W)?(?:\d+(?:[.,]\d+)?D)?(?:T(?:\d+(?:[.,]\d+)?H)?(?:\d+(?:[.,]\d+)?M)?(?:\d+(?:[.,]\d+)?S)?)?$",
                )
                .expect("static ISO-8601 duration regex")
                .is_match(value)
        }
        "email" => {
            let Some((local, domain)) = value.rsplit_once('@') else {
                return false;
            };
            !local.is_empty()
                && !local.chars().any(char::is_whitespace)
                && hostname_is_valid(domain)
        }
        "hostname" => hostname_is_valid(value),
        "ipv4" => value.parse::<std::net::Ipv4Addr>().is_ok(),
        "ipv6" => value.parse::<std::net::Ipv6Addr>().is_ok(),
        "uuid" => uuid::Uuid::parse_str(value).is_ok(),
        "uri" => url::Url::parse(value).is_ok(),
        _ => false,
    }
}

fn hostname_is_valid(value: &str) -> bool {
    if value.is_empty() || value.len() > 253 || !value.is_ascii() {
        return false;
    }
    value.trim_end_matches('.').split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

/// Validate a completed model answer against the requested Responses text
/// format. This runs only after a clean upstream stop; incomplete output is
/// reported as incomplete rather than being reclassified as invalid JSON.
pub(crate) fn validate_structured_output(
    text: Option<&crate::models::responses::TextControls>,
    output: &[ResponseItem],
) -> AppResult<()> {
    let Some(format) = text.and_then(|controls| controls.format.as_ref()) else {
        return Ok(());
    };
    if format.kind == "text" {
        return Ok(());
    }

    let Some(content) = output.iter().rev().find_map(|item| match item {
        ResponseItem::Message { content, .. } => Some(content),
        _ => None,
    }) else {
        return Err(invalid_structured_output(
            "model returned no assistant message for structured output",
        ));
    };
    if content
        .iter()
        .any(|part| matches!(part, ContentItem::Refusal { .. }))
        && !content
            .iter()
            .any(|part| matches!(part, ContentItem::OutputText { .. }))
    {
        // A safety refusal is a distinct successful response shape; it is not
        // malformed structured JSON.
        return Ok(());
    }
    let rendered = content
        .iter()
        .filter_map(|part| match part {
            ContentItem::OutputText { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    let value: Value = serde_json::from_str(rendered.trim()).map_err(|error| {
        invalid_structured_output(format!("model output is not valid JSON: {error}"))
    })?;

    match format.kind.as_str() {
        "json_object" if !value.is_object() => Err(invalid_structured_output(
            "model output is valid JSON but is not an object",
        )),
        "json_schema" => {
            validate_json_schema_value(&format.schema, &value, "$", 0).map_err(|detail| {
                invalid_structured_output(format!(
                    "model output does not match the requested JSON Schema: {detail}"
                ))
            })
        }
        "json_object" => Ok(()),
        _ => Err(AppError::internal("unvalidated structured output format")),
    }
}

fn invalid_structured_output(message: impl Into<String>) -> AppError {
    let mut error = AppError::upstream(message).with_code("invalid_structured_output");
    error.client_message = "the model returned invalid structured output".to_string();
    error
}

fn append_tool_call(
    messages: &mut Vec<ChatMessage>,
    call_id: String,
    name: String,
    arguments: Value,
    pending_reasoning: Option<PendingReasoning>,
) {
    if let Some(last) = messages.last_mut()
        && last.role == "assistant"
        && (last.tool_calls.is_some() || last.content.is_none())
    {
        let index = last.tool_calls.as_ref().map(|v| v.len()).unwrap_or(0);
        let tool_call = ChatToolCall {
            id: Some(call_id),
            index: Some(index),
            kind: "function".to_string(),
            function: ChatFunctionCall {
                name: Some(name),
                arguments: Some(arguments),
            },
        };
        if let Some(existing) = &mut last.tool_calls {
            existing.push(tool_call);
        } else {
            last.tool_calls = Some(vec![tool_call]);
        }
        if let Some(reasoning) = pending_reasoning
            && last.reasoning_content.is_none()
        {
            let (reasoning_content, thinking) = reasoning.into_chat_parts();
            last.reasoning_content = reasoning_content;
            last.thinking = thinking;
        }
        return;
    }
    let tool_call = ChatToolCall {
        id: Some(call_id),
        index: Some(0),
        kind: "function".to_string(),
        function: ChatFunctionCall {
            name: Some(name),
            arguments: Some(arguments),
        },
    };
    messages.push(ChatMessage {
        role: "assistant".to_string(),
        content: None,
        tool_call_id: None,
        name: None,
        reasoning_content: pending_reasoning
            .as_ref()
            .map(|reasoning| reasoning.text.clone()),
        thinking: pending_reasoning.and_then(|reasoning| {
            reasoning.signature.map(|signature| ChatThinking {
                content: reasoning.text,
                signature: Some(signature),
            })
        }),
        tool_calls: Some(vec![tool_call]),
    });
}

fn parse_json_string(raw: &str) -> AppResult<Value> {
    serde_json::from_str(raw)
        .map_err(|err| AppError::bad_request(format!("invalid JSON tool arguments: {err}")))
}

fn web_search_arguments(action: &Option<crate::models::responses::WebSearchAction>) -> Value {
    match action {
        Some(crate::models::responses::WebSearchAction::Search { query, queries, .. }) => {
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
    use crate::models::responses::WebSearchAction;

    // One base sentence, differing only by an optional action label, plus the
    // optional fields present for this action, in field order.
    let (action_label, fragments): (&str, Vec<String>) = match action {
        Some(WebSearchAction::Search { query, queries, .. }) => {
            let query = query.clone().or_else(|| {
                queries
                    .as_ref()
                    .and_then(|queries| queries.first().cloned())
            });
            (
                "",
                query.into_iter().map(|q| format!("Query: {q}")).collect(),
            )
        }
        Some(WebSearchAction::OpenPage { url }) => (
            " open_page",
            url.iter().map(|u| format!("URL: {u}")).collect(),
        ),
        Some(WebSearchAction::FindInPage { url, pattern }) => {
            let mut fragments = Vec::new();
            if let Some(url) = url {
                fragments.push(format!("URL: {url}"));
            }
            if let Some(pattern) = pattern {
                fragments.push(format!("Pattern: {pattern}"));
            }
            (" find_in_page", fragments)
        }
        Some(WebSearchAction::Other) | None => ("", Vec::new()),
    };

    let mut result = format!(
        "Previous web_search{action_label} completed in an earlier turn, but the original tool result is unavailable because replay state was missing."
    );
    if !fragments.is_empty() {
        result.push_str(&format!(" {}", fragments.join(". ")));
    }
    result
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
        parts.push(content_item_to_chat_part(item));
    }
    Ok(Value::Array(parts))
}

fn content_item_to_chat_value(item: &ContentItem) -> AppResult<Value> {
    match item {
        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
            Ok(Value::String(text.clone()))
        }
        ContentItem::Refusal { refusal } => Ok(Value::String(refusal.clone())),
        ContentItem::InputImage { .. } | ContentItem::InputFile { .. } | ContentItem::Other(_) => {
            Ok(Value::Array(vec![content_item_to_chat_part(item)]))
        }
    }
}

fn content_item_to_chat_part(item: &ContentItem) -> Value {
    match item {
        ContentItem::InputText { text } | ContentItem::OutputText { text } => json!({
            "type": "text",
            "text": text,
        }),
        ContentItem::Refusal { refusal } => json!({
            "type": "text",
            "text": refusal,
        }),
        ContentItem::InputImage {
            image_url: Some(image_url),
            detail,
            ..
        } => {
            let mut image_url_value = Map::new();
            image_url_value.insert("url".to_string(), Value::String(image_url.clone()));
            if let Some(detail) = detail {
                image_url_value.insert("detail".to_string(), Value::String(detail.clone()));
            }
            json!({
                "type": "image_url",
                "image_url": Value::Object(image_url_value)
            })
        }
        ContentItem::InputImage {
            image_url: None,
            file_id,
            detail,
        } => {
            let mut part = Map::new();
            part.insert("type".to_string(), Value::String("input_image".to_string()));
            if let Some(file_id) = file_id {
                part.insert("file_id".to_string(), Value::String(file_id.clone()));
            }
            if let Some(detail) = detail {
                part.insert("detail".to_string(), Value::String(detail.clone()));
            }
            Value::Object(part)
        }
        ContentItem::InputFile {
            file_id,
            file_url,
            filename,
            file_data,
        } => {
            let mut part = Map::new();
            part.insert("type".to_string(), Value::String("input_file".to_string()));
            insert_optional_string(&mut part, "file_id", file_id);
            insert_optional_string(&mut part, "file_url", file_url);
            insert_optional_string(&mut part, "filename", filename);
            insert_optional_string(&mut part, "file_data", file_data);
            Value::Object(part)
        }
        ContentItem::Other(value) => value.clone(),
    }
}

fn insert_optional_string(map: &mut Map<String, Value>, key: &str, value: &Option<String>) {
    if let Some(value) = value {
        map.insert(key.to_string(), Value::String(value.clone()));
    }
}

fn function_call_output_to_chat_value(
    output: &crate::models::responses::FunctionCallOutputContent,
) -> AppResult<Value> {
    match output {
        crate::models::responses::FunctionCallOutputContent::Text(text) => {
            Ok(Value::String(text.clone()))
        }
        // Internal ingress adapters can preserve provider-specific structured
        // tool results as `Other` content items (for example Anthropic's
        // `web_search_result`). Those are not valid Chat Completions content
        // parts, so keep the historical behavior and serialize the complete
        // result as tool text instead of forwarding an invalid part type.
        crate::models::responses::FunctionCallOutputContent::Content(content)
            if content
                .iter()
                .any(|item| matches!(item, ContentItem::Other(_))) =>
        {
            let value = serde_json::to_value(content).map_err(|error| {
                AppError::internal(format!(
                    "failed to serialize structured function output: {error}"
                ))
            })?;
            Ok(Value::String(stringify_tool_output(&value)))
        }
        crate::models::responses::FunctionCallOutputContent::Content(content) => {
            message_content_to_chat_value(content)
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::responses::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn base_test_request() -> ResponsesRequest {
        ResponsesRequest {
            model: "test".to_string(),
            instructions: String::new().into(),
            input: vec![],
            tools: vec![],
            tool_choice: serde_json::Value::String("auto".to_string()),
            parallel_tool_calls: Some(false),
            reasoning: None,
            thinking: None,
            store: false,
            stream: true,
            include: vec![],
            service_tier: None,
            prompt_cache_key: None,
            prompt_cache_retention: None,
            text: None,
            previous_response_id: None,
            llmconduit_replay: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            frequency_penalty: None,
            presence_penalty: None,
            truncation: None,
            metadata: None,
            stop: None,
            extra_body: Default::default(),
        }
    }

    fn user_msg(text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            phase: None,
        }
    }

    #[test]
    fn extension_function_output_parts_are_stringified_for_chat_tools() {
        let output = FunctionCallOutputContent::Content(vec![ContentItem::Other(json!({
            "type": "web_search_result",
            "url": "https://example.com/weather",
            "title": "Weather"
        }))]);

        let Value::String(lowered) =
            function_call_output_to_chat_value(&output).expect("lower function output")
        else {
            panic!("extension result must lower to tool text");
        };
        assert_eq!(
            serde_json::from_str::<Value>(&lowered).expect("serialized JSON tool output"),
            json!([{
                "type": "web_search_result",
                "url": "https://example.com/weather",
                "title": "Weather"
            }])
        );
    }

    #[test]
    fn validate_accepts_stream_false() {
        let mut req = base_test_request();
        req.stream = false;
        assert!(validate_request(&req).is_ok());
    }

    #[test]
    fn validate_accepts_previous_response_id_after_state_resolution() {
        let mut req = base_test_request();
        req.previous_response_id = Some("resp_123".to_string());
        assert!(validate_request(&req).is_ok());
    }

    #[test]
    fn strict_function_arguments_are_checked_against_the_declared_schema() {
        let mut req = base_test_request();
        req.tools = vec![ToolSpec::Function {
            name: "echo".to_string(),
            description: String::new(),
            strict: true,
            parameters: json!({
                "type": "object",
                "properties": { "value": { "type": "string" } },
                "required": ["value"],
                "additionalProperties": false
            }),
        }];
        let registry = build_tool_registry(&req.tools, false).expect("registry");
        assert!(
            registry
                .validate_function_arguments("echo", &json!({ "value": "ok" }))
                .is_ok()
        );
        let error = registry
            .validate_function_arguments("echo", &json!({ "value": 1 }))
            .expect_err("integer must fail the string schema");
        assert_eq!(error.code.as_deref(), Some("invalid_tool_call"));
    }

    #[test]
    fn validate_accepts_all_tool_choice_values() {
        let req = base_test_request();
        assert!(validate_request(&req).is_ok());

        let mut req2 = base_test_request();
        req2.tool_choice = serde_json::Value::String("required".to_string());
        req2.tools = vec![ToolSpec::Function {
            name: "f".to_string(),
            description: "d".to_string(),
            strict: false,
            parameters: json!({}),
        }];
        assert!(validate_request(&req2).is_ok());

        let mut req3 = base_test_request();
        req3.tool_choice = serde_json::Value::String("none".to_string());
        assert!(validate_request(&req3).is_ok());

        let mut req4 = base_test_request();
        req4.tool_choice = json!({"type": "function", "function": {"name": "echo"}});
        req4.tools = vec![ToolSpec::Function {
            name: "echo".to_string(),
            description: "d".to_string(),
            strict: false,
            parameters: json!({}),
        }];
        assert!(validate_request(&req4).is_ok());
    }

    #[test]
    fn validate_accepts_image_generation_tool() {
        let mut req = base_test_request();
        req.tools = vec![ToolSpec::ImageGeneration {
            output_format: None,
        }];
        assert!(validate_request(&req).is_ok());
    }

    #[test]
    fn trailing_reasoning_flushed_as_message() {
        let mut req = base_test_request();
        req.input = vec![
            user_msg("hello"),
            ResponseItem::Reasoning {
                id: "rsn_1".to_string(),
                summary: vec![ReasoningSummaryItem::SummaryText {
                    text: "thinking".to_string(),
                }],
                content: None,
                encrypted_content: None,
            },
        ];
        let result = lower_request(&req, vec![]).unwrap();
        let last = result.messages.last().unwrap();
        assert_eq!(last.role, "assistant");
        assert!(last.reasoning_content.is_some());
        assert!(last.content.is_none());
    }

    #[test]
    fn signed_reasoning_history_preserves_chat_thinking_signature() {
        let mut req = base_test_request();
        req.input = vec![
            user_msg("hello"),
            ResponseItem::Reasoning {
                id: "rsn_1".to_string(),
                summary: Vec::new(),
                content: Some(vec![ReasoningContentItem::ReasoningText {
                    text: "private chain".to_string(),
                }]),
                encrypted_content: Some("sig_history".to_string()),
            },
            ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: "answer".to_string(),
                }],
                phase: None,
            },
        ];

        let result = lower_request(&req, vec![]).unwrap();
        let assistant = result
            .messages
            .iter()
            .find(|message| message.role == "assistant")
            .expect("assistant message");
        assert_eq!(
            assistant.reasoning_content.as_deref(),
            Some("private chain")
        );
        let thinking = assistant.thinking.as_ref().expect("signed thinking");
        assert_eq!(thinking.content, "private chain");
        assert_eq!(thinking.signature.as_deref(), Some("sig_history"));
    }

    #[test]
    fn lowers_reasoning_effort_raw_for_the_leaf() {
        // Lowering passes the canonical level through RAW (no clamp); the leaf
        // clamps or maps it. xhigh/max stay distinct here.
        for raw in ["none", "low", "medium", "high", "xhigh", "max"] {
            let mut req = base_test_request();
            req.reasoning = Some(ReasoningRequest {
                effort: Some(raw.to_string()),
                summary: None,
            });
            let result = lower_request(&req, vec![]).expect("lower_request");
            assert_eq!(
                result.reasoning_effort.as_deref(),
                Some(raw),
                "{raw} must pass through raw"
            );
        }
    }

    #[test]
    fn lowers_reasoning_effort_trimmed_and_lowercased() {
        let mut req = base_test_request();
        req.reasoning = Some(ReasoningRequest {
            effort: Some("  XHigh  ".to_string()),
            summary: None,
        });
        let result = lower_request(&req, vec![]).expect("lower_request");
        assert_eq!(result.reasoning_effort.as_deref(), Some("xhigh"));
    }

    #[test]
    fn role_rules_shape_only_new_tail_then_merge_adjacent() {
        let roles: RolesConfig = serde_json::from_value(json!({
            "merge_adjacent": ["user"],
            "developer": {
                "action": "rewrite",
                "target_role": "user",
                "tag": "instruction",
                "tag_attributes": {"priority": "high & urgent"}
            },
            "user": {},
            "*": {"action": "reject"}
        }))
        .expect("roles config");
        let baseline = vec![ChatMessage {
            role: "developer".to_string(),
            content: Some(json!("already shaped history")),
            tool_call_id: None,
            name: None,
            reasoning_content: None,
            thinking: None,
            tool_calls: None,
        }];
        let mut request = base_test_request();
        request.input = vec![
            ResponseItem::Message {
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: "new policy".to_string(),
                }],
                phase: None,
            },
            user_msg("question"),
        ];

        let lowered =
            lower_request_with_image_agent_and_roles(&request, baseline, false, Some(&roles))
                .expect("lower request");

        assert_eq!(lowered.messages.len(), 2);
        assert_eq!(lowered.messages[0].role, "developer");
        assert_eq!(
            lowered.messages[0].content,
            Some(json!("already shaped history")),
            "replay baseline must not be rewritten or tagged again"
        );
        assert_eq!(lowered.messages[1].role, "user");
        assert_eq!(
            lowered.messages[1].content.as_ref().and_then(Value::as_str),
            Some(
                "<instruction priority=\"high &amp; urgent\">new policy</instruction>\n\nquestion"
            )
        );
    }

    #[test]
    fn merge_adjacent_if_configured_is_idempotent() {
        let roles: RolesConfig = serde_json::from_value(json!({
            "merge_adjacent": ["developer"],
            "developer": {},
            "user": {},
        }))
        .expect("roles");
        let msg = |role: &str, text: &str| ChatMessage {
            role: role.to_string(),
            content: Some(json!(text)),
            tool_call_id: None,
            name: None,
            reasoning_content: None,
            thinking: None,
            tool_calls: None,
        };
        let mut messages = vec![
            msg("developer", "A"),
            msg("developer", "B"),
            msg("user", "q"),
        ];
        merge_adjacent_if_configured(&mut messages, Some(&roles));
        assert_eq!(messages.len(), 2, "adjacent developer run collapsed to one");
        assert_eq!(messages[0].role, "developer");
        assert_eq!(messages[0].content, Some(json!("A\n\nB")));
        assert_eq!(messages[1].role, "user");
        // Re-running before the next upstream send changes nothing (no re-join,
        // no `\n\n` accumulation) — safe to run on every round.
        merge_adjacent_if_configured(&mut messages, Some(&roles));
        assert_eq!(messages.len(), 2, "second pass is a no-op");
        assert_eq!(
            messages[0].content,
            Some(json!("A\n\nB")),
            "no separator accumulation on re-merge"
        );
    }

    #[test]
    fn shape_tail_message_rewrites_inline_system_to_developer() {
        let roles: RolesConfig = serde_json::from_value(json!({
            "system": [
                { "when": "leading" },
                { "when": "inline", "action": "rewrite", "target_role": "developer" }
            ],
            "developer": {},
        }))
        .expect("roles");
        let note = ChatMessage {
            role: "system".to_string(),
            content: Some(json!("closed tool set note")),
            tool_call_id: None,
            name: None,
            reasoning_content: None,
            thinking: None,
            tool_calls: None,
        };
        // A tail append is inline (never leading), so the system note rides the
        // inline rule and is rewritten to developer — mirrors the repair path.
        let shaped = shape_tail_message(note, Some(&roles))
            .expect("shape ok")
            .expect("not dropped");
        assert_eq!(shaped.role, "developer");
        assert_eq!(shaped.content, Some(json!("closed tool set note")));
    }

    #[test]
    fn shape_tail_message_rewrites_tool_to_user() {
        // A no-tool-role template profile rewrites `tool` results to `user`. An
        // internally-injected tool result (repair / server-tool output) must ride
        // the same rule so it is not sent as a raw `tool` role the profile forbids.
        let roles: RolesConfig = serde_json::from_value(json!({
            "merge_adjacent": ["user"],
            "tool": { "action": "rewrite", "target_role": "user" },
            "user": {},
        }))
        .expect("roles");
        let result = ChatMessage {
            role: "tool".to_string(),
            content: Some(json!("tool_unavailable: Grep")),
            tool_call_id: Some("call_bad".to_string()),
            name: None,
            reasoning_content: None,
            thinking: None,
            tool_calls: None,
        };
        let shaped = shape_tail_message(result, Some(&roles))
            .expect("shape ok")
            .expect("not dropped");
        assert_eq!(shaped.role, "user");
        assert_eq!(shaped.content, Some(json!("tool_unavailable: Grep")));
    }

    #[test]
    fn shape_tail_message_passthrough_when_no_roles() {
        let note = ChatMessage {
            role: "system".to_string(),
            content: Some(json!("note")),
            tool_call_id: None,
            name: None,
            reasoning_content: None,
            thinking: None,
            tool_calls: None,
        };
        let shaped = shape_tail_message(note, None)
            .expect("shape ok")
            .expect("not dropped");
        assert_eq!(
            shaped.role, "system",
            "no roles config ⇒ legacy passthrough"
        );
    }

    #[test]
    fn duplicate_tool_name_rejected() {
        // Rejection is the registry's sole responsibility, reached end-to-end
        // via `lower_request`. Exact-case duplicate.
        let mut req = base_test_request();
        req.tools = vec![
            ToolSpec::Function {
                name: "echo".to_string(),
                description: "a".to_string(),
                strict: false,
                parameters: json!({}),
            },
            ToolSpec::Function {
                name: "echo".to_string(),
                description: "b".to_string(),
                strict: false,
                parameters: json!({}),
            },
        ];
        let err = lower_request(&req, vec![]).expect_err("duplicate name must be rejected");
        assert_eq!(err.to_string(), "duplicate tool names are not supported");
    }

    #[test]
    fn duplicate_tool_name_rejected_case_insensitive() {
        // The surviving (registry) check folds case: `echo` vs `ECHO` collide,
        // which the removed case-sensitive `lower_tools` map never caught.
        let mut req = base_test_request();
        req.tools = vec![
            ToolSpec::Function {
                name: "echo".to_string(),
                description: "a".to_string(),
                strict: false,
                parameters: json!({}),
            },
            ToolSpec::Function {
                name: "ECHO".to_string(),
                description: "b".to_string(),
                strict: false,
                parameters: json!({}),
            },
        ];
        let err = lower_request(&req, vec![]).expect_err("case-insensitive duplicate must reject");
        assert_eq!(err.to_string(), "duplicate tool names are not supported");
    }

    #[test]
    fn mixed_text_and_image_content() {
        let content = vec![
            ContentItem::InputText {
                text: "hi".to_string(),
            },
            ContentItem::InputImage {
                image_url: Some("http://img.png".to_string()),
                file_id: None,
                detail: None,
            },
        ];
        let value = message_content_to_chat_value(&content).unwrap();
        assert!(value.is_array());
        let arr = value.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[1]["type"], "image_url");
        assert_eq!(arr[1]["image_url"]["url"], "http://img.png");
    }

    #[test]
    fn single_input_image_wraps_as_array() {
        let item = ContentItem::InputImage {
            image_url: Some("http://img.png".to_string()),
            file_id: None,
            detail: Some("high".to_string()),
        };
        let value = content_item_to_chat_value(&item).unwrap();
        assert!(value.is_array());
        assert_eq!(value[0]["type"], "image_url");
        assert_eq!(value[0]["image_url"]["url"], "http://img.png");
        assert_eq!(value[0]["image_url"]["detail"], "high");
    }

    #[test]
    fn input_file_passes_through_as_content_part() {
        let item = ContentItem::InputFile {
            file_id: Some("file_123".to_string()),
            file_url: None,
            filename: Some("brief.pdf".to_string()),
            file_data: None,
        };
        let value = content_item_to_chat_value(&item).unwrap();
        assert!(value.is_array());
        assert_eq!(value[0]["type"], "input_file");
        assert_eq!(value[0]["file_id"], "file_123");
        assert_eq!(value[0]["filename"], "brief.pdf");
    }

    #[test]
    fn web_search_arguments_all_actions() {
        let search = Some(WebSearchAction::Search {
            query: Some("test".to_string()),
            queries: None,
            sources: None,
        });
        assert_eq!(web_search_arguments(&search), json!({"query": "test"}));

        let open = Some(WebSearchAction::OpenPage {
            url: Some("http://x.com".to_string()),
        });
        assert_eq!(web_search_arguments(&open), json!({"url": "http://x.com"}));

        let find = Some(WebSearchAction::FindInPage {
            url: Some("http://x.com".to_string()),
            pattern: Some("foo".to_string()),
        });
        assert_eq!(
            web_search_arguments(&find),
            json!({"url": "http://x.com", "pattern": "foo"})
        );

        assert_eq!(
            web_search_arguments(&Some(WebSearchAction::Other)),
            json!({})
        );
        assert_eq!(web_search_arguments(&None), json!({}));
    }

    #[test]
    fn web_search_placeholder_result_all_actions() {
        let search = Some(WebSearchAction::Search {
            query: Some("test".to_string()),
            queries: None,
            sources: None,
        });
        assert!(web_search_placeholder_result(&search).contains("test"));

        let open = Some(WebSearchAction::OpenPage {
            url: Some("http://x.com".to_string()),
        });
        assert!(web_search_placeholder_result(&open).contains("http://x.com"));

        let find = Some(WebSearchAction::FindInPage {
            url: Some("http://x.com".to_string()),
            pattern: Some("foo".to_string()),
        });
        let result = web_search_placeholder_result(&find);
        assert!(result.contains("http://x.com"));
        assert!(result.contains("foo"));

        assert!(
            web_search_placeholder_result(&Some(WebSearchAction::Other))
                .contains("replay state was missing")
        );
        assert!(web_search_placeholder_result(&None).contains("replay state was missing"));
    }

    #[test]
    fn web_search_placeholder_result_byte_exact() {
        // Full-string equality guards against wording/spacing/punctuation drift.
        const BASE: &str = "Previous web_search completed in an earlier turn, but the original tool result is unavailable because replay state was missing.";

        // Search: with `query`, with only `queries`, with neither.
        assert_eq!(
            web_search_placeholder_result(&Some(WebSearchAction::Search {
                query: Some("cats".to_string()),
                queries: Some(vec!["ignored".to_string()]),
                sources: None,
            })),
            format!("{BASE} Query: cats")
        );
        assert_eq!(
            web_search_placeholder_result(&Some(WebSearchAction::Search {
                query: None,
                queries: Some(vec!["dogs".to_string(), "second".to_string()]),
                sources: None,
            })),
            format!("{BASE} Query: dogs")
        );
        assert_eq!(
            web_search_placeholder_result(&Some(WebSearchAction::Search {
                query: None,
                queries: None,
                sources: None,
            })),
            BASE
        );

        // OpenPage: with/without url.
        assert_eq!(
            web_search_placeholder_result(&Some(WebSearchAction::OpenPage {
                url: Some("http://x.com".to_string()),
            })),
            "Previous web_search open_page completed in an earlier turn, but the original tool result is unavailable because replay state was missing. URL: http://x.com"
        );
        assert_eq!(
            web_search_placeholder_result(&Some(WebSearchAction::OpenPage { url: None })),
            "Previous web_search open_page completed in an earlier turn, but the original tool result is unavailable because replay state was missing."
        );

        // FindInPage: url+pattern, url-only, pattern-only, neither.
        assert_eq!(
            web_search_placeholder_result(&Some(WebSearchAction::FindInPage {
                url: Some("http://x.com".to_string()),
                pattern: Some("foo".to_string()),
            })),
            "Previous web_search find_in_page completed in an earlier turn, but the original tool result is unavailable because replay state was missing. URL: http://x.com. Pattern: foo"
        );
        assert_eq!(
            web_search_placeholder_result(&Some(WebSearchAction::FindInPage {
                url: Some("http://x.com".to_string()),
                pattern: None,
            })),
            "Previous web_search find_in_page completed in an earlier turn, but the original tool result is unavailable because replay state was missing. URL: http://x.com"
        );
        assert_eq!(
            web_search_placeholder_result(&Some(WebSearchAction::FindInPage {
                url: None,
                pattern: Some("foo".to_string()),
            })),
            "Previous web_search find_in_page completed in an earlier turn, but the original tool result is unavailable because replay state was missing. Pattern: foo"
        );
        assert_eq!(
            web_search_placeholder_result(&Some(WebSearchAction::FindInPage {
                url: None,
                pattern: None,
            })),
            "Previous web_search find_in_page completed in an earlier turn, but the original tool result is unavailable because replay state was missing."
        );

        // Other and None: bare base.
        assert_eq!(
            web_search_placeholder_result(&Some(WebSearchAction::Other)),
            BASE
        );
        assert_eq!(web_search_placeholder_result(&None), BASE);
    }

    #[test]
    fn tool_search_call_non_client_error() {
        let mut req = base_test_request();
        req.input = vec![
            user_msg("hello"),
            ResponseItem::ToolSearchCall {
                id: None,
                call_id: Some("ts_1".to_string()),
                status: None,
                execution: "server".to_string(),
                arguments: json!({}),
            },
        ];
        let result = lower_request(&req, vec![]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("execution=client"));
    }

    #[test]
    fn stringify_tool_output_non_string() {
        assert_eq!(stringify_tool_output(&json!(42)), "42");
        assert_eq!(stringify_tool_output(&json!(true)), "true");
        assert_eq!(stringify_tool_output(&json!({"a": 1})), r#"{"a":1}"#);
    }

    #[test]
    fn tool_call_arguments_object_edge_cases() {
        assert_eq!(tool_call_arguments_object(&None), json!({}));
        assert_eq!(tool_call_arguments_object(&Some(json!("x"))), json!("x"));
        assert_eq!(
            tool_call_arguments_object(&Some(json!({"a": 1}))),
            json!({"a": 1})
        );
    }

    #[test]
    fn hoist_system_messages_non_string_content() {
        let mut messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: Some(json!({"key": "value"})),
                tool_call_id: None,
                name: None,
                reasoning_content: None,
                thinking: None,
                tool_calls: None,
            },
            ChatMessage {
                role: "user".to_string(),
                content: Some(json!("hello")),
                tool_call_id: None,
                name: None,
                reasoning_content: None,
                thinking: None,
                tool_calls: None,
            },
        ];
        hoist_system_messages(&mut messages);
        assert_eq!(messages[0].role, "system");
        let content = messages[0].content.as_ref().unwrap().as_str().unwrap();
        assert!(content.contains("key"));
        assert!(content.contains("value"));
    }

    #[test]
    fn reasoning_item_text_variants() {
        let summary = vec![ReasoningSummaryItem::SummaryText {
            text: "summary".to_string(),
        }];
        let content = Some(vec![
            ReasoningContentItem::ReasoningText {
                text: "reasoning".to_string(),
            },
            ReasoningContentItem::Text {
                text: "text".to_string(),
            },
        ]);
        let result = reasoning_item_text(&summary, &content);
        assert!(result.contains("summary"));
        assert!(result.contains("reasoning"));
        assert!(result.contains("text"));
    }

    // --- C1 tests ---

    #[test]
    fn test_validate_tool_choice_valid_strings() {
        for val in &["auto", "none"] {
            let mut req = base_test_request();
            req.tool_choice = Value::String(val.to_string());
            assert!(validate_request(&req).is_ok(), "expected {val} to pass");
        }
        let mut req = base_test_request();
        req.tool_choice = Value::String("required".to_string());
        req.tools = vec![ToolSpec::Function {
            name: "f".to_string(),
            description: "d".to_string(),
            strict: false,
            parameters: json!({}),
        }];
        assert!(validate_request(&req).is_ok());
    }

    #[test]
    fn test_validate_tool_choice_valid_object() {
        let mut req = base_test_request();
        req.tool_choice = json!({"type": "function", "function": {"name": "foo"}});
        req.tools = vec![ToolSpec::Function {
            name: "foo".to_string(),
            description: "d".to_string(),
            strict: false,
            parameters: json!({}),
        }];
        assert!(validate_request(&req).is_ok());
    }

    #[test]
    fn test_validate_tool_choice_rejects_arbitrary_json() {
        let mut req = base_test_request();
        req.tool_choice = json!(42);
        assert!(validate_request(&req).is_err());

        let mut req2 = base_test_request();
        req2.tool_choice = json!([1, 2, 3]);
        assert!(validate_request(&req2).is_err());

        let mut req3 = base_test_request();
        req3.tool_choice = json!({"type": "unknown"});
        assert!(validate_request(&req3).is_err());

        let mut req4 = base_test_request();
        req4.tool_choice = Value::String("bogus".to_string());
        assert!(validate_request(&req4).is_err());
    }

    #[test]
    fn test_validate_tool_choice_required_without_tools_rejected() {
        let mut req = base_test_request();
        req.tool_choice = Value::String("required".to_string());
        req.tools = vec![];
        assert!(validate_request(&req).is_err());
    }

    // --- M1+M4 tests ---

    #[test]
    fn test_append_tool_call_sequential_indices() {
        let mut messages: Vec<ChatMessage> = vec![];
        for i in 0..3 {
            append_tool_call(
                &mut messages,
                format!("call_{i}"),
                format!("fn_{i}"),
                json!({}),
                None,
            );
        }
        assert_eq!(messages.len(), 1);
        let calls = messages[0].tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].index, Some(0));
        assert_eq!(calls[1].index, Some(1));
        assert_eq!(calls[2].index, Some(2));
    }

    #[test]
    fn test_append_tool_call_no_merge_into_content_message() {
        let mut messages = vec![ChatMessage {
            role: "assistant".to_string(),
            content: Some(Value::String("some text".to_string())),
            tool_call_id: None,
            name: None,
            reasoning_content: None,
            thinking: None,
            tool_calls: None,
        }];
        append_tool_call(
            &mut messages,
            "call_1".to_string(),
            "fn_1".to_string(),
            json!({}),
            None,
        );
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[0].content,
            Some(Value::String("some text".to_string()))
        );
        assert!(messages[0].tool_calls.is_none());
        assert!(messages[1].tool_calls.is_some());
        assert_eq!(messages[1].tool_calls.as_ref().unwrap()[0].index, Some(0));
    }

    // --- M2 test ---

    #[test]
    fn test_hoist_preserves_mid_conversation_system_messages() {
        let mut messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: Some(Value::String("top".to_string())),
                tool_call_id: None,
                name: None,
                reasoning_content: None,
                thinking: None,
                tool_calls: None,
            },
            ChatMessage {
                role: "user".to_string(),
                content: Some(Value::String("hello".to_string())),
                tool_call_id: None,
                name: None,
                reasoning_content: None,
                thinking: None,
                tool_calls: None,
            },
            ChatMessage {
                role: "system".to_string(),
                content: Some(Value::String("mid".to_string())),
                tool_call_id: None,
                name: None,
                reasoning_content: None,
                thinking: None,
                tool_calls: None,
            },
            ChatMessage {
                role: "assistant".to_string(),
                content: Some(Value::String("hi".to_string())),
                tool_call_id: None,
                name: None,
                reasoning_content: None,
                thinking: None,
                tool_calls: None,
            },
        ];
        hoist_system_messages(&mut messages);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, "system");
        assert_eq!(
            messages[0].content.as_ref().unwrap().as_str().unwrap(),
            "top"
        );
        assert_eq!(messages[1].role, "user");
        assert_eq!(messages[2].role, "system");
        assert_eq!(
            messages[2].content.as_ref().unwrap().as_str().unwrap(),
            "mid"
        );
        assert_eq!(messages[3].role, "assistant");
    }

    #[test]
    fn structured_output_validates_json_object_and_schema() {
        let output = |text: &str| {
            vec![ResponseItem::Message {
                id: Some("msg_test".to_string()),
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: text.to_string(),
                }],
                phase: None,
            }]
        };
        let json_object = TextControls {
            verbosity: None,
            format: Some(TextFormat {
                kind: "json_object".to_string(),
                strict: false,
                schema: Value::Null,
                name: String::new(),
                description: None,
            }),
        };
        assert!(validate_structured_output(Some(&json_object), &output(r#"{"ok":true}"#)).is_ok());
        assert!(validate_structured_output(Some(&json_object), &output("[]")).is_err());

        let json_schema = TextControls {
            verbosity: None,
            format: Some(TextFormat {
                kind: "json_schema".to_string(),
                strict: true,
                name: "answer".to_string(),
                description: None,
                schema: json!({
                    "type": "object",
                    "properties": { "answer": { "type": "string" } },
                    "required": ["answer"],
                    "additionalProperties": false
                }),
            }),
        };
        assert!(
            validate_structured_output(Some(&json_schema), &output(r#"{"answer":"yes"}"#)).is_ok()
        );
        let error = validate_structured_output(Some(&json_schema), &output(r#"{"wrong":1}"#))
            .expect_err("schema mismatch");
        assert_eq!(error.code.as_deref(), Some("invalid_structured_output"));
        assert_eq!(
            error.client_message,
            "the model returned invalid structured output"
        );
    }

    #[test]
    fn structured_output_enforces_refs_unions_and_standard_constraints() {
        let schema = json!({
            "type": "object",
            "$defs": {
                "result": {
                    "type": "object",
                    "properties": {
                        "count": {
                            "type": "integer",
                            "minimum": 0,
                            "exclusiveMaximum": 10,
                            "multipleOf": 2
                        },
                        "code": {
                            "type": ["string", "null"],
                            "minLength": 3,
                            "maxLength": 8,
                            "pattern": "^[A-Z]+$"
                        }
                    },
                    "required": ["count", "code"],
                    "additionalProperties": false
                }
            },
            "properties": {
                "result": {
                    "anyOf": [
                        { "$ref": "#/$defs/result" },
                        { "type": "null" }
                    ]
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string", "minLength": 1 },
                    "minItems": 1,
                    "maxItems": 2,
                    "uniqueItems": true
                }
            },
            "required": ["result", "tags"],
            "additionalProperties": false
        });

        validate_json_schema_value(
            &schema,
            &json!({ "result": { "count": 4, "code": null }, "tags": ["a", "b"] }),
            "$",
            0,
        )
        .expect("valid constrained output");
        validate_json_schema_value(&schema, &json!({ "result": null, "tags": ["a"] }), "$", 0)
            .expect("nullable anyOf branch");

        for invalid in [
            json!({ "result": { "count": 3, "code": "ABC" }, "tags": ["a"] }),
            json!({ "result": { "count": 4, "code": "lower" }, "tags": ["a"] }),
            json!({ "result": { "count": 4, "code": "ABC" }, "tags": ["a", "a"] }),
        ] {
            validate_json_schema_value(&schema, &invalid, "$", 0)
                .expect_err("constraint mismatch must be rejected");
        }
    }

    #[test]
    fn schema_valued_additional_properties_are_enforced() {
        let schema = json!({
            "type": "object",
            "properties": { "known": { "type": "string" } },
            "additionalProperties": {
                "type": "integer",
                "minimum": 0
            }
        });

        validate_json_schema_value(&schema, &json!({ "known": "ok", "dynamic": 2 }), "$", 0)
            .expect("schema-conformant additional property");
        let error =
            validate_json_schema_value(&schema, &json!({ "known": "ok", "dynamic": -1 }), "$", 0)
                .expect_err("additional property schema must be enforced");
        assert!(error.contains("$.dynamic"), "unexpected error: {error}");
    }
}
