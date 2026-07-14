//! The [`VisionClient`] seam and its production [`ReqwestVisionClient`].
//!
//! Mirrors `src/search.rs`'s `SearchClient`: a trait object so the engine can
//! run `analyzeImage` server-side against a vision-capable backend (and tests
//! inject a mock). [`VisionRequest`] is the parsed `analyzeImage` tool call with
//! its cached images resolved; [`VisionOutcome`] is the description injected back
//! into the chat history as the tool result. All model-visible/logged text from
//! the backend is funneled through [`crate::redaction::redact_vision_text`] so a
//! backend that echoes a submitted image URL cannot re-leak it.

use crate::config::Config;
use crate::error::AppError;
use crate::error::AppResult;
use crate::redaction::redact_vision_text_with_literals;
use async_trait::async_trait;
use serde_json::Value;
use serde_json::json;
use url::Url;

use super::cache::CachedImage;
use super::cache::ImageCache;

/// System prompt sent to the vision backend itself (claude-relay's
/// `VISION_SYSTEM_PROMPT`).
pub const VISION_SYSTEM_PROMPT: &str = "Analyze the provided image(s) according to the task \
description below. Be thorough, specific, and accurate in your analysis. If conversation context \
is provided, use it to focus your analysis on the most relevant aspects of the image. Describe \
exactly what you observe.";

/// Parsed `analyzeImage` arguments. `image_ids` may be empty (the executor then
/// surfaces a model-visible "no images" message rather than erroring).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisionRequest {
    pub image_ids: Vec<String>,
    pub task: String,
    pub context: Option<String>,
    pub images: Vec<CachedImage>,
}

impl VisionRequest {
    /// Parse the `analyzeImage` tool arguments into a structured request,
    /// resolving each requested image id against the cache for `session_id`.
    /// Missing ids are simply skipped (claude-relay logs + skips); the executor
    /// decides what model-visible text to inject.
    pub fn from_arguments(arguments: &Value, session_id: &str, cache: &ImageCache) -> Self {
        let image_ids = arguments
            .get("imageId")
            .and_then(Value::as_array)
            .map(|ids| ids.iter().filter_map(value_to_image_id).collect::<Vec<_>>())
            .unwrap_or_default();
        let task = arguments
            .get("task")
            .and_then(Value::as_str)
            .filter(|task| !task.trim().is_empty())
            .unwrap_or("Describe this image in detail")
            .to_string();
        let context = arguments
            .get("context")
            .and_then(Value::as_str)
            .filter(|context| !context.trim().is_empty())
            .map(ToString::to_string);
        let images = image_ids
            .iter()
            .filter_map(|id| cache.get(session_id, &ImageCache::image_key(session_id, id)))
            .collect();
        Self {
            image_ids,
            task,
            context,
            images,
        }
    }
}

/// Coerce a JSON `imageId` array element to a string id. Accepts string or
/// numeric ids (`["1"]` or `[1]`) since models occasionally emit the latter.
fn value_to_image_id(value: &Value) -> Option<String> {
    match value {
        Value::String(id) if !id.trim().is_empty() => Some(id.trim().to_string()),
        Value::Number(num) => Some(num.to_string()),
        _ => None,
    }
}

/// Result of a vision analysis: the description text injected back into the chat
/// history as the `analyzeImage` tool result.
#[derive(Debug, Clone, Default)]
pub struct VisionOutcome {
    pub text: String,
}

#[async_trait]
pub trait VisionClient: Send + Sync {
    /// Analyze the request's images and return the model-visible description.
    /// A backend error is surfaced as `Err`; the engine converts it to
    /// model-visible tool text so the turn can still complete (mirroring the
    /// Brave web_search degrade-gracefully contract).
    async fn analyze(&self, request: &VisionRequest) -> AppResult<VisionOutcome>;
}

/// Production [`VisionClient`]: POSTs an OpenAI-compatible chat completion (with
/// `image_url` content parts) to the configured vision backend and returns the
/// assistant content. Non-streaming, matching claude-relay's `vision_body`.
#[derive(Debug, Clone)]
pub struct ReqwestVisionClient {
    client: reqwest::Client,
    url: Option<Url>,
    model: Option<String>,
}

impl ReqwestVisionClient {
    pub fn new(client: reqwest::Client, config: &Config) -> Self {
        Self {
            client,
            url: config.vision_url.clone(),
            model: config.vision_model.clone(),
        }
    }

    fn build_body(&self, request: &VisionRequest) -> Value {
        let mut user_content: Vec<Value> = request
            .images
            .iter()
            .map(|image| {
                let mut image_url = serde_json::Map::new();
                image_url.insert("url".to_string(), Value::String(image.image_url.clone()));
                if let Some(detail) = &image.detail {
                    image_url.insert("detail".to_string(), Value::String(detail.clone()));
                }
                json!({ "type": "image_url", "image_url": Value::Object(image_url) })
            })
            .collect();
        let mut prompt = format!("Task: {}", request.task);
        if let Some(context) = &request.context {
            prompt.push_str(&format!("\nContext: {context}"));
        }
        user_content.push(json!({ "type": "text", "text": prompt }));
        json!({
            "model": self.model.clone().unwrap_or_default(),
            "messages": [
                { "role": "system", "content": VISION_SYSTEM_PROMPT },
                { "role": "user", "content": user_content },
            ],
            "max_tokens": 4096,
            "stream": false,
        })
    }
}

#[async_trait]
impl VisionClient for ReqwestVisionClient {
    async fn analyze(&self, request: &VisionRequest) -> AppResult<VisionOutcome> {
        let url = self
            .url
            .clone()
            .ok_or_else(|| AppError::internal("image agent is active but vision_url is missing"))?;
        let body = self.build_body(request);
        let response = self
            .client
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|_| AppError::upstream("vision request failed"))?;
        if !response.status().is_success() {
            let status = response.status();
            // The engine degrades this error into model-visible tool text. Do
            // not retain or interpolate a provider-controlled body: it can echo
            // an image locator or credential under a neutral field name that a
            // key-based JSON redactor cannot identify.
            return Err(AppError::upstream(format!(
                "vision backend returned HTTP {}",
                status.as_u16()
            )));
        }
        let (body, truncated) =
            crate::redaction::read_reqwest_body_capped(response, 4 * 1024 * 1024)
                .await
                .map_err(|_| AppError::upstream("failed to read vision response"))?;
        if truncated {
            return Err(AppError::upstream("vision response was too large"));
        }
        let payload: Value =
            serde_json::from_slice(&body).map_err(|_| AppError::upstream("invalid vision JSON"))?;
        if payload.get("error").is_some() {
            return Err(AppError::upstream(
                "vision backend returned an error payload",
            ));
        }
        let text = payload
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| AppError::upstream("vision response missing message content"))?;
        // Round-3 #3: even a SUCCESSFUL description can echo a submitted
        // `data:`/signed image URL; redact before it becomes a model-visible
        // tool result or is logged. Redacting here makes EVERY VisionOutcome
        // safe regardless of caller (the engine injects + previews it).
        Ok(VisionOutcome {
            text: redact_vision_text_with_literals(
                &text,
                request.images.iter().map(|image| image.image_url.as_str()),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    fn cache() -> ImageCache {
        ImageCache::new(100, std::time::Duration::from_secs(300))
    }

    fn img(url: &str) -> CachedImage {
        CachedImage {
            image_url: url.to_string(),
            detail: None,
        }
    }

    #[test]
    fn vision_request_parses_arguments_and_resolves_images() {
        let cache = cache();
        cache.store("sess", ImageCache::image_key("sess", "1"), img("data:one"));
        cache.store("sess", ImageCache::image_key("sess", "2"), img("data:two"));
        let args = json!({
            "imageId": ["1", "2", "9"],
            "task": "read the sign",
            "context": "user asked about the sign"
        });
        let request = VisionRequest::from_arguments(&args, "sess", &cache);
        assert_eq!(request.image_ids, vec!["1", "2", "9"]);
        assert_eq!(request.task, "read the sign");
        assert_eq!(
            request.context.as_deref(),
            Some("user asked about the sign")
        );
        // #9 missing from cache, so only two images resolve.
        assert_eq!(request.images, vec![img("data:one"), img("data:two")]);
    }

    #[test]
    fn vision_request_defaults_task_and_accepts_numeric_ids() {
        let cache = cache();
        let args = json!({ "imageId": [1] });
        let request = VisionRequest::from_arguments(&args, "sess", &cache);
        assert_eq!(request.image_ids, vec!["1"]);
        assert_eq!(request.task, "Describe this image in detail");
        assert_eq!(request.context, None);
    }

    #[test]
    fn reqwest_vision_client_builds_body_with_image_parts() {
        let config = crate::config::Config::from_persisted(&crate::config::PersistedConfig {
            vision_url: Some("http://127.0.0.1:9000/v1/chat/completions".to_string()),
            vision_model: Some("vision-model".to_string()),
            ..crate::config::PersistedConfig::default()
        })
        .expect("config");
        let client = ReqwestVisionClient::new(reqwest::Client::new(), &config);
        let request = VisionRequest {
            image_ids: vec!["1".to_string()],
            task: "describe".to_string(),
            context: Some("ctx".to_string()),
            images: vec![CachedImage {
                image_url: "data:image/png;base64,AAAA".to_string(),
                detail: Some("high".to_string()),
            }],
        };
        let body = client.build_body(&request);
        assert_eq!(body["model"], "vision-model");
        assert_eq!(body["stream"], false);
        let user = &body["messages"][1];
        assert_eq!(user["role"], "user");
        assert_eq!(user["content"][0]["type"], "image_url");
        assert_eq!(
            user["content"][0]["image_url"]["url"],
            "data:image/png;base64,AAAA"
        );
        assert_eq!(user["content"][0]["image_url"]["detail"], "high");
        let prompt = user["content"][1]["text"].as_str().unwrap();
        assert!(prompt.contains("Task: describe"));
        assert!(prompt.contains("Context: ctx"));
    }

    #[tokio::test]
    async fn vision_backend_never_echoes_neutral_error_bodies_or_image_locators() {
        const IMAGE_SECRET: &str = "s3://private-bucket/image-secret-9bce";
        const BODY_SECRET: &str = "vision-neutral-body-secret-44ef";

        let failed = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(502).set_body_json(json!({
                "message": format!("{BODY_SECRET} {IMAGE_SECRET}"),
            })))
            .mount(&failed)
            .await;
        let failed_config =
            crate::config::Config::from_persisted(&crate::config::PersistedConfig {
                vision_url: Some(format!("{}/v1/chat/completions", failed.uri())),
                vision_model: Some("vision-model".to_string()),
                ..crate::config::PersistedConfig::default()
            })
            .expect("failed-backend config");
        let request = VisionRequest {
            image_ids: vec!["1".to_string()],
            task: "describe".to_string(),
            context: None,
            images: vec![img(IMAGE_SECRET)],
        };
        let error = ReqwestVisionClient::new(reqwest::Client::new(), &failed_config)
            .analyze(&request)
            .await
            .expect_err("non-success response must fail");
        assert_eq!(error.message, "vision backend returned HTTP 502");
        assert!(!error.to_string().contains(BODY_SECRET));
        assert!(!error.to_string().contains(IMAGE_SECRET));

        let succeeded = MockServer::start().await;
        let encoded_image_secret =
            url::form_urlencoded::byte_serialize(IMAGE_SECRET.as_bytes()).collect::<String>();
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": format!(
                    "The neutral content field echoed {IMAGE_SECRET} and {encoded_image_secret}"
                )}}]
            })))
            .mount(&succeeded)
            .await;
        let success_config =
            crate::config::Config::from_persisted(&crate::config::PersistedConfig {
                vision_url: Some(format!("{}/v1/chat/completions", succeeded.uri())),
                vision_model: Some("vision-model".to_string()),
                ..crate::config::PersistedConfig::default()
            })
            .expect("success-backend config");
        let outcome = ReqwestVisionClient::new(reqwest::Client::new(), &success_config)
            .analyze(&request)
            .await
            .expect("vision outcome");
        assert!(!outcome.text.contains(IMAGE_SECRET));
        assert!(!outcome.text.contains(&encoded_image_secret));
        assert!(outcome.text.contains("<redacted secret>"));
    }
}
