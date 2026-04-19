use crate::config::Config;
use crate::error::AppError;
use crate::error::AppResult;
use async_trait::async_trait;
use serde::Deserialize;
use url::Url;

#[async_trait]
pub trait SearchClient: Send + Sync {
    async fn search(&self, query: &str) -> AppResult<String>;
}

#[derive(Debug, Clone)]
pub struct BraveSearchClient {
    client: reqwest::Client,
    base_url: Url,
    api_key: Option<String>,
    max_results: usize,
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
    async fn search(&self, query: &str) -> AppResult<String> {
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
            .map_err(|err| AppError::upstream(format!("Brave search request failed: {err}")))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(AppError::upstream(format!(
                "Brave search failed with {status}: {body}"
            )));
        }
        let payload: BraveSearchResponse = response
            .json()
            .await
            .map_err(|err| AppError::upstream(format!("invalid Brave search JSON: {err}")))?;
        Ok(format_search_results(&payload))
    }
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

    use super::BraveSearchResponse;
    use super::BraveWebResult;
    use super::BraveWebResults;
    use super::format_search_results;

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
    fn endpoint_url_preserves_v1_without_trailing_slash() {
        let client = BraveSearchClient::new(
            reqwest::Client::new(),
            Config {
                bind_addr: "127.0.0.1:0".parse().expect("socket addr"),
                upstream_base_url: url::Url::parse("http://127.0.0.1:8000/v1/").expect("url"),
                upstream_api_key: None,
                upstream_model: None,
                upstream_request_log_path: None,
                upstream_chat_kwargs: serde_json::Map::new(),
                brave_base_url: url::Url::parse("https://api.search.brave.com/res/v1")
                    .expect("url"),
                brave_api_key: Some("secret".to_string()),
                brave_max_results: 5,
                request_timeout: std::time::Duration::from_secs(30),
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
                upstream_request_log_path: None,
                upstream_chat_kwargs: serde_json::Map::new(),
                brave_base_url: url::Url::parse("https://api.search.brave.com/res/v1/")
                    .expect("url"),
                brave_api_key: Some("secret".to_string()),
                brave_max_results: 5,
                request_timeout: std::time::Duration::from_secs(30),
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
}
