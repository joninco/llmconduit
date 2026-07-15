//! Conservative OpenAI Responses capability declarations and request gating.
//!
//! Capability checks run after routing has selected a primary candidate.  The
//! primary must support every requested feature; fallback candidates that do
//! not are excluded from that request without affecting health or cooldown.

use crate::error::{AppError, AppResult};
use crate::models::responses::{
    AgentMessageInputContent, ContentItem, FunctionCallOutputContent, ResponseItem,
    ResponsesRequest, ToolSpec,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::Arc;

/// Internal, consumed-before-dispatch marker distinguishing raw Responses
/// ingress from Chat/Anthropic requests converted through the canonical model.
pub const ENFORCE_EXTENSION: &str = "llmconduit_responses_capability_validation";
pub const FORWARD_PROMPT_CACHE_KEY_EXTENSION: &str = "llmconduit_forward_prompt_cache_key";
/// Internal SHA-256 cache-affinity namespace. This is consumed before upstream
/// serialization and never contains the caller's opaque key.
pub const PROMPT_CACHE_AFFINITY_EXTENSION: &str = "llmconduit_prompt_cache_affinity_sha256";
/// Internal, consumed-before-dispatch marker permitting the selected local
/// Chat backend to interpret Codex v2 agent-message encrypted content as the
/// plaintext tool payload that Codex placed in that channel.
pub const AGENT_MESSAGE_PLAINTEXT_COMPAT_EXTENSION: &str =
    "llmconduit_agent_message_plaintext_compat";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuredOutputCapability {
    Text,
    JsonObject,
    JsonSchema,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSummaryCapability {
    Upstream,
    #[default]
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EncryptedReasoningCapability {
    Passthrough,
    #[default]
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentMessageEncryptedContentCapability {
    PlaintextCompat,
    #[default]
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InputImageCapability {
    Native,
    Agent,
    Placeholder,
    #[default]
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InputFileCapability {
    Native,
    #[default]
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TruncationAutoCapability {
    Upstream,
    #[default]
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TextVerbosityCapability {
    Upstream,
    #[default]
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheKeyCapability {
    GatewayHash,
    Upstream,
    #[default]
    Unsupported,
}

/// Partial declaration used at the global, provider, and model-profile layers.
/// `None` means "inherit" while a resolved missing value is conservative.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ResponsesCapabilitiesConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_outputs: Option<Vec<StructuredOutputCapability>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_summary: Option<ReasoningSummaryCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_reasoning: Option<EncryptedReasoningCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_message_encrypted_content: Option<AgentMessageEncryptedContentCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_image: Option<InputImageCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_file: Option<InputFileCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation_auto: Option<TruncationAutoCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_verbosity: Option<TextVerbosityCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tiers: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<PromptCacheKeyCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_retention: Option<Vec<String>>,
}

impl ResponsesCapabilitiesConfig {
    /// Overlay explicitly-declared fields from `other` onto this layer.
    pub fn overlay(&mut self, other: &Self) {
        macro_rules! overlay {
            ($field:ident) => {
                if other.$field.is_some() {
                    self.$field.clone_from(&other.$field);
                }
            };
        }
        overlay!(parallel_tool_calls);
        overlay!(structured_outputs);
        overlay!(reasoning_summary);
        overlay!(encrypted_reasoning);
        overlay!(agent_message_encrypted_content);
        overlay!(input_image);
        overlay!(input_file);
        overlay!(truncation_auto);
        overlay!(text_verbosity);
        overlay!(service_tiers);
        overlay!(prompt_cache_key);
        overlay!(prompt_cache_retention);
    }

    pub fn overlaid(mut self, other: &Self) -> Self {
        self.overlay(other);
        self
    }

    pub fn resolve(&self) -> ResponsesCapabilities {
        ResponsesCapabilities {
            parallel_tool_calls: self.parallel_tool_calls.unwrap_or(false),
            structured_outputs: self
                .structured_outputs
                .clone()
                .unwrap_or_else(|| vec![StructuredOutputCapability::Text]),
            reasoning_summary: self.reasoning_summary.unwrap_or_default(),
            encrypted_reasoning: self.encrypted_reasoning.unwrap_or_default(),
            agent_message_encrypted_content: self
                .agent_message_encrypted_content
                .unwrap_or_default(),
            // Raw Responses capability metadata is conservative by default.
            // Chat/Anthropic ingress is not gated here and retains the gateway's
            // established placeholder/agent behavior.
            input_image: self.input_image.unwrap_or_default(),
            input_file: self.input_file.unwrap_or_default(),
            truncation_auto: self.truncation_auto.unwrap_or_default(),
            text_verbosity: self.text_verbosity.unwrap_or_default(),
            service_tiers: self.service_tiers.clone().unwrap_or_default(),
            prompt_cache_key: self.prompt_cache_key.unwrap_or_default(),
            prompt_cache_retention: self.prompt_cache_retention.clone().unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponsesCapabilities {
    pub parallel_tool_calls: bool,
    pub structured_outputs: Vec<StructuredOutputCapability>,
    pub reasoning_summary: ReasoningSummaryCapability,
    pub encrypted_reasoning: EncryptedReasoningCapability,
    pub agent_message_encrypted_content: AgentMessageEncryptedContentCapability,
    pub input_image: InputImageCapability,
    pub input_file: InputFileCapability,
    pub truncation_auto: TruncationAutoCapability,
    pub text_verbosity: TextVerbosityCapability,
    pub service_tiers: Vec<String>,
    pub prompt_cache_key: PromptCacheKeyCapability,
    pub prompt_cache_retention: Vec<String>,
}

impl Default for ResponsesCapabilities {
    fn default() -> Self {
        ResponsesCapabilitiesConfig::default().resolve()
    }
}

/// Stable provider+served-model identity for per-request fallback filtering.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CapabilityTarget {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone)]
pub struct CapabilityCandidate {
    pub target: CapabilityTarget,
    pub capabilities: ResponsesCapabilities,
}

#[derive(Debug, Clone, Default)]
pub struct CapabilityPlan {
    pub candidates: Vec<CapabilityCandidate>,
}

#[derive(Debug, Clone, Default)]
pub struct CapabilityAllowlist {
    targets: Option<Arc<HashSet<CapabilityTarget>>>,
    input_image: Option<InputImageCapability>,
}

impl CapabilityAllowlist {
    pub fn unrestricted() -> Self {
        Self::default()
    }

    pub fn from_targets(targets: impl IntoIterator<Item = CapabilityTarget>) -> Self {
        Self {
            targets: Some(Arc::new(targets.into_iter().collect())),
            input_image: None,
        }
    }

    fn for_responses(
        targets: impl IntoIterator<Item = CapabilityTarget>,
        input_image: InputImageCapability,
    ) -> Self {
        Self {
            targets: Some(Arc::new(targets.into_iter().collect())),
            input_image: Some(input_image),
        }
    }

    pub fn permits(&self, provider: &str, model: &str) -> bool {
        self.targets.as_ref().is_none_or(|targets| {
            targets.contains(&CapabilityTarget {
                provider: provider.to_string(),
                model: model.to_string(),
            })
        })
    }

    /// Whether any capability-compatible provider serves `model`. Native-image
    /// safety uses this only to omit fallbacks already pruned by provider+model;
    /// native vision itself is still checked from the concrete model profile.
    pub fn permits_model(&self, model: &str) -> bool {
        self.targets
            .as_ref()
            .is_none_or(|targets| targets.iter().any(|target| target.model == model))
    }

    /// Present only for raw Responses ingress, whose primary capability must
    /// drive image handling instead of the gateway-wide legacy image policy.
    pub fn input_image_policy(&self) -> Option<InputImageCapability> {
        self.input_image
    }
}

/// Return the first exact request path unsupported by `capabilities`.
pub fn unsupported_parameter(
    request: &ResponsesRequest,
    capabilities: &ResponsesCapabilities,
) -> Option<String> {
    for field in [
        "background",
        "conversation",
        "max_tool_calls",
        "prompt",
        "safety_identifier",
        "top_logprobs",
    ] {
        if request.extra_body.contains_key(field) {
            return Some(field.to_string());
        }
    }

    if request.parallel_tool_calls == Some(true) && !capabilities.parallel_tool_calls {
        return Some("parallel_tool_calls".to_string());
    }

    if let Some(format) = request.text.as_ref().and_then(|text| text.format.as_ref()) {
        let required = match format.kind.as_str() {
            "text" => StructuredOutputCapability::Text,
            "json_object" => StructuredOutputCapability::JsonObject,
            "json_schema" => StructuredOutputCapability::JsonSchema,
            _ => return Some("text.format.type".to_string()),
        };
        if !capabilities.structured_outputs.contains(&required) {
            return Some("text.format".to_string());
        }
    }

    if request
        .reasoning
        .as_ref()
        .and_then(|reasoning| reasoning.summary.as_deref())
        .is_some_and(|summary| summary != "none")
        && capabilities.reasoning_summary != ReasoningSummaryCapability::Upstream
    {
        return Some("reasoning.summary".to_string());
    }

    let encrypted_in_history = request
        .instructions
        .items()
        .unwrap_or_default()
        .iter()
        .chain(request.input.iter())
        .any(|item| {
            matches!(
                item,
                ResponseItem::Reasoning {
                    encrypted_content: Some(_),
                    ..
                }
            )
        });
    for (index, include) in request.include.iter().enumerate() {
        if include == "reasoning.encrypted_content" {
            if capabilities.encrypted_reasoning != EncryptedReasoningCapability::Passthrough {
                return Some(format!("include[{index}]"));
            }
        } else if include == "web_search_call.action.sources" {
            // Gateway-owned web search can attach its measured URL sources to
            // the standard web_search_call action when explicitly requested.
        } else {
            return Some(format!("include[{index}]"));
        }
    }
    if encrypted_in_history
        && capabilities.encrypted_reasoning != EncryptedReasoningCapability::Passthrough
    {
        return Some(first_encrypted_reasoning_path(request));
    }

    if let Some(param) = unsupported_items_parameter(
        request.instructions.items().unwrap_or_default(),
        "instructions",
        capabilities,
    ) {
        return Some(param);
    }
    if let Some(param) = unsupported_items_parameter(&request.input, "input", capabilities) {
        return Some(param);
    }

    if request.truncation.as_ref().is_some_and(|value| {
        matches!(value.as_str(), Some(mode) if mode != "disabled") || !value.is_string()
    }) && capabilities.truncation_auto != TruncationAutoCapability::Upstream
    {
        return Some("truncation".to_string());
    }

    if request
        .text
        .as_ref()
        .and_then(|text| text.verbosity.as_ref())
        .is_some()
        && capabilities.text_verbosity != TextVerbosityCapability::Upstream
    {
        return Some("text.verbosity".to_string());
    }

    if let Some(tier) = request.service_tier.as_deref()
        && !capabilities
            .service_tiers
            .iter()
            .any(|allowed| allowed == tier)
    {
        return Some("service_tier".to_string());
    }

    if request.prompt_cache_key.is_some()
        && capabilities.prompt_cache_key == PromptCacheKeyCapability::Unsupported
    {
        return Some("prompt_cache_key".to_string());
    }

    if let Some(retention) = request.prompt_cache_retention.as_deref()
        && !capabilities
            .prompt_cache_retention
            .iter()
            .any(|allowed| allowed == retention)
    {
        return Some("prompt_cache_retention".to_string());
    }

    if let Some((index, _)) = request
        .tools
        .iter()
        .enumerate()
        .find(|(_, tool)| matches!(tool, ToolSpec::ImageGeneration { .. }))
    {
        return Some(format!("tools[{index}]"));
    }

    // The gateway-owned Brave executor currently implements only the bare
    // `web_search` query action.  These standard controls must not be accepted
    // and then silently discarded: doing so changes the caller's requested
    // search scope/privacy policy while falsely advertising success.  Keep the
    // paths precise so OpenAI clients can identify the unsupported field.
    for (index, tool) in request.tools.iter().enumerate() {
        let ToolSpec::WebSearch {
            external_web_access,
            filters,
            user_location,
            search_context_size,
            search_content_types,
        } = tool
        else {
            continue;
        };
        if external_web_access.is_some() {
            return Some(format!("tools[{index}].external_web_access"));
        }
        if filters.is_some() {
            return Some(format!("tools[{index}].filters"));
        }
        if user_location.is_some() {
            return Some(format!("tools[{index}].user_location"));
        }
        if search_context_size.is_some() {
            return Some(format!("tools[{index}].search_context_size"));
        }
        if search_content_types.is_some() {
            return Some(format!("tools[{index}].search_content_types"));
        }
    }

    None
}

fn unsupported_items_parameter(
    items: &[ResponseItem],
    base: &str,
    capabilities: &ResponsesCapabilities,
) -> Option<String> {
    for (item_index, item) in items.iter().enumerate() {
        match item {
            ResponseItem::Message { content, .. } => {
                if let Some(param) = unsupported_content_parameter(
                    content,
                    &format!("{base}[{item_index}].content"),
                    capabilities,
                    false,
                ) {
                    return Some(param);
                }
            }
            ResponseItem::AgentMessage { content, .. } => {
                if capabilities.agent_message_encrypted_content
                    != AgentMessageEncryptedContentCapability::PlaintextCompat
                    && let Some(content_index) = content.iter().position(|part| {
                        matches!(part, AgentMessageInputContent::EncryptedContent { .. })
                    })
                {
                    return Some(format!("{base}[{item_index}].content[{content_index}]"));
                }
            }
            ResponseItem::FunctionCallOutput {
                output: FunctionCallOutputContent::Content(content),
                ..
            }
            | ResponseItem::CustomToolCallOutput {
                output: FunctionCallOutputContent::Content(content),
                ..
            } => {
                if let Some(param) = unsupported_content_parameter(
                    content,
                    &format!("{base}[{item_index}].output"),
                    capabilities,
                    true,
                ) {
                    return Some(param);
                }
            }
            ResponseItem::ImageGenerationCall { .. } => {
                return Some(format!("{base}[{item_index}]"));
            }
            _ => {}
        }
    }

    None
}

fn unsupported_content_parameter(
    content: &[ContentItem],
    base: &str,
    capabilities: &ResponsesCapabilities,
    function_output: bool,
) -> Option<String> {
    for (content_index, part) in content.iter().enumerate() {
        let param = format!("{base}[{content_index}]");
        match part {
            ContentItem::InputImage { .. }
                if capabilities.input_image == InputImageCapability::Reject =>
            {
                return Some(param);
            }
            ContentItem::InputFile { .. }
                if capabilities.input_file != InputFileCapability::Native =>
            {
                return Some(param);
            }
            ContentItem::OutputText { .. } | ContentItem::Refusal { .. } if function_output => {
                return Some(param);
            }
            ContentItem::Other(_) => return Some(param),
            _ => {}
        }
    }
    None
}

pub fn request_has_input_images(request: &ResponsesRequest) -> bool {
    first_input_image_parameter(request).is_some()
}

pub fn first_input_image_parameter(request: &ResponsesRequest) -> Option<String> {
    first_image_in_items(
        request.instructions.items().unwrap_or_default(),
        "instructions",
    )
    .or_else(|| first_image_in_items(&request.input, "input"))
}

fn first_image_in_items(items: &[ResponseItem], base: &str) -> Option<String> {
    items.iter().enumerate().find_map(|(item_index, item)| {
        let (content, base) = match item {
            ResponseItem::Message { content, .. } => {
                (content.as_slice(), format!("{base}[{item_index}].content"))
            }
            ResponseItem::FunctionCallOutput {
                output: FunctionCallOutputContent::Content(content),
                ..
            }
            | ResponseItem::CustomToolCallOutput {
                output: FunctionCallOutputContent::Content(content),
                ..
            } => (content.as_slice(), format!("{base}[{item_index}].output")),
            _ => return None,
        };
        content
            .iter()
            .position(|part| matches!(part, ContentItem::InputImage { .. }))
            .map(|content_index| format!("{base}[{content_index}]"))
    })
}

fn first_encrypted_reasoning_path(request: &ResponsesRequest) -> String {
    for (base, items) in request
        .instructions
        .items()
        .into_iter()
        .map(|items| ("instructions", items))
        .chain(std::iter::once(("input", request.input.as_slice())))
    {
        if let Some(index) = items.iter().position(|item| {
            matches!(
                item,
                ResponseItem::Reasoning {
                    encrypted_content: Some(_),
                    ..
                }
            )
        }) {
            return format!("{base}[{index}].encrypted_content");
        }
    }
    "input".to_string()
}

/// Validate the selected primary, build the fallback allowlist, and prepare
/// fields whose declared capability requires local or upstream handling.
pub fn validate_and_prepare(
    request: &mut ResponsesRequest,
    plan: &CapabilityPlan,
) -> AppResult<CapabilityAllowlist> {
    let Some(primary) = plan.candidates.first() else {
        // A catalog outage must not invent capabilities. The conservative
        // default still permits baseline text and existing safe image policy.
        let capabilities = ResponsesCapabilities::default();
        if let Some(param) = unsupported_parameter(request, &capabilities) {
            return Err(AppError::unsupported_parameter(param));
        }
        prepare_capability_fields(request, &capabilities);
        return Ok(CapabilityAllowlist {
            targets: None,
            input_image: Some(capabilities.input_image),
        });
    };

    if let Some(param) = unsupported_parameter(request, &primary.capabilities) {
        return Err(AppError::unsupported_parameter(param));
    }

    let has_input_images = request_has_input_images(request);
    let allowed = plan
        .candidates
        .iter()
        .filter(|candidate| {
            unsupported_parameter(request, &candidate.capabilities).is_none()
                && (!has_input_images
                    || candidate.capabilities.input_image == primary.capabilities.input_image)
        })
        .map(|candidate| candidate.target.clone())
        .collect::<Vec<_>>();
    prepare_capability_fields(request, &primary.capabilities);
    Ok(CapabilityAllowlist::for_responses(
        allowed,
        primary.capabilities.input_image,
    ))
}

fn prepare_capability_fields(request: &mut ResponsesRequest, capabilities: &ResponsesCapabilities) {
    if request.parallel_tool_calls.is_none() {
        request.parallel_tool_calls = Some(capabilities.parallel_tool_calls);
    }
    if capabilities.agent_message_encrypted_content
        == AgentMessageEncryptedContentCapability::PlaintextCompat
    {
        request.extra_body.insert(
            AGENT_MESSAGE_PLAINTEXT_COMPAT_EXTENSION.to_string(),
            Value::Bool(true),
        );
    } else {
        request
            .extra_body
            .remove(AGENT_MESSAGE_PLAINTEXT_COMPAT_EXTENSION);
    }
    if let Some(key) = request.prompt_cache_key.as_ref() {
        let hash = format!("{:x}", Sha256::digest(key.as_bytes()));
        request.extra_body.insert(
            PROMPT_CACHE_AFFINITY_EXTENSION.to_string(),
            hash.clone().into(),
        );
        match capabilities.prompt_cache_key {
            PromptCacheKeyCapability::GatewayHash => {}
            PromptCacheKeyCapability::Upstream => {
                request
                    .extra_body
                    .insert(FORWARD_PROMPT_CACHE_KEY_EXTENSION.to_string(), true.into());
            }
            PromptCacheKeyCapability::Unsupported => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(value: serde_json::Value) -> ResponsesRequest {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn capability_config_overlays_by_field() {
        let base: ResponsesCapabilitiesConfig = serde_json::from_value(serde_json::json!({
            "parallel_tool_calls": true,
            "service_tiers": ["default"]
        }))
        .unwrap();
        let child: ResponsesCapabilitiesConfig = serde_json::from_value(serde_json::json!({
            "parallel_tool_calls": false,
            "prompt_cache_key": "gateway_hash",
            "agent_message_encrypted_content": "plaintext_compat"
        }))
        .unwrap();
        let resolved = base.overlaid(&child).resolve();
        assert!(!resolved.parallel_tool_calls);
        assert_eq!(resolved.service_tiers, ["default"]);
        assert_eq!(
            resolved.prompt_cache_key,
            PromptCacheKeyCapability::GatewayHash
        );
        assert_eq!(
            resolved.agent_message_encrypted_content,
            AgentMessageEncryptedContentCapability::PlaintextCompat
        );
    }

    #[test]
    fn exact_unsupported_paths_are_reported() {
        let input_request = request(serde_json::json!({
            "model": "m",
            "input": [{"role":"user","content":[{"type":"input_file","file_id":"f"}]}]
        }));
        assert_eq!(
            unsupported_parameter(&input_request, &ResponsesCapabilities::default()).as_deref(),
            Some("input[0].content[0]")
        );

        let instructions = request(serde_json::json!({
            "model": "m",
            "instructions": [{
                "role":"developer",
                "content":[{"type":"input_file","file_id":"f"}]
            }],
            "input": "hello"
        }));
        assert_eq!(
            unsupported_parameter(&instructions, &ResponsesCapabilities::default()).as_deref(),
            Some("instructions[0].content[0]")
        );
    }

    #[test]
    fn primary_rejects_but_incapable_fallback_is_only_pruned() {
        let mut request = request(serde_json::json!({
            "model": "m",
            "input": "hello",
            "parallel_tool_calls": true
        }));
        let capable = ResponsesCapabilitiesConfig {
            parallel_tool_calls: Some(true),
            ..Default::default()
        }
        .resolve();
        let plan = CapabilityPlan {
            candidates: vec![
                CapabilityCandidate {
                    target: CapabilityTarget {
                        provider: "primary".into(),
                        model: "m".into(),
                    },
                    capabilities: capable,
                },
                CapabilityCandidate {
                    target: CapabilityTarget {
                        provider: "fallback".into(),
                        model: "m".into(),
                    },
                    capabilities: ResponsesCapabilities::default(),
                },
            ],
        };
        let allowlist = validate_and_prepare(&mut request, &plan).unwrap();
        assert!(allowlist.permits("primary", "m"));
        assert!(!allowlist.permits("fallback", "m"));
    }

    #[test]
    fn cache_key_affinity_is_hashed_without_replacing_public_value() {
        let mut request = request(serde_json::json!({
            "model": "m", "input": "hello", "prompt_cache_key": "opaque-secret"
        }));
        let caps = ResponsesCapabilitiesConfig {
            prompt_cache_key: Some(PromptCacheKeyCapability::GatewayHash),
            ..Default::default()
        }
        .resolve();
        prepare_capability_fields(&mut request, &caps);
        assert_eq!(request.prompt_cache_key.as_deref(), Some("opaque-secret"));
        let hashed = request
            .extra_body
            .get(PROMPT_CACHE_AFFINITY_EXTENSION)
            .and_then(serde_json::Value::as_str)
            .expect("hashed affinity");
        assert_eq!(hashed.len(), 64);
        assert!(!hashed.contains("opaque-secret"));
        assert!(!request.extra_body.contains_key("prompt_cache_key"));
    }

    #[test]
    fn web_search_controls_are_rejected_with_exact_paths() {
        for (field, value) in [
            ("external_web_access", serde_json::json!(false)),
            (
                "filters",
                serde_json::json!({"allowed_domains": ["example.com"]}),
            ),
            ("user_location", serde_json::json!({"country": "US"})),
            ("search_context_size", serde_json::json!("low")),
            ("search_content_types", serde_json::json!(["text"])),
        ] {
            let mut tool = serde_json::json!({"type": "web_search"});
            tool.as_object_mut()
                .unwrap()
                .insert(field.to_string(), value);
            let request = request(serde_json::json!({
                "model": "m",
                "input": "hello",
                "tools": [tool]
            }));
            let expected = format!("tools[0].{field}");
            assert_eq!(
                unsupported_parameter(&request, &ResponsesCapabilities::default()).as_deref(),
                Some(expected.as_str()),
            );
        }

        let bare = request(serde_json::json!({
            "model": "m",
            "input": "hello",
            "tools": [{"type": "web_search"}]
        }));
        assert_eq!(
            unsupported_parameter(&bare, &ResponsesCapabilities::default()),
            None,
        );
    }
}
