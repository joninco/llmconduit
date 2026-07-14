use serde::Deserialize;
use serde::Serialize;
use serde::de::{SeqAccess, Visitor};
use serde::ser::SerializeMap;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt;

fn deserialize_input<'de, D>(deserializer: D) -> Result<Vec<ResponseItem>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct ResponsesInputVisitor;

    impl<'de> Visitor<'de> for ResponsesInputVisitor {
        type Value = Vec<ResponseItem>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a string or an array of Responses input items")
        }

        fn visit_str<E>(self, text: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(vec![ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: text.to_string(),
                }],
                phase: None,
            }])
        }

        fn visit_string<E>(self, text: String) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            self.visit_str(&text)
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            deserialize_normalized_items(&mut sequence)
        }
    }

    deserializer.deserialize_any(ResponsesInputVisitor)
}

fn deserialize_normalized_items<'de, A>(sequence: &mut A) -> Result<Vec<ResponseItem>, A::Error>
where
    A: SeqAccess<'de>,
{
    let mut items = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
    while let Some(item) = sequence.next_element::<NormalizedInputItem>()? {
        items.push(item.0);
    }
    Ok(items)
}

/// The official Responses `instructions` union. String instructions retain
/// the gateway's historic system-message behavior; item instructions use the
/// same normalized canonical item representation as `input`.
#[derive(Debug, Clone, PartialEq)]
pub enum ResponseInstructions {
    Text(String),
    Items(Vec<ResponseItem>),
}

impl Default for ResponseInstructions {
    fn default() -> Self {
        Self::Text(String::new())
    }
}

impl ResponseInstructions {
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Text(text) => text.is_empty(),
            Self::Items(items) => items.is_empty(),
        }
    }

    pub fn items(&self) -> Option<&[ResponseItem]> {
        match self {
            Self::Text(_) => None,
            Self::Items(items) => Some(items),
        }
    }

    pub fn items_mut(&mut self) -> Option<&mut Vec<ResponseItem>> {
        match self {
            Self::Text(_) => None,
            Self::Items(items) => Some(items),
        }
    }

    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::Items(_) => None,
        }
    }

    /// Prepend gateway-owned system text without flattening structured
    /// instruction items into a lossy string.
    pub fn prepend_system_text(&mut self, prefix: String) {
        match self {
            Self::Text(text) if text.is_empty() => *text = prefix,
            Self::Text(text) => *text = format!("{prefix}\n\n{text}"),
            Self::Items(items) => items.insert(
                0,
                ResponseItem::Message {
                    id: None,
                    role: "system".to_string(),
                    content: vec![ContentItem::InputText { text: prefix }],
                    phase: None,
                },
            ),
        }
    }

    /// Preserve the historical replay hash for string instructions exactly.
    /// Structured instructions receive an explicit namespace plus their
    /// canonical serialization so they cannot collide with a string that
    /// happens to contain the same JSON bytes.
    pub fn replay_key(&self) -> Cow<'_, str> {
        match self {
            Self::Text(text) => Cow::Borrowed(text),
            Self::Items(items) => Cow::Owned(format!(
                "\0responses-instruction-items:{}",
                serde_json::to_string(items).unwrap_or_else(|_| "[]".to_string())
            )),
        }
    }

    pub fn character_count(&self) -> usize {
        match self {
            Self::Text(text) => text.chars().count(),
            Self::Items(_) => self.replay_key().chars().count(),
        }
    }

    pub fn contains(&self, pattern: &str) -> bool {
        self.text().is_some_and(|text| text.contains(pattern))
    }
}

impl PartialEq<&str> for ResponseInstructions {
    fn eq(&self, other: &&str) -> bool {
        self.text() == Some(*other)
    }
}

impl PartialEq<ResponseInstructions> for &str {
    fn eq(&self, other: &ResponseInstructions) -> bool {
        other == self
    }
}

impl From<String> for ResponseInstructions {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for ResponseInstructions {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

impl Serialize for ResponseInstructions {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Text(text) => text.serialize(serializer),
            Self::Items(items) => items.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ResponseInstructions {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct InstructionsVisitor;

        impl<'de> Visitor<'de> for InstructionsVisitor {
            type Value = ResponseInstructions;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a string or an array of Responses input items")
            }

            fn visit_str<E>(self, text: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(ResponseInstructions::Text(text.to_string()))
            }

            fn visit_string<E>(self, text: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(ResponseInstructions::Text(text))
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                deserialize_normalized_items(&mut sequence).map(ResponseInstructions::Items)
            }
        }

        deserializer.deserialize_any(InstructionsVisitor)
    }
}

struct NormalizedInputItem(ResponseItem);

impl<'de> Deserialize<'de> for NormalizedInputItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let item = normalize_input_item(Value::deserialize(deserializer)?);
        serde_json::from_value(item)
            .map(Self)
            .map_err(<D::Error as serde::de::Error>::custom)
    }
}

fn normalize_input_item(mut item: Value) -> Value {
    let Some(object) = item.as_object_mut() else {
        return item;
    };

    // Legacy Chat-style tool history is accepted only when it names the call it
    // answers. Convert that unambiguous alias to the canonical Responses item;
    // a bare role=tool message is left for request validation to reject rather
    // than guessing which function call it belongs to.
    if object.get("role").and_then(Value::as_str) == Some("tool") {
        let call_id = object
            .get("call_id")
            .or_else(|| object.get("tool_call_id"))
            .and_then(Value::as_str)
            .map(ToString::to_string);
        if let Some(call_id) = call_id {
            let output = object
                .remove("content")
                .unwrap_or(Value::String(String::new()));
            let output = normalize_legacy_function_output(output);
            return serde_json::json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": output,
            });
        }
    }

    // OpenAI's EasyInputMessage permits omitting `type` when `role` + `content`
    // already identify the item as a message. Normalize it to the canonical
    // tagged shape before `ResponseItem` deserialization.
    if !object.contains_key("type") && object.contains_key("role") && object.contains_key("content")
    {
        object.insert("type".to_string(), Value::String("message".to_string()));
    }

    // EasyInputMessage also permits bare string content. Internally every
    // message uses the content-part array so all downstream adapters keep one
    // representation regardless of which public shorthand the client used.
    if object.get("type").and_then(Value::as_str) == Some("message")
        && let Some(Value::String(text)) = object.get("content")
    {
        let text = text.clone();
        object.insert(
            "content".to_string(),
            serde_json::json!([{ "type": "input_text", "text": text }]),
        );
    }

    item
}

fn normalize_legacy_function_output(output: Value) -> Value {
    match output {
        Value::String(_) | Value::Array(_) => output,
        Value::Null => Value::String(String::new()),
        other => {
            Value::String(serde_json::to_string(&other).unwrap_or_else(|_| "null".to_string()))
        }
    }
}

fn deserialize_tool_choice<'de, D>(deserializer: D) -> Result<Value, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut choice = Value::deserialize(deserializer)?;
    let Value::Object(object) = &mut choice else {
        return Ok(choice);
    };

    // Responses uses {"type":"function","name":"..."}; the internal chat
    // lowering historically consumes {"type":"function","function":{"name":
    // "..."}}. Accept both public spellings and normalize only the flat one at
    // the request boundary so validation and backend lowering remain singular.
    if object.get("type").and_then(Value::as_str) == Some("function")
        && !object.contains_key("function")
        && let Some(name) = object.remove("name")
    {
        object.insert("function".to_string(), serde_json::json!({ "name": name }));
    }

    Ok(choice)
}

fn serialize_response_tool_choice<S>(choice: &Value, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    // Ingress normalizes the accepted legacy Chat-style spelling to the
    // nested shape used by upstream lowering. Responses resources must never
    // echo that alias: the public function selector is flat.
    if let Some(object) = choice.as_object()
        && object.get("type").and_then(Value::as_str) == Some("function")
        && let Some(name) = object
            .get("function")
            .and_then(Value::as_object)
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str)
    {
        let mut public = serde_json::Map::new();
        public.insert("type".to_string(), Value::String("function".to_string()));
        public.insert("name".to_string(), Value::String(name.to_string()));
        return Value::Object(public).serialize(serializer);
    }

    choice.serialize(serializer)
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesRequest {
    #[serde(default, deserialize_with = "crate::models::chat::deserialize_model")]
    pub model: String,
    #[serde(default)]
    pub instructions: ResponseInstructions,
    #[serde(default, deserialize_with = "deserialize_input")]
    pub input: Vec<ResponseItem>,
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    #[serde(
        default = "default_tool_choice",
        deserialize_with = "deserialize_tool_choice"
    )]
    pub tool_choice: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default)]
    pub reasoning: Option<ReasoningRequest>,
    /// Request-path thinking override resolved from an Anthropic model profile.
    /// This is internal gateway state: native Responses/Chat clients continue to
    /// control backend-specific thinking kwargs through their normal fields.
    #[serde(skip)]
    pub thinking: Option<bool>,
    #[serde(default = "default_store_true")]
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
    pub prompt_cache_retention: Option<String>,
    #[serde(default)]
    pub text: Option<TextControls>,
    #[serde(default)]
    pub previous_response_id: Option<String>,
    /// Non-standard per-request replay opt-out/allow hint. The server-wide
    /// replay gate remains authoritative; this field is consumed by the gateway
    /// and is never forwarded to an upstream provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llmconduit_replay: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    #[serde(default)]
    pub truncation: Option<Value>,
    #[serde(default)]
    pub metadata: Option<HashMap<String, Value>>,
    #[serde(
        default,
        deserialize_with = "crate::models::chat::deserialize_opt_stop",
        skip_serializing_if = "Option::is_none"
    )]
    pub stop: Option<Vec<String>>,
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_body: BTreeMap<String, Value>,
}

impl ResponsesRequest {
    /// Typed request keys are owned by their fields even if internal callers
    /// programmatically place a colliding key in `extra_body`.
    pub(crate) fn is_typed_field_name(key: &str) -> bool {
        matches!(
            key,
            "model"
                | "instructions"
                | "input"
                | "tools"
                | "tool_choice"
                | "parallel_tool_calls"
                | "reasoning"
                | "store"
                | "stream"
                | "include"
                | "service_tier"
                | "prompt_cache_key"
                | "prompt_cache_retention"
                | "text"
                | "previous_response_id"
                | "llmconduit_replay"
                | "temperature"
                | "top_p"
                | "max_output_tokens"
                | "frequency_penalty"
                | "presence_penalty"
                | "truncation"
                | "metadata"
                | "stop"
        )
    }
}

impl Serialize for ResponsesRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // Serialize the canonical request directly so a flattened extension
        // map can never emit a duplicate JSON key. Explicit typed fields win;
        // every non-colliding vendor field remains flattened at top level.
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("model", &self.model)?;
        map.serialize_entry("instructions", &self.instructions)?;
        map.serialize_entry("input", &self.input)?;
        map.serialize_entry("tools", &self.tools)?;
        map.serialize_entry("tool_choice", &self.tool_choice)?;
        if let Some(value) = self.parallel_tool_calls {
            map.serialize_entry("parallel_tool_calls", &value)?;
        }
        map.serialize_entry("reasoning", &self.reasoning)?;
        map.serialize_entry("store", &self.store)?;
        map.serialize_entry("stream", &self.stream)?;
        map.serialize_entry("include", &self.include)?;
        map.serialize_entry("service_tier", &self.service_tier)?;
        map.serialize_entry("prompt_cache_key", &self.prompt_cache_key)?;
        map.serialize_entry("prompt_cache_retention", &self.prompt_cache_retention)?;
        map.serialize_entry("text", &self.text)?;
        map.serialize_entry("previous_response_id", &self.previous_response_id)?;
        if let Some(value) = self.llmconduit_replay {
            map.serialize_entry("llmconduit_replay", &value)?;
        }
        if let Some(value) = self.temperature {
            map.serialize_entry("temperature", &value)?;
        }
        if let Some(value) = self.top_p {
            map.serialize_entry("top_p", &value)?;
        }
        if let Some(value) = self.max_output_tokens {
            map.serialize_entry("max_output_tokens", &value)?;
        }
        if let Some(value) = self.frequency_penalty {
            map.serialize_entry("frequency_penalty", &value)?;
        }
        if let Some(value) = self.presence_penalty {
            map.serialize_entry("presence_penalty", &value)?;
        }
        map.serialize_entry("truncation", &self.truncation)?;
        map.serialize_entry("metadata", &self.metadata)?;
        if let Some(value) = &self.stop {
            map.serialize_entry("stop", value)?;
        }
        for (key, value) in &self.extra_body {
            if !Self::is_typed_field_name(key) {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

fn default_tool_choice() -> Value {
    Value::String("auto".to_string())
}

fn default_store_true() -> bool {
    true
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolSpec {
    Function {
        name: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        description: String,
        #[serde(default = "default_true")]
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
        #[serde(default, skip_serializing_if = "String::is_empty")]
        description: String,
        #[serde(default)]
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
        #[serde(default, skip_serializing_if = "String::is_empty")]
        description: String,
        #[serde(default)]
        strict: bool,
        parameters: Value,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CustomToolFormat {
    #[default]
    Text,
    Grammar {
        syntax: String,
        definition: String,
    },
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

#[derive(Debug, Clone, PartialEq)]
pub struct TextFormat {
    pub kind: String,
    pub strict: bool,
    pub schema: Value,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TextFormatWire {
    Text,
    JsonObject,
    JsonSchema {
        name: String,
        schema: Value,
        #[serde(default)]
        strict: bool,
        #[serde(default)]
        description: Option<String>,
    },
}

impl<'de> Deserialize<'de> for TextFormat {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match TextFormatWire::deserialize(deserializer)? {
            TextFormatWire::Text => Self {
                kind: "text".to_string(),
                strict: false,
                schema: Value::Null,
                name: String::new(),
                description: None,
            },
            TextFormatWire::JsonObject => Self {
                kind: "json_object".to_string(),
                strict: false,
                schema: Value::Null,
                name: String::new(),
                description: None,
            },
            TextFormatWire::JsonSchema {
                name,
                schema,
                strict,
                description,
            } => Self {
                kind: "json_schema".to_string(),
                strict,
                schema,
                name,
                description,
            },
        })
    }
}

impl Serialize for TextFormat {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.kind.as_str() {
            "text" | "json_object" => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("type", &self.kind)?;
                map.end()
            }
            "json_schema" => {
                let mut map =
                    serializer.serialize_map(Some(4 + usize::from(self.description.is_some())))?;
                map.serialize_entry("type", "json_schema")?;
                map.serialize_entry("name", &self.name)?;
                map.serialize_entry("schema", &self.schema)?;
                map.serialize_entry("strict", &self.strict)?;
                if let Some(description) = &self.description {
                    map.serialize_entry("description", description)?;
                }
                map.end()
            }
            other => Err(<S::Error as serde::ser::Error>::custom(format!(
                "unsupported text format type: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ContentItem {
    InputText {
        text: String,
    },
    InputImage {
        image_url: Option<String>,
        file_id: Option<String>,
        detail: Option<String>,
    },
    InputFile {
        file_id: Option<String>,
        file_url: Option<String>,
        filename: Option<String>,
        file_data: Option<String>,
    },
    OutputText {
        text: String,
    },
    Refusal {
        refusal: String,
    },
    Other(Value),
}

impl Serialize for ContentItem {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            ContentItem::InputText { text } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "input_text")?;
                map.serialize_entry("text", text)?;
                map.end()
            }
            ContentItem::InputImage {
                image_url,
                file_id,
                detail,
            } => {
                let len = 1
                    + usize::from(image_url.is_some())
                    + usize::from(file_id.is_some())
                    + usize::from(detail.is_some());
                let mut map = serializer.serialize_map(Some(len))?;
                map.serialize_entry("type", "input_image")?;
                if let Some(image_url) = image_url {
                    map.serialize_entry("image_url", image_url)?;
                }
                if let Some(file_id) = file_id {
                    map.serialize_entry("file_id", file_id)?;
                }
                if let Some(detail) = detail {
                    map.serialize_entry("detail", detail)?;
                }
                map.end()
            }
            ContentItem::InputFile {
                file_id,
                file_url,
                filename,
                file_data,
            } => {
                let len = 1
                    + usize::from(file_id.is_some())
                    + usize::from(file_url.is_some())
                    + usize::from(filename.is_some())
                    + usize::from(file_data.is_some());
                let mut map = serializer.serialize_map(Some(len))?;
                map.serialize_entry("type", "input_file")?;
                if let Some(file_id) = file_id {
                    map.serialize_entry("file_id", file_id)?;
                }
                if let Some(file_url) = file_url {
                    map.serialize_entry("file_url", file_url)?;
                }
                if let Some(filename) = filename {
                    map.serialize_entry("filename", filename)?;
                }
                if let Some(file_data) = file_data {
                    map.serialize_entry("file_data", file_data)?;
                }
                map.end()
            }
            ContentItem::OutputText { text } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "output_text")?;
                map.serialize_entry("text", text)?;
                map.end()
            }
            ContentItem::Refusal { refusal } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "refusal")?;
                map.serialize_entry("refusal", refusal)?;
                map.end()
            }
            ContentItem::Other(value) => value.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ContentItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let Some(object) = value.as_object() else {
            return Err(<D::Error as serde::de::Error>::custom(
                "content item must be an object",
            ));
        };
        Ok(match object.get("type").and_then(Value::as_str) {
            Some("input_text") => ContentItem::InputText {
                text: required_string::<D::Error>(object, "text")?,
            },
            Some("input_image") => ContentItem::InputImage {
                image_url: optional_image_url::<D::Error>(object)?,
                file_id: optional_string::<D::Error>(object, "file_id")?,
                detail: optional_string::<D::Error>(object, "detail")?,
            },
            Some("input_file") => ContentItem::InputFile {
                file_id: optional_string::<D::Error>(object, "file_id")?,
                file_url: optional_string::<D::Error>(object, "file_url")?,
                filename: optional_string::<D::Error>(object, "filename")?,
                file_data: optional_string::<D::Error>(object, "file_data")?,
            },
            Some("output_text") => ContentItem::OutputText {
                text: required_string::<D::Error>(object, "text")?,
            },
            Some("refusal") => ContentItem::Refusal {
                refusal: required_string::<D::Error>(object, "refusal")?,
            },
            _ => ContentItem::Other(value),
        })
    }
}

fn required_string<E: serde::de::Error>(
    object: &serde_json::Map<String, Value>,
    key: &'static str,
) -> Result<String, E> {
    match object.get(key) {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(E::custom(format!("{key} must be a string"))),
        None => Err(E::missing_field(key)),
    }
}

fn optional_string<E: serde::de::Error>(
    object: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<String>, E> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(E::custom(format!("{key} must be a string"))),
    }
}

fn optional_image_url<E: serde::de::Error>(
    object: &serde_json::Map<String, Value>,
) -> Result<Option<String>, E> {
    match object.get("image_url") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(url)) => Ok(Some(url.clone())),
        Some(Value::Object(map)) => match map.get("url") {
            Some(Value::String(url)) => Ok(Some(url.clone())),
            Some(_) => Err(E::custom("image_url.url must be a string")),
            None => Err(E::custom("image_url.url is required")),
        },
        Some(_) => Err(E::custom("image_url must be a string")),
    }
}

/// Official Responses function output: either a text result or an array of
/// input content parts. Keeping the union typed prevents arbitrary JSON from
/// bypassing multimodal capability and residual-image checks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum FunctionCallOutputContent {
    Text(String),
    Content(Vec<ContentItem>),
}

impl FunctionCallOutputContent {
    pub fn content(&self) -> Option<&[ContentItem]> {
        match self {
            Self::Text(_) => None,
            Self::Content(content) => Some(content),
        }
    }

    pub fn content_mut(&mut self) -> Option<&mut Vec<ContentItem>> {
        match self {
            Self::Text(_) => None,
            Self::Content(content) => Some(content),
        }
    }
}

impl From<Value> for FunctionCallOutputContent {
    fn from(value: Value) -> Self {
        match value {
            Value::String(text) => Self::Text(text),
            Value::Array(values) => Self::Content(
                values
                    .into_iter()
                    .map(|value| {
                        serde_json::from_value(value.clone()).unwrap_or(ContentItem::Other(value))
                    })
                    .collect(),
            ),
            Value::Null => Self::Text(String::new()),
            other => {
                Self::Text(serde_json::to_string(&other).unwrap_or_else(|_| "null".to_string()))
            }
        }
    }
}

impl fmt::Display for FunctionCallOutputContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let serialized = serde_json::to_string(self).map_err(|_| fmt::Error)?;
        formatter.write_str(&serialized)
    }
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
    ItemReference {
        id: String,
    },
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
        output: FunctionCallOutputContent,
    },
    CustomToolCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
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
        output: FunctionCallOutputContent,
    },
    ToolSearchCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sources: Option<Vec<Value>>,
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
    pub response: ResponseResource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseStub {
    pub id: String,
    /// An EARLY, coarse ESTIMATE of prompt input tokens
    /// (`engine::estimate_input_tokens`,
    /// ~4 bytes/token over the lowered upstream payload), threaded onto
    /// `response.created` ONLY -- never `response.in_progress`, which reuses
    /// this same struct via `ResponseCreatedPayload` -- so the Anthropic
    /// streaming converter can seed `message_start.usage.input_tokens` with a
    /// plausible non-zero value instead of a hardcoded `0`. The REAL upstream
    /// tokenizer count is not known this early; it arrives later on
    /// `response.completed`'s `usage` and always overrides this estimate at
    /// the terminal event. `skip_serializing_if` keeps the OpenAI/Responses
    /// wire shape byte-unchanged for any consumer that never reads this
    /// additive, canonical-protocol-internal field.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub estimated_input_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponseCompletedPayload {
    pub response: ResponseResource,
}

/// Typed terminal reason for a canonical response (T7). The engine sets this
/// from the upstream `finish_reason` at terminal emission; G8 reasoning-
/// promotion gating reads it (`Stop` ⇒ clean stop ⇒ may promote reasoning to
/// text) instead of string-matching the event type (`response.completed` vs
/// `response.incomplete`), so a future non-stop terminal reason arriving as
/// `response.completed` can no longer wrongly promote.
///
/// Serialized as a kebab/snake string on the wire (`"stop"`, `"length"`,
/// `"tool_calls"`, `"content_filter"`, `"other"`). Unknown upstream finish
/// reasons map to `Other` (non-clean ⇒ never promote).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalReason {
    /// `finish_reason: stop` — a clean completion. The ONLY reason that permits
    /// promoting a reasoning-only turn to a `text` block (G8).
    Stop,
    /// `finish_reason: length` — hit the output-token cap (`response.incomplete`,
    /// `incomplete_details.reason: max_output_tokens`). Never promote.
    Length,
    /// `finish_reason: tool_calls` — the turn ended on a tool-call batch. Never
    /// promote (the reasoning prefaced tools, not a final answer). Serialized as
    /// `tool_calls` (matching the upstream finish_reason vocabulary), NOT
    /// `tool_call` (which `rename_all = "snake_case"` would produce).
    #[serde(rename = "tool_calls")]
    ToolCall,
    /// `finish_reason: content_filter` — upstream content filter blocked output.
    /// Never promote.
    ContentFilter,
    /// Any other / unknown finish reason. Never promote (conservative: a reason
    /// the gateway does not recognize is not a clean stop).
    Other,
}

impl TerminalReason {
    /// Whether this is a CLEAN STOP — the only terminal reason that permits G8
    /// reasoning-promotion to a `text` block.
    pub fn is_clean_stop(self) -> bool {
        matches!(self, TerminalReason::Stop)
    }

    /// Map an upstream `finish_reason` string to a typed terminal reason.
    /// Unknown/blank ⇒ `Other` (non-clean).
    pub fn from_finish_reason(finish_reason: Option<&str>) -> Self {
        match finish_reason {
            Some("stop") => TerminalReason::Stop,
            Some("length") => TerminalReason::Length,
            Some("tool_calls") => TerminalReason::ToolCall,
            Some("content_filter") => TerminalReason::ContentFilter,
            _ => TerminalReason::Other,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponseResource {
    pub id: String,
    pub object: String,
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<i64>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<FailedError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<ResponseInstructions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i64>,
    pub output: Vec<ResponseItem>,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningRequest>,
    pub store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<TextControls>,
    #[serde(serialize_with = "serialize_response_tool_choice")]
    pub tool_choice: Value,
    pub tools: Vec<ToolSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_retention: Option<String>,
    /// Canonical-only early usage hint consumed by the Anthropic adapter. The
    /// raw Responses projection removes it before serialization.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<ResponseUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incomplete_details: Option<IncompleteDetails>,
    /// Matched upstream stop string, when vLLM reports one. This internal
    /// extension lets the Anthropic adapter distinguish a configured stop
    /// sequence from a natural end-of-sequence completion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
    /// Typed terminal reason (T7). `None` only on non-terminal `response`
    /// resources (e.g. `response.created`); the terminal `response.completed` /
    /// `response.incomplete` event always carries it so the Anthropic converter
    /// can gate reasoning-promotion on `reason.is_clean_stop()` rather than the
    /// event type string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<TerminalReason>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IncompleteDetails {
    pub reason: String,
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
    pub output_index: usize,
    pub item: ResponseItem,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeltaPayload {
    pub item_id: String,
    pub output_index: usize,
    pub content_index: usize,
    pub delta: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReasoningDeltaPayload {
    pub item_id: String,
    pub output_index: usize,
    pub summary_index: usize,
    pub delta: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReasoningTextDonePayload {
    pub item_id: String,
    pub output_index: usize,
    pub summary_index: usize,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReasoningSignatureDeltaPayload {
    pub item_id: String,
    pub output_index: usize,
    pub summary_index: usize,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FailedPayload {
    pub response: ResponseResource,
}

#[derive(Debug, Clone, Serialize)]
pub struct FailedError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TextDonePayload {
    pub item_id: String,
    pub output_index: usize,
    pub content_index: usize,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionCallArgsDeltaPayload {
    pub item_id: String,
    pub output_index: usize,
    pub call_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub delta: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionCallArgsDonePayload {
    pub item_id: String,
    pub output_index: usize,
    pub call_id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CustomToolCallInputDeltaPayload {
    pub item_id: String,
    pub output_index: usize,
    pub delta: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CustomToolCallInputDonePayload {
    pub item_id: String,
    pub output_index: usize,
    pub input: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefusalDeltaPayload {
    pub item_id: String,
    pub output_index: usize,
    pub content_index: usize,
    pub delta: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefusalDonePayload {
    pub item_id: String,
    pub output_index: usize,
    pub content_index: usize,
    pub refusal: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContentPartPayload {
    pub item_id: String,
    pub output_index: usize,
    pub content_index: usize,
    pub part: ContentPartRef,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefusalContentPartPayload {
    pub item_id: String,
    pub output_index: usize,
    pub content_index: usize,
    pub part: RefusalContentPartRef,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContentPartRef {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
    pub annotations: Vec<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefusalContentPartRef {
    #[serde(rename = "type")]
    pub kind: String,
    pub refusal: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReasoningSummaryPartPayload {
    pub item_id: String,
    pub output_index: usize,
    pub summary_index: usize,
    pub part: ReasoningSummaryPartRef,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReasoningSummaryPartRef {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
}

pub enum TerminalStatus {
    Completed,
    Incomplete { reason: String },
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

    #[test]
    fn response_tool_choice_serializes_function_selection_in_official_flat_form() {
        let resource = |tool_choice| ResponseResource {
            id: "resp_test".to_string(),
            object: "response".to_string(),
            created_at: 1,
            completed_at: None,
            status: "in_progress".to_string(),
            error: None,
            instructions: None,
            max_output_tokens: None,
            output: Vec::new(),
            model: "test-model".to_string(),
            parallel_tool_calls: None,
            previous_response_id: None,
            reasoning: None,
            store: false,
            temperature: None,
            text: None,
            tool_choice,
            tools: Vec::new(),
            top_p: None,
            truncation: None,
            service_tier: None,
            prompt_cache_key: None,
            prompt_cache_retention: None,
            estimated_input_tokens: None,
            usage: None,
            metadata: None,
            incomplete_details: None,
            stop_sequence: None,
            terminal_reason: None,
        };
        let serialized = serde_json::to_value(resource(serde_json::json!({
            "type": "function",
            "function": { "name": "echo" }
        })))
        .expect("serialize tool choice");
        assert_eq!(
            serialized["tool_choice"],
            serde_json::json!({ "type": "function", "name": "echo" })
        );

        let serialized = serde_json::to_value(resource(Value::String("auto".to_string())))
            .expect("serialize automatic tool choice");
        assert_eq!(serialized["tool_choice"], "auto");
    }
    use pretty_assertions::assert_eq;

    #[test]
    fn terminal_reason_wire_strings_are_pinned() {
        // Load-bearing wire contract: the engine serializes `TerminalReason` onto
        // the terminal resource and the Anthropic converter re-reads these exact
        // strings via `from_finish_reason`. The `ToolCall => "tool_calls"` case is
        // the critical one — without `#[serde(rename = "tool_calls")]` snake_case
        // would emit `"tool_call"`, silently degrading the converter to `Other`.
        for (variant, expected) in [
            (TerminalReason::Stop, "stop"),
            (TerminalReason::Length, "length"),
            (TerminalReason::ToolCall, "tool_calls"),
            (TerminalReason::ContentFilter, "content_filter"),
            (TerminalReason::Other, "other"),
        ] {
            assert_eq!(
                serde_json::to_value(variant).unwrap(),
                serde_json::Value::String(expected.to_string()),
                "wire string for {variant:?}",
            );
            // Belt-and-braces: producer spelling round-trips through the canonical
            // mapper, so producer + consumer agree on every variant.
            assert_eq!(
                TerminalReason::from_finish_reason(Some(expected)),
                variant,
                "round-trip for {variant:?}",
            );
        }
    }

    /// C3 (AGENTS.md: no new wire field without a round-trip test): `ResponseStub`
    /// gained `estimated_input_tokens` (the early G3 estimate the engine threads
    /// onto `response.created` so the Anthropic converter can seed a non-zero
    /// `message_start.usage.input_tokens`). PRESENT survives a
    /// serialize -> deserialize round-trip; ABSENT (`None`) is OMITTED from the
    /// wire entirely (not serialized as `null`), so the OpenAI/Responses shape
    /// stays byte-unchanged for any caller that never reads this field.
    #[test]
    fn response_stub_estimated_input_tokens_round_trips_present_and_absent() {
        let present = ResponseStub {
            id: "resp_123".to_string(),
            estimated_input_tokens: Some(20),
        };
        let value = serde_json::to_value(&present).expect("serialize present");
        assert_eq!(value["estimated_input_tokens"], serde_json::json!(20));
        let roundtripped: ResponseStub =
            serde_json::from_value(value).expect("deserialize present");
        assert_eq!(present, roundtripped);

        let absent = ResponseStub {
            id: "resp_456".to_string(),
            estimated_input_tokens: None,
        };
        let value = serde_json::to_value(&absent).expect("serialize absent");
        assert!(
            !value
                .as_object()
                .unwrap()
                .contains_key("estimated_input_tokens"),
            "None must be OMITTED, not serialized as null: {value}"
        );
        let roundtripped: ResponseStub = serde_json::from_value(value).expect("deserialize absent");
        assert_eq!(absent, roundtripped);
    }

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
    fn content_item_multimodal_serde_roundtrip() {
        let items = vec![
            ContentItem::InputImage {
                image_url: None,
                file_id: Some("file_img".to_string()),
                detail: Some("high".to_string()),
            },
            ContentItem::InputFile {
                file_id: Some("file_doc".to_string()),
                file_url: None,
                filename: Some("brief.pdf".to_string()),
                file_data: None,
            },
            ContentItem::Other(serde_json::json!({
                "type": "input_audio",
                "input_audio": {
                    "data": "abc",
                    "format": "wav"
                }
            })),
        ];
        let json = serde_json::to_string(&items).unwrap();
        let roundtripped: Vec<ContentItem> = serde_json::from_str(&json).unwrap();
        assert_eq!(items, roundtripped);
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
    fn test_store_defaults_to_true() {
        let json = r#"{"model":"gpt-4","input":[]}"#;
        let req: ResponsesRequest = serde_json::from_str(json).unwrap();
        assert!(req.store, "store should default to true");
    }

    #[test]
    fn test_store_explicit_false() {
        let json = r#"{"model":"gpt-4","input":[],"store":false}"#;
        let req: ResponsesRequest = serde_json::from_str(json).unwrap();
        assert!(!req.store, "store should be false when explicitly set");
    }

    #[test]
    fn test_input_bare_string_deserializes() {
        let json = r#"{"model":"gpt-4","input":"hello","stream":true}"#;
        let req: ResponsesRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.input.len(), 1);
        match &req.input[0] {
            ResponseItem::Message { role, content, .. } => {
                assert_eq!(role, "user");
                assert_eq!(content.len(), 1);
                match &content[0] {
                    ContentItem::InputText { text } => assert_eq!(text, "hello"),
                    other => panic!("expected InputText, got {:?}", other),
                }
            }
            other => panic!("expected Message, got {:?}", other),
        }
    }

    #[test]
    fn test_penalties_roundtrip() {
        let json = r#"{"model":"gpt-4","input":[],"frequency_penalty":0.5,"presence_penalty":0.3}"#;
        let req: ResponsesRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.frequency_penalty, Some(0.5));
        assert_eq!(req.presence_penalty, Some(0.3));
    }

    #[test]
    fn response_item_web_search_call_serde_roundtrip() {
        let item = ResponseItem::WebSearchCall {
            id: Some("ws_1".to_string()),
            status: Some("completed".to_string()),
            action: Some(WebSearchAction::Search {
                query: Some("rust async".to_string()),
                queries: None,
                sources: None,
            }),
        };
        let json = serde_json::to_string(&item).unwrap();
        let roundtripped: ResponseItem = serde_json::from_str(&json).unwrap();
        assert_eq!(item, roundtripped);
    }
}
