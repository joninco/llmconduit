use serde::Deserialize;
use serde::Serialize;
use serde::de::{Deserializer, Error as DeError};
use serde::ser::{SerializeMap, Serializer};
use serde_json::Map as JsonMap;
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use url::Url;

/// Per-profile reasoning-effort shaping (config key `reasoning_effort`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReasoningConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub map: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "is_default_thinking_param_name")]
    pub thinking_param_name: String,
    #[serde(skip_serializing_if = "is_default_thinking_param_value_on")]
    pub thinking_param_value_on: JsonValue,
    #[serde(skip_serializing_if = "is_default_thinking_param_value_off")]
    pub thinking_param_value_off: JsonValue,
}

const DEFAULT_THINKING_PARAM_NAME: &str = "enable_thinking";

fn default_thinking_param_name() -> String {
    DEFAULT_THINKING_PARAM_NAME.to_string()
}
fn default_thinking_param_value_on() -> JsonValue {
    JsonValue::Bool(true)
}
fn default_thinking_param_value_off() -> JsonValue {
    JsonValue::Bool(false)
}
fn is_default_thinking_param_name(name: &str) -> bool {
    name == DEFAULT_THINKING_PARAM_NAME
}
fn is_default_thinking_param_value_on(value: &JsonValue) -> bool {
    matches!(value, JsonValue::Bool(true))
}
fn is_default_thinking_param_value_off(value: &JsonValue) -> bool {
    matches!(value, JsonValue::Bool(false))
}

impl Default for ReasoningConfig {
    fn default() -> Self {
        Self {
            default: None,
            map: BTreeMap::new(),
            thinking_param_name: default_thinking_param_name(),
            thinking_param_value_on: default_thinking_param_value_on(),
            thinking_param_value_off: default_thinking_param_value_off(),
        }
    }
}

impl ReasoningConfig {
    /// Value to inject for the thinking template kwarg given the request's thinking state.
    pub fn thinking_param_value(&self, on: bool) -> &JsonValue {
        if on {
            &self.thinking_param_value_on
        } else {
            &self.thinking_param_value_off
        }
    }
}

impl<'de> Deserialize<'de> for ReasoningConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            #[serde(default)]
            default: Option<String>,
            #[serde(default)]
            map: BTreeMap<String, String>,
            #[serde(default = "default_thinking_param_name")]
            thinking_param_name: String,
            #[serde(default = "default_thinking_param_value_on")]
            thinking_param_value_on: JsonValue,
            #[serde(default = "default_thinking_param_value_off")]
            thinking_param_value_off: JsonValue,
        }
        let raw = Raw::deserialize(deserializer)?;
        let map = raw
            .map
            .into_iter()
            .filter_map(|(key, value)| {
                let key = key.trim().to_ascii_lowercase();
                let value = value.trim().to_string();
                (!key.is_empty() && !value.is_empty()).then_some((key, value))
            })
            .collect();
        let default = raw.default.and_then(|value| {
            let value = value.trim().to_string();
            (!value.is_empty()).then_some(value)
        });
        let thinking_param_name = if raw.thinking_param_name.trim().is_empty() {
            default_thinking_param_name()
        } else {
            raw.thinking_param_name.trim().to_string()
        };
        Ok(Self {
            default,
            map,
            thinking_param_name,
            thinking_param_value_on: raw.thinking_param_value_on,
            thinking_param_value_off: raw.thinking_param_value_off,
        })
    }
}

fn default_true() -> bool {
    true
}

/// Thinking modes a profile can advertise for the `thinking` capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingType {
    Adaptive,
    Enabled,
}

impl ThinkingType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ThinkingType::Adaptive => "adaptive",
            ThinkingType::Enabled => "enabled",
        }
    }
}

/// Reasoning-effort levels a profile can advertise for the `effort` capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffortLevel {
    Max,
    Xhigh,
    High,
    Medium,
    Low,
    Minimal,
    // Named `Disabled` (not `None`) so the variant does not shadow `Option::None` lexically;
    // `rename` keeps the wire format as "none" over the container's `rename_all="lowercase"`.
    #[serde(rename = "none")]
    Disabled,
}

impl EffortLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            EffortLevel::Max => "max",
            EffortLevel::Xhigh => "xhigh",
            EffortLevel::High => "high",
            EffortLevel::Medium => "medium",
            EffortLevel::Low => "low",
            EffortLevel::Minimal => "minimal",
            EffortLevel::Disabled => "none",
        }
    }
}

/// Context-management features a profile can advertise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextFeature {
    #[serde(rename = "clear_thinking_20251015")]
    ClearThinking20251015,
    #[serde(rename = "clear_tool_uses_20250919")]
    ClearToolUses20250919,
    #[serde(rename = "compact_20260112")]
    Compact20260112,
}

impl ContextFeature {
    pub fn as_str(&self) -> &'static str {
        match self {
            ContextFeature::ClearThinking20251015 => "clear_thinking_20251015",
            ContextFeature::ClearToolUses20250919 => "clear_tool_uses_20250919",
            ContextFeature::Compact20260112 => "compact_20260112",
        }
    }
}

fn supported_obj(supported: bool) -> JsonValue {
    let mut map = JsonMap::new();
    map.insert("supported".to_string(), JsonValue::Bool(supported));
    JsonValue::Object(map)
}

/// A capability with only a `supported` flag. A bare bool is shorthand for
/// `{supported: <bool>}`; an object may omit `supported` (defaults to true).
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct SimpleCap {
    pub supported: bool,
}

impl SimpleCap {
    pub fn to_wire(&self) -> JsonValue {
        supported_obj(self.supported)
    }
}

impl<'de> Deserialize<'de> for SimpleCap {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Some(supported) = value.as_bool() {
            return Ok(SimpleCap { supported });
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            #[serde(default = "default_true")]
            supported: bool,
        }
        let raw = serde_json::from_value::<Raw>(value)
            .map_err(|err| <D::Error as serde::de::Error>::custom(err.to_string()))?;
        Ok(SimpleCap {
            supported: raw.supported,
        })
    }
}

/// The `thinking` capability. `types` lists advertised thinking modes; each emitted
/// type inherits the cap's `supported` flag (one knob, per the capabilities design).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThinkingCap {
    #[serde(default = "default_true")]
    pub supported: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<ThinkingType>,
}

impl ThinkingCap {
    pub fn to_wire(&self) -> JsonValue {
        let mut types = JsonMap::new();
        for thinking_type in &self.types {
            types.insert(
                thinking_type.as_str().to_string(),
                supported_obj(self.supported),
            );
        }
        let mut map = JsonMap::new();
        map.insert("supported".to_string(), JsonValue::Bool(self.supported));
        map.insert("types".to_string(), JsonValue::Object(types));
        JsonValue::Object(map)
    }
}

/// The `effort` capability. `levels` are emitted as siblings of `supported` on the
/// wire, each inheriting the cap's `supported` flag.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffortCap {
    #[serde(default = "default_true")]
    pub supported: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub levels: Vec<EffortLevel>,
}

impl EffortCap {
    pub fn to_wire(&self) -> JsonValue {
        let mut map = JsonMap::new();
        map.insert("supported".to_string(), JsonValue::Bool(self.supported));
        for level in &self.levels {
            map.insert(level.as_str().to_string(), supported_obj(self.supported));
        }
        JsonValue::Object(map)
    }
}

/// The `context_management` capability. `features` are emitted as siblings of
/// `supported`, each inheriting the cap's `supported` flag.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextManagementCap {
    #[serde(default = "default_true")]
    pub supported: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<ContextFeature>,
}

impl ContextManagementCap {
    pub fn to_wire(&self) -> JsonValue {
        let mut map = JsonMap::new();
        map.insert("supported".to_string(), JsonValue::Bool(self.supported));
        for feature in &self.features {
            map.insert(feature.as_str().to_string(), supported_obj(self.supported));
        }
        JsonValue::Object(map)
    }
}

/// Per-profile Anthropic model capabilities. Only `supported` is a knob (defaulting
/// to true); configured caps override the base capabilities wholesale per cap key.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<SimpleCap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub citations: Option<SimpleCap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_execution: Option<SimpleCap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_input: Option<SimpleCap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdf_input: Option<SimpleCap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_outputs: Option<SimpleCap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingCap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<EffortCap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_management: Option<ContextManagementCap>,
}

impl CapabilitiesConfig {
    /// Override configured caps in `base` per cap key, wholesale. Unconfigured caps
    /// keep their base value.
    pub fn merge_into(&self, mut base: JsonValue) -> JsonValue {
        if let Some(map) = base.as_object_mut() {
            if let Some(cap) = &self.batch {
                map.insert("batch".to_string(), cap.to_wire());
            }
            if let Some(cap) = &self.citations {
                map.insert("citations".to_string(), cap.to_wire());
            }
            if let Some(cap) = &self.code_execution {
                map.insert("code_execution".to_string(), cap.to_wire());
            }
            if let Some(cap) = &self.image_input {
                map.insert("image_input".to_string(), cap.to_wire());
            }
            if let Some(cap) = &self.pdf_input {
                map.insert("pdf_input".to_string(), cap.to_wire());
            }
            if let Some(cap) = &self.structured_outputs {
                map.insert("structured_outputs".to_string(), cap.to_wire());
            }
            if let Some(cap) = &self.thinking {
                map.insert("thinking".to_string(), cap.to_wire());
            }
            if let Some(cap) = &self.effort {
                map.insert("effort".to_string(), cap.to_wire());
            }
            if let Some(cap) = &self.context_management {
                map.insert("context_management".to_string(), cap.to_wire());
            }
        }
        base
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub upstream_base_url: Url,
    pub upstream_api_key: Option<String>,
    pub upstream_model: Option<String>,
    pub system_prompt_prefix: Option<String>,
    pub upstream_request_log_path: Option<PathBuf>,
    pub upstream_chat_kwargs: JsonMap<String, JsonValue>,
    pub upstreams: Vec<UpstreamConfig>,
    pub fallback_upstreams: Vec<FallbackUpstreamConfig>,
    pub upstream_failure_cooldown_secs: u64,
    /// Per-model profiles keyed by name (case-insensitive lookup). The name `"*"` is
    /// reserved as the fallback profile for per-model settings that no specific profile covers.
    pub model_profiles: BTreeMap<String, ModelProfile>,
    pub brave_base_url: Url,
    pub brave_api_key: Option<String>,
    pub brave_max_results: usize,
    pub request_timeout: Duration,
    pub connect_timeout_secs: u64,
    pub max_web_search_rounds: usize,
    pub flatten_content: bool,
    pub max_replay_entries: usize,
}

#[derive(Debug, Clone)]
pub struct UpstreamConfig {
    pub name: String,
    pub upstream_base_url: Url,
    pub upstream_api_key: Option<String>,
    pub upstream_model: Option<String>,
    pub upstream_chat_kwargs: JsonMap<String, JsonValue>,
    pub upstream_request_log_path: Option<PathBuf>,
    pub fallback_upstreams: Vec<FallbackUpstreamConfig>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct PersistedModelProfile {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extends: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roles: Option<RolesConfig>,
    #[serde(default, skip_serializing_if = "JsonMap::is_empty")]
    pub upstream_chat_kwargs: JsonMap<String, JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<CapabilitiesConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningConfig>,
}

impl<'de> Deserialize<'de> for PersistedModelProfile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawPersistedModelProfile {
            #[serde(default)]
            extends: Vec<String>,
            #[serde(default)]
            upstream_model: Option<String>,
            #[serde(default)]
            system_prompt_prefix: Option<String>,
            #[serde(default)]
            roles: Option<RolesConfig>,
            #[serde(default)]
            upstream_chat_kwargs: JsonMap<String, JsonValue>,
            #[serde(default, flatten)]
            shorthand_upstream_chat_kwargs: JsonMap<String, JsonValue>,
            #[serde(default)]
            capabilities: Option<CapabilitiesConfig>,
            #[serde(default)]
            reasoning_effort: Option<ReasoningConfig>,
        }

        let raw = RawPersistedModelProfile::deserialize(deserializer)?;
        let mut upstream_chat_kwargs = raw.shorthand_upstream_chat_kwargs;
        merge_json_maps(&mut upstream_chat_kwargs, &raw.upstream_chat_kwargs);
        Ok(Self {
            extends: raw.extends,
            upstream_model: raw.upstream_model,
            system_prompt_prefix: raw.system_prompt_prefix,
            roles: raw.roles,
            upstream_chat_kwargs,
            capabilities: raw.capabilities,
            reasoning_effort: raw.reasoning_effort,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct ModelProfile {
    pub upstream_model: Option<String>,
    pub system_prompt_prefix: Option<String>,
    pub roles: Option<RolesConfig>,
    pub upstream_chat_kwargs: JsonMap<String, JsonValue>,
    pub capabilities: Option<CapabilitiesConfig>,
    pub reasoning_effort: Option<ReasoningConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    #[default]
    Accept,
    Reject,
    Drop,
    Rewrite,
}

impl Action {
    fn is_accept(&self) -> bool {
        *self == Action::Accept
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum When {
    Leading,
    Inline,
    Always,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RoleRule {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub when: Option<When>,
    #[serde(skip_serializing_if = "Action::is_accept")]
    pub action: Action,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub tag_attributes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoleRuleSet {
    pub rules: Vec<RoleRule>,
}

impl<'de> Deserialize<'de> for RoleRuleSet {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let rules = if value.is_array() {
            serde_json::from_value::<Vec<RoleRule>>(value).map_err(DeError::custom)?
        } else {
            vec![serde_json::from_value::<RoleRule>(value).map_err(DeError::custom)?]
        };
        Ok(RoleRuleSet { rules })
    }
}

impl Serialize for RoleRuleSet {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if self.rules.len() == 1 {
            self.rules[0].serialize(serializer)
        } else {
            self.rules.serialize(serializer)
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RolesConfig {
    pub merge_adjacent: Vec<String>,
    pub rules: BTreeMap<String, RoleRuleSet>,
}

impl<'de> Deserialize<'de> for RolesConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let map = match value {
            serde_json::Value::Object(map) => map,
            _ => {
                return Err(DeError::custom(
                    "roles must be a map of role names to rules",
                ));
            }
        };
        let mut merge_adjacent = Vec::new();
        let mut rules = BTreeMap::new();
        for (key, val) in map {
            if key == "merge_adjacent" {
                merge_adjacent =
                    serde_json::from_value::<Vec<String>>(val).map_err(DeError::custom)?;
            } else {
                rules.insert(
                    key,
                    serde_json::from_value::<RoleRuleSet>(val).map_err(DeError::custom)?,
                );
            }
        }
        Ok(RolesConfig {
            merge_adjacent,
            rules,
        })
    }
}

impl Serialize for RolesConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let len = self.rules.len() + if self.merge_adjacent.is_empty() { 0 } else { 1 };
        let mut map = serializer.serialize_map(Some(len))?;
        if !self.merge_adjacent.is_empty() {
            map.serialize_entry("merge_adjacent", &self.merge_adjacent)?;
        }
        for (key, val) in &self.rules {
            map.serialize_entry(key, val)?;
        }
        map.end()
    }
}

impl RolesConfig {
    /// Resolve the rule list for a role: explicit key first, then the `*` wildcard.
    pub fn rules_for(&self, role: &str) -> Option<&[RoleRule]> {
        self.rules
            .get(role)
            .map(|set| set.rules.as_slice())
            .or_else(|| self.rules.get("*").map(|set| set.rules.as_slice()))
    }

    pub fn validate(&self) -> Result<(), String> {
        for (name, set) in &self.rules {
            for rule in &set.rules {
                let has_target = rule
                    .target_role
                    .as_deref()
                    .map_or(false, |t| !t.trim().is_empty());
                if rule.action == Action::Rewrite {
                    if !has_target {
                        return Err(format!(
                            "roles[{name}]: action `rewrite` requires a non-empty `target_role`"
                        ));
                    }
                } else if rule.target_role.is_some() {
                    return Err(format!(
                        "roles[{name}]: `target_role` is only valid with action `rewrite`"
                    ));
                }
                if !rule.tag_attributes.is_empty()
                    && rule.tag.as_deref().map_or(true, |t| t.trim().is_empty())
                {
                    return Err(format!(
                        "roles[{name}]: `tag_attributes` requires a non-empty `tag`"
                    ));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct FallbackUpstreamConfig {
    pub name: String,
    pub upstream_base_url: Url,
    pub upstream_api_key: Option<String>,
    pub upstream_model: Option<String>,
    pub exposed_model: Option<String>,
    pub upstream_chat_kwargs: JsonMap<String, JsonValue>,
    pub upstream_request_log_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PersistedFallbackUpstream {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub upstream_base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exposed_model: Option<String>,
    #[serde(default, skip_serializing_if = "JsonMap::is_empty")]
    pub upstream_chat_kwargs: JsonMap<String, JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_request_log_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PersistedUpstream {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub upstream_base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_model: Option<String>,
    #[serde(default, skip_serializing_if = "JsonMap::is_empty")]
    pub upstream_chat_kwargs: JsonMap<String, JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_request_log_path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback_upstreams: Vec<PersistedFallbackUpstream>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedConfig {
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,
    #[serde(default = "default_upstream_base_url")]
    pub upstream_base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_effort: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_request_log_path: Option<String>,
    #[serde(default, skip_serializing_if = "JsonMap::is_empty")]
    pub upstream_chat_kwargs: JsonMap<String, JsonValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upstreams: Vec<PersistedUpstream>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback_upstreams: Vec<PersistedFallbackUpstream>,
    #[serde(default = "default_upstream_failure_cooldown_secs")]
    pub upstream_failure_cooldown_secs: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_profile_templates: BTreeMap<String, PersistedModelProfile>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_profiles: BTreeMap<String, PersistedModelProfile>,
    #[serde(default = "default_brave_base_url")]
    pub brave_base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brave_api_key: Option<String>,
    #[serde(default = "default_brave_max_results")]
    pub brave_max_results: usize,
    #[serde(default = "default_request_timeout_secs")]
    pub request_timeout_secs: u64,
    #[serde(default = "default_connect_timeout_secs")]
    pub connect_timeout_secs: u64,
    #[serde(default = "default_max_web_search_rounds")]
    pub max_web_search_rounds: usize,
    #[serde(default = "default_flatten_content")]
    pub flatten_content: bool,
    #[serde(default = "default_max_replay_entries")]
    pub max_replay_entries: usize,
}

fn default_bind_addr() -> String {
    "127.0.0.1:4000".to_string()
}

fn default_upstream_base_url() -> String {
    "http://127.0.0.1:8000/v1".to_string()
}

fn default_brave_base_url() -> String {
    "https://api.search.brave.com/res/v1".to_string()
}

fn default_brave_max_results() -> usize {
    5
}

fn default_request_timeout_secs() -> u64 {
    60
}

fn default_connect_timeout_secs() -> u64 {
    10
}

fn default_max_web_search_rounds() -> usize {
    5
}

fn default_flatten_content() -> bool {
    true
}

fn default_max_replay_entries() -> usize {
    1000
}

fn default_upstream_failure_cooldown_secs() -> u64 {
    30
}

impl Default for PersistedConfig {
    fn default() -> Self {
        Self {
            bind_addr: default_bind_addr(),
            upstream_base_url: default_upstream_base_url(),
            upstream_api_key: None,
            upstream_model: None,
            default_reasoning_effort: None,
            system_prompt_prefix: None,
            upstream_request_log_path: None,
            upstream_chat_kwargs: JsonMap::new(),
            upstreams: Vec::new(),
            fallback_upstreams: Vec::new(),
            upstream_failure_cooldown_secs: default_upstream_failure_cooldown_secs(),
            model_profile_templates: BTreeMap::new(),
            model_profiles: BTreeMap::new(),
            brave_base_url: default_brave_base_url(),
            brave_api_key: None,
            brave_max_results: default_brave_max_results(),
            request_timeout_secs: default_request_timeout_secs(),
            connect_timeout_secs: 10,
            max_web_search_rounds: 5,
            flatten_content: true,
            max_replay_entries: 1000,
        }
    }
}

impl Config {
    pub fn from_env_and_file(path: Option<&Path>) -> Result<Self, String> {
        let mut persisted = if let Some(path) = path {
            load_persisted_config(path)?
        } else {
            load_default_persisted_config()?
        };
        apply_env_overrides(&mut persisted);
        Self::from_persisted(&persisted)
    }

    pub fn connect_timeout(&self) -> Duration {
        Duration::from_secs(self.connect_timeout_secs)
    }

    pub fn from_persisted(config: &PersistedConfig) -> Result<Self, String> {
        let bind_addr = config
            .bind_addr
            .parse()
            .map_err(|err| format!("invalid bind_addr: {err}"))?;
        let upstream_base_url = Url::parse(&config.upstream_base_url)
            .map_err(|err| format!("invalid upstream_base_url: {err}"))?;
        let brave_base_url = Url::parse(&config.brave_base_url)
            .map_err(|err| format!("invalid brave_base_url: {err}"))?;
        if config.default_reasoning_effort.is_some() {
            return Err(
                "`default_reasoning_effort` was removed; set `reasoning_effort.default` on the reserved `*` model profile instead".to_string(),
            );
        }
        if let Ok(value) = env::var("LLMCONDUIT_DEFAULT_REASONING_EFFORT")
            && !value.trim().is_empty()
        {
            return Err(
                "`LLMCONDUIT_DEFAULT_REASONING_EFFORT` was removed; set `reasoning_effort.default` on the reserved `*` model profile instead".to_string(),
            );
        }
        let fallback_upstreams = config
            .fallback_upstreams
            .iter()
            .enumerate()
            .map(|(index, provider)| parse_fallback_upstream(provider, index, "fallback_upstreams"))
            .collect::<Result<Vec<_>, String>>()?;
        let upstreams = config
            .upstreams
            .iter()
            .enumerate()
            .map(parse_upstream)
            .collect::<Result<Vec<_>, String>>()?;
        let model_profiles =
            resolve_model_profiles(&config.model_profiles, &config.model_profile_templates)?;
        Ok(Self {
            bind_addr,
            upstream_base_url,
            upstream_api_key: config
                .upstream_api_key
                .as_ref()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            upstream_model: config
                .upstream_model
                .as_ref()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            system_prompt_prefix: config
                .system_prompt_prefix
                .as_ref()
                .filter(|value| !value.trim().is_empty())
                .cloned(),
            upstream_request_log_path: config
                .upstream_request_log_path
                .as_ref()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            upstream_chat_kwargs: config.upstream_chat_kwargs.clone(),
            upstreams,
            fallback_upstreams,
            upstream_failure_cooldown_secs: config.upstream_failure_cooldown_secs,
            model_profiles,
            brave_base_url,
            brave_api_key: config
                .brave_api_key
                .as_ref()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            brave_max_results: config.brave_max_results,
            request_timeout: Duration::from_secs(config.request_timeout_secs),
            connect_timeout_secs: config.connect_timeout_secs,
            max_web_search_rounds: config.max_web_search_rounds,
            flatten_content: config.flatten_content,
            max_replay_entries: config.max_replay_entries,
        })
    }

    pub fn resolve_upstream_model(&self, request_model: &str) -> String {
        self.model_profile(request_model)
            .and_then(|profile| profile.upstream_model.clone())
            .or_else(|| self.upstream_model.clone())
            .unwrap_or_else(|| request_model.to_string())
    }

    pub fn resolve_upstream_chat_kwargs(&self, request_model: &str) -> JsonMap<String, JsonValue> {
        let upstream_model = self.resolve_upstream_model(request_model);
        self.resolve_upstream_chat_kwargs_for_resolved_model(request_model, &upstream_model)
    }

    pub fn resolve_upstream_chat_kwargs_for_resolved_model(
        &self,
        request_model: &str,
        resolved_model: &str,
    ) -> JsonMap<String, JsonValue> {
        let mut kwargs = self.upstream_chat_kwargs.clone();
        for profile in self.model_profiles_for_resolved_model(request_model, resolved_model) {
            merge_json_maps(&mut kwargs, &profile.upstream_chat_kwargs);
        }
        kwargs
    }

    pub fn resolve_system_prompt_prefix(&self, request_model: &str) -> Option<String> {
        let upstream_model = self.resolve_upstream_model(request_model);
        self.resolve_system_prompt_prefix_for_resolved_model(request_model, &upstream_model)
    }

    pub fn resolve_system_prompt_prefix_for_resolved_model(
        &self,
        request_model: &str,
        resolved_model: &str,
    ) -> Option<String> {
        let profile_prefix = self
            .model_profiles_for_resolved_model(request_model, resolved_model)
            .into_iter()
            .rev()
            .find_map(|profile| profile.system_prompt_prefix.clone());
        join_prompt_prefixes(
            [self.system_prompt_prefix.clone(), profile_prefix]
                .into_iter()
                .flatten(),
        )
    }

    /// Resolve the capabilities config advertised for an upstream model id. A profile
    /// keyed by the id wins; otherwise the first alias (BTreeMap key order, i.e.
    /// lexicographically smallest profile key) whose `upstream_model` targets the id
    /// case-insensitively wins - so among several profiles aliasing the same upstream id
    /// the lexicographically-smallest profile key is selected, and the reserved `*`
    /// profile participates in this same tie-break. A matching profile without a
    /// `capabilities` block yields `None` (no fill-in). If no profile matches, the
    /// reserved `*` profile's `capabilities` is used.
    pub fn resolve_capabilities_for_upstream(&self, id: &str) -> Option<&CapabilitiesConfig> {
        if let Some(profile) = self.model_profile(id) {
            return profile.capabilities.as_ref();
        }
        for profile in self.model_profiles.values() {
            if profile
                .upstream_model
                .as_deref()
                .map(|upstream| upstream.eq_ignore_ascii_case(id))
                .unwrap_or(false)
            {
                return profile.capabilities.as_ref();
            }
        }
        self.model_profile("*")
            .and_then(|p| p.capabilities.as_ref())
    }

    /// Resolve reasoning shaping with the same request/resolved-model precedence
    /// used for other per-profile request settings.
    pub fn resolve_reasoning_config(&self, request_model: &str) -> Option<&ReasoningConfig> {
        let upstream_model = self.resolve_upstream_model(request_model);
        self.resolve_reasoning_config_for_resolved_model(request_model, &upstream_model)
    }

    /// Like `resolve_reasoning_config` but takes the already-resolved backend id
    /// (the normalized model the generation and token-count paths use for
    /// `upstream_chat_kwargs` and the system-prompt prefix) so reasoning profile
    /// matching sees the same id those paths use, instead of re-resolving it here.
    pub fn resolve_reasoning_config_for_resolved_model(
        &self,
        request_model: &str,
        resolved_model: &str,
    ) -> Option<&ReasoningConfig> {
        self.model_profiles_for_resolved_model(request_model, resolved_model)
            .into_iter()
            .rev()
            .find_map(|profile| profile.reasoning_effort.as_ref())
    }

    /// Collect profiles matching the request model chain. `resolved_model` is the
    /// catalog-normalized backend id. The pre-normalization upstream alias is also
    /// tried because a profile may be keyed by that alias before normalization
    /// collapses it to the backend id. Lookup order is [resolved, alias, request],
    /// with pointer-dedup to keep precedence stable. The reserved `*` profile is a
    /// pure fallback: it is included only when no specific profile matches, so an
    /// explicit match never inherits unset fields from `*` (use profile
    pub fn resolve_roles_config(&self, request_model: &str) -> Option<&RolesConfig> {
        let upstream_model = self.resolve_upstream_model(request_model);
        self.resolve_roles_config_for_resolved_model(request_model, &upstream_model)
    }

    pub(crate) fn resolve_roles_config_for_resolved_model(
        &self,
        request_model: &str,
        resolved_model: &str,
    ) -> Option<&RolesConfig> {
        self.model_profiles_for_resolved_model(request_model, resolved_model)
            .into_iter()
            .find_map(|profile| profile.roles.as_ref())
    }

    /// Collect profiles matching the request model chain. `resolved_model` must be
    /// `resolve_upstream_model(request_model)` (callers pass the already-resolved upstream
    /// id); it is not re-resolved here. The resolved/upstream model is tried first, then the
    /// request model itself, with pointer-dedup to keep the precedence order stable. The
    /// reserved `*` profile is a pure fallback: it is included only when no specific profile
    /// matches, so an explicit match never inherits unset fields from `*` (use profile
    /// templates to share fields between explicit profiles).
    fn model_profiles_for_resolved_model(
        &self,
        request_model: &str,
        resolved_model: &str,
    ) -> Vec<&ModelProfile> {
        let upstream_alias = self.resolve_upstream_model(request_model);
        let mut profiles: Vec<&ModelProfile> = Vec::new();
        for model in [resolved_model, upstream_alias.as_str(), request_model] {
            if let Some(profile) = self.model_profile(model)
                && !profiles
                    .iter()
                    .any(|existing| std::ptr::eq(*existing, profile))
            {
                profiles.push(profile);
            }
        }
        if profiles.is_empty() {
            if let Some(profile) = self.model_profile("*") {
                profiles.push(profile);
            }
        }
        profiles
    }

    fn model_profile(&self, request_model: &str) -> Option<&ModelProfile> {
        self.model_profiles.get(request_model).or_else(|| {
            self.model_profiles
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(request_model))
                .map(|(_, profile)| profile)
        })
    }
}

#[derive(Debug, Clone, Default)]
struct ResolvedModelProfile {
    upstream_model: Option<String>,
    system_prompt_prefixes: Vec<String>,
    roles: Option<RolesConfig>,
    upstream_chat_kwargs: JsonMap<String, JsonValue>,
    capabilities: Option<CapabilitiesConfig>,
    reasoning_effort: Option<ReasoningConfig>,
}

impl ResolvedModelProfile {
    fn into_model_profile(self) -> ModelProfile {
        ModelProfile {
            upstream_model: self.upstream_model,
            system_prompt_prefix: join_prompt_prefixes(self.system_prompt_prefixes),
            roles: self.roles,
            upstream_chat_kwargs: self.upstream_chat_kwargs,
            capabilities: self.capabilities,
            reasoning_effort: self.reasoning_effort,
        }
    }
}

fn resolve_model_profiles(
    profiles: &BTreeMap<String, PersistedModelProfile>,
    templates: &BTreeMap<String, PersistedModelProfile>,
) -> Result<BTreeMap<String, ModelProfile>, String> {
    let mut resolved = BTreeMap::new();
    for (name, profile) in profiles {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let profile = resolve_persisted_model_profile(profile, templates, &mut Vec::new())
            .map_err(|err| format!("model_profiles[{name}]: {err}"))?;
        let profile = profile.into_model_profile();
        if let Some(roles) = &profile.roles {
            roles
                .validate()
                .map_err(|err| format!("model_profiles[{name}]: {err}"))?;
        }
        resolved.insert(name.to_string(), profile);
    }
    Ok(resolved)
}

fn resolve_persisted_model_profile(
    profile: &PersistedModelProfile,
    templates: &BTreeMap<String, PersistedModelProfile>,
    stack: &mut Vec<String>,
) -> Result<ResolvedModelProfile, String> {
    let mut resolved = ResolvedModelProfile::default();
    for template_name in &profile.extends {
        let template_name = template_name.trim();
        if template_name.is_empty() {
            continue;
        }
        if stack.iter().any(|name| name == template_name) {
            let mut cycle = stack.clone();
            cycle.push(template_name.to_string());
            return Err(format!("template cycle: {}", cycle.join(" -> ")));
        }
        let template = templates
            .get(template_name)
            .or_else(|| {
                templates
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(template_name))
                    .map(|(_, template)| template)
            })
            .ok_or_else(|| format!("unknown template {template_name:?}"))?;
        stack.push(template_name.to_string());
        let template = resolve_persisted_model_profile(template, templates, stack)?;
        stack.pop();
        merge_resolved_model_profile(&mut resolved, template);
    }
    merge_persisted_model_profile(&mut resolved, profile);
    Ok(resolved)
}

/// Merge a resolved template (`source`) into the accumulating `destination`. The
/// `capabilities` block overrides wholesale: a child re-specifies a block to replace
/// the inherited one; omitting it (or writing `null`) does NOT clear an inherited
/// block - there is no opt-out, only override. Applies to `merge_persisted_model_profile`
/// below as well.
fn merge_resolved_model_profile(
    destination: &mut ResolvedModelProfile,
    source: ResolvedModelProfile,
) {
    if source.upstream_model.is_some() {
        destination.upstream_model = source.upstream_model;
    }
    if source.capabilities.is_some() {
        destination.capabilities = source.capabilities;
    }
    if source.reasoning_effort.is_some() {
        destination.reasoning_effort = source.reasoning_effort;
    }
    if source.roles.is_some() {
        destination.roles = source.roles;
    }
    destination
        .system_prompt_prefixes
        .extend(source.system_prompt_prefixes);
    merge_json_maps(
        &mut destination.upstream_chat_kwargs,
        &source.upstream_chat_kwargs,
    );
}

fn merge_persisted_model_profile(
    destination: &mut ResolvedModelProfile,
    source: &PersistedModelProfile,
) {
    if let Some(upstream_model) = trim_nonempty(source.upstream_model.as_deref()) {
        destination.upstream_model = Some(upstream_model);
    }
    if let Some(system_prompt_prefix) = trim_nonempty(source.system_prompt_prefix.as_deref()) {
        destination
            .system_prompt_prefixes
            .push(system_prompt_prefix);
    }
    if source.capabilities.is_some() {
        destination.capabilities = source.capabilities.clone();
    }
    if source.reasoning_effort.is_some() {
        destination.reasoning_effort = source.reasoning_effort.clone();
    }
    if let Some(roles) = &source.roles {
        destination.roles = Some(roles.clone());
    }
    merge_json_maps(
        &mut destination.upstream_chat_kwargs,
        &source.upstream_chat_kwargs,
    );
}

fn join_prompt_prefixes(prefixes: impl IntoIterator<Item = String>) -> Option<String> {
    let prefixes = prefixes
        .into_iter()
        .map(|prefix| prefix.trim().to_string())
        .filter(|prefix| !prefix.is_empty())
        .collect::<Vec<_>>();
    if prefixes.is_empty() {
        None
    } else {
        Some(prefixes.join("\n\n"))
    }
}

fn parse_upstream(
    (index, provider): (usize, &PersistedUpstream),
) -> Result<UpstreamConfig, String> {
    let upstream_base_url = Url::parse(provider.upstream_base_url.trim())
        .map_err(|err| format!("invalid upstreams[{index}].upstream_base_url: {err}"))?;
    let fallback_upstreams = provider
        .fallback_upstreams
        .iter()
        .enumerate()
        .map(|(fallback_index, fallback)| {
            parse_fallback_upstream(
                fallback,
                fallback_index,
                &format!("upstreams[{index}].fallback_upstreams"),
            )
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(UpstreamConfig {
        name: provider
            .name
            .as_ref()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| format!("upstream-{}", index + 1)),
        upstream_base_url,
        upstream_api_key: trim_nonempty(provider.upstream_api_key.as_deref()),
        upstream_model: trim_nonempty(provider.upstream_model.as_deref()),
        upstream_chat_kwargs: provider.upstream_chat_kwargs.clone(),
        upstream_request_log_path: trim_nonempty(provider.upstream_request_log_path.as_deref())
            .map(PathBuf::from),
        fallback_upstreams,
    })
}

fn parse_fallback_upstream(
    provider: &PersistedFallbackUpstream,
    index: usize,
    path: &str,
) -> Result<FallbackUpstreamConfig, String> {
    let upstream_base_url = Url::parse(provider.upstream_base_url.trim())
        .map_err(|err| format!("invalid {path}[{index}].upstream_base_url: {err}"))?;
    Ok(FallbackUpstreamConfig {
        name: provider
            .name
            .as_ref()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| format!("fallback-{}", index + 1)),
        upstream_base_url,
        upstream_api_key: trim_nonempty(provider.upstream_api_key.as_deref()),
        upstream_model: trim_nonempty(provider.upstream_model.as_deref()),
        exposed_model: trim_nonempty(provider.exposed_model.as_deref()),
        upstream_chat_kwargs: provider.upstream_chat_kwargs.clone(),
        upstream_request_log_path: trim_nonempty(provider.upstream_request_log_path.as_deref())
            .map(PathBuf::from),
    })
}

fn trim_nonempty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

pub fn default_config_path() -> Result<PathBuf, String> {
    let config_dir = dirs::config_dir()
        .ok_or_else(|| "unable to determine configuration directory".to_string())?;
    Ok(config_dir.join("llmconduit").join("config.yaml"))
}

pub fn load_default_persisted_config() -> Result<PersistedConfig, String> {
    let path = default_config_path()?;
    load_persisted_config(&path)
}

pub fn load_persisted_config(path: &Path) -> Result<PersistedConfig, String> {
    if !path.exists() {
        return Ok(PersistedConfig::default());
    }
    let contents = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    serde_yaml::from_str(&contents)
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))
}

pub fn write_persisted_config(path: &Path, config: &PersistedConfig) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("config path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    let yaml = serde_yaml::to_string(config)
        .map_err(|err| format!("failed to serialize config: {err}"))?;
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true).mode(0o600);
        let mut file = opts
            .open(path)
            .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
        file.write_all(yaml.as_bytes())
            .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        fs::write(path, yaml)
            .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    }
    Ok(())
}

fn apply_env_overrides(config: &mut PersistedConfig) {
    if let Ok(value) = env::var("LLMCONDUIT_BIND_ADDR")
        && !value.trim().is_empty()
    {
        config.bind_addr = value;
    }
    if let Ok(value) = env::var("LLMCONDUIT_UPSTREAM_BASE_URL")
        && !value.trim().is_empty()
    {
        config.upstream_base_url = value;
    }
    if let Ok(value) = env::var("LLMCONDUIT_UPSTREAM_API_KEY")
        && !value.trim().is_empty()
    {
        config.upstream_api_key = Some(value);
    } else if config.upstream_api_key.is_none()
        && let Ok(value) = env::var("OPENAI_API_KEY")
        && !value.trim().is_empty()
    {
        config.upstream_api_key = Some(value);
    }
    if let Ok(value) = env::var("LLMCONDUIT_UPSTREAM_MODEL")
        && !value.trim().is_empty()
    {
        config.upstream_model = Some(value);
    }
    if let Ok(value) = env::var("LLMCONDUIT_SYSTEM_PROMPT_PREFIX")
        && !value.trim().is_empty()
    {
        config.system_prompt_prefix = Some(value);
    }
    if let Ok(value) = env::var("LLMCONDUIT_UPSTREAM_REQUEST_LOG_PATH")
        && !value.trim().is_empty()
    {
        config.upstream_request_log_path = Some(value);
    }
    if let Ok(value) = env::var("LLMCONDUIT_UPSTREAM_CHAT_KWARGS_JSON")
        && !value.trim().is_empty()
        && let Ok(parsed) = serde_json::from_str::<JsonMap<String, JsonValue>>(&value)
    {
        config.upstream_chat_kwargs = parsed;
    }
    if let Ok(value) = env::var("LLMCONDUIT_UPSTREAM_FAILURE_COOLDOWN_SECS")
        && let Ok(parsed) = value.parse()
    {
        config.upstream_failure_cooldown_secs = parsed;
    }
    if let Ok(value) = env::var("LLMCONDUIT_BRAVE_BASE_URL")
        && !value.trim().is_empty()
    {
        config.brave_base_url = value;
    }
    if let Ok(value) = env::var("BRAVE_SEARCH_API_KEY")
        && !value.trim().is_empty()
    {
        config.brave_api_key = Some(value);
    }
    if let Ok(value) = env::var("LLMCONDUIT_BRAVE_MAX_RESULTS")
        && let Ok(parsed) = value.parse()
    {
        config.brave_max_results = parsed;
    }
    if let Ok(value) = env::var("LLMCONDUIT_REQUEST_TIMEOUT_SECS")
        && let Ok(parsed) = value.parse()
    {
        config.request_timeout_secs = parsed;
    }
    if let Ok(value) = env::var("LLMCONDUIT_CONNECT_TIMEOUT_SECS")
        && let Ok(parsed) = value.parse()
    {
        config.connect_timeout_secs = parsed;
    }
    if let Ok(value) = env::var("LLMCONDUIT_MAX_WEB_SEARCH_ROUNDS")
        && let Ok(parsed) = value.parse()
    {
        config.max_web_search_rounds = parsed;
    }
    if let Ok(value) = env::var("LLMCONDUIT_FLATTEN_CONTENT")
        && let Ok(parsed) = value.parse()
    {
        config.flatten_content = parsed;
    }
    if let Ok(value) = env::var("LLMCONDUIT_MAX_REPLAY_ENTRIES")
        && let Ok(parsed) = value.parse()
    {
        config.max_replay_entries = parsed;
    }
}

pub fn merge_json_maps(
    destination: &mut JsonMap<String, JsonValue>,
    source: &JsonMap<String, JsonValue>,
) {
    for (key, source_value) in source {
        match (destination.get_mut(key), source_value) {
            (Some(JsonValue::Object(destination_object)), JsonValue::Object(source_object)) => {
                merge_json_maps(destination_object, source_object);
            }
            _ => {
                destination.insert(key.clone(), source_value.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Action;
    use super::CapabilitiesConfig;
    use super::Config;
    use super::ContextFeature;
    use super::EffortLevel;
    use super::JsonMap;
    use super::JsonValue;
    use super::PersistedConfig;
    use super::PersistedFallbackUpstream;
    use super::PersistedModelProfile;
    use super::PersistedUpstream;
    use super::RoleRule;
    use super::RoleRuleSet;
    use super::RolesConfig;
    use super::SimpleCap;
    use super::ThinkingCap;
    use super::apply_env_overrides;
    use super::default_config_path;
    use super::load_persisted_config;
    use super::merge_json_maps;
    use super::write_persisted_config;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn default_config_path_uses_llmconduit_config_dir() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let config_home = std::env::temp_dir().join(format!(
            "llmconduit-xdg-config-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let previous_xdg_config_home = std::env::var_os("XDG_CONFIG_HOME");
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &config_home);
        }

        let path = default_config_path().expect("default config path");
        assert_eq!(path, config_home.join("llmconduit").join("config.yaml"));

        unsafe {
            match previous_xdg_config_home {
                Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }

    #[test]
    fn from_persisted_invalid_base_url() {
        let config = PersistedConfig {
            upstream_base_url: "not a url".to_string(),
            ..PersistedConfig::default()
        };
        assert!(Config::from_persisted(&config).is_err());
    }

    #[test]
    fn whitespace_api_key_trimmed() {
        let config = PersistedConfig {
            upstream_api_key: Some("  secret  ".to_string()),
            ..PersistedConfig::default()
        };
        let result = Config::from_persisted(&config).unwrap();
        assert_eq!(result.upstream_api_key, Some("secret".to_string()));

        let config2 = PersistedConfig {
            upstream_api_key: Some("   ".to_string()),
            ..PersistedConfig::default()
        };
        let result2 = Config::from_persisted(&config2).unwrap();
        assert_eq!(result2.upstream_api_key, None);
    }

    #[test]
    fn default_reasoning_effort_is_rejected_with_migration_error() {
        let config = PersistedConfig {
            default_reasoning_effort: Some(json!("max")),
            ..PersistedConfig::default()
        };
        let err = Config::from_persisted(&config).unwrap_err();
        assert!(err.contains("default_reasoning_effort"));
        assert!(err.contains("reasoning_effort.default"));
    }

    #[test]
    fn from_persisted_parses_fallback_upstreams() {
        let config = PersistedConfig {
            fallback_upstreams: vec![
                PersistedFallbackUpstream {
                    name: Some(" backup ".to_string()),
                    upstream_base_url: "  http://127.0.0.1:8001/v1  ".to_string(),
                    upstream_api_key: Some(" backup-secret ".to_string()),
                    upstream_model: Some(" fallback-model ".to_string()),
                    exposed_model: Some(" fallback-alias ".to_string()),
                    upstream_chat_kwargs: JsonMap::from_iter([(
                        "provider".to_string(),
                        json!({
                            "order": ["z-ai"],
                            "allow_fallbacks": true
                        }),
                    )]),
                    upstream_request_log_path: Some(" /tmp/llmconduit-fallback.jsonl ".to_string()),
                },
                PersistedFallbackUpstream {
                    name: Some("   ".to_string()),
                    upstream_base_url: "http://127.0.0.1:8002/v1".to_string(),
                    upstream_api_key: Some("   ".to_string()),
                    upstream_model: None,
                    exposed_model: None,
                    upstream_chat_kwargs: JsonMap::new(),
                    upstream_request_log_path: None,
                },
            ],
            upstream_failure_cooldown_secs: 12,
            ..PersistedConfig::default()
        };

        let result = Config::from_persisted(&config).expect("config");

        assert_eq!(result.upstream_failure_cooldown_secs, 12);
        assert_eq!(result.fallback_upstreams.len(), 2);
        assert_eq!(result.fallback_upstreams[0].name, "backup");
        assert_eq!(
            result.fallback_upstreams[0].upstream_base_url.as_str(),
            "http://127.0.0.1:8001/v1"
        );
        assert_eq!(
            result.fallback_upstreams[0].upstream_api_key.as_deref(),
            Some("backup-secret")
        );
        assert_eq!(
            result.fallback_upstreams[0].upstream_model.as_deref(),
            Some("fallback-model")
        );
        assert_eq!(
            result.fallback_upstreams[0].exposed_model.as_deref(),
            Some("fallback-alias")
        );
        assert_eq!(
            result.fallback_upstreams[0]
                .upstream_chat_kwargs
                .get("provider"),
            Some(&json!({
                "order": ["z-ai"],
                "allow_fallbacks": true
            }))
        );
        assert_eq!(
            result.fallback_upstreams[0]
                .upstream_request_log_path
                .as_deref(),
            Some(std::path::Path::new("/tmp/llmconduit-fallback.jsonl"))
        );
        assert_eq!(result.fallback_upstreams[1].name, "fallback-2");
        assert_eq!(result.fallback_upstreams[1].upstream_api_key, None);
    }

    #[test]
    fn from_persisted_parses_explicit_upstreams_with_nested_fallbacks() {
        let config = PersistedConfig {
            upstreams: vec![PersistedUpstream {
                name: Some(" local ".to_string()),
                upstream_base_url: " http://127.0.0.1:8000/v1 ".to_string(),
                upstream_api_key: Some(" local-secret ".to_string()),
                upstream_model: Some(" local-model ".to_string()),
                upstream_chat_kwargs: JsonMap::from_iter([(
                    "chat_template_kwargs".to_string(),
                    json!({"thinking": true}),
                )]),
                upstream_request_log_path: Some(" /tmp/llmconduit-local.jsonl ".to_string()),
                fallback_upstreams: vec![PersistedFallbackUpstream {
                    name: Some(" backup ".to_string()),
                    upstream_base_url: " https://openrouter.ai/api/v1 ".to_string(),
                    upstream_api_key: Some(" backup-secret ".to_string()),
                    upstream_model: Some(" backup-model ".to_string()),
                    exposed_model: Some(" backup-alias ".to_string()),
                    upstream_chat_kwargs: JsonMap::from_iter([(
                        "provider".to_string(),
                        json!({"order": ["openai"]}),
                    )]),
                    upstream_request_log_path: Some(" /tmp/llmconduit-backup.jsonl ".to_string()),
                }],
            }],
            ..PersistedConfig::default()
        };

        let result = Config::from_persisted(&config).expect("config");

        assert_eq!(result.upstreams.len(), 1);
        let upstream = &result.upstreams[0];
        assert_eq!(upstream.name, "local");
        assert_eq!(
            upstream.upstream_base_url.as_str(),
            "http://127.0.0.1:8000/v1"
        );
        assert_eq!(upstream.upstream_api_key.as_deref(), Some("local-secret"));
        assert_eq!(upstream.upstream_model.as_deref(), Some("local-model"));
        assert_eq!(
            upstream.upstream_chat_kwargs.get("chat_template_kwargs"),
            Some(&json!({"thinking": true}))
        );
        assert_eq!(
            upstream.upstream_request_log_path.as_deref(),
            Some(std::path::Path::new("/tmp/llmconduit-local.jsonl"))
        );
        assert_eq!(upstream.fallback_upstreams.len(), 1);
        assert_eq!(upstream.fallback_upstreams[0].name, "backup");
        assert_eq!(
            upstream.fallback_upstreams[0].upstream_model.as_deref(),
            Some("backup-model")
        );
        assert_eq!(
            upstream.fallback_upstreams[0].exposed_model.as_deref(),
            Some("backup-alias")
        );
    }

    #[test]
    fn from_persisted_rejects_invalid_fallback_upstream_url() {
        let config = PersistedConfig {
            fallback_upstreams: vec![PersistedFallbackUpstream {
                upstream_base_url: "not a url".to_string(),
                ..PersistedFallbackUpstream::default()
            }],
            ..PersistedConfig::default()
        };

        let error = Config::from_persisted(&config).expect_err("invalid fallback URL");

        assert!(error.contains("invalid fallback_upstreams[0].upstream_base_url"));
    }

    #[test]
    fn load_persisted_config_missing_file_returns_default() {
        let result = load_persisted_config(std::path::Path::new(
            "/tmp/nonexistent-llmconduit-config-test.yaml",
        ));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), PersistedConfig::default());
    }

    #[test]
    fn apply_env_overrides_upstream_api_key() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
            std::env::remove_var("LLMCONDUIT_UPSTREAM_API_KEY");
            std::env::set_var("LLMCONDUIT_UPSTREAM_API_KEY", "test-key-12345");
        }
        let mut config = PersistedConfig::default();
        apply_env_overrides(&mut config);
        assert_eq!(config.upstream_api_key, Some("test-key-12345".to_string()));
        unsafe {
            std::env::remove_var("LLMCONDUIT_UPSTREAM_API_KEY");
            std::env::remove_var("OPENAI_API_KEY");
        };
    }

    #[test]
    fn apply_env_overrides_openai_fallback() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        unsafe {
            std::env::remove_var("LLMCONDUIT_UPSTREAM_API_KEY");
            std::env::remove_var("OPENAI_API_KEY");
            std::env::set_var("OPENAI_API_KEY", "fallback-key-67890");
        }
        let mut config = PersistedConfig::default();
        config.upstream_api_key = None;
        apply_env_overrides(&mut config);
        assert_eq!(
            config.upstream_api_key,
            Some("fallback-key-67890".to_string())
        );
        unsafe {
            std::env::remove_var("LLMCONDUIT_UPSTREAM_API_KEY");
            std::env::remove_var("OPENAI_API_KEY");
        };
    }

    #[test]
    fn apply_env_overrides_system_prompt_prefix() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        unsafe {
            std::env::set_var("LLMCONDUIT_SYSTEM_PROMPT_PREFIX", "Global prefix.");
        }
        let mut config = PersistedConfig::default();
        apply_env_overrides(&mut config);
        assert_eq!(
            config.system_prompt_prefix,
            Some("Global prefix.".to_string())
        );
        unsafe {
            std::env::remove_var("LLMCONDUIT_SYSTEM_PROMPT_PREFIX");
        };
    }

    #[test]
    fn apply_env_overrides_upstream_failure_cooldown() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        unsafe {
            std::env::set_var("LLMCONDUIT_UPSTREAM_FAILURE_COOLDOWN_SECS", "7");
        }
        let mut config = PersistedConfig::default();
        apply_env_overrides(&mut config);
        assert_eq!(config.upstream_failure_cooldown_secs, 7);
        unsafe {
            std::env::remove_var("LLMCONDUIT_UPSTREAM_FAILURE_COOLDOWN_SECS");
        };
    }

    #[test]
    fn persisted_config_roundtrips() {
        let path = std::env::temp_dir().join(format!(
            "llmconduit-config-{}.yaml",
            uuid::Uuid::new_v4().simple()
        ));
        let config = PersistedConfig {
            bind_addr: "127.0.0.1:4010".to_string(),
            upstream_base_url: "http://127.0.0.1:8000/v1".to_string(),
            upstream_api_key: Some("upstream-secret".to_string()),
            upstream_model: Some("grok-4".to_string()),
            default_reasoning_effort: None,
            system_prompt_prefix: Some("Global prefix.".to_string()),
            upstream_request_log_path: Some("/tmp/llmconduit-upstream.jsonl".to_string()),
            upstream_chat_kwargs: JsonMap::from_iter([(
                "clear_thinking".to_string(),
                JsonValue::Bool(false),
            )]),
            upstreams: Vec::new(),
            fallback_upstreams: Vec::new(),
            upstream_failure_cooldown_secs: 30,
            model_profile_templates: BTreeMap::from_iter([(
                "streaming-reasoning".to_string(),
                PersistedModelProfile {
                    upstream_chat_kwargs: JsonMap::from_iter([(
                        "stream_reasoning".to_string(),
                        JsonValue::Bool(true),
                    )]),
                    ..Default::default()
                },
            )]),
            model_profiles: BTreeMap::from_iter([(
                "Kimi-K2.6".to_string(),
                PersistedModelProfile {
                    extends: vec!["streaming-reasoning".to_string()],
                    system_prompt_prefix: Some("Use Kimi-compatible behavior.".to_string()),
                    upstream_chat_kwargs: JsonMap::from_iter([(
                        "chat_template_kwargs".to_string(),
                        json!({
                            "thinking": true,
                            "preserve_thinking": true
                        }),
                    )]),
                    ..Default::default()
                },
            )]),
            brave_base_url: "https://api.search.brave.com/res/v1".to_string(),
            brave_api_key: Some("secret".to_string()),
            brave_max_results: 7,
            request_timeout_secs: 45,
            connect_timeout_secs: 10,
            max_web_search_rounds: 10,
            flatten_content: false,
            max_replay_entries: 1000,
        };
        write_persisted_config(&path, &config).expect("write config");
        let loaded = load_persisted_config(&path).expect("load config");
        assert_eq!(loaded, config);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn resolves_profile_specific_upstream_chat_kwargs() {
        let config = Config::from_persisted(&PersistedConfig {
            bind_addr: "127.0.0.1:4010".to_string(),
            upstream_base_url: "http://127.0.0.1:8000/v1".to_string(),
            upstream_api_key: None,
            upstream_model: None,
            default_reasoning_effort: None,
            system_prompt_prefix: None,
            upstream_request_log_path: None,
            upstream_chat_kwargs: JsonMap::new(),
            upstreams: Vec::new(),
            fallback_upstreams: Vec::new(),
            upstream_failure_cooldown_secs: 30,
            model_profile_templates: BTreeMap::new(),
            model_profiles: BTreeMap::from_iter([(
                "Kimi-K2.6".to_string(),
                PersistedModelProfile {
                    upstream_chat_kwargs: JsonMap::from_iter([(
                        "chat_template_kwargs".to_string(),
                        json!({
                            "thinking": true,
                            "preserve_thinking": true
                        }),
                    )]),
                    ..Default::default()
                },
            )]),
            brave_base_url: "https://api.search.brave.com/res/v1".to_string(),
            brave_api_key: None,
            brave_max_results: 5,
            request_timeout_secs: 60,
            connect_timeout_secs: 10,
            max_web_search_rounds: 5,
            flatten_content: true,
            max_replay_entries: 1000,
        })
        .expect("config");

        assert_eq!(
            config.resolve_upstream_model("Kimi-K2.6"),
            "Kimi-K2.6".to_string()
        );
        assert_eq!(
            config.resolve_upstream_chat_kwargs("Kimi-K2.6"),
            JsonMap::from_iter([(
                "chat_template_kwargs".to_string(),
                json!({
                    "thinking": true,
                    "preserve_thinking": true
                }),
            )])
        );
    }

    #[test]
    fn star_profile_provides_upstream_chat_kwargs_for_unmatched_model() {
        let config = Config::from_persisted(&PersistedConfig {
            bind_addr: "127.0.0.1:4010".to_string(),
            upstream_base_url: "http://127.0.0.1:8000/v1".to_string(),
            upstream_api_key: None,
            upstream_model: None,
            default_reasoning_effort: None,
            system_prompt_prefix: None,
            upstream_request_log_path: None,
            upstream_chat_kwargs: JsonMap::new(),
            upstreams: Vec::new(),
            fallback_upstreams: Vec::new(),
            upstream_failure_cooldown_secs: 30,
            model_profile_templates: BTreeMap::new(),
            model_profiles: BTreeMap::from_iter([(
                "*".to_string(),
                PersistedModelProfile {
                    upstream_chat_kwargs: JsonMap::from_iter([(
                        "chat_template_kwargs".to_string(),
                        json!({ "enable_thinking": true }),
                    )]),
                    ..Default::default()
                },
            )]),
            brave_base_url: "https://api.search.brave.com/res/v1".to_string(),
            brave_api_key: None,
            brave_max_results: 5,
            request_timeout_secs: 60,
            connect_timeout_secs: 10,
            max_web_search_rounds: 5,
            flatten_content: true,
            max_replay_entries: 1000,
        })
        .expect("config");

        // No specific profile matches; the reserved `*` profile supplies the kwargs.
        assert_eq!(
            config.resolve_upstream_chat_kwargs("unmatched-model"),
            JsonMap::from_iter([(
                "chat_template_kwargs".to_string(),
                json!({ "enable_thinking": true }),
            )])
        );
    }

    #[test]
    fn explicit_profile_match_does_not_inherit_star_upstream_chat_kwargs() {
        let config = Config::from_persisted(&PersistedConfig {
            bind_addr: "127.0.0.1:4010".to_string(),
            upstream_base_url: "http://127.0.0.1:8000/v1".to_string(),
            upstream_api_key: None,
            upstream_model: None,
            default_reasoning_effort: None,
            system_prompt_prefix: None,
            upstream_request_log_path: None,
            upstream_chat_kwargs: JsonMap::new(),
            upstreams: Vec::new(),
            fallback_upstreams: Vec::new(),
            upstream_failure_cooldown_secs: 30,
            model_profile_templates: BTreeMap::new(),
            model_profiles: BTreeMap::from_iter([
                (
                    "*".to_string(),
                    PersistedModelProfile {
                        upstream_chat_kwargs: JsonMap::from_iter([(
                            "chat_template_kwargs".to_string(),
                            json!({ "enable_thinking": true, "keep_all_reasoning": true }),
                        )]),
                        ..Default::default()
                    },
                ),
                (
                    "glm-5.2".to_string(),
                    PersistedModelProfile {
                        upstream_chat_kwargs: JsonMap::from_iter([(
                            "chat_template_kwargs".to_string(),
                            json!({ "enable_thinking": false }),
                        )]),
                        ..Default::default()
                    },
                ),
            ]),
            brave_base_url: "https://api.search.brave.com/res/v1".to_string(),
            brave_api_key: None,
            brave_max_results: 5,
            request_timeout_secs: 60,
            connect_timeout_secs: 10,
            max_web_search_rounds: 5,
            flatten_content: true,
            max_replay_entries: 1000,
        })
        .expect("config");

        // An explicit match uses only that profile; the `*` profile does not fill in unset
        // fields (keep_all_reasoning is absent even though `*` sets it).
        assert_eq!(
            config.resolve_upstream_chat_kwargs("glm-5.2"),
            JsonMap::from_iter([(
                "chat_template_kwargs".to_string(),
                json!({ "enable_thinking": false }),
            )])
        );
    }

    #[test]
    fn resolves_model_profiles_case_insensitively() {
        let config = Config::from_persisted(&PersistedConfig {
            bind_addr: "127.0.0.1:4010".to_string(),
            upstream_base_url: "http://127.0.0.1:8000/v1".to_string(),
            upstream_api_key: None,
            upstream_model: None,
            default_reasoning_effort: None,
            system_prompt_prefix: None,
            upstream_request_log_path: None,
            upstream_chat_kwargs: JsonMap::new(),
            upstreams: Vec::new(),
            fallback_upstreams: Vec::new(),
            upstream_failure_cooldown_secs: 30,
            model_profile_templates: BTreeMap::new(),
            model_profiles: BTreeMap::from_iter([(
                "MiMo-V2.5".to_string(),
                PersistedModelProfile {
                    upstream_model: Some("mimo-v2.5".to_string()),
                    system_prompt_prefix: Some("Prefer concise answers.".to_string()),
                    upstream_chat_kwargs: JsonMap::from_iter([
                        ("separate_reasoning".to_string(), JsonValue::Bool(true)),
                        (
                            "chat_template_kwargs".to_string(),
                            json!({
                                "enable_thinking": true,
                                "keep_all_reasoning": true
                            }),
                        ),
                    ]),
                    ..Default::default()
                },
            )]),
            brave_base_url: "https://api.search.brave.com/res/v1".to_string(),
            brave_api_key: None,
            brave_max_results: 5,
            request_timeout_secs: 60,
            connect_timeout_secs: 10,
            max_web_search_rounds: 5,
            flatten_content: true,
            max_replay_entries: 1000,
        })
        .expect("config");

        assert_eq!(config.resolve_upstream_model("mimo-v2.5"), "mimo-v2.5");
        assert_eq!(
            config.resolve_upstream_chat_kwargs("mimo-v2.5"),
            JsonMap::from_iter([
                ("separate_reasoning".to_string(), JsonValue::Bool(true)),
                (
                    "chat_template_kwargs".to_string(),
                    json!({
                        "enable_thinking": true,
                        "keep_all_reasoning": true
                    }),
                ),
            ])
        );
    }

    #[test]
    fn resolves_upstream_model_profile_after_global_model_remap() {
        let config = Config::from_persisted(&PersistedConfig {
            bind_addr: "127.0.0.1:4010".to_string(),
            upstream_base_url: "https://openrouter.ai/api/v1".to_string(),
            upstream_api_key: None,
            upstream_model: Some("xiaomi/mimo-v2.5-pro".to_string()),
            default_reasoning_effort: None,
            system_prompt_prefix: None,
            upstream_request_log_path: None,
            upstream_chat_kwargs: JsonMap::new(),
            upstreams: Vec::new(),
            fallback_upstreams: Vec::new(),
            upstream_failure_cooldown_secs: 30,
            model_profile_templates: BTreeMap::new(),
            model_profiles: BTreeMap::from_iter([(
                "xiaomi/mimo-v2.5-pro".to_string(),
                PersistedModelProfile {
                    system_prompt_prefix: Some("Use MiMo-compatible behavior.".to_string()),
                    upstream_chat_kwargs: JsonMap::from_iter([(
                        "reasoning".to_string(),
                        json!({
                            "enabled": true
                        }),
                    )]),
                    ..Default::default()
                },
            )]),
            brave_base_url: "https://api.search.brave.com/res/v1".to_string(),
            brave_api_key: None,
            brave_max_results: 5,
            request_timeout_secs: 60,
            connect_timeout_secs: 10,
            max_web_search_rounds: 5,
            flatten_content: true,
            max_replay_entries: 1000,
        })
        .expect("config");

        assert_eq!(
            config.resolve_upstream_model("client-default-model"),
            "xiaomi/mimo-v2.5-pro"
        );
        assert_eq!(
            config.resolve_upstream_chat_kwargs("client-default-model"),
            JsonMap::from_iter([(
                "reasoning".to_string(),
                json!({
                    "enabled": true
                }),
            )])
        );
        assert_eq!(
            config
                .resolve_system_prompt_prefix("client-default-model")
                .as_deref(),
            Some("Use MiMo-compatible behavior.")
        );
    }

    #[test]
    fn request_model_profile_overrides_upstream_model_profile_kwargs() {
        let config = Config::from_persisted(&PersistedConfig {
            bind_addr: "127.0.0.1:4010".to_string(),
            upstream_base_url: "https://openrouter.ai/api/v1".to_string(),
            upstream_api_key: None,
            upstream_model: Some("xiaomi/mimo-v2.5-pro".to_string()),
            default_reasoning_effort: None,
            system_prompt_prefix: None,
            upstream_request_log_path: None,
            upstream_chat_kwargs: JsonMap::new(),
            upstreams: Vec::new(),
            fallback_upstreams: Vec::new(),
            upstream_failure_cooldown_secs: 30,
            model_profile_templates: BTreeMap::new(),
            model_profiles: BTreeMap::from_iter([
                (
                    "xiaomi/mimo-v2.5-pro".to_string(),
                    PersistedModelProfile {
                        system_prompt_prefix: Some("Backend prefix.".to_string()),
                        upstream_chat_kwargs: JsonMap::from_iter([(
                            "reasoning".to_string(),
                            json!({
                                "enabled": true,
                                "effort": "medium"
                            }),
                        )]),
                        ..Default::default()
                    },
                ),
                (
                    "client-default-model".to_string(),
                    PersistedModelProfile {
                        system_prompt_prefix: Some("Client prefix.".to_string()),
                        upstream_chat_kwargs: JsonMap::from_iter([(
                            "reasoning".to_string(),
                            json!({
                                "effort": "high"
                            }),
                        )]),
                        ..Default::default()
                    },
                ),
            ]),
            brave_base_url: "https://api.search.brave.com/res/v1".to_string(),
            brave_api_key: None,
            brave_max_results: 5,
            request_timeout_secs: 60,
            connect_timeout_secs: 10,
            max_web_search_rounds: 5,
            flatten_content: true,
            max_replay_entries: 1000,
        })
        .expect("config");

        assert_eq!(
            config.resolve_upstream_chat_kwargs("client-default-model"),
            JsonMap::from_iter([(
                "reasoning".to_string(),
                json!({
                    "enabled": true,
                    "effort": "high"
                }),
            )])
        );
        assert_eq!(
            config
                .resolve_system_prompt_prefix("client-default-model")
                .as_deref(),
            Some("Client prefix.")
        );
    }

    #[test]
    fn resolves_exact_model_profile_before_case_insensitive_fallback() {
        let config = Config::from_persisted(&PersistedConfig {
            bind_addr: "127.0.0.1:4010".to_string(),
            upstream_base_url: "http://127.0.0.1:8000/v1".to_string(),
            upstream_api_key: None,
            upstream_model: None,
            default_reasoning_effort: None,
            system_prompt_prefix: None,
            upstream_request_log_path: None,
            upstream_chat_kwargs: JsonMap::new(),
            upstreams: Vec::new(),
            fallback_upstreams: Vec::new(),
            upstream_failure_cooldown_secs: 30,
            model_profile_templates: BTreeMap::new(),
            model_profiles: BTreeMap::from_iter([
                (
                    "MiMo-V2.5".to_string(),
                    PersistedModelProfile {
                        upstream_model: Some("upper-profile".to_string()),
                        system_prompt_prefix: Some("Upper prefix.".to_string()),
                        upstream_chat_kwargs: JsonMap::from_iter([(
                            "stream_reasoning".to_string(),
                            JsonValue::Bool(true),
                        )]),
                        ..Default::default()
                    },
                ),
                (
                    "mimo-v2.5".to_string(),
                    PersistedModelProfile {
                        upstream_model: Some("lower-profile".to_string()),
                        system_prompt_prefix: Some("Lower prefix.".to_string()),
                        upstream_chat_kwargs: JsonMap::from_iter([(
                            "stream_reasoning".to_string(),
                            JsonValue::Bool(false),
                        )]),
                        ..Default::default()
                    },
                ),
            ]),
            brave_base_url: "https://api.search.brave.com/res/v1".to_string(),
            brave_api_key: None,
            brave_max_results: 5,
            request_timeout_secs: 60,
            connect_timeout_secs: 10,
            max_web_search_rounds: 5,
            flatten_content: true,
            max_replay_entries: 1000,
        })
        .expect("config");

        assert_eq!(config.resolve_upstream_model("mimo-v2.5"), "lower-profile");
        assert_eq!(
            config.resolve_upstream_chat_kwargs("mimo-v2.5"),
            JsonMap::from_iter([("stream_reasoning".to_string(), JsonValue::Bool(false))])
        );
        assert_eq!(
            config.resolve_system_prompt_prefix("mimo-v2.5").as_deref(),
            Some("Lower prefix.")
        );
    }

    #[test]
    fn resolves_global_system_prompt_prefix_with_profile_prefix() {
        let config = Config::from_persisted(&PersistedConfig {
            system_prompt_prefix: Some("Global prefix.".to_string()),
            model_profiles: BTreeMap::from_iter([(
                "GLM-5.1".to_string(),
                PersistedModelProfile {
                    system_prompt_prefix: Some("Profile prefix.".to_string()),
                    ..Default::default()
                },
            )]),
            ..PersistedConfig::default()
        })
        .expect("config");

        assert_eq!(
            config.resolve_system_prompt_prefix("GLM-5.1").as_deref(),
            Some("Global prefix.\n\nProfile prefix.")
        );
        assert_eq!(
            config
                .resolve_system_prompt_prefix("unprofiled-model")
                .as_deref(),
            Some("Global prefix.")
        );
    }

    #[test]
    fn model_profiles_extend_templates_in_order() {
        let config = Config::from_persisted(&PersistedConfig {
            model_profile_templates: BTreeMap::from_iter([
                (
                    "reasoning".to_string(),
                    PersistedModelProfile {
                        system_prompt_prefix: Some("Reasoning prefix.".to_string()),
                        upstream_chat_kwargs: JsonMap::from_iter([
                            (
                                "reasoning".to_string(),
                                json!({
                                    "enabled": true,
                                    "effort": "medium"
                                }),
                            ),
                            (
                                "chat_template_kwargs".to_string(),
                                json!({
                                    "nested": {
                                        "shared": "reasoning",
                                        "template_only": true
                                    }
                                }),
                            ),
                        ]),
                        ..Default::default()
                    },
                ),
                (
                    "streaming".to_string(),
                    PersistedModelProfile {
                        extends: vec!["reasoning".to_string()],
                        upstream_chat_kwargs: JsonMap::from_iter([
                            ("stream_reasoning".to_string(), JsonValue::Bool(true)),
                            (
                                "reasoning".to_string(),
                                json!({
                                    "effort": "high"
                                }),
                            ),
                            (
                                "chat_template_kwargs".to_string(),
                                json!({
                                    "nested": {
                                        "shared": "streaming"
                                    }
                                }),
                            ),
                        ]),
                        ..Default::default()
                    },
                ),
            ]),
            model_profiles: BTreeMap::from_iter([(
                "GLM-5.1".to_string(),
                PersistedModelProfile {
                    extends: vec!["streaming".to_string()],
                    system_prompt_prefix: Some("Model prefix.".to_string()),
                    upstream_chat_kwargs: JsonMap::from_iter([
                        (
                            "reasoning".to_string(),
                            json!({
                                "max_tokens": 512
                            }),
                        ),
                        (
                            "chat_template_kwargs".to_string(),
                            json!({
                                "clear_thinking": false,
                                "nested": {
                                    "profile_only": true
                                }
                            }),
                        ),
                    ]),
                    ..Default::default()
                },
            )]),
            ..PersistedConfig::default()
        })
        .expect("config");

        assert_eq!(
            config.resolve_system_prompt_prefix("GLM-5.1").as_deref(),
            Some("Reasoning prefix.\n\nModel prefix.")
        );
        assert_eq!(
            config.resolve_upstream_chat_kwargs("GLM-5.1"),
            JsonMap::from_iter([
                (
                    "reasoning".to_string(),
                    json!({
                        "enabled": true,
                        "effort": "high",
                        "max_tokens": 512
                    }),
                ),
                (
                    "chat_template_kwargs".to_string(),
                    json!({
                        "clear_thinking": false,
                        "nested": {
                            "shared": "streaming",
                            "template_only": true,
                            "profile_only": true
                        }
                    }),
                ),
                ("stream_reasoning".to_string(), JsonValue::Bool(true)),
            ])
        );
    }

    #[test]
    fn model_profile_shorthand_kwargs_merge_with_explicit_wrapper() {
        let persisted: PersistedConfig = serde_yaml::from_str(
            r#"
model_profile_templates:
  reasoning:
    separate_reasoning: true
    chat_template_kwargs:
      thinking: true

model_profiles:
  DeepSeek-V4-Pro:
    extends:
      - reasoning
    stream_reasoning: true
    chat_template_kwargs:
      separate_reasoning: true
      thinking: false
    upstream_chat_kwargs:
      reasoning_effort: high
      chat_template_kwargs:
        thinking: true
"#,
        )
        .expect("yaml");
        let config = Config::from_persisted(&persisted).expect("config");

        assert_eq!(
            config.resolve_upstream_chat_kwargs("DeepSeek-V4-Pro"),
            JsonMap::from_iter([
                ("separate_reasoning".to_string(), JsonValue::Bool(true)),
                ("stream_reasoning".to_string(), JsonValue::Bool(true)),
                ("reasoning_effort".to_string(), json!("high")),
                (
                    "chat_template_kwargs".to_string(),
                    json!({
                        "thinking": true,
                        "separate_reasoning": true
                    }),
                ),
            ])
        );
    }

    #[test]
    fn model_profiles_reject_unknown_template() {
        let error = Config::from_persisted(&PersistedConfig {
            model_profiles: BTreeMap::from_iter([(
                "GLM-5.1".to_string(),
                PersistedModelProfile {
                    extends: vec!["missing".to_string()],
                    ..Default::default()
                },
            )]),
            ..PersistedConfig::default()
        })
        .expect_err("unknown template should fail");

        assert!(error.contains("model_profiles[GLM-5.1]: unknown template \"missing\""));
    }

    #[test]
    fn model_profiles_reject_template_cycles() {
        let error = Config::from_persisted(&PersistedConfig {
            model_profile_templates: BTreeMap::from_iter([
                (
                    "a".to_string(),
                    PersistedModelProfile {
                        extends: vec!["b".to_string()],
                        ..Default::default()
                    },
                ),
                (
                    "b".to_string(),
                    PersistedModelProfile {
                        extends: vec!["a".to_string()],
                        ..Default::default()
                    },
                ),
            ]),
            model_profiles: BTreeMap::from_iter([(
                "GLM-5.1".to_string(),
                PersistedModelProfile {
                    extends: vec!["a".to_string()],
                    ..Default::default()
                },
            )]),
            ..PersistedConfig::default()
        })
        .expect_err("template cycle should fail");

        assert!(error.contains("model_profiles[GLM-5.1]: template cycle: a -> b -> a"));
    }

    fn invalid_roles_profile(roles: RolesConfig) -> PersistedConfig {
        PersistedConfig {
            model_profiles: BTreeMap::from_iter([(
                "GLM-5.1".to_string(),
                PersistedModelProfile {
                    roles: Some(roles),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        }
    }

    #[test]
    fn roles_config_rejects_rewrite_without_target_role() {
        let config = invalid_roles_profile(RolesConfig {
            merge_adjacent: vec![],
            rules: BTreeMap::from_iter([(
                "developer".to_string(),
                RoleRuleSet {
                    rules: vec![RoleRule {
                        action: Action::Rewrite,
                        target_role: None,
                        ..Default::default()
                    }],
                },
            )]),
        });
        let error =
            Config::from_persisted(&config).expect_err("rewrite without target_role should fail");
        assert!(error.contains(
            "model_profiles[GLM-5.1]: roles[developer]: action `rewrite` requires a non-empty `target_role`"
        ));
    }

    #[test]
    fn roles_config_rejects_target_role_on_non_rewrite() {
        let config = invalid_roles_profile(RolesConfig {
            merge_adjacent: vec![],
            rules: BTreeMap::from_iter([(
                "user".to_string(),
                RoleRuleSet {
                    rules: vec![RoleRule {
                        action: Action::Accept,
                        target_role: Some("system".to_string()),
                        ..Default::default()
                    }],
                },
            )]),
        });
        let error =
            Config::from_persisted(&config).expect_err("target_role on non-rewrite should fail");
        assert!(error.contains(
            "model_profiles[GLM-5.1]: roles[user]: `target_role` is only valid with action `rewrite`"
        ));
    }

    #[test]
    fn roles_config_rejects_tag_attributes_without_tag() {
        let config = invalid_roles_profile(RolesConfig {
            merge_adjacent: vec![],
            rules: BTreeMap::from_iter([(
                "tool".to_string(),
                RoleRuleSet {
                    rules: vec![RoleRule {
                        action: Action::Accept,
                        tag: None,
                        tag_attributes: BTreeMap::from_iter([("k".to_string(), "v".to_string())]),
                        ..Default::default()
                    }],
                },
            )]),
        });
        let error =
            Config::from_persisted(&config).expect_err("tag_attributes without tag should fail");
        assert!(error.contains(
            "model_profiles[GLM-5.1]: roles[tool]: `tag_attributes` requires a non-empty `tag`"
        ));
    }

    #[test]
    fn roles_config_rejects_unknown_rule_key() {
        let yaml = "\
roles:
  user:
    acton: accept
";
        let error = serde_yaml::from_str::<PersistedModelProfile>(yaml)
            .expect_err("typo'd rule key should fail to deserialize");
        assert!(
            error.to_string().contains("acton"),
            "expected error mentioning `acton`, got: {error}"
        );
    }

    #[test]
    fn roles_config_round_trips_through_yaml() {
        let yaml = "\
roles:
  merge_adjacent: [system]
  \"*\": { action: reject }
  user: {}
  developer: { action: rewrite, target_role: system }
  tool:
    - { when: inline, action: rewrite, target_role: user, tag: tool_result, tag_attributes: { a: \"1&2\", b: \"<x>\" } }
    - {}
";
        let original = serde_yaml::from_str::<PersistedModelProfile>(yaml).expect("parse");
        let reserialized = serde_yaml::to_string(&original).expect("serialize");
        let roundtrip =
            serde_yaml::from_str::<PersistedModelProfile>(&reserialized).expect("reparse");
        assert_eq!(roundtrip.roles, original.roles);
    }

    #[test]
    fn merges_nested_profile_chat_kwargs() {
        let mut destination = JsonMap::from_iter([
            (
                "chat_template_kwargs".to_string(),
                json!({
                    "enable_thinking": true,
                    "clear_thinking": false
                }),
            ),
            ("stream_reasoning".to_string(), JsonValue::Bool(true)),
        ]);
        let source = JsonMap::from_iter([(
            "chat_template_kwargs".to_string(),
            json!({
                "thinking": true,
                "preserve_thinking": true
            }),
        )]);

        merge_json_maps(&mut destination, &source);

        assert_eq!(
            destination,
            JsonMap::from_iter([
                (
                    "chat_template_kwargs".to_string(),
                    json!({
                        "enable_thinking": true,
                        "clear_thinking": false,
                        "thinking": true,
                        "preserve_thinking": true
                    }),
                ),
                ("stream_reasoning".to_string(), JsonValue::Bool(true)),
            ])
        );
    }

    #[test]
    fn test_connect_timeout_default_is_10() {
        let persisted = PersistedConfig::default();
        assert_eq!(persisted.connect_timeout_secs, 10);
        let config = Config::from_persisted(&persisted).unwrap();
        assert_eq!(config.connect_timeout_secs, 10);
        assert_eq!(config.connect_timeout(), std::time::Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[test]
    fn config_file_has_restricted_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "llmconduit-perms-{}.yaml",
            uuid::Uuid::new_v4().simple()
        ));
        let config = PersistedConfig::default();
        write_persisted_config(&path, &config).expect("write config");
        let metadata = std::fs::metadata(&path).expect("metadata");
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config file should have 0600 permissions");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn passes_prefixed_model_name_unmodified_when_no_profile() {
        let config = Config::from_persisted(&PersistedConfig {
            bind_addr: "127.0.0.1:4010".to_string(),
            upstream_base_url: "http://127.0.0.1:8000/v1".to_string(),
            upstream_api_key: None,
            upstream_model: None,
            default_reasoning_effort: None,
            system_prompt_prefix: None,
            upstream_request_log_path: None,
            upstream_chat_kwargs: JsonMap::new(),
            upstreams: Vec::new(),
            fallback_upstreams: Vec::new(),
            upstream_failure_cooldown_secs: 30,
            model_profile_templates: BTreeMap::new(),
            model_profiles: BTreeMap::new(),
            brave_base_url: "https://api.search.brave.com/res/v1".to_string(),
            brave_api_key: None,
            brave_max_results: 5,
            request_timeout_secs: 60,
            connect_timeout_secs: 10,
            max_web_search_rounds: 5,
            flatten_content: true,
            max_replay_entries: 1000,
        })
        .expect("config");

        assert_eq!(
            config.resolve_upstream_model("anthropic/Kimi-K2.6"),
            "anthropic/Kimi-K2.6"
        );
        assert_eq!(
            config.resolve_upstream_chat_kwargs("anthropic/Kimi-K2.6"),
            JsonMap::new()
        );
    }

    #[test]
    fn resolves_exact_prefix_model_profile_when_present() {
        let config = Config::from_persisted(&PersistedConfig {
            bind_addr: "127.0.0.1:4010".to_string(),
            upstream_base_url: "http://127.0.0.1:8000/v1".to_string(),
            upstream_api_key: None,
            upstream_model: None,
            default_reasoning_effort: None,
            system_prompt_prefix: None,
            upstream_request_log_path: None,
            upstream_chat_kwargs: JsonMap::new(),
            upstreams: Vec::new(),
            fallback_upstreams: Vec::new(),
            upstream_failure_cooldown_secs: 30,
            model_profile_templates: BTreeMap::new(),
            model_profiles: BTreeMap::from_iter([(
                "anthropic/Kimi-K2.6".to_string(),
                PersistedModelProfile {
                    upstream_model: Some("anthropic-custom".to_string()),
                    ..Default::default()
                },
            )]),
            brave_base_url: "https://api.search.brave.com/res/v1".to_string(),
            brave_api_key: None,
            brave_max_results: 5,
            request_timeout_secs: 60,
            connect_timeout_secs: 10,
            max_web_search_rounds: 5,
            flatten_content: true,
            max_replay_entries: 1000,
        })
        .expect("config");

        assert_eq!(
            config.resolve_upstream_model("anthropic/Kimi-K2.6"),
            "anthropic-custom"
        );
    }

    #[test]
    fn simple_cap_accepts_bool_shorthand() {
        assert_eq!(
            serde_json::from_value::<SimpleCap>(json!(true)).unwrap(),
            SimpleCap { supported: true }
        );
        assert_eq!(
            serde_json::from_value::<SimpleCap>(json!(false)).unwrap(),
            SimpleCap { supported: false }
        );
    }

    #[test]
    fn simple_cap_object_supported_defaults_true() {
        assert_eq!(
            serde_json::from_value::<SimpleCap>(json!({})).unwrap(),
            SimpleCap { supported: true }
        );
        assert_eq!(
            serde_json::from_value::<SimpleCap>(json!({"supported": false})).unwrap(),
            SimpleCap { supported: false }
        );
    }

    #[test]
    fn simple_cap_rejects_unknown_keys() {
        assert!(serde_json::from_value::<SimpleCap>(json!({"supported": true, "x": 1})).is_err());
    }

    #[test]
    fn capabilities_reject_unknown_cap_key() {
        assert!(serde_json::from_value::<CapabilitiesConfig>(json!({"bogus": {}})).is_err());
    }

    #[test]
    fn effort_cap_rejects_unknown_level() {
        assert!(
            serde_json::from_value::<CapabilitiesConfig>(json!({"effort": {"levels": ["turbo"]}}))
                .is_err()
        );
    }

    #[test]
    fn thinking_cap_rejects_unknown_type() {
        assert!(
            serde_json::from_value::<CapabilitiesConfig>(json!({"thinking": {"types": ["bogus"]}}))
                .is_err()
        );
    }

    #[test]
    fn context_management_rejects_unknown_feature() {
        assert!(
            serde_json::from_value::<CapabilitiesConfig>(
                json!({"context_management": {"features": ["nope"]}})
            )
            .is_err()
        );
    }

    #[test]
    fn effort_cap_supported_defaults_true() {
        let caps: CapabilitiesConfig =
            serde_json::from_value(json!({"effort": {"levels": ["max"]}})).expect("parse");
        let effort = caps.effort.unwrap();
        assert!(effort.supported);
        assert_eq!(effort.levels, vec![EffortLevel::Max]);
    }

    #[test]
    fn capabilities_to_wire_thinking() {
        let caps: CapabilitiesConfig =
            serde_json::from_value(json!({"thinking": {"types": ["adaptive", "enabled"]}}))
                .expect("parse");
        assert_eq!(
            caps.thinking.unwrap().to_wire(),
            json!({
                "supported": true,
                "types": {
                    "adaptive": {"supported": true},
                    "enabled": {"supported": true}
                }
            })
        );
    }

    #[test]
    fn thinking_cap_to_wire_empty_types() {
        let cap = ThinkingCap {
            supported: true,
            types: vec![],
        };
        assert_eq!(cap.to_wire(), json!({"supported": true, "types": {}}));
    }

    #[test]
    fn capabilities_to_wire_effort_levels_are_siblings_of_supported() {
        let caps: CapabilitiesConfig =
            serde_json::from_value(json!({"effort": {"levels": ["max", "medium", "none"]}}))
                .expect("parse");
        assert_eq!(
            caps.effort.unwrap().to_wire(),
            json!({
                "supported": true,
                "max": {"supported": true},
                "medium": {"supported": true},
                "none": {"supported": true}
            })
        );
    }

    #[test]
    fn capabilities_to_wire_context_management() {
        let caps: CapabilitiesConfig = serde_json::from_value(json!({
            "context_management": {"features": ["clear_thinking_20251015", "compact_20260112"]}
        }))
        .expect("parse");
        assert_eq!(
            caps.context_management.unwrap().to_wire(),
            json!({
                "supported": true,
                "clear_thinking_20251015": {"supported": true},
                "compact_20260112": {"supported": true}
            })
        );
    }

    #[test]
    fn capabilities_to_wire_simple_caps() {
        let caps: CapabilitiesConfig = serde_json::from_value(json!({
            "batch": true,
            "image_input": {"supported": false}
        }))
        .expect("parse");
        assert_eq!(caps.batch.unwrap().to_wire(), json!({"supported": true}));
        assert_eq!(
            caps.image_input.unwrap().to_wire(),
            json!({"supported": false})
        );
    }

    #[test]
    fn capabilities_to_wire_supported_false_propagates_to_children() {
        let caps: CapabilitiesConfig =
            serde_json::from_value(json!({"effort": {"supported": false, "levels": ["max"]}}))
                .expect("parse");
        assert_eq!(
            caps.effort.unwrap().to_wire(),
            json!({
                "supported": false,
                "max": {"supported": false}
            })
        );
    }

    #[test]
    fn capabilities_merge_into_overrides_configured_caps_only() {
        let base = json!({
            "thinking": {"supported": false, "types": {"adaptive": {"supported": false}}},
            "effort": {"supported": false, "max": {"supported": false}},
            "image_input": {"supported": false}
        });
        let caps: CapabilitiesConfig =
            serde_json::from_value(json!({"thinking": {"types": ["enabled"]}})).expect("parse");
        let merged = caps.merge_into(base);
        assert_eq!(
            merged["thinking"],
            json!({
                "supported": true,
                "types": {"enabled": {"supported": true}}
            })
        );
        // Unconfigured caps keep the base value (wholesale, no fill-in).
        assert_eq!(merged["effort"]["supported"], false);
        assert_eq!(merged["image_input"]["supported"], false);
    }

    #[test]
    fn context_feature_roundtrips_through_serde() {
        let feature: ContextFeature =
            serde_json::from_value(json!("clear_tool_uses_20250919")).expect("parse");
        assert_eq!(feature.as_str(), "clear_tool_uses_20250919");
    }

    fn persisted_with_profiles(profiles: serde_json::Value) -> Config {
        let mut root = serde_json::Map::new();
        root.insert(
            "upstream_base_url".to_string(),
            json!("http://127.0.0.1:8000/v1"),
        );
        if let serde_json::Value::Object(map) = profiles {
            for (key, value) in map {
                root.insert(key, value);
            }
        }
        let persisted: PersistedConfig =
            serde_json::from_value(serde_json::Value::Object(root)).expect("parse config");
        Config::from_persisted(&persisted).expect("config")
    }

    #[test]
    fn resolve_capabilities_id_keyed_wins() {
        let config = persisted_with_profiles(json!({
            "model_profiles": {"glm-5.2": {"capabilities": {"thinking": {"types": ["adaptive"]}}}}
        }));
        let caps = config
            .resolve_capabilities_for_upstream("glm-5.2")
            .expect("caps");
        assert!(caps.thinking.is_some());
    }

    #[test]
    fn resolve_capabilities_alias_target_matches() {
        let config = persisted_with_profiles(json!({
            "model_profiles": {"glm-alias": {"upstream_model": "glm-5.2", "capabilities": {"effort": {"levels": ["max"]}}}}
        }));
        let caps = config
            .resolve_capabilities_for_upstream("glm-5.2")
            .expect("caps");
        assert!(caps.effort.is_some());
    }

    #[test]
    fn resolve_capabilities_unprofiled_uses_default() {
        let config = persisted_with_profiles(json!({
            "model_profiles": {"*": {"capabilities": {"thinking": {"types": ["adaptive"]}}}}
        }));
        let caps = config
            .resolve_capabilities_for_upstream("unknown-id")
            .expect("caps");
        assert!(caps.thinking.is_some());
    }

    #[test]
    fn resolve_capabilities_profiled_without_block_gets_none_no_fillin() {
        let config = persisted_with_profiles(json!({
            "model_profiles": {
                "*": {"capabilities": {"thinking": {"types": ["adaptive"]}}},
                "glm-5.2": {"upstream_model": "glm-5.2-upstream"}
            }
        }));
        assert!(
            config
                .resolve_capabilities_for_upstream("glm-5.2")
                .is_none()
        );
    }

    #[test]
    fn resolve_reasoning_config_for_resolved_model_uses_resolved_id() {
        // The resolved backend id can differ from `resolve_upstream_model(request_model)`
        // when the upstream catalog canonicalizes the name. A profile keyed by that
        // resolved id must still drive reasoning selection, matching how
        // `upstream_chat_kwargs` and the prefix resolve profiles off the normalized id.
        let config = persisted_with_profiles(json!({
            "model_profiles": {
                "glm-5.2": {},
                "glm-5.2-canonical": {"reasoning_effort": {"default": "low"}}
            }
        }));
        let rc = config
            .resolve_reasoning_config_for_resolved_model("glm-5.2", "glm-5.2-canonical")
            .expect("reasoning");
        assert_eq!(rc.default.as_deref(), Some("low"));
        // Without the resolved id, the wrapper only sees `glm-5.2` (no reasoning block).
        assert!(config.resolve_reasoning_config("glm-5.2").is_none());
    }
}
