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
    pub tool_choice: String,
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
}

fn default_tool_choice() -> String {
    "auto".to_string()
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
