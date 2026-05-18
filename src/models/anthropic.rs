use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Default helpers
// ---------------------------------------------------------------------------

fn default_input_schema() -> Value {
    json!({"type": "object"})
}

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicRequest {
    pub model: String,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub system: Option<AnthropicSystemContent>,
    pub messages: Vec<AnthropicMessage>,
    #[serde(default)]
    pub tools: Option<Vec<AnthropicTool>>,
    #[serde(default)]
    pub tool_choice: Option<AnthropicToolChoice>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub top_k: Option<u32>,
    #[serde(default)]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    pub metadata: Option<Value>,
    #[serde(default)]
    pub thinking: Option<AnthropicThinking>,
    #[serde(default)]
    pub output_config: Option<Value>,
}

#[derive(Debug, Clone)]
pub enum AnthropicSystemContent {
    Text(String),
    Blocks(Vec<AnthropicTextBlock>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicTextBlock {
    Text { text: String },
}

#[derive(Debug, Clone)]
pub enum AnthropicContent {
    Text(String),
    Blocks(Vec<AnthropicContentBlock>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicContentBlock {
    Text {
        text: String,
    },
    Image {
        source: AnthropicImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    Thinking {
        thinking: String,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: Option<AnthropicContent>,
        #[serde(default)]
        is_error: Option<bool>,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicImageSource {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub media_type: Option<String>,
    #[serde(default)]
    pub data: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub file_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: AnthropicContent,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicTool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "default_input_schema")]
    pub input_schema: Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicToolChoice {
    Auto,
    Any,
    None,
    Tool { name: String },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicThinking {
    Enabled { budget_tokens: Option<u64> },
    Adaptive { budget_tokens: Option<u64> },
    Disabled,
}

// Custom Deserialize for AnthropicSystemContent (string or array)
impl<'de> Deserialize<'de> for AnthropicSystemContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        match &value {
            Value::String(text) => Ok(Self::Text(text.clone())),
            Value::Array(_) => serde_json::from_value::<Vec<AnthropicTextBlock>>(value)
                .map(Self::Blocks)
                .map_err(serde::de::Error::custom),
            Value::Null => Ok(Self::Text(String::new())),
            _ => Err(serde::de::Error::custom(
                "expected string or array for system content",
            )),
        }
    }
}

// Custom Deserialize for AnthropicContent (string or array)
impl<'de> Deserialize<'de> for AnthropicContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        match &value {
            Value::String(text) => Ok(Self::Text(text.clone())),
            Value::Array(_) => serde_json::from_value::<Vec<AnthropicContentBlock>>(value)
                .map(Self::Blocks)
                .map_err(serde::de::Error::custom),
            Value::Null => Ok(Self::Text(String::new())),
            _ => Err(serde::de::Error::custom(
                "expected string or array for message content",
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming response types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicStreamEvent {
    MessageStart {
        message: AnthropicMessageStart,
    },
    ContentBlockStart {
        index: usize,
        content_block: AnthropicContentBlockStart,
    },
    ContentBlockDelta {
        index: usize,
        delta: AnthropicDelta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: AnthropicMessageDeltaBody,
        usage: AnthropicUsage,
    },
    MessageStop,
    Ping,
    Error {
        error: AnthropicErrorBody,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct AnthropicMessageStart {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub role: String,
    pub content: Vec<Value>,
    pub model: String,
    pub stop_reason: Option<Value>,
    pub stop_sequence: Option<Value>,
    pub usage: AnthropicUsage,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicContentBlockStart {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    Thinking {
        thinking: String,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicDelta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
    ThinkingDelta { thinking: String },
    SignatureDelta { signature: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct AnthropicMessageDeltaBody {
    pub stop_reason: String,
    pub stop_sequence: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AnthropicUsage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AnthropicErrorBody {
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
}

impl AnthropicStreamEvent {
    pub fn sse_event_type(&self) -> &'static str {
        match self {
            Self::MessageStart { .. } => "message_start",
            Self::ContentBlockStart { .. } => "content_block_start",
            Self::ContentBlockDelta { .. } => "content_block_delta",
            Self::ContentBlockStop { .. } => "content_block_stop",
            Self::MessageDelta { .. } => "message_delta",
            Self::MessageStop => "message_stop",
            Self::Ping => "ping",
            Self::Error { .. } => "error",
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }
}

// ---------------------------------------------------------------------------
// Non-streaming response types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct AnthropicMessageResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub role: String,
    pub content: Vec<AnthropicResponseContentBlock>,
    pub model: String,
    pub stop_reason: String,
    pub stop_sequence: Option<Value>,
    pub usage: AnthropicMessageUsage,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicResponseContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct AnthropicMessageUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_without_input_schema_gets_default() {
        let value = serde_json::json!({"name": "web_search"});
        let tool: AnthropicTool = serde_json::from_value(value).unwrap();
        assert_eq!(tool.name, "web_search");
        assert_eq!(tool.input_schema, serde_json::json!({"type": "object"}));
    }

    #[test]
    fn tool_with_input_schema_preserves_value() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "query": { "type": "string" } }
        });
        let value = serde_json::json!({"name": "search", "input_schema": schema});
        let tool: AnthropicTool = serde_json::from_value(value).unwrap();
        assert_eq!(tool.name, "search");
        assert_eq!(tool.input_schema, schema);
    }
}
