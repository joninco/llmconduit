use crate::error::AppError;
use crate::error::AppResult;
use crate::models::chat::ChatCompletionChunk;
use crate::models::chat::ChatCompletionRequest;
use async_trait::async_trait;
use axum::body::Bytes;
use eventsource_stream::Eventsource;
use futures::Stream;
use futures::StreamExt;
use http::HeaderMap;
use http::HeaderName;
use regex::Regex;
use reqwest::RequestBuilder;
use reqwest::StatusCode;
use serde_json::Map as JsonMap;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::Mutex as AsyncMutex;
use url::Url;

pub type UpstreamStream =
    Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, AppError>> + Send + 'static>>;

const ROUTING_MODEL_CATALOG_TTL_SECS: u64 = 300;

#[async_trait]
pub trait UpstreamClient: Send + Sync {
    async fn stream_chat_completion(
        &self,
        request: &ChatCompletionRequest,
    ) -> AppResult<UpstreamStream>;
    async fn stream_chat_completion_with_timeout(
        &self,
        request: &ChatCompletionRequest,
        request_timeout: Duration,
    ) -> AppResult<UpstreamStream> {
        let stream = self.stream_chat_completion(request).await?;
        Ok(timeout_upstream_stream(stream, request_timeout))
    }
    async fn list_models(&self) -> AppResult<reqwest::Response>;
    async fn proxy_completions(
        &self,
        _headers: HeaderMap,
        _body: Bytes,
    ) -> AppResult<reqwest::Response> {
        Err(AppError::internal(
            "upstream completions proxy is not implemented",
        ))
    }
    async fn supported_model_ids(&self) -> AppResult<Vec<String>>;
}

#[derive(Debug, Clone)]
pub struct ReqwestUpstreamClient {
    client: reqwest::Client,
    base_url: Url,
    api_key: Option<String>,
    request_logger: Option<UpstreamRequestLogger>,
    flatten_content: bool,
    /// Floor for the shrink-and-retry completion budget on a context-window
    /// overflow (G1). A retry never reduces `max_completion_tokens` below this.
    min_completion_tokens: i64,
}

#[derive(Debug, Clone)]
pub struct FailoverUpstreamProvider {
    name: String,
    client: ReqwestUpstreamClient,
    upstream_model: Option<String>,
    exposed_model: Option<String>,
    upstream_chat_kwargs: JsonMap<String, Value>,
}

impl FailoverUpstreamProvider {
    pub fn new(
        name: impl Into<String>,
        client: ReqwestUpstreamClient,
        upstream_model: Option<String>,
        exposed_model: Option<String>,
        upstream_chat_kwargs: JsonMap<String, Value>,
    ) -> Self {
        Self {
            name: name.into(),
            client,
            upstream_model,
            exposed_model,
            upstream_chat_kwargs,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FailoverUpstreamClient {
    providers: Vec<FailoverUpstreamProvider>,
    cooldown: Duration,
    states: Arc<Mutex<Vec<ProviderCooldownState>>>,
}

#[derive(Debug, Clone)]
pub struct RoutingUpstreamProvider {
    name: String,
    primary_client: ReqwestUpstreamClient,
    primary_upstream_model: Option<String>,
    fallback_exposed_models: Vec<RoutingFallbackExposedModel>,
    client: FailoverUpstreamClient,
}

impl RoutingUpstreamProvider {
    pub fn new(
        name: impl Into<String>,
        primary_client: ReqwestUpstreamClient,
        primary_upstream_model: Option<String>,
        primary_upstream_chat_kwargs: JsonMap<String, Value>,
        fallback_providers: Vec<FailoverUpstreamProvider>,
        cooldown: Duration,
    ) -> Self {
        let name = name.into();
        let mut providers = vec![FailoverUpstreamProvider::new(
            name.clone(),
            primary_client.clone(),
            primary_upstream_model.clone(),
            None,
            primary_upstream_chat_kwargs,
        )];
        let fallback_exposed_models = fallback_providers
            .iter()
            .enumerate()
            .filter_map(|(index, provider)| {
                provider
                    .exposed_model
                    .clone()
                    .map(|model_id| RoutingFallbackExposedModel {
                        model_id,
                        failover_provider_index: index + 1,
                    })
            })
            .collect();
        providers.extend(fallback_providers);
        Self {
            name,
            primary_client,
            primary_upstream_model,
            fallback_exposed_models,
            client: FailoverUpstreamClient::new(providers, cooldown),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RoutingUpstreamClient {
    providers: Vec<RoutingUpstreamProvider>,
    catalog: Arc<AsyncMutex<Option<CachedRoutingModelCatalog>>>,
}

#[derive(Debug, Clone)]
struct CachedRoutingModelCatalog {
    fetched_at: Instant,
    catalog: RoutingModelCatalog,
}

#[derive(Debug, Clone, Default)]
struct RoutingModelCatalog {
    provider_catalogs: Vec<RoutingProviderModelCatalog>,
    union_entries: Vec<Value>,
    union_ids: Vec<String>,
    ids_by_key: HashMap<String, Vec<RoutingModelCandidate>>,
}

#[derive(Debug, Clone, Default)]
struct RoutingProviderModelCatalog {
    candidates: Vec<RoutingModelCandidate>,
}

#[derive(Debug, Clone)]
struct RoutingModelCandidate {
    provider_index: usize,
    model_id: String,
    target: RoutingModelTarget,
}

#[derive(Debug, Clone)]
enum RoutingModelTarget {
    Primary,
    Fallback { failover_provider_index: usize },
}

#[derive(Debug, Clone)]
struct RoutingFallbackExposedModel {
    model_id: String,
    failover_provider_index: usize,
}

#[derive(Debug, Clone, Default)]
struct ProviderCooldownState {
    cooling_until: Option<Instant>,
    last_error: Option<String>,
}

#[derive(Debug, Clone)]
struct UpstreamRequestLogger {
    path: PathBuf,
    write_lock: Arc<Mutex<()>>,
}

impl UpstreamRequestLogger {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    async fn log(&self, request: &ChatCompletionRequest) -> std::io::Result<()> {
        let mut payload = serde_json::to_vec(request).map_err(std::io::Error::other)?;
        payload.push(b'\n');
        let path = self.path.clone();
        let write_lock = self.write_lock.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = write_lock.lock().map_err(|err| {
                std::io::Error::other(format!("request log lock poisoned: {err}"))
            })?;
            let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
            file.write_all(&payload)
        })
        .await
        .map_err(|err| std::io::Error::other(format!("spawn_blocking failed: {err}")))?
    }
}

impl ReqwestUpstreamClient {
    pub fn new(
        client: reqwest::Client,
        base_url: Url,
        api_key: Option<String>,
        request_log_path: Option<PathBuf>,
        flatten_content: bool,
        min_completion_tokens: i64,
    ) -> Self {
        Self {
            client,
            base_url,
            api_key,
            request_logger: request_log_path.map(UpstreamRequestLogger::new),
            flatten_content,
            min_completion_tokens: min_completion_tokens.max(1),
        }
    }

    fn with_auth(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.api_key {
            Some(api_key) => request.bearer_auth(api_key),
            None => request,
        }
    }

    fn endpoint_url(&self, path: &str) -> AppResult<Url> {
        let mut url = self.base_url.clone();
        if !url.path().ends_with('/') {
            let new_path = format!("{}/", url.path());
            url.set_path(&new_path);
        }
        url.join(path)
            .map_err(|err| AppError::internal(format!("invalid upstream URL: {err}")))
    }

    async fn send_chat_request(
        &self,
        url: &Url,
        request: &ChatCompletionRequest,
    ) -> AppResult<reqwest::Response> {
        self.with_auth(self.client.post(url.clone()))
            .json(request)
            .send()
            .await
            .map_err(|err| AppError::upstream(format!("upstream chat request failed: {err}")))
    }
}

#[async_trait]
impl UpstreamClient for ReqwestUpstreamClient {
    async fn stream_chat_completion(
        &self,
        request: &ChatCompletionRequest,
    ) -> AppResult<UpstreamStream> {
        let url = self.endpoint_url("chat/completions")?;
        // Inject family-specific `chat_template_kwargs` (G2) HERE: this leaf is
        // the single point that sees the FINAL provider model after any
        // routing/failover/exposed-alias remap, with provider kwargs already
        // merged by `request_for_provider`. The engine threads the
        // `template_family` override and the client's explicit kwargs on the
        // request; sniffing happens against `request.model`.
        let mut request = request.clone();
        apply_family_chat_template_kwargs(&mut request);
        let request = sanitize_chat_request(request, self.flatten_content);
        if let Some(ref logger) = self.request_logger
            && let Err(err) = logger.log(&request).await
        {
            tracing::warn!(
                path = %logger.path.display(),
                error = %err,
                "failed to append upstream request log"
            );
        }

        // First attempt. On a non-2xx whose body indicates a context/completion
        // token-limit overflow, shrink `max_completion_tokens` and retry ONCE.
        // This happens before any SSE chunk is parsed/yielded downstream, so it
        // can never duplicate already-streamed tokens, and it stays inside the
        // leaf client so the failover/routing layers never see a context-limit
        // error as a provider failure (it is a same-provider shrink-and-retry).
        let response = self.send_chat_request(&url, &request).await?;
        let status = response.status();
        if status.is_success() {
            return stream_success_response(response).await;
        }

        let body = response.text().await.unwrap_or_default();
        if let Some(retry) = classify_context_overflow(&body, self.min_completion_tokens, None) {
            let mut retried = request.clone();
            retried.max_output_tokens = Some(retry.max_completion_tokens);
            // The reduced budget lives on the typed `max_output_tokens` field
            // (serialized as `max_tokens`). Any max-token ALIAS that flowed into
            // `extra_body` (`max_tokens`, `max_completion_tokens`,
            // `max_output_tokens`) would serialize alongside and let the upstream
            // honor the stale oversized value, defeating the retry. Mirror the
            // engine's "explicit field removes conflicting default keys" rule and
            // strip them so only the reduced typed field applies.
            remove_max_token_aliases(&mut retried.extra_body);
            tracing::warn!(
                reason = retry.reason,
                ctx_limit = retry.ctx_limit,
                input_tokens = retry.input_tokens.unwrap_or(0),
                input_is_lower_bound = retry.input_tokens_is_lower_bound,
                new_max_completion_tokens = retry.max_completion_tokens,
                "upstream context-window overflow; retrying once with reduced completion budget"
            );
            let retry_response = self.send_chat_request(&url, &retried).await?;
            let retry_status = retry_response.status();
            if retry_status.is_success() {
                return stream_success_response(retry_response).await;
            }
            // The retry ALSO failed. If it is again a context overflow, this is a
            // same-provider sizing problem, not a provider failure: surface a
            // TERMINAL error so `FailoverUpstreamClient`/routing does NOT retry
            // the same oversized prompt on another provider (only one shrink-and-
            // retry is allowed; we do not loop). Any other non-2xx is a normal
            // (failover-eligible) upstream error.
            let retry_body = retry_response.text().await.unwrap_or_default();
            if classify_context_overflow(&retry_body, self.min_completion_tokens, None).is_some() {
                return Err(AppError::upstream_terminal(format!(
                    "upstream context-window overflow persisted after shrink-and-retry; \
                     failed with {retry_status}: {}",
                    truncate_for_error(&retry_body, 500)
                )));
            }
            return Err(AppError::upstream(format!(
                "upstream chat failed with {retry_status}: {}",
                truncate_for_error(&retry_body, 500)
            )));
        }

        Err(AppError::upstream(format!(
            "upstream chat failed with {status}: {}",
            truncate_for_error(&body, 500)
        )))
    }

    async fn list_models(&self) -> AppResult<reqwest::Response> {
        let url = self.endpoint_url("models")?;
        let response = self
            .with_auth(self.client.get(url))
            .send()
            .await
            .map_err(|err| AppError::upstream(format!("upstream models request failed: {err}")))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(AppError::upstream(format!(
                "upstream /models failed with {status}: {body}"
            )));
        }
        Ok(response)
    }

    async fn proxy_completions(
        &self,
        headers: HeaderMap,
        body: Bytes,
    ) -> AppResult<reqwest::Response> {
        let url = self.endpoint_url("completions")?;
        let request = copy_proxy_request_headers(self.client.post(url), &headers).body(body);
        self.with_auth(request).send().await.map_err(|err| {
            AppError::upstream(format!("upstream completions request failed: {err}"))
        })
    }

    async fn supported_model_ids(&self) -> AppResult<Vec<String>> {
        let response = self.list_models().await?;
        collect_supported_model_ids(response).await
    }
}

impl FailoverUpstreamClient {
    pub fn new(providers: Vec<FailoverUpstreamProvider>, cooldown: Duration) -> Self {
        let states = vec![ProviderCooldownState::default(); providers.len()];
        Self {
            providers,
            cooldown,
            states: Arc::new(Mutex::new(states)),
        }
    }

    fn available_provider_indices(&self) -> Vec<usize> {
        let now = Instant::now();
        let states = self
            .states
            .lock()
            .expect("upstream provider cooldown state lock poisoned");
        self.providers
            .iter()
            .enumerate()
            .filter_map(|(index, _)| {
                let cooling = states
                    .get(index)
                    .and_then(|state| state.cooling_until)
                    .is_some_and(|until| until > now);
                (!cooling).then_some(index)
            })
            .collect()
    }

    fn provider_is_available(&self, provider_index: usize) -> bool {
        let now = Instant::now();
        let states = self
            .states
            .lock()
            .expect("upstream provider cooldown state lock poisoned");
        !states
            .get(provider_index)
            .and_then(|state| state.cooling_until)
            .is_some_and(|until| until > now)
    }

    fn cooldown_error(&self) -> AppError {
        let now = Instant::now();
        let states = self
            .states
            .lock()
            .expect("upstream provider cooldown state lock poisoned");
        let next_retry_secs = states
            .iter()
            .filter_map(|state| state.cooling_until)
            .filter(|until| *until > now)
            .map(|until| until.duration_since(now).as_secs().max(1))
            .min()
            .unwrap_or(0);
        let last_error = states
            .iter()
            .rev()
            .find_map(|state| state.last_error.as_deref())
            .unwrap_or("no provider is currently available");
        AppError::upstream(format!(
            "all upstream providers are in cooldown; next retry in {next_retry_secs}s; last error: {last_error}"
        ))
    }

    fn request_for_provider(
        provider: &FailoverUpstreamProvider,
        request: &ChatCompletionRequest,
    ) -> ChatCompletionRequest {
        let mut request = request.clone();
        if let Some(model) = &provider.upstream_model {
            request.model = model.clone();
        }
        merge_fallback_chat_kwargs(&mut request, &provider.upstream_chat_kwargs);
        request
    }

    async fn prefetch_first_chunk(
        mut stream: UpstreamStream,
        request_timeout: Duration,
    ) -> AppResult<(ChatCompletionChunk, UpstreamStream)> {
        match tokio::time::timeout(request_timeout, stream.next()).await {
            Ok(Some(Ok(chunk))) => Ok((chunk, stream)),
            Ok(Some(Err(err))) => Err(err),
            Ok(None) => Err(AppError::upstream(
                "upstream stream ended before the first chunk",
            )),
            Err(_) => Err(AppError::upstream("upstream stream timed out".to_string())),
        }
    }

    fn stream_after_prefetch(
        &self,
        provider_index: usize,
        first_chunk: ChatCompletionChunk,
        mut stream: UpstreamStream,
        request_timeout: Duration,
    ) -> UpstreamStream {
        let states = Arc::clone(&self.states);
        let cooldown = self.cooldown;
        let provider_name = self.providers[provider_index].name.clone();
        Box::pin(async_stream::stream! {
            yield Ok(first_chunk);
            loop {
                match tokio::time::timeout(request_timeout, stream.next()).await {
                    Ok(Some(Ok(chunk))) => yield Ok(chunk),
                    Ok(Some(Err(err))) => {
                        Self::mark_provider_failure(
                            &states,
                            provider_index,
                            &provider_name,
                            cooldown,
                            err.to_string(),
                        );
                        yield Err(err);
                        break;
                    }
                    Ok(None) => break,
                    Err(_) => {
                        let err = AppError::upstream("upstream stream timed out".to_string());
                        Self::mark_provider_failure(
                            &states,
                            provider_index,
                            &provider_name,
                            cooldown,
                            err.to_string(),
                        );
                        yield Err(err);
                        break;
                    }
                }
            }
        })
    }

    fn mark_provider_success(&self, provider_index: usize) {
        let mut states = self
            .states
            .lock()
            .expect("upstream provider cooldown state lock poisoned");
        if let Some(state) = states.get_mut(provider_index) {
            state.cooling_until = None;
            state.last_error = None;
        }
    }

    fn mark_provider_failure(
        states: &Arc<Mutex<Vec<ProviderCooldownState>>>,
        provider_index: usize,
        provider_name: &str,
        cooldown: Duration,
        error: String,
    ) {
        let cooling_until = (cooldown > Duration::ZERO).then(|| Instant::now() + cooldown);
        let mut states = states
            .lock()
            .expect("upstream provider cooldown state lock poisoned");
        if let Some(state) = states.get_mut(provider_index) {
            state.cooling_until = cooling_until;
            state.last_error = Some(error.clone());
        }
        if cooldown > Duration::ZERO {
            tracing::warn!(
                provider = provider_name,
                cooldown_secs = cooldown.as_secs(),
                error = %error,
                "upstream provider failed; entering cooldown"
            );
        } else {
            tracing::warn!(
                provider = provider_name,
                error = %error,
                "upstream provider failed"
            );
        }
    }

    fn mark_failure(&self, provider_index: usize, error: &AppError) {
        Self::mark_provider_failure(
            &self.states,
            provider_index,
            &self.providers[provider_index].name,
            self.cooldown,
            error.to_string(),
        );
    }

    async fn stream_chat_completion_with_timeout_from_provider(
        &self,
        provider_index: usize,
        request: &ChatCompletionRequest,
        request_timeout: Duration,
    ) -> AppResult<UpstreamStream> {
        if provider_index >= self.providers.len() {
            return Err(AppError::internal(
                "resolved fallback provider index was out of range",
            ));
        }
        if !self.provider_is_available(provider_index) {
            return Err(self.cooldown_error());
        }
        self.stream_chat_completion_with_provider_indices(
            vec![provider_index],
            request,
            request_timeout,
        )
        .await
    }

    async fn stream_chat_completion_with_provider_indices(
        &self,
        provider_indices: Vec<usize>,
        request: &ChatCompletionRequest,
        request_timeout: Duration,
    ) -> AppResult<UpstreamStream> {
        let mut last_error = None;
        for provider_index in provider_indices {
            let provider = &self.providers[provider_index];
            let provider_request = Self::request_for_provider(provider, request);
            let stream = match provider
                .client
                .stream_chat_completion(&provider_request)
                .await
            {
                Ok(stream) => stream,
                Err(err) if !err.is_failover_eligible() => {
                    // Terminal same-provider error (e.g. a context overflow that
                    // survived the leaf shrink-and-retry). Trying the next
                    // provider would just overflow again, so surface it as-is and
                    // do NOT mark this provider failed (it is not at fault).
                    return Err(err);
                }
                Err(err) => {
                    self.mark_failure(provider_index, &err);
                    last_error = Some(err);
                    continue;
                }
            };
            match Self::prefetch_first_chunk(stream, request_timeout).await {
                Ok((first_chunk, stream)) => {
                    self.mark_provider_success(provider_index);
                    if provider_index > 0 {
                        tracing::info!(
                            provider = %provider.name,
                            "using fallback upstream provider"
                        );
                    }
                    return Ok(self.stream_after_prefetch(
                        provider_index,
                        first_chunk,
                        stream,
                        request_timeout,
                    ));
                }
                Err(err) => {
                    self.mark_failure(provider_index, &err);
                    last_error = Some(err);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            AppError::upstream("all upstream providers failed before producing a response")
        }))
    }

    async fn proxy_completions_from_provider(
        &self,
        provider_index: usize,
        headers: HeaderMap,
        body: Bytes,
    ) -> AppResult<reqwest::Response> {
        if provider_index >= self.providers.len() {
            return Err(AppError::internal(
                "resolved fallback provider index was out of range",
            ));
        }
        if !self.provider_is_available(provider_index) {
            return Err(self.cooldown_error());
        }
        self.proxy_completions_with_provider_indices(vec![provider_index], headers, body)
            .await
    }

    async fn proxy_completions_with_provider_indices(
        &self,
        provider_indices: Vec<usize>,
        headers: HeaderMap,
        body: Bytes,
    ) -> AppResult<reqwest::Response> {
        let mut last_error = None;
        for provider_index in provider_indices {
            let provider = &self.providers[provider_index];
            let provider_body = proxy_body_for_provider(provider, &body);
            match self.providers[provider_index]
                .client
                .proxy_completions(headers.clone(), provider_body)
                .await
            {
                Ok(response) => {
                    let status = response.status();
                    if !should_failover_proxy_status(status) {
                        return Ok(response);
                    }
                    let body = response.text().await.unwrap_or_default();
                    let err = AppError::upstream(format!(
                        "upstream completions failed with {status}: {body}"
                    ));
                    self.mark_failure(provider_index, &err);
                    last_error = Some(err);
                }
                Err(err) => {
                    self.mark_failure(provider_index, &err);
                    last_error = Some(err);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            AppError::upstream("all upstream providers failed to proxy completions")
        }))
    }
}

impl RoutingUpstreamClient {
    pub fn new(providers: Vec<RoutingUpstreamProvider>) -> Self {
        Self {
            providers,
            catalog: Arc::new(AsyncMutex::new(None)),
        }
    }

    async fn load_catalog(&self) -> AppResult<RoutingModelCatalog> {
        let mut cache = self.catalog.lock().await;
        if let Some(cached) = cache.as_ref()
            && cached.fetched_at.elapsed().as_secs() < ROUTING_MODEL_CATALOG_TTL_SECS
        {
            return Ok(cached.catalog.clone());
        }
        let catalog = self.refresh_catalog().await?;
        *cache = Some(CachedRoutingModelCatalog {
            fetched_at: Instant::now(),
            catalog: catalog.clone(),
        });
        Ok(catalog)
    }

    async fn refresh_catalog(&self) -> AppResult<RoutingModelCatalog> {
        let mut provider_catalogs = Vec::with_capacity(self.providers.len());
        let mut union_entries = Vec::new();
        let mut union_ids = Vec::new();
        let mut ids_by_key: HashMap<String, Vec<RoutingModelCandidate>> = HashMap::new();
        let mut seen_union_ids = HashSet::new();
        let mut last_error = None;

        for (provider_index, provider) in self.providers.iter().enumerate() {
            let entries = match primary_provider_model_entries(provider).await {
                Ok(entries) => entries,
                Err(err) => {
                    tracing::warn!(
                        provider = %provider.name,
                        error = %err,
                        "failed to load upstream model catalog"
                    );
                    last_error = Some(err);
                    Vec::new()
                }
            };

            let mut provider_candidates = Vec::new();
            for entry in entries {
                let Some((model_id, entry)) = normalized_model_entry(entry) else {
                    continue;
                };
                register_routing_model(
                    provider_index,
                    model_id,
                    entry,
                    RoutingModelTarget::Primary,
                    &mut provider_candidates,
                    &mut union_entries,
                    &mut union_ids,
                    &mut ids_by_key,
                    &mut seen_union_ids,
                );
            }
            for model in &provider.fallback_exposed_models {
                register_routing_model(
                    provider_index,
                    model.model_id.clone(),
                    single_model_entry(&model.model_id),
                    RoutingModelTarget::Fallback {
                        failover_provider_index: model.failover_provider_index,
                    },
                    &mut provider_candidates,
                    &mut union_entries,
                    &mut union_ids,
                    &mut ids_by_key,
                    &mut seen_union_ids,
                );
            }
            provider_catalogs.push(RoutingProviderModelCatalog {
                candidates: provider_candidates,
            });
        }

        if union_ids.is_empty() {
            return Err(last_error.unwrap_or_else(|| {
                AppError::upstream("no models are currently available from configured upstreams")
            }));
        }

        Ok(RoutingModelCatalog {
            provider_catalogs,
            union_entries,
            union_ids,
            ids_by_key,
        })
    }
}

impl RoutingModelCatalog {
    fn resolve(&self, requested_model: &str) -> Option<RoutingModelCandidate> {
        let trimmed = requested_model.trim();
        if !trimmed.is_empty() {
            for (provider_index, provider) in self.provider_catalogs.iter().enumerate() {
                if let Some(candidate) = provider
                    .candidates
                    .iter()
                    .find(|candidate| candidate.model_id == trimmed)
                {
                    debug_assert_eq!(candidate.provider_index, provider_index);
                    return Some(candidate.clone());
                }
            }

            let key = canonical_model_key(trimmed);
            if let Some(candidates) = self.ids_by_key.get(&key)
                && let Some(model_id) = unique_candidate_model_id(candidates)
            {
                return candidates
                    .iter()
                    .find(|candidate| candidate.model_id == model_id)
                    .cloned();
            }
        }

        self.default_candidate()
    }

    fn default_candidate(&self) -> Option<RoutingModelCandidate> {
        self.provider_catalogs
            .iter()
            .enumerate()
            .find_map(|(provider_index, provider)| {
                provider.candidates.first().map(|candidate| {
                    debug_assert_eq!(candidate.provider_index, provider_index);
                    candidate.clone()
                })
            })
    }

    fn union_body(&self) -> Value {
        serde_json::json!({
            "object": "list",
            "data": self.union_entries,
        })
    }
}

fn unique_candidate_model_id(candidates: &[RoutingModelCandidate]) -> Option<String> {
    let mut unique = candidates
        .iter()
        .map(|candidate| candidate.model_id.as_str())
        .collect::<HashSet<_>>();
    if unique.len() == 1 {
        unique.drain().next().map(ToString::to_string)
    } else {
        None
    }
}

async fn primary_provider_model_entries(
    provider: &RoutingUpstreamProvider,
) -> AppResult<Vec<Value>> {
    let Some(model) = provider.primary_upstream_model.as_deref() else {
        let response = provider.primary_client.list_models().await?;
        let (_, body, _) = collect_models_response(response).await?;
        return Ok(model_entries_from_body(&body));
    };

    match provider.primary_client.list_models().await {
        Ok(response) => {
            let (_, body, _) = collect_models_response(response).await?;
            Ok(filter_model_entries(&model_entries_from_body(&body), model))
        }
        Err(err) => {
            tracing::warn!(
                provider = %provider.name,
                model,
                error = %err,
                "failed to load model metadata for configured upstream model; using synthetic entry"
            );
            Ok(vec![single_model_entry(model)])
        }
    }
}

fn normalized_model_entry(entry: Value) -> Option<(String, Value)> {
    match entry {
        Value::String(id) => Some((id.clone(), single_model_entry(&id))),
        Value::Object(map) => {
            let id = map.get("id").and_then(Value::as_str)?.to_string();
            Some((id, Value::Object(map)))
        }
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn register_routing_model(
    provider_index: usize,
    model_id: String,
    entry: Value,
    target: RoutingModelTarget,
    provider_candidates: &mut Vec<RoutingModelCandidate>,
    union_entries: &mut Vec<Value>,
    union_ids: &mut Vec<String>,
    ids_by_key: &mut HashMap<String, Vec<RoutingModelCandidate>>,
    seen_union_ids: &mut HashSet<String>,
) {
    let key = canonical_model_key(&model_id);
    if key.is_empty() {
        return;
    }
    let candidate = RoutingModelCandidate {
        provider_index,
        model_id: model_id.clone(),
        target,
    };
    if !provider_candidates
        .iter()
        .any(|candidate| candidate.model_id == model_id)
    {
        provider_candidates.push(candidate.clone());
    }
    ids_by_key.entry(key).or_default().push(candidate);
    if seen_union_ids.insert(model_id.clone()) {
        union_ids.push(model_id);
        union_entries.push(entry);
    }
}

#[async_trait]
impl UpstreamClient for FailoverUpstreamClient {
    async fn stream_chat_completion(
        &self,
        request: &ChatCompletionRequest,
    ) -> AppResult<UpstreamStream> {
        self.stream_chat_completion_with_timeout(request, Duration::from_secs(60))
            .await
    }

    async fn stream_chat_completion_with_timeout(
        &self,
        request: &ChatCompletionRequest,
        request_timeout: Duration,
    ) -> AppResult<UpstreamStream> {
        let provider_indices = self.available_provider_indices();
        if provider_indices.is_empty() {
            return Err(self.cooldown_error());
        }
        self.stream_chat_completion_with_provider_indices(
            provider_indices,
            request,
            request_timeout,
        )
        .await
    }

    async fn list_models(&self) -> AppResult<reqwest::Response> {
        let mut last_error = None;
        let provider_indices = self.available_provider_indices();
        if provider_indices.is_empty() {
            return Err(self.cooldown_error());
        }
        for provider_index in provider_indices {
            let provider = &self.providers[provider_index];
            match provider.client.list_models().await {
                Ok(response) => {
                    if let Some(model) = &provider.upstream_model {
                        return filter_models_response(response, model).await;
                    }
                    return Ok(response);
                }
                Err(err) => last_error = Some(err),
            }
        }
        Err(last_error
            .unwrap_or_else(|| AppError::upstream("all upstream providers failed to list models")))
    }

    async fn proxy_completions(
        &self,
        headers: HeaderMap,
        body: Bytes,
    ) -> AppResult<reqwest::Response> {
        let provider_indices = self.available_provider_indices();
        if provider_indices.is_empty() {
            return Err(self.cooldown_error());
        }
        self.proxy_completions_with_provider_indices(provider_indices, headers, body)
            .await
    }

    async fn supported_model_ids(&self) -> AppResult<Vec<String>> {
        let response = self.list_models().await?;
        collect_supported_model_ids(response).await
    }
}

#[async_trait]
impl UpstreamClient for RoutingUpstreamClient {
    async fn stream_chat_completion(
        &self,
        request: &ChatCompletionRequest,
    ) -> AppResult<UpstreamStream> {
        self.stream_chat_completion_with_timeout(request, Duration::from_secs(60))
            .await
    }

    async fn stream_chat_completion_with_timeout(
        &self,
        request: &ChatCompletionRequest,
        request_timeout: Duration,
    ) -> AppResult<UpstreamStream> {
        let catalog = self.load_catalog().await?;
        let resolution = catalog.resolve(&request.model).ok_or_else(|| {
            AppError::upstream("no models are currently available from configured upstreams")
        })?;
        let provider = self
            .providers
            .get(resolution.provider_index)
            .ok_or_else(|| {
                AppError::internal("resolved upstream provider index was out of range")
            })?;
        let mut routed_request = request.clone();
        if routed_request.model != resolution.model_id {
            tracing::info!(
                requested_model = %routed_request.model,
                routed_model = %resolution.model_id,
                provider = %provider.name,
                "routed request model to upstream catalog model"
            );
            routed_request.model = resolution.model_id;
        }
        match resolution.target {
            RoutingModelTarget::Primary => {
                provider
                    .client
                    .stream_chat_completion_with_timeout(&routed_request, request_timeout)
                    .await
            }
            RoutingModelTarget::Fallback {
                failover_provider_index,
            } => {
                provider
                    .client
                    .stream_chat_completion_with_timeout_from_provider(
                        failover_provider_index,
                        &routed_request,
                        request_timeout,
                    )
                    .await
            }
        }
    }

    async fn list_models(&self) -> AppResult<reqwest::Response> {
        let catalog = self.load_catalog().await?;
        json_response(catalog.union_body())
    }

    async fn proxy_completions(
        &self,
        headers: HeaderMap,
        body: Bytes,
    ) -> AppResult<reqwest::Response> {
        let catalog = self.load_catalog().await?;
        let requested_model = proxy_body_model(&body).unwrap_or_default();
        let resolution = catalog.resolve(&requested_model).ok_or_else(|| {
            AppError::upstream("no models are currently available from configured upstreams")
        })?;
        let provider = self
            .providers
            .get(resolution.provider_index)
            .ok_or_else(|| {
                AppError::internal("resolved upstream provider index was out of range")
            })?;
        let body = proxy_body_with_model(body, &resolution.model_id);
        match resolution.target {
            RoutingModelTarget::Primary => provider.client.proxy_completions(headers, body).await,
            RoutingModelTarget::Fallback {
                failover_provider_index,
            } => {
                provider
                    .client
                    .proxy_completions_from_provider(failover_provider_index, headers, body)
                    .await
            }
        }
    }

    async fn supported_model_ids(&self) -> AppResult<Vec<String>> {
        let catalog = self.load_catalog().await?;
        Ok(catalog.union_ids)
    }
}

fn timeout_upstream_stream(
    mut stream: UpstreamStream,
    request_timeout: Duration,
) -> UpstreamStream {
    Box::pin(async_stream::stream! {
        loop {
            match tokio::time::timeout(request_timeout, stream.next()).await {
                Ok(Some(chunk)) => yield chunk,
                Ok(None) => break,
                Err(_) => {
                    yield Err(AppError::upstream("upstream stream timed out".to_string()));
                    break;
                }
            }
        }
    })
}

fn should_failover_proxy_status(status: StatusCode) -> bool {
    status.is_server_error()
        || status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
}

/// Backend chat-template contract a model speaks. vLLM/SGLang expose
/// reasoning through different `chat_template_kwargs` per model family, so we
/// inject the right knobs (G2). Mirrors claude-relay `backend.py`.
///
/// Detection + injection live HERE, in the upstream client, rather than in the
/// engine: routing/failover/exposed-alias paths rewrite the actual provider
/// model AFTER the engine resolves its model (`request_for_provider` /
/// `RoutingUpstreamClient::stream_chat_completion`). Sniffing the family from
/// the engine's model would send e.g. Kimi kwargs to a DeepSeek fallback (or
/// none to a Kimi fallback). The leaf is the single point that always sees the
/// FINAL `request.model` with provider `upstream_chat_kwargs` already merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelFamily {
    Kimi,
    DeepSeek,
}

/// Resolve the backend family for the FINAL provider model. An explicit
/// `template_family` override (already normalized to `kimi`/`deepseek`) wins;
/// otherwise sniff the model id case-insensitively. The concrete provider model
/// is authoritative — a stale configured `upstream_model`, an exposed alias, or
/// a failover remap must not push real DeepSeek traffic through Kimi mutation
/// (claude-relay lesson, now enforced at the layer that knows the real model).
///
/// Unlike claude-relay (which defaults unrecognized names to DeepSeek),
/// llmconduit returns `None` for anything unrecognized: the gateway serves
/// arbitrary OpenAI-compatible upstreams (glm, qwen, gpt, ...), and injecting
/// DeepSeek `enable_thinking` into those would be a regression.
pub(crate) fn detect_model_family(
    resolved_model: &str,
    family_override: Option<&str>,
) -> Option<ModelFamily> {
    match family_override {
        Some("kimi") => return Some(ModelFamily::Kimi),
        Some("deepseek") => return Some(ModelFamily::DeepSeek),
        _ => {}
    }
    let name = resolved_model.to_ascii_lowercase();
    if name.contains("kimi") {
        Some(ModelFamily::Kimi)
    } else if name.contains("deepseek") {
        Some(ModelFamily::DeepSeek)
    } else {
        None
    }
}

/// Inject family-specific `chat_template_kwargs` into the request `extra_body`
/// from the FINAL provider model. Composes with (does not clobber) the
/// already-merged configured/provider `upstream_chat_kwargs`, and re-overlays
/// the client's explicit `chat_template_kwargs` last so an explicit request
/// value still WINS over a forced family default. A no-op when the family is
/// unrecognized.
///
/// Public so the integration test harness's mock upstream can mirror the
/// production leaf and record the request the backend would actually receive.
pub fn apply_family_chat_template_kwargs(request: &mut ChatCompletionRequest) {
    let Some(family) = detect_model_family(&request.model, request.template_family.as_deref())
    else {
        return;
    };
    let reasoning_effort = request.reasoning_effort.clone();
    let entry = request
        .extra_body
        .entry("chat_template_kwargs".to_string())
        .or_insert_with(|| Value::Object(JsonMap::new()));
    let kwargs = match entry {
        Value::Object(kwargs) => kwargs,
        // A non-object configured `chat_template_kwargs` is malformed; replace
        // it with a fresh object rather than silently no-op'ing.
        other => {
            *other = Value::Object(JsonMap::new());
            other.as_object_mut().expect("just set to object")
        }
    };
    write_family_kwargs(kwargs, family, &reasoning_effort);
    // Request-supplied keys win over the forced family default (AGENTS.md:
    // request `extra_body` beats configured/injected defaults). Deep-merge so a
    // nested client object overlays the already-merged family/provider object
    // leaf-by-leaf (client wins on conflicts) instead of clobbering sibling keys
    // that came from the deep-merge of configured/provider defaults.
    if let Some(client_kwargs) = &request.client_chat_template_kwargs {
        for (key, value) in client_kwargs {
            match kwargs.get_mut(key) {
                Some(existing) => merge_json_value_prefer_source(existing, value),
                None => {
                    kwargs.insert(key.clone(), value.clone());
                }
            }
        }
    }
}

/// Recursively overlay `source` onto `destination`, with `source` (the
/// client/request value) winning on conflicting leaves while sibling keys
/// present only in `destination` are preserved. Mirror of
/// `merge_json_value_preserve_destination` with the precedence flipped.
fn merge_json_value_prefer_source(destination: &mut Value, source: &Value) {
    if let Value::Object(destination_object) = destination
        && let Value::Object(source_object) = source
    {
        for (key, source_value) in source_object {
            match destination_object.get_mut(key) {
                Some(destination_value) => {
                    merge_json_value_prefer_source(destination_value, source_value);
                }
                None => {
                    destination_object.insert(key.clone(), source_value.clone());
                }
            }
        }
        return;
    }
    *destination = source.clone();
}

fn write_family_kwargs(
    kwargs: &mut JsonMap<String, Value>,
    family: ModelFamily,
    reasoning_effort: &Option<String>,
) {
    match family {
        ModelFamily::Kimi => {
            // Kimi K2 templates read `thinking`/`preserve_thinking` (bools),
            // NOT the DeepSeek vars. Strip the DeepSeek-only keys so a stale
            // configured default for the wrong family can't leak through.
            for key in ["enable_thinking", "clear_thinking", "reasoning_effort"] {
                kwargs.remove(key);
            }
            // Force thinking ON unconditionally — even when the client did not
            // request reasoning. With `thinking=false` the vLLM kimi parser
            // falls through to the identity parser and leaks the raw chain of
            // thought (plus a stray `</think>`) into `content`. `thinking=true`
            // routes it to `delta.reasoning`, where the response-side handling
            // decides whether the client actually sees it. This intentionally
            // OVERRIDES a configured default; an explicit request
            // `chat_template_kwargs.thinking` still wins (re-overlaid after).
            kwargs.insert("thinking".to_string(), Value::Bool(true));
            kwargs.insert("preserve_thinking".to_string(), Value::Bool(true));
        }
        ModelFamily::DeepSeek => {
            // DeepSeek reads `enable_thinking` (+ `reasoning_effort`). Use
            // setdefault semantics: respect any configured default, and don't
            // fight the separately-handled top-level `reasoning_effort`.
            kwargs.entry("enable_thinking").or_insert(Value::Bool(true));
            if let Some(effort) = reasoning_effort
                .as_deref()
                .map(str::trim)
                .filter(|effort| !effort.is_empty())
            {
                kwargs
                    .entry("reasoning_effort")
                    .or_insert_with(|| Value::String(effort.to_string()));
            }
        }
    }
}

fn merge_fallback_chat_kwargs(
    request: &mut ChatCompletionRequest,
    defaults: &JsonMap<String, Value>,
) {
    for (key, value) in defaults {
        if chat_request_field_is_set(request, key) {
            continue;
        }
        match request.extra_body.get_mut(key) {
            Some(existing) => merge_json_value_preserve_destination(existing, value),
            None => {
                request.extra_body.insert(key.clone(), value.clone());
            }
        }
    }
}

fn chat_request_field_is_set(request: &ChatCompletionRequest, key: &str) -> bool {
    match key {
        "model" | "messages" | "stream" | "parallel_tool_calls" => true,
        "tools" => request.tools.is_some(),
        "tool_choice" => request.tool_choice.is_some(),
        "reasoning_effort" => request.reasoning_effort.is_some(),
        "response_format" => request.response_format.is_some(),
        "stream_options" => request.stream_options.is_some(),
        "temperature" => request.temperature.is_some(),
        "top_p" => request.top_p.is_some(),
        "max_tokens" | "max_output_tokens" | "max_completion_tokens" => {
            request.max_output_tokens.is_some()
        }
        "frequency_penalty" => request.frequency_penalty.is_some(),
        "presence_penalty" => request.presence_penalty.is_some(),
        _ => false,
    }
}

/// Max-token alias keys that can coexist with the typed `max_output_tokens`
/// field inside the flattened `extra_body`. The engine treats these as one
/// logical knob (`engine.rs` `remove_*` helpers); the leaf shrink-and-retry
/// must strip all of them so a stale oversized alias cannot override the
/// reduced typed `max_tokens` on the retried request.
const MAX_TOKEN_ALIAS_KEYS: [&str; 3] =
    ["max_tokens", "max_output_tokens", "max_completion_tokens"];

/// Remove every max-token alias from `extra_body` so only the typed,
/// reduced `max_output_tokens` (serialized as `max_tokens`) applies on retry.
fn remove_max_token_aliases(extra_body: &mut std::collections::BTreeMap<String, Value>) {
    for key in MAX_TOKEN_ALIAS_KEYS {
        extra_body.remove(key);
    }
}

fn merge_json_value_preserve_destination(destination: &mut Value, source: &Value) {
    if let Value::Object(destination_object) = destination
        && let Value::Object(source_object) = source
    {
        for (key, source_value) in source_object {
            match destination_object.get_mut(key) {
                Some(destination_value) => {
                    merge_json_value_preserve_destination(destination_value, source_value);
                }
                None => {
                    destination_object.insert(key.clone(), source_value.clone());
                }
            }
        }
    }
}

fn proxy_body_for_provider(provider: &FailoverUpstreamProvider, body: &Bytes) -> Bytes {
    match provider.upstream_model.as_deref() {
        Some(model) => proxy_body_with_model(body.clone(), model),
        None => body.clone(),
    }
}

fn proxy_body_model(body: &Bytes) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("model")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(ToString::to_string)
        })
}

fn proxy_body_with_model(body: Bytes, model: &str) -> Bytes {
    let Ok(mut value) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };
    let Some(object) = value.as_object_mut() else {
        return body;
    };
    object.insert("model".to_string(), Value::String(model.to_string()));
    serde_json::to_vec(&value).map(Bytes::from).unwrap_or(body)
}

pub(crate) fn canonical_model_key(model: &str) -> String {
    model
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

fn copy_proxy_request_headers(mut request: RequestBuilder, headers: &HeaderMap) -> RequestBuilder {
    for (name, value) in headers {
        if should_proxy_request_header(name) {
            request = request.header(name.clone(), value.clone());
        }
    }
    request
}

fn should_proxy_request_header(name: &HeaderName) -> bool {
    !is_hop_by_hop_header(name)
        && !header_name_eq(name, "authorization")
        && !header_name_eq(name, "host")
        && !header_name_eq(name, "content-length")
}

fn is_hop_by_hop_header(name: &HeaderName) -> bool {
    [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ]
    .iter()
    .any(|header| header_name_eq(name, header))
}

fn header_name_eq(name: &HeaderName, other: &str) -> bool {
    name.as_str().eq_ignore_ascii_case(other)
}

/// Small fixed reserve subtracted from the context window when recomputing the
/// completion budget for an exact-token overflow error.
const CONTEXT_RETRY_MARGIN: i64 = 100;
/// Larger reserve used when the reported input-token count is only a lower
/// bound (vLLM's "prompt contains at least N input tokens"). The real prompt is
/// at least that large, so a wider margin avoids chasing a one-token-over
/// boundary across retries.
const CONTEXT_LOWER_BOUND_RETRY_MARGIN: i64 = 1024;

/// A structured decision to retry an upstream request after a context/
/// completion token-limit overflow, carrying the recomputed completion budget.
///
/// `max_completion_tokens` is already clamped to the configured minimum floor;
/// callers reduce the request's `max_completion_tokens` to this value and retry
/// exactly once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextOverflowRetry {
    /// Recomputed `max_completion_tokens` for the retry (>= the min floor).
    pub max_completion_tokens: i64,
    /// The model's context window as reported by the error body.
    pub ctx_limit: i64,
    /// Input/prompt tokens parsed from the error body, when available.
    pub input_tokens: Option<i64>,
    /// Which overflow shape matched: `"completion_limit"` or `"context_limit"`.
    pub reason: &'static str,
    /// True when `input_tokens` is a lower bound ("prompt contains at least N").
    pub input_tokens_is_lower_bound: bool,
}

fn context_retry_completion_budget(
    ctx_limit: i64,
    input_tokens: Option<i64>,
    min_completion_tokens: i64,
    input_tokens_is_lower_bound: bool,
) -> i64 {
    let margin = if input_tokens_is_lower_bound {
        CONTEXT_LOWER_BOUND_RETRY_MARGIN
    } else {
        CONTEXT_RETRY_MARGIN
    };
    let available = ctx_limit - margin - input_tokens.unwrap_or(0);
    available.max(min_completion_tokens)
}

/// vLLM/SGLang output-only limit:
/// `max_completion_tokens=X cannot be greater than max_model_len=Y`.
///
/// Mirrors the reference regex
/// `max_completion_tokens\s*=\s*([\d,]+).*?max_model_len\D*([\d,]+)`
/// (claude-relay `server._context_overflow_retry_from_error`), with the literal
/// `cannot be greater than` comparison pinned BETWEEN the two anchored fields.
/// The reference's bare `.*?` would also match an unrelated validation body
/// that merely names both fields with numbers (e.g. "max_completion_tokens=N is
/// not allowed together with max_model_len=N"); requiring the actual overflow
/// comparison — the exact wording this shape always carries — rejects that
/// false positive while still matching every genuine overflow. Group 1 =
/// requested completion tokens (presence enforces the shape), group 2 =
/// `max_model_len` (the ctx limit).
static COMPLETION_LIMIT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)max_completion_tokens\s*=\s*([\d,]+).*?cannot be greater than.*?max_model_len\D*([\d,]+)",
    )
    .expect("completion-limit overflow regex is valid")
});

/// vLLM combined input+output limit:
/// `maximum context length is X. However, you requested Y output tokens and
/// your prompt contains [at least] Z input tokens`.
///
/// Mirrors the reference regex
/// `maximum context length is\s*([\d,]+).*?requested\s*([\d,]+)\s*output tokens
/// .*?prompt contains(?P<lower_bound>\s+at least)?\s*([\d,]+)\s*input tokens`.
/// The tight `requested\s*([\d,]+)\s*output tokens` adjacency (only whitespace
/// between `requested`, the number and `output tokens`) is what rejects bodies
/// that merely contain the bare words out of shape. Group 1 = ctx limit,
/// `lower_bound` = the optional ` at least` qualifier, group 3 = input tokens.
static VLLM_COMBINED_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)maximum context length is\s*([\d,]+).*?requested\s*([\d,]+)\s*output tokens.*?prompt contains(?P<lower_bound>\s+at least)?\s*([\d,]+)\s*input tokens",
    )
    .expect("vLLM combined overflow regex is valid")
});

/// OpenAI-compatible/SGLang shape:
/// `maximum context length is X tokens. However, you requested Y tokens
/// (Z in the messages, W in the completion)`.
/// The canonical OpenAI wording uses `in the prompt` rather than `in the
/// messages`.
///
/// Mirrors the reference regex
/// `maximum context length is\s*([\d,]+).*?requested\s*([\d,]+)\s*tokens.*?
/// \(([\d,]+)\s*in (?:the )?(?:messages|prompt).*?([\d,]+)\s*in (?:the )?
/// (?:completion|output)`. The literal `\(` before the input count and the
/// tight `requested\s*([\d,]+)\s*tokens` adjacency, plus the required
/// `... in the completion` half, reject bodies that only echo the input-side
/// phrase or carry the clauses out of order. Group 1 = ctx limit, group 3 =
/// input tokens.
static OPENAI_COMPATIBLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)maximum context length is\s*([\d,]+).*?requested\s*([\d,]+)\s*tokens.*?\(([\d,]+)\s*in (?:the )?(?:messages|prompt).*?([\d,]+)\s*in (?:the )?(?:completion|output)",
    )
    .expect("OpenAI-compatible overflow regex is valid")
});

/// vLLM/SGLang newer shape:
/// `Requested token count exceeds the model's maximum context length of X
/// tokens. You requested a total of Y tokens: Z tokens from the input messages
/// and W tokens for the completion`.
///
/// Mirrors the reference regex
/// `maximum context length of\s*([\d,]+)\s*tokens.*?requested a total of
/// \s*([\d,]+)\s*tokens.*?([\d,]+)\s*tokens from (?:the )?
/// (?:input messages|messages|prompt).*?([\d,]+)\s*tokens for (?:the )?
/// (?:completion|output)`, hardened (like the other shapes) with this shape's
/// DISTINCTIVE LEADING LITERAL `requested token count exceeds`. That phrase is
/// the unique opening sentence this overflow always carries; an unrelated 4xx
/// that merely echoes the generic `maximum context length of …` +
/// `requested a total of N tokens` anchors (e.g. a validation/diagnostics body
/// rejecting some other field) does NOT contain it. Requiring it before the
/// anchors rejects that false positive (Codex round 6: the regex rewrite had
/// dropped this literal an earlier round added) while still matching every
/// genuine overflow. Group 1 = ctx limit, group 3 = input tokens.
static REQUESTED_TOTAL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)requested token count exceeds.*?maximum context length of\s*([\d,]+)\s*tokens.*?requested a total of\s*([\d,]+)\s*tokens.*?([\d,]+)\s*tokens from (?:the )?(?:input messages|messages|prompt).*?([\d,]+)\s*tokens for (?:the )?(?:completion|output)",
    )
    .expect("requested-total overflow regex is valid")
});

/// Parse a non-streaming upstream error body for a context/completion
/// token-limit overflow and compute the reduced completion budget for a retry.
///
/// Returns `None` for any error text that is not a recognizable token-limit
/// overflow, so the original error is surfaced unchanged (no retry).
///
/// `estimated_input_tokens` supplies the prompt size for the completion-only
/// (`max_model_len`) shape, which does not itself report the input size; the
/// leaf upstream path passes `None` (no local estimate), matching the reference
/// implementation's behavior when no estimate is available.
///
/// Classification uses the SAME anchored regexes the reference implementation
/// uses (`server._context_overflow_retry_from_error`, tests
/// `test_non_200_retry_*`): each shape is matched by a precompiled
/// [`Regex`] that mirrors the reference pattern exactly, and token counts are
/// extracted from the capture groups (commas stripped before parsing).
pub fn classify_context_overflow(
    error_body: &str,
    min_completion_tokens: i64,
    estimated_input_tokens: Option<i64>,
) -> Option<ContextOverflowRetry> {
    // vLLM/SGLang output-only limit (group 2 = max_model_len = ctx limit).
    if let Some(captures) = COMPLETION_LIMIT_RE.captures(error_body) {
        let ctx_limit = parse_token_count(&captures[2]);
        return Some(ContextOverflowRetry {
            max_completion_tokens: context_retry_completion_budget(
                ctx_limit,
                estimated_input_tokens,
                min_completion_tokens,
                false,
            ),
            ctx_limit,
            input_tokens: estimated_input_tokens,
            reason: "completion_limit",
            input_tokens_is_lower_bound: false,
        });
    }

    // vLLM combined input+output limit. Groups: 1 = ctx limit, 2 = requested
    // output tokens, 3 (named `lower_bound`) = the optional " at least"
    // qualifier, 4 = input tokens. The `lower_bound` group is a numbered slot in
    // its own right (Rust `regex` counts named captures positionally), so the
    // input count is group 4, not 3.
    if let Some(captures) = VLLM_COMBINED_RE.captures(error_body) {
        let ctx_limit = parse_token_count(&captures[1]);
        let input_tokens = parse_token_count(&captures[4]);
        let is_lower_bound = captures.name("lower_bound").is_some();
        return Some(ContextOverflowRetry {
            max_completion_tokens: context_retry_completion_budget(
                ctx_limit,
                Some(input_tokens),
                min_completion_tokens,
                is_lower_bound,
            ),
            ctx_limit,
            input_tokens: Some(input_tokens),
            reason: "context_limit",
            input_tokens_is_lower_bound: is_lower_bound,
        });
    }

    // OpenAI-compatible/SGLang and the newer "requested a total of" shape both
    // report ctx limit in group 1 and input tokens in group 3.
    if let Some(captures) = OPENAI_COMPATIBLE_RE
        .captures(error_body)
        .or_else(|| REQUESTED_TOTAL_RE.captures(error_body))
    {
        let ctx_limit = parse_token_count(&captures[1]);
        let input_tokens = parse_token_count(&captures[3]);
        return Some(ContextOverflowRetry {
            max_completion_tokens: context_retry_completion_budget(
                ctx_limit,
                Some(input_tokens),
                min_completion_tokens,
                false,
            ),
            ctx_limit,
            input_tokens: Some(input_tokens),
            reason: "context_limit",
            input_tokens_is_lower_bound: false,
        });
    }

    None
}

/// Parse a comma-grouped integer token count from a regex capture (e.g.
/// "202,752" -> 202752). Mirrors the reference `_parse_token_count`.
fn parse_token_count(group: &str) -> i64 {
    group
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or(0)
}

/// Convert an already-confirmed-success upstream response into the parsed SSE
/// chunk stream. The first chunk is only produced lazily by the returned
/// stream, so the context-overflow retry (which happens before this is called)
/// remains strictly pre-first-chunk.
async fn stream_success_response(response: reqwest::Response) -> AppResult<UpstreamStream> {
    let stream = response
        .bytes_stream()
        .eventsource()
        .filter_map(|result| async move {
            match result {
                Ok(event) if event.data == "[DONE]" => None,
                Ok(event) => Some(parse_chat_completion_chunk(&event.data).map_err(|err| {
                    AppError::upstream(format!(
                        "failed to parse upstream chat chunk: {err}; payload={}",
                        truncate_for_error(&event.data, 500)
                    ))
                })),
                Err(err) => Some(Err(AppError::upstream(format!(
                    "failed to read upstream SSE: {err}"
                )))),
            }
        });
    Ok(Box::pin(stream))
}

fn parse_chat_completion_chunk(data: &str) -> Result<ChatCompletionChunk, serde_json::Error> {
    let first_error = match serde_json::from_str::<ChatCompletionChunk>(data) {
        Ok(chunk) => return Ok(chunk),
        Err(err) => err,
    };
    let Ok(mut value) = serde_json::from_str::<Value>(data) else {
        return Err(first_error);
    };
    if !normalize_sparse_tool_call_types(&mut value) {
        return Err(first_error);
    }
    serde_json::from_value(value)
}

fn normalize_sparse_tool_call_types(value: &mut Value) -> bool {
    let Some(choices) = value.get_mut("choices").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut changed = false;
    for choice in choices {
        let Some(tool_calls) = choice
            .get_mut("delta")
            .and_then(|delta| delta.get_mut("tool_calls"))
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        for tool_call in tool_calls {
            let Some(object) = tool_call.as_object_mut() else {
                continue;
            };
            if !object.contains_key("type") {
                object.insert("type".to_string(), Value::String("function".to_string()));
                changed = true;
            }
        }
    }
    changed
}

pub async fn collect_models_response(
    response: reqwest::Response,
) -> AppResult<(StatusCode, Value, Option<String>)> {
    let status = response.status();
    let etag = response
        .headers()
        .get(http::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    let body = response
        .json::<Value>()
        .await
        .map_err(|err| AppError::upstream(format!("invalid upstream /models JSON: {err}")))?;
    Ok((status, body, etag))
}

pub async fn collect_supported_model_ids(response: reqwest::Response) -> AppResult<Vec<String>> {
    let (_, body, _) = collect_models_response(response).await?;
    Ok(extract_supported_model_ids(&body))
}

async fn filter_models_response(
    response: reqwest::Response,
    model: &str,
) -> AppResult<reqwest::Response> {
    let status = response.status();
    let body = response
        .json::<Value>()
        .await
        .map_err(|err| AppError::upstream(format!("invalid upstream /models JSON: {err}")))?;
    let body = filter_models_body(body, model);
    let body = serde_json::to_string(&body).map_err(|err| {
        AppError::internal(format!("failed to serialize /models response: {err}"))
    })?;
    let response = http::Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(body)
        .map_err(|err| AppError::internal(format!("failed to build /models response: {err}")))?;
    Ok(reqwest::Response::from(response))
}

fn json_response(body: Value) -> AppResult<reqwest::Response> {
    let body = serde_json::to_string(&body)
        .map_err(|err| AppError::internal(format!("failed to serialize JSON response: {err}")))?;
    let response = http::Response::builder()
        .status(StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(body)
        .map_err(|err| AppError::internal(format!("failed to build JSON response: {err}")))?;
    Ok(reqwest::Response::from(response))
}

fn filter_models_body(body: Value, model: &str) -> Value {
    match body {
        Value::Object(mut map) => {
            if let Some(entries) = map.get("data").and_then(Value::as_array) {
                map.insert(
                    "data".to_string(),
                    Value::Array(filter_model_entries(entries, model)),
                );
                Value::Object(map)
            } else if let Some(entries) = map.get("models").and_then(Value::as_array) {
                map.insert(
                    "models".to_string(),
                    Value::Array(filter_model_entries(entries, model)),
                );
                Value::Object(map)
            } else {
                single_model_list_body(model)
            }
        }
        Value::Array(entries) => Value::Array(filter_model_entries(&entries, model)),
        _ => single_model_list_body(model),
    }
}

fn filter_model_entries(entries: &[Value], model: &str) -> Vec<Value> {
    match entries
        .iter()
        .find(|entry| model_entry_id(entry).is_some_and(|id| id == model))
    {
        Some(entry) => vec![entry.clone()],
        None => vec![single_model_entry(model)],
    }
}

fn model_entry_id(entry: &Value) -> Option<&str> {
    match entry {
        Value::String(id) => Some(id.as_str()),
        Value::Object(map) => map.get("id").and_then(Value::as_str),
        _ => None,
    }
}

fn single_model_list_body(model: &str) -> Value {
    serde_json::json!({
        "object": "list",
        "data": [single_model_entry(model)]
    })
}

fn single_model_entry(model: &str) -> Value {
    serde_json::json!({
        "id": model,
        "object": "model",
    })
}

fn extract_supported_model_ids(body: &Value) -> Vec<String> {
    extract_model_ids_from_array(&model_entries_from_body(body))
}

fn model_entries_from_body(body: &Value) -> Vec<Value> {
    match body {
        Value::Array(entries) => entries.clone(),
        Value::Object(map) => map
            .get("data")
            .or_else(|| map.get("models"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn extract_model_ids_from_array(entries: &[Value]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            Value::String(id) => Some(id.clone()),
            Value::Object(map) => map
                .get("id")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            _ => None,
        })
        .collect()
}

pub(crate) fn sanitize_chat_request(
    mut request: ChatCompletionRequest,
    flatten_content: bool,
) -> ChatCompletionRequest {
    if request.tools.as_ref().is_none_or(Vec::is_empty)
        && request.tool_choice.as_ref().is_none_or(|v| v == "auto")
    {
        request.tool_choice = None;
    }
    for message in &mut request.messages {
        if let Some(content) = message.content.take() {
            message.content = sanitize_message_content(content, flatten_content);
        }
        if let Some(tool_calls) = message.tool_calls.as_mut() {
            for tool_call in tool_calls {
                if let Some(arguments) = tool_call.function.arguments.take() {
                    tool_call.function.arguments = Some(stringify_json_value(arguments));
                }
            }
        }
    }
    request
}

fn sanitize_message_content(content: Value, flatten_content: bool) -> Option<Value> {
    match content {
        Value::Null => None,
        Value::String(text) => Some(Value::String(text)),
        Value::Array(parts) => {
            if flatten_content && content_parts_are_text_only(&parts) {
                Some(Value::String(flatten_content_parts(&parts)))
            } else {
                Some(Value::Array(parts))
            }
        }
        other => Some(stringify_json_value(other)),
    }
}

fn content_parts_are_text_only(parts: &[Value]) -> bool {
    parts.iter().all(|part| {
        let has_text = part.get("text").and_then(Value::as_str).is_some();
        let text_kind = matches!(
            part.get("type").and_then(Value::as_str),
            None | Some("text") | Some("input_text") | Some("output_text")
        );
        has_text && text_kind
    })
}

fn flatten_content_parts(parts: &[Value]) -> String {
    let mut text_parts = Vec::new();
    for part in parts {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            text_parts.push(text.to_string());
        } else {
            text_parts.push(serde_json::to_string(part).unwrap_or_else(|_| "null".to_string()));
        }
    }
    text_parts.join("\n")
}

fn truncate_for_error(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    match s.char_indices().nth(max) {
        Some((byte_idx, _)) => format!("{}...[truncated]", &s[..byte_idx]),
        None => s.to_string(),
    }
}

fn stringify_json_value(value: Value) -> Value {
    Value::String(serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string()))
}

#[cfg(test)]
mod tests {
    use super::ReqwestUpstreamClient;
    use super::UpstreamRequestLogger;
    use super::extract_supported_model_ids;
    use super::sanitize_chat_request;
    use crate::models::chat::ChatCompletionRequest;
    use crate::models::chat::ChatMessage;
    use serde_json::Value;
    use std::collections::BTreeMap;

    #[test]
    fn endpoint_url_preserves_v1_without_trailing_slash() {
        let client = ReqwestUpstreamClient::new(
            reqwest::Client::new(),
            url::Url::parse("https://api.x.ai/v1").expect("url"),
            None,
            None,
            true,
            4096,
        );

        assert_eq!(
            client
                .endpoint_url("chat/completions")
                .expect("endpoint")
                .as_str(),
            "https://api.x.ai/v1/chat/completions"
        );
    }

    #[test]
    fn endpoint_url_preserves_v1_with_trailing_slash() {
        let client = ReqwestUpstreamClient::new(
            reqwest::Client::new(),
            url::Url::parse("https://api.x.ai/v1/").expect("url"),
            None,
            None,
            true,
            4096,
        );

        assert_eq!(
            client.endpoint_url("models").expect("endpoint").as_str(),
            "https://api.x.ai/v1/models"
        );
    }

    // --- G2: family detection + chat_template_kwargs injection at the leaf ---

    use super::FailoverUpstreamClient;
    use super::FailoverUpstreamProvider;
    use super::ModelFamily;
    use super::apply_family_chat_template_kwargs;
    use super::detect_model_family;
    use super::write_family_kwargs;
    use serde_json::Map as JsonMap;
    use serde_json::json;

    /// Minimal request for family-injection tests. `model` is the FINAL provider
    /// model the leaf sees (after any routing/failover/alias rewrite).
    fn family_request(model: &str) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: model.to_string(),
            messages: Vec::new(),
            stream: true,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: false,
            reasoning_effort: None,
            response_format: None,
            stream_options: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            frequency_penalty: None,
            presence_penalty: None,
            stop: None,
            extra_body: BTreeMap::new(),
            template_family: None,
            client_chat_template_kwargs: None,
        }
    }

    fn kwargs_of(request: &ChatCompletionRequest) -> &serde_json::Map<String, Value> {
        request.extra_body["chat_template_kwargs"]
            .as_object()
            .expect("chat_template_kwargs object")
    }

    #[test]
    fn detect_family_sniffs_resolved_model_case_insensitively() {
        assert_eq!(
            detect_model_family("kimi-k2-instruct", None),
            Some(ModelFamily::Kimi)
        );
        assert_eq!(
            detect_model_family("Moonshot-KIMI-K2", None),
            Some(ModelFamily::Kimi)
        );
        assert_eq!(
            detect_model_family("DeepSeek-V3", None),
            Some(ModelFamily::DeepSeek)
        );
        assert_eq!(detect_model_family("glm-5.1", None), None);
        assert_eq!(detect_model_family("gpt-4o", None), None);
    }

    #[test]
    fn detect_family_override_beats_model_name() {
        assert_eq!(
            detect_model_family("glm-5.1", Some("kimi")),
            Some(ModelFamily::Kimi)
        );
        assert_eq!(
            detect_model_family("kimi-k2", Some("deepseek")),
            Some(ModelFamily::DeepSeek)
        );
        // An unrecognized override falls through to name sniffing.
        assert_eq!(
            detect_model_family("kimi-k2", Some("bogus")),
            Some(ModelFamily::Kimi)
        );
    }

    #[test]
    fn write_kimi_kwargs_forces_thinking_and_strips_deepseek_keys() {
        let mut kwargs = JsonMap::new();
        kwargs.insert("enable_thinking".to_string(), json!(true));
        kwargs.insert("clear_thinking".to_string(), json!(true));
        kwargs.insert("reasoning_effort".to_string(), json!("low"));
        kwargs.insert("keep_me".to_string(), json!(1));
        write_family_kwargs(&mut kwargs, ModelFamily::Kimi, &Some("high".to_string()));
        assert_eq!(kwargs["thinking"], json!(true));
        assert_eq!(kwargs["preserve_thinking"], json!(true));
        assert!(!kwargs.contains_key("enable_thinking"));
        assert!(!kwargs.contains_key("clear_thinking"));
        assert!(!kwargs.contains_key("reasoning_effort"));
        assert_eq!(kwargs["keep_me"], json!(1));
    }

    #[test]
    fn write_deepseek_kwargs_setdefaults_enable_thinking_and_effort() {
        let mut kwargs = JsonMap::new();
        write_family_kwargs(
            &mut kwargs,
            ModelFamily::DeepSeek,
            &Some("high".to_string()),
        );
        assert_eq!(kwargs["enable_thinking"], json!(true));
        assert_eq!(kwargs["reasoning_effort"], json!("high"));

        let mut kwargs = JsonMap::new();
        kwargs.insert("enable_thinking".to_string(), json!(false));
        kwargs.insert("reasoning_effort".to_string(), json!("low"));
        write_family_kwargs(
            &mut kwargs,
            ModelFamily::DeepSeek,
            &Some("high".to_string()),
        );
        assert_eq!(kwargs["enable_thinking"], json!(false));
        assert_eq!(kwargs["reasoning_effort"], json!("low"));

        let mut kwargs = JsonMap::new();
        write_family_kwargs(&mut kwargs, ModelFamily::DeepSeek, &None);
        assert_eq!(kwargs["enable_thinking"], json!(true));
        assert!(!kwargs.contains_key("reasoning_effort"));
    }

    /// Finding 1: the FINAL provider model drives the family. A request that a
    /// failover/exposed-alias path rewrote to a Kimi model gets Kimi kwargs for
    /// THAT provider, even though the engine never saw "kimi".
    #[test]
    fn kimi_final_provider_model_gets_kimi_kwargs() {
        let mut request = family_request("kimi-k2-instruct");
        apply_family_chat_template_kwargs(&mut request);
        let kwargs = kwargs_of(&request);
        assert_eq!(kwargs["thinking"], json!(true));
        assert_eq!(kwargs["preserve_thinking"], json!(true));
    }

    /// Finding 1 (negative): a DeepSeek FINAL provider model does NOT get the
    /// Kimi `thinking` knob — the wrong-family kwargs cannot leak across.
    #[test]
    fn deepseek_final_provider_model_does_not_get_kimi_kwargs() {
        let mut request = family_request("deepseek-v3");
        request.reasoning_effort = Some("high".to_string());
        apply_family_chat_template_kwargs(&mut request);
        let kwargs = kwargs_of(&request);
        assert_eq!(kwargs["enable_thinking"], json!(true));
        assert_eq!(kwargs["reasoning_effort"], json!("high"));
        assert!(
            kwargs.get("thinking").is_none(),
            "DeepSeek must not carry the Kimi `thinking` key"
        );
    }

    /// Finding 1 end-to-end through `request_for_provider`: a failover provider
    /// that remaps the model to Kimi AND carries its own `upstream_chat_kwargs`
    /// produces a request that, once the leaf injects, has BOTH the merged
    /// provider kwarg and the Kimi family knobs (deep-merge, not clobber).
    #[test]
    fn request_for_provider_kimi_remap_then_inject_deep_merges() {
        let mut provider_kwargs = JsonMap::new();
        provider_kwargs.insert(
            "chat_template_kwargs".to_string(),
            json!({ "configured_only": true }),
        );
        let provider = FailoverUpstreamProvider::new(
            "kimi-fallback",
            ReqwestUpstreamClient::new(
                reqwest::Client::new(),
                url::Url::parse("https://example.invalid/v1").expect("url"),
                None,
                None,
                true,
                4096,
            ),
            Some("kimi-k2-instruct".to_string()),
            None,
            provider_kwargs,
        );
        // Engine-level request still names a non-Kimi model; the provider remaps.
        let base = family_request("glm-5.1");
        let mut provider_request = FailoverUpstreamClient::request_for_provider(&provider, &base);
        assert_eq!(provider_request.model, "kimi-k2-instruct");
        // Leaf injects family from the FINAL (remapped) model.
        apply_family_chat_template_kwargs(&mut provider_request);
        let kwargs = kwargs_of(&provider_request);
        assert_eq!(kwargs["thinking"], json!(true));
        assert_eq!(kwargs["preserve_thinking"], json!(true));
        assert_eq!(kwargs["configured_only"], json!(true));
    }

    /// Finding 1: an explicit client `chat_template_kwargs` value still WINS
    /// over the forced family default, while non-conflicting forced keys remain.
    #[test]
    fn client_chat_template_kwargs_win_over_forced_family_default() {
        let mut request = family_request("kimi-k2");
        request.client_chat_template_kwargs = Some(
            json!({ "thinking": false, "custom": 1 })
                .as_object()
                .unwrap()
                .clone(),
        );
        // The engine bakes the client value into extra_body; mirror that.
        request.extra_body.insert(
            "chat_template_kwargs".to_string(),
            json!({ "thinking": false, "custom": 1 }),
        );
        apply_family_chat_template_kwargs(&mut request);
        let kwargs = kwargs_of(&request);
        assert_eq!(kwargs["thinking"], json!(false), "client value wins");
        assert_eq!(kwargs["custom"], json!(1));
        assert_eq!(kwargs["preserve_thinking"], json!(true));
    }

    /// Finding 2: the client `chat_template_kwargs` re-overlay must DEEP-merge
    /// over the already-deep-merged family/provider object. A nested client
    /// object wins ONLY on its conflicting leaf; sibling keys placed there by the
    /// configured/provider deep-merge survive (a shallow `insert` would clobber
    /// the whole nested object and drop those siblings).
    #[test]
    fn client_kwargs_deep_merge_preserves_sibling_keys() {
        let mut request = family_request("kimi-k2-instruct");
        // Configured/provider deep-merge already produced a nested object with
        // two siblings under `mm_processor_kwargs`.
        request.extra_body.insert(
            "chat_template_kwargs".to_string(),
            json!({ "mm_processor_kwargs": { "from_config": true, "shared": "config" } }),
        );
        // The client only overrides ONE nested leaf and adds a new nested key.
        request.client_chat_template_kwargs = Some(
            json!({ "mm_processor_kwargs": { "shared": "client", "from_client": 1 } })
                .as_object()
                .unwrap()
                .clone(),
        );
        apply_family_chat_template_kwargs(&mut request);
        let nested = kwargs_of(&request)["mm_processor_kwargs"]
            .as_object()
            .expect("nested object survives deep-merge");
        assert_eq!(
            nested["shared"],
            json!("client"),
            "client wins the conflict"
        );
        assert_eq!(
            nested["from_config"],
            json!(true),
            "sibling key from the config deep-merge must survive"
        );
        assert_eq!(
            nested["from_client"],
            json!(1),
            "client adds new nested key"
        );
        // Forced family knobs still applied alongside the nested object.
        assert_eq!(kwargs_of(&request)["thinking"], json!(true));
    }

    /// Unrecognized family => no injection at all.
    #[test]
    fn unrecognized_final_model_injects_nothing() {
        let mut request = family_request("glm-5.1");
        apply_family_chat_template_kwargs(&mut request);
        assert!(!request.extra_body.contains_key("chat_template_kwargs"));
    }

    #[test]
    fn normalize_sparse_tool_call_types_fills_chat_function_type() {
        let mut value = serde_json::json!({
            "id": "gen-1778509925-7119UkUjPTix9sGQ4vZf",
            "object": "chat.completion.chunk",
            "created": 1778509925,
            "model": "xiaomi/mimo-v2.5-pro-20260422",
            "provider": "Xiaomi",
            "choices": [{
                "index": 0,
                "delta": {
                    "content": null,
                    "role": "assistant",
                    "tool_calls": [{
                        "index": 0,
                        "function": {
                            "arguments": "{\"file_path\": \"/home/luke/.claude/projects/-home-luke-projects-demo/memory/smb_clone.md\"}"
                        }
                    }]
                },
                "finish_reason": null,
                "native_finish_reason": null
            }]
        });

        assert!(super::normalize_sparse_tool_call_types(&mut value));
        assert_eq!(
            value["choices"][0]["delta"]["tool_calls"][0]["type"],
            Value::String("function".to_string())
        );
    }

    #[test]
    fn parse_chat_completion_chunk_accepts_openrouter_sparse_tool_call() {
        let payload = serde_json::json!({
            "id": "gen-1778509925-7119UkUjPTix9sGQ4vZf",
            "object": "chat.completion.chunk",
            "created": 1778509925,
            "model": "xiaomi/mimo-v2.5-pro-20260422",
            "provider": "Xiaomi",
            "choices": [{
                "index": 0,
                "delta": {
                    "content": null,
                    "role": "assistant",
                    "tool_calls": [{
                        "index": 0,
                        "function": {
                            "arguments": "{\"file_path\": \"/home/luke/.claude/projects/-home-luke-projects-demo/memory/smb_clone.md\"}"
                        }
                    }]
                },
                "finish_reason": null,
                "native_finish_reason": null
            }]
        })
        .to_string();

        let chunk = super::parse_chat_completion_chunk(&payload).expect("parse chunk");
        let tool_call = &chunk.choices[0].delta.tool_calls.as_ref().unwrap()[0];
        assert_eq!(tool_call.kind, "function");
        assert_eq!(tool_call.index, Some(0));
        assert_eq!(
            tool_call
                .function
                .arguments
                .as_ref()
                .and_then(Value::as_str),
            Some(
                "{\"file_path\": \"/home/luke/.claude/projects/-home-luke-projects-demo/memory/smb_clone.md\"}"
            )
        );
    }

    #[test]
    fn sanitize_chat_request_clears_auto_tool_choice_and_preserves_reasoning() {
        let request = ChatCompletionRequest {
            model: "grok-4".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: Some(Value::String("hello".to_string())),
                tool_call_id: None,
                name: None,
                reasoning_content: None,
                thinking: None,
                tool_calls: None,
            }],
            stream: true,
            tools: None,
            tool_choice: Some(Value::String("auto".to_string())),
            parallel_tool_calls: false,
            reasoning_effort: Some("high".to_string()),
            response_format: None,
            stream_options: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            frequency_penalty: None,
            presence_penalty: None,
            stop: None,
            extra_body: BTreeMap::new(),
            template_family: None,
            client_chat_template_kwargs: None,
        };

        let sanitized = sanitize_chat_request(request, true);

        assert_eq!(sanitized.reasoning_effort, Some("high".to_string()));
        assert_eq!(sanitized.tool_choice, None);
    }

    #[test]
    fn test_sanitize_clears_auto_when_no_tools() {
        let request = ChatCompletionRequest {
            model: "grok-4".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: Some(Value::String("hi".to_string())),
                tool_call_id: None,
                name: None,
                reasoning_content: None,
                thinking: None,
                tool_calls: None,
            }],
            stream: true,
            tools: Some(Vec::new()),
            tool_choice: Some(Value::String("auto".to_string())),
            parallel_tool_calls: false,
            reasoning_effort: None,
            response_format: None,
            stream_options: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            frequency_penalty: None,
            presence_penalty: None,
            stop: None,
            extra_body: BTreeMap::new(),
            template_family: None,
            client_chat_template_kwargs: None,
        };

        let sanitized = sanitize_chat_request(request, true);
        assert_eq!(sanitized.tool_choice, None);
    }

    #[test]
    fn test_sanitize_preserves_none_and_required_without_tools() {
        let make = |tc: &str| ChatCompletionRequest {
            model: "grok-4".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: Some(Value::String("hi".to_string())),
                tool_call_id: None,
                name: None,
                reasoning_content: None,
                thinking: None,
                tool_calls: None,
            }],
            stream: true,
            tools: None,
            tool_choice: Some(Value::String(tc.to_string())),
            parallel_tool_calls: false,
            reasoning_effort: None,
            response_format: None,
            stream_options: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            frequency_penalty: None,
            presence_penalty: None,
            stop: None,
            extra_body: BTreeMap::new(),
            template_family: None,
            client_chat_template_kwargs: None,
        };

        let sanitized_none = sanitize_chat_request(make("none"), true);
        assert_eq!(
            sanitized_none.tool_choice,
            Some(Value::String("none".to_string()))
        );

        let sanitized_required = sanitize_chat_request(make("required"), true);
        assert_eq!(
            sanitized_required.tool_choice,
            Some(Value::String("required".to_string()))
        );
    }

    #[test]
    fn test_sanitize_preserves_reasoning_effort() {
        let request = ChatCompletionRequest {
            model: "grok-4".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: Some(Value::String("hi".to_string())),
                tool_call_id: None,
                name: None,
                reasoning_content: None,
                thinking: None,
                tool_calls: None,
            }],
            stream: true,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: false,
            reasoning_effort: Some("high".to_string()),
            response_format: None,
            stream_options: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            frequency_penalty: None,
            presence_penalty: None,
            stop: None,
            extra_body: BTreeMap::new(),
            template_family: None,
            client_chat_template_kwargs: None,
        };

        let sanitized = sanitize_chat_request(request, true);
        assert_eq!(sanitized.reasoning_effort, Some("high".to_string()));
    }

    #[test]
    fn sanitize_chat_request_stringifies_structured_message_content_and_tool_args() {
        let request = ChatCompletionRequest {
            model: "grok-4".to_string(),
            messages: vec![ChatMessage {
                role: "assistant".to_string(),
                content: Some(serde_json::json!([
                    { "type": "text", "text": "hello" },
                    { "type": "text", "text": "world" }
                ])),
                tool_call_id: None,
                name: None,
                reasoning_content: None,
                thinking: None,
                tool_calls: Some(vec![crate::models::chat::ChatToolCall {
                    id: Some("call_1".to_string()),
                    index: Some(0),
                    kind: "function".to_string(),
                    function: crate::models::chat::ChatFunctionCall {
                        name: Some("echo".to_string()),
                        arguments: Some(serde_json::json!({ "value": "hi" })),
                    },
                }]),
            }],
            stream: true,
            tools: Some(Vec::new()),
            tool_choice: Some(Value::String("auto".to_string())),
            parallel_tool_calls: false,
            reasoning_effort: None,
            response_format: None,
            stream_options: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            frequency_penalty: None,
            presence_penalty: None,
            stop: None,
            extra_body: BTreeMap::new(),
            template_family: None,
            client_chat_template_kwargs: None,
        };

        let sanitized = sanitize_chat_request(request, true);

        assert_eq!(
            sanitized.messages[0].content,
            Some(Value::String("hello\nworld".to_string()))
        );
        assert_eq!(
            sanitized.messages[0]
                .tool_calls
                .as_ref()
                .expect("tool calls")[0]
                .function
                .arguments,
            Some(Value::String("{\"value\":\"hi\"}".to_string()))
        );
    }

    #[test]
    fn sanitize_null_content() {
        assert_eq!(super::sanitize_message_content(Value::Null, true), None);
    }

    #[test]
    fn sanitize_non_string_content() {
        let result = super::sanitize_message_content(Value::Bool(true), true);
        assert_eq!(result, Some(Value::String("true".to_string())));
    }

    #[test]
    fn flatten_content_parts_non_text() {
        let parts = vec![serde_json::json!({"image": "data"})];
        let result = super::flatten_content_parts(&parts);
        assert!(result.contains("image"));
        assert!(result.contains("data"));
    }

    #[test]
    fn endpoint_url_bare_domain() {
        let client = ReqwestUpstreamClient::new(
            reqwest::Client::new(),
            url::Url::parse("https://api.example.com").expect("url"),
            None,
            None,
            true,
            4096,
        );
        let url = client.endpoint_url("chat/completions").expect("endpoint");
        assert_eq!(url.as_str(), "https://api.example.com/chat/completions");
    }

    #[tokio::test]
    async fn upstream_request_logger_writes_jsonl() {
        let path = std::env::temp_dir().join(format!(
            "llmconduit-upstream-log-{}.jsonl",
            uuid::Uuid::new_v4().simple()
        ));
        let logger = UpstreamRequestLogger::new(path.clone());
        let request = sanitize_chat_request(
            ChatCompletionRequest {
                model: "grok-4".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: Some(serde_json::json!([
                        { "type": "text", "text": "hello" },
                        { "type": "text", "text": "world" }
                    ])),
                    tool_call_id: None,
                    name: None,
                    reasoning_content: None,
                    thinking: None,
                    tool_calls: None,
                }],
                stream: true,
                tools: None,
                tool_choice: Some(Value::String("auto".to_string())),
                parallel_tool_calls: false,
                reasoning_effort: Some("high".to_string()),
                response_format: None,
                stream_options: None,
                temperature: None,
                top_p: None,
                max_output_tokens: None,
                frequency_penalty: None,
                presence_penalty: None,
                stop: None,
                extra_body: BTreeMap::new(),
                template_family: None,
                client_chat_template_kwargs: None,
            },
            true,
        );

        logger.log(&request).await.expect("write request log");

        let contents = std::fs::read_to_string(&path).expect("read request log");
        assert_eq!(
            contents,
            format!(
                "{}\n",
                serde_json::to_string(&request).expect("serialize request")
            )
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_truncate_short_body_unchanged() {
        assert_eq!(super::truncate_for_error("hello", 500), "hello");
    }

    #[test]
    fn test_truncate_long_body() {
        let long = "x".repeat(1000);
        let result = super::truncate_for_error(&long, 500);
        assert!(result.ends_with("...[truncated]"));
        assert_eq!(result.len(), 500 + "...[truncated]".len());
    }

    #[test]
    fn test_truncate_unicode_safe() {
        let base = "héllo wörld ";
        let repeated: String = base.repeat(100);
        let result = super::truncate_for_error(&repeated, 50);
        assert!(result.ends_with("...[truncated]"));
        // Verify truncation happened at a char boundary by checking it's valid UTF-8
        assert_eq!(result, result.to_string());
        let prefix = result.trim_end_matches("...[truncated]");
        assert_eq!(prefix.chars().count(), 50);
    }

    #[test]
    fn test_truncate_exact_boundary() {
        let exact = "a".repeat(500);
        assert_eq!(super::truncate_for_error(&exact, 500), exact);
    }

    #[test]
    fn test_sanitize_preserves_array_when_flatten_disabled() {
        let array = serde_json::json!([
            { "type": "text", "text": "hello" },
            { "type": "image_url", "image_url": { "url": "data:image/png;base64,abc" } }
        ]);
        let result = super::sanitize_message_content(array.clone(), false);
        assert_eq!(result, Some(array));
    }

    #[test]
    fn test_sanitize_flattens_array_when_flatten_enabled() {
        let array = serde_json::json!([
            { "type": "text", "text": "hello" },
            { "type": "text", "text": "world" }
        ]);
        let result = super::sanitize_message_content(array, true);
        assert_eq!(result, Some(Value::String("hello\nworld".to_string())));
    }

    #[test]
    fn test_sanitize_preserves_multimodal_array_when_flatten_enabled() {
        let array = serde_json::json!([
            { "type": "text", "text": "look" },
            { "type": "image_url", "image_url": { "url": "data:image/png;base64,abc" } }
        ]);
        let result = super::sanitize_message_content(array.clone(), true);
        assert_eq!(result, Some(array));
    }

    #[test]
    fn test_sanitize_non_array_unchanged_regardless() {
        let text = Value::String("hello".to_string());
        assert_eq!(
            super::sanitize_message_content(text.clone(), true),
            Some(text.clone())
        );
        assert_eq!(
            super::sanitize_message_content(text.clone(), false),
            Some(text)
        );
    }

    #[test]
    fn extract_supported_model_ids_reads_standard_models_list() {
        let body = serde_json::json!({
            "object": "list",
            "data": [
                {"id": "glm-5.1"},
                {"id": "Qwen3.5"},
                "grok-4"
            ]
        });

        assert_eq!(
            extract_supported_model_ids(&body),
            vec!["glm-5.1", "Qwen3.5", "grok-4"]
        );
    }

    #[test]
    fn extract_supported_model_ids_reads_models_array() {
        let body = serde_json::json!({"models": ["glm-5.1"]});
        assert_eq!(extract_supported_model_ids(&body), vec!["glm-5.1"]);
    }
}
