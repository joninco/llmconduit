use crate::config::Config;
use crate::error::AppError;
use crate::error::AppResult;
use async_trait::async_trait;
use serde::Deserialize;
use url::Url;

/// A single web result, structured for the Anthropic `web_search_tool_result`
/// block (so clients render source citations). The model-facing prose still
/// comes from [`SearchOutcome::formatted`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchSource {
    pub title: String,
    pub url: String,
}

/// Result of a web search: the flattened text injected into the model's
/// context, plus the structured sources surfaced to the client.
#[derive(Debug, Clone, Default)]
pub struct SearchOutcome {
    pub formatted: String,
    pub sources: Vec<SearchSource>,
}

#[async_trait]
pub trait SearchClient: Send + Sync {
    async fn search(&self, query: &str) -> AppResult<SearchOutcome>;
}

#[derive(Clone)]
pub struct BraveSearchClient {
    client: reqwest::Client,
    base_url: Url,
    api_key: Option<String>,
    max_results: usize,
}

impl std::fmt::Debug for BraveSearchClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BraveSearchClient")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .field("max_results", &self.max_results)
            .finish_non_exhaustive()
    }
}

impl BraveSearchClient {
    pub fn new(client: reqwest::Client, config: Config) -> Self {
        Self {
            client,
            base_url: config.brave_base_url,
            api_key: config.brave_api_key,
            max_results: config.brave_max_results,
        }
    }

    fn endpoint_url(&self, path: &str) -> AppResult<Url> {
        let mut url = self.base_url.clone();
        if !url.path().ends_with('/') {
            let new_path = format!("{}/", url.path());
            url.set_path(&new_path);
        }
        url.join(path)
            .map_err(|err| AppError::internal(format!("invalid Brave URL: {err}")))
    }
}

#[async_trait]
impl SearchClient for BraveSearchClient {
    async fn search(&self, query: &str) -> AppResult<SearchOutcome> {
        let api_key = self.api_key.as_deref().ok_or_else(|| {
            AppError::internal("web_search is configured but BRAVE_SEARCH_API_KEY is missing")
        })?;
        let url = self.endpoint_url("web/search")?;
        let response = self
            .client
            .get(url)
            .header("X-Subscription-Token", api_key)
            .query(&[
                ("q", query),
                ("count", &self.max_results.to_string()),
                ("text_decorations", "false"),
                ("spellcheck", "false"),
            ])
            .send()
            .await
            .map_err(|_| AppError::upstream("Brave search request failed"))?;
        if !response.status().is_success() {
            let status = response.status();
            // The body is provider-controlled and the request carried the
            // configured Brave credential. Never echo response bytes into an
            // AppError: the engine deliberately turns this error into a
            // model-visible tool result, and a backend can return the header
            // value under an innocuous field such as `message`.
            return Err(AppError::upstream(format!(
                "Brave search backend returned HTTP {}",
                status.as_u16()
            )));
        }
        let (body, truncated) =
            crate::redaction::read_reqwest_body_capped(response, 4 * 1024 * 1024)
                .await
                .map_err(|_| AppError::upstream("failed to read Brave search response"))?;
        if truncated {
            return Err(AppError::upstream("Brave search response was too large"));
        }
        let mut payload: BraveSearchResponse = serde_json::from_slice(&body)
            .map_err(|_| AppError::upstream("invalid Brave search JSON"))?;
        redact_brave_response_credentials(&mut payload, api_key);
        Ok(SearchOutcome {
            formatted: format_search_results(&payload),
            sources: collect_sources(&payload),
        })
    }
}

/// Scrub the configured credential from every provider-controlled text field
/// before it can become model-visible or enter a structured source event. The
/// JSON field name is irrelevant: exact and URL-encoded echoes are removed even
/// from neutral `title`, `description`, and `url` fields.
fn redact_brave_response_credentials(payload: &mut BraveSearchResponse, api_key: &str) {
    let encoded = url::form_urlencoded::byte_serialize(api_key.as_bytes()).collect::<String>();
    let encoded_lower = encoded.to_ascii_lowercase();
    let literals = [api_key, encoded.as_str(), encoded_lower.as_str()];
    let Some(web) = payload.web.as_mut() else {
        return;
    };
    for result in &mut web.results {
        result.title = crate::redaction::redact_sensitive_literals(&result.title, literals);
        result.description =
            crate::redaction::redact_sensitive_literals(&result.description, literals);
        if literals
            .iter()
            .any(|literal| !literal.is_empty() && result.url.contains(literal))
        {
            // Replacing a token inside a URL creates an invalid citation and
            // can leave surrounding signed parameters meaningful. Omit the URL
            // altogether; `collect_sources` then drops this source.
            result.url.clear();
        }
    }
}

/// Defense-in-depth for alternate `SearchClient` implementations. Production's
/// Brave client sanitizes fields before formatting, but the engine calls this
/// on every outcome so an injected client cannot echo the configured key.
pub(crate) fn redact_search_outcome_credentials(
    outcome: &mut SearchOutcome,
    api_key: Option<&str>,
) {
    let Some(api_key) = api_key.filter(|key| !key.is_empty()) else {
        return;
    };
    let encoded = url::form_urlencoded::byte_serialize(api_key.as_bytes()).collect::<String>();
    let encoded_lower = encoded.to_ascii_lowercase();
    let literals = [api_key, encoded.as_str(), encoded_lower.as_str()];
    outcome.formatted = crate::redaction::redact_sensitive_literals(&outcome.formatted, literals);
    outcome.sources.retain_mut(|source| {
        source.title = crate::redaction::redact_sensitive_literals(&source.title, literals);
        !literals
            .iter()
            .any(|literal| !literal.is_empty() && source.url.contains(literal))
    });
}

fn collect_sources(payload: &BraveSearchResponse) -> Vec<SearchSource> {
    payload
        .web
        .as_ref()
        .map(|web| web.results.as_slice())
        .unwrap_or(&[])
        .iter()
        .filter(|result| !result.url.is_empty())
        .map(|result| SearchSource {
            title: result.title.clone(),
            url: result.url.clone(),
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct BraveSearchResponse {
    #[serde(default)]
    web: Option<BraveWebResults>,
}

#[derive(Debug, Deserialize)]
struct BraveWebResults {
    #[serde(default)]
    results: Vec<BraveWebResult>,
}

#[derive(Debug, Deserialize)]
struct BraveWebResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    description: String,
}

fn format_search_results(payload: &BraveSearchResponse) -> String {
    let mut lines = Vec::new();
    for (index, result) in payload
        .web
        .as_ref()
        .map(|web| web.results.as_slice())
        .unwrap_or(&[])
        .iter()
        .enumerate()
    {
        lines.push(format!("{}. {}", index + 1, result.title));
        if !result.url.is_empty() {
            lines.push(format!("URL: {}", result.url));
        }
        if !result.description.is_empty() {
            lines.push(format!("Snippet: {}", result.description));
        }
        lines.push(String::new());
    }
    if lines.is_empty() {
        "No Brave search results found.".to_string()
    } else {
        lines.join("\n").trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::BraveSearchClient;
    use crate::config::Config;
    use crate::config::UnsupportedImagePolicy;

    use super::BraveSearchResponse;
    use super::BraveWebResult;
    use super::BraveWebResults;
    use super::SearchClient;
    use super::SearchSource;
    use super::collect_sources;
    use super::format_search_results;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    #[test]
    fn format_search_results_empty() {
        let response = BraveSearchResponse { web: None };
        assert_eq!(
            format_search_results(&response),
            "No Brave search results found."
        );
    }

    #[test]
    fn format_search_results_missing_fields() {
        let response = BraveSearchResponse {
            web: Some(BraveWebResults {
                results: vec![BraveWebResult {
                    title: String::new(),
                    url: String::new(),
                    description: String::new(),
                }],
            }),
        };
        let result = format_search_results(&response);
        assert!(result.contains("1."));
    }

    #[test]
    fn collect_sources_extracts_structured_url_title_skipping_empty_urls() {
        // The structured sources feed the Anthropic `web_search_tool_result`
        // block; results without a URL can't be a citation and are dropped.
        // The model-facing formatted text must stay byte-identical.
        let response = BraveSearchResponse {
            web: Some(BraveWebResults {
                results: vec![
                    BraveWebResult {
                        title: "Alpha".to_string(),
                        url: "https://a.test".to_string(),
                        description: "da".to_string(),
                    },
                    BraveWebResult {
                        title: "No URL".to_string(),
                        url: String::new(),
                        description: "d".to_string(),
                    },
                    BraveWebResult {
                        title: "Beta".to_string(),
                        url: "https://b.test".to_string(),
                        description: "db".to_string(),
                    },
                ],
            }),
        };
        let sources = collect_sources(&response);
        assert_eq!(
            sources,
            vec![
                SearchSource {
                    title: "Alpha".to_string(),
                    url: "https://a.test".to_string(),
                },
                SearchSource {
                    title: "Beta".to_string(),
                    url: "https://b.test".to_string(),
                },
            ]
        );
        assert!(format_search_results(&response).contains("URL: https://a.test"));
    }

    #[test]
    fn endpoint_url_preserves_v1_without_trailing_slash() {
        let client = BraveSearchClient::new(
            reqwest::Client::new(),
            Config {
                bind_addr: "127.0.0.1:0".parse().expect("socket addr"),
                upstream_base_url: url::Url::parse("http://127.0.0.1:8000/v1/").expect("url"),
                upstream_api_key: None,
                upstream_model: None,
                system_prompt_prefix: None,
                upstream_request_log_path: None,
                turn_capture_dir: None,
                api_log_body_mode: Default::default(),
                upstream_request_log_body_mode: Default::default(),
                upstream_chat_kwargs: serde_json::Map::new(),
                upstreams: Vec::new(),
                fallback_upstreams: Vec::new(),
                upstream_retry: Default::default(),
                upstream_circuit_breaker: Default::default(),
                upstream_bulkhead: Default::default(),
                upstream_failure_cooldown_secs: 30,
                model_profiles: std::collections::BTreeMap::new(),
                responses_capabilities: Default::default(),
                model_routes: Vec::new(),
                anthropic_passthrough: None,
                template_family: None,
                brave_base_url: url::Url::parse("https://api.search.brave.com/res/v1")
                    .expect("url"),
                brave_api_key: Some("secret".to_string()),
                brave_max_results: 5,
                request_timeout: std::time::Duration::from_secs(30),
                connect_timeout_secs: 10,
                max_web_search_rounds: 5,
                flatten_content: true,
                max_replay_entries: 1000,
                response_store: Default::default(),
                replay: Default::default(),
                debug_log_max_age_hours: None,
                min_completion_tokens: 4096,
                max_sse_frame_bytes: 8 * 1024 * 1024,
                max_request_body_bytes: 10 * 1024 * 1024,
                image_agent_enabled: false,
                vision_url: None,
                vision_model: None,
                image_cache_max_size: 100,
                image_cache_ttl_secs: 300,
                unsupported_image_policy: UnsupportedImagePolicy::Placeholder,
                price_table: std::collections::HashMap::new(),
            },
        );

        assert_eq!(
            client
                .endpoint_url("web/search")
                .expect("endpoint")
                .as_str(),
            "https://api.search.brave.com/res/v1/web/search"
        );
    }

    #[test]
    fn endpoint_url_preserves_v1_with_trailing_slash() {
        let client = BraveSearchClient::new(
            reqwest::Client::new(),
            Config {
                bind_addr: "127.0.0.1:0".parse().expect("socket addr"),
                upstream_base_url: url::Url::parse("http://127.0.0.1:8000/v1/").expect("url"),
                upstream_api_key: None,
                upstream_model: None,
                system_prompt_prefix: None,
                upstream_request_log_path: None,
                turn_capture_dir: None,
                api_log_body_mode: Default::default(),
                upstream_request_log_body_mode: Default::default(),
                upstream_chat_kwargs: serde_json::Map::new(),
                upstreams: Vec::new(),
                fallback_upstreams: Vec::new(),
                upstream_retry: Default::default(),
                upstream_circuit_breaker: Default::default(),
                upstream_bulkhead: Default::default(),
                upstream_failure_cooldown_secs: 30,
                model_profiles: std::collections::BTreeMap::new(),
                responses_capabilities: Default::default(),
                model_routes: Vec::new(),
                anthropic_passthrough: None,
                template_family: None,
                brave_base_url: url::Url::parse("https://api.search.brave.com/res/v1/")
                    .expect("url"),
                brave_api_key: Some("secret".to_string()),
                brave_max_results: 5,
                request_timeout: std::time::Duration::from_secs(30),
                connect_timeout_secs: 10,
                max_web_search_rounds: 5,
                flatten_content: true,
                max_replay_entries: 1000,
                response_store: Default::default(),
                replay: Default::default(),
                debug_log_max_age_hours: None,
                min_completion_tokens: 4096,
                max_sse_frame_bytes: 8 * 1024 * 1024,
                max_request_body_bytes: 10 * 1024 * 1024,
                image_agent_enabled: false,
                vision_url: None,
                vision_model: None,
                image_cache_max_size: 100,
                image_cache_ttl_secs: 300,
                unsupported_image_policy: UnsupportedImagePolicy::Placeholder,
                price_table: std::collections::HashMap::new(),
            },
        );

        assert_eq!(
            client
                .endpoint_url("web/search")
                .expect("endpoint")
                .as_str(),
            "https://api.search.brave.com/res/v1/web/search"
        );
    }

    #[tokio::test]
    async fn brave_backend_never_echoes_key_from_neutral_error_or_result_fields() {
        const SENTINEL: &str = "brave-key-sentinel-7f291";

        let failed = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/res/v1/web/search"))
            .respond_with(ResponseTemplate::new(502).set_body_json(serde_json::json!({
                "message": format!("backend reflected {SENTINEL}"),
            })))
            .mount(&failed)
            .await;
        let failed_config = Config::from_persisted(&crate::config::PersistedConfig {
            brave_base_url: format!("{}/res/v1", failed.uri()),
            brave_api_key: Some(SENTINEL.to_string()),
            ..crate::config::PersistedConfig::default()
        })
        .expect("failed-backend config");
        let error = BraveSearchClient::new(reqwest::Client::new(), failed_config)
            .search("safe query")
            .await
            .expect_err("non-success response must fail");
        assert_eq!(error.message, "Brave search backend returned HTTP 502");
        assert!(!error.to_string().contains(SENTINEL));
        assert!(!error.client_message.contains(SENTINEL));

        let succeeded = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/res/v1/web/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "web": {"results": [
                    {
                        "title": format!("reflected title {SENTINEL}"),
                        "url": format!("https://result.test/path?token={SENTINEL}"),
                        "description": format!("reflected description {SENTINEL}")
                    },
                    {
                        "title": format!("safe URL but reflected {SENTINEL}"),
                        "url": "https://safe-result.test/path",
                        "description": "ordinary description"
                    }
                ]}
            })))
            .mount(&succeeded)
            .await;
        let success_config = Config::from_persisted(&crate::config::PersistedConfig {
            brave_base_url: format!("{}/res/v1", succeeded.uri()),
            brave_api_key: Some(SENTINEL.to_string()),
            ..crate::config::PersistedConfig::default()
        })
        .expect("success-backend config");
        let outcome = BraveSearchClient::new(reqwest::Client::new(), success_config)
            .search("safe query")
            .await
            .expect("search outcome");
        assert!(!outcome.formatted.contains(SENTINEL));
        assert!(
            outcome
                .sources
                .iter()
                .all(|source| !source.title.contains(SENTINEL) && !source.url.contains(SENTINEL))
        );
        assert_eq!(
            outcome.sources.len(),
            1,
            "credential-bearing URL is omitted"
        );
        assert_eq!(outcome.sources[0].url, "https://safe-result.test/path");
    }
}
