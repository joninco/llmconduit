use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    #[serde(default)]
    pub instructions: String,
    #[serde(default)]
    pub input: Vec<ResponseItem>,
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    #[serde(default = "default_tool_choice")]
    pub tool_choice: Value,
    #[serde(default)]
    pub parallel_tool_calls: bool,
    #[serde(default)]
    pub reasoning: Option<ReasoningRequest>,
    #[serde(default)]
    pub store: bool,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub service_tier: Option<String>,
    #[serde(default)]
    pub prompt_cache_key: Option<String>,
    #[serde(default)]
    pub text: Option<TextControls>,
    #[serde(default)]
    pub client_metadata: Option<HashMap<String, String>>,
    #[serde(default)]
    pub previous_response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i64>,
}

fn default_tool_choice() -> Value {
    Value::String("auto".to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolSpec {
    Function {
        name: String,
        description: String,
        #[serde(default)]
        strict: bool,
        parameters: Value,
    },
    Namespace {
        name: String,
        description: String,
        tools: Vec<NamespaceToolSpec>,
    },
    ToolSearch {
        execution: String,
        description: String,
        parameters: Value,
    },
    LocalShell {},
    WebSearch {
        #[serde(default)]
        external_web_access: Option<bool>,
        #[serde(default)]
        filters: Option<Value>,
        #[serde(default)]
        user_location: Option<Value>,
        #[serde(default)]
        search_context_size: Option<String>,
        #[serde(default)]
        search_content_types: Option<Vec<String>>,
    },
    Custom {
        name: String,
        description: String,
        format: CustomToolFormat,
    },
    ImageGeneration {
        #[serde(default)]
        output_format: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NamespaceToolSpec {
    Function {
        name: String,
        description: String,
        #[serde(default)]
        strict: bool,
        parameters: Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CustomToolFormat {
    #[serde(rename = "type")]
    pub kind: String,
    pub syntax: String,
    pub definition: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningRequest {
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextControls {
    #[serde(default)]
    pub verbosity: Option<String>,
    #[serde(default)]
    pub format: Option<TextFormat>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextFormat {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub strict: bool,
    pub schema: Value,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentItem {
    InputText { text: String },
    InputImage { image_url: String },
    OutputText { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningSummaryItem {
    SummaryText { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningContentItem {
    ReasoningText { text: String },
    Text { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseItem {
    Message {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        role: String,
        content: Vec<ContentItem>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phase: Option<String>,
    },
    Reasoning {
        #[serde(default = "default_reasoning_id")]
        id: String,
        #[serde(default)]
        summary: Vec<ReasoningSummaryItem>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<Vec<ReasoningContentItem>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
    FunctionCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        arguments: String,
        call_id: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: Value,
    },
    CustomToolCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        call_id: String,
        name: String,
        input: String,
    },
    CustomToolCallOutput {
        call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        output: Value,
    },
    ToolSearchCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        execution: String,
        arguments: Value,
    },
    ToolSearchOutput {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        status: String,
        execution: String,
        tools: Vec<Value>,
    },
    LocalShellCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        status: String,
        action: LocalShellAction,
    },
    WebSearchCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<WebSearchAction>,
    },
    ImageGenerationCall {
        id: String,
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revised_prompt: Option<String>,
        result: String,
    },
}

fn default_reasoning_id() -> String {
    "rsn_pending".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LocalShellAction {
    Exec(LocalShellExecAction),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocalShellExecAction {
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WebSearchAction {
    Search {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        queries: Option<Vec<String>>,
    },
    OpenPage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    FindInPage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
    },
    Other,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponsesEnvelope<T> {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub payload: T,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponseCreatedPayload {
    pub response: ResponseStub,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponseStub {
    pub id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponseCompletedPayload {
    pub response: ResponseCompleted,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponseCompleted {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<ResponseUsage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponseUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens_details: Option<ResponseInputTokensDetails>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens_details: Option<ResponseOutputTokensDetails>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponseInputTokensDetails {
    pub cached_tokens: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponseOutputTokensDetails {
    pub reasoning_tokens: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutputItemPayload {
    pub item: ResponseItem,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeltaPayload {
    pub delta: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReasoningDeltaPayload {
    pub delta: String,
    pub content_index: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FailedPayload {
    pub response: FailedResponse,
}

#[derive(Debug, Clone, Serialize)]
pub struct FailedResponse {
    pub error: FailedError,
}

#[derive(Debug, Clone, Serialize)]
pub struct FailedError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TextDonePayload {
    pub text: String,
    pub content_index: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionCallArgsDeltaPayload {
    pub call_id: String,
    pub delta: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionCallArgsDonePayload {
    pub call_id: String,
    pub name: String,
    pub arguments: String,
}

impl ResponseItem {
    pub fn message_text(role: impl Into<String>, text: impl Into<String>) -> Self {
        Self::Message {
            id: None,
            role: role.into(),
            content: vec![ContentItem::OutputText { text: text.into() }],
            phase: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn response_item_message_serde_roundtrip() {
        let item = ResponseItem::Message {
            id: Some("msg_1".to_string()),
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "hello".to_string(),
            }],
            phase: None,
        };
        let json = serde_json::to_string(&item).unwrap();
        let roundtripped: ResponseItem = serde_json::from_str(&json).unwrap();
        assert_eq!(item, roundtripped);
    }

    #[test]
    fn response_item_function_call_serde_roundtrip() {
        let item = ResponseItem::FunctionCall {
            id: Some("fc_1".to_string()),
            name: "calculator".to_string(),
            namespace: Some("mcp__math".to_string()),
            arguments: r#"{"expr":"1+1"}"#.to_string(),
            call_id: "call_1".to_string(),
        };
        let json = serde_json::to_string(&item).unwrap();
        let roundtripped: ResponseItem = serde_json::from_str(&json).unwrap();
        assert_eq!(item, roundtripped);
    }

    #[test]
    fn response_item_web_search_call_serde_roundtrip() {
        let item = ResponseItem::WebSearchCall {
            id: Some("ws_1".to_string()),
            status: Some("completed".to_string()),
            action: Some(WebSearchAction::Search {
                query: Some("rust async".to_string()),
                queries: None,
            }),
        };
        let json = serde_json::to_string(&item).unwrap();
        let roundtripped: ResponseItem = serde_json::from_str(&json).unwrap();
        assert_eq!(item, roundtripped);
    }
}
