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

/// One entry of the upstream `/v1/models` catalog: the model id plus its
/// context-window length (`None` when the upstream reports no positive context
/// length for it). Ids and context limits are derived from a SINGLE
/// `/v1/models` snapshot so they always describe the same provider/state (G3:
/// a separate context-limit fetch could otherwise pair one provider's ids with
/// another's limits under failover).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamModelEntry {
    pub id: String,
    pub context_limit: Option<i64>,
}

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
    /// The upstream model catalog (ids + per-model context length) from a single
    /// `/v1/models` snapshot. The default impl fetches `list_models()` once and
    /// parses both the id list and the context-window length per entry, so model
    /// routing/normalization and G3 pre-flight context budgeting always read a
    /// consistent provider state. Clients whose `list_models()` is unavailable
    /// surface the error; entries without a positive context length carry
    /// `context_limit: None` (budgeting no-ops for them).
    async fn supported_model_catalog(&self) -> AppResult<Vec<UpstreamModelEntry>> {
        let response = self.list_models().await?;
        collect_supported_model_catalog(response).await
    }

    /// Every backend model `requested_model` could ACTUALLY be served by
    /// pre-first-chunk, after any routing/route/exposed-alias + per-provider
    /// `upstream_model` rewrite (G4 review #2 + round-2 #1). This enumerates the
    /// full candidate set — the primary AND all eligible failover providers (and
    /// the routing target) — because failover happens before the first chunk, so
    /// the model that ultimately serves may be any of them. Native-vision gating
    /// uses the SAFE invariant over this set (passthrough only if EVERY candidate
    /// is native-vision), so it can never disagree with the provider that serves.
    ///
    /// The default impl (single `ReqwestUpstreamClient`) sends the request model
    /// unchanged, so the only candidate is `requested_model` itself.
    async fn candidate_backend_models(&self, requested_model: &str) -> Vec<String> {
        vec![requested_model.to_string()]
    }
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
    /// Per-frame byte ceiling for the upstream SSE read path (G6 DoS guard).
    /// Bounds bytes accumulated between event boundaries so a hostile/buggy
    /// upstream cannot grow the parser buffer without bound; an oversized or
    /// unterminated frame is rejected as an `AppError`.
    max_sse_frame_bytes: usize,
    /// Per-backend-model reasoning-effort policies (keyed by resolved model id).
    /// Applied at this leaf because it is the single point that sees the FINAL
    /// provider model after routing/failover/exposed-alias remap. Shared (cheap
    /// clone) across all providers; empty when no profile defines a map.
    effort_policies: Arc<std::collections::BTreeMap<String, crate::config::ReasoningEffortPolicy>>,
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

    /// The effective backend model of one of this routing provider's nested
    /// failover providers (its `upstream_model` rewrite, if any). Used by G4
    /// native-vision gating for an exposed-alias/fallback target that serves from
    /// exactly one provider. `None` when the index is out of range or the
    /// provider sends the request model through unchanged.
    fn failover_provider_model(&self, failover_provider_index: usize) -> Option<String> {
        self.client.provider_upstream_model(failover_provider_index)
    }
}

/// A synthetic upstream backing one or more ad-hoc model routes (G7). Unlike a
/// catalog provider, a route provider is matched by request-model *name* (in
/// `ModelRouteSpec`), never enumerated into the `/v1/models` union, so routes
/// stay invisible to the model listing exactly like claude-relay's
/// `model_routes`.
#[derive(Debug, Clone)]
pub struct RouteUpstreamProvider {
    name: String,
    client: FailoverUpstreamClient,
}

impl RouteUpstreamProvider {
    pub fn new(name: impl Into<String>, client: ReqwestUpstreamClient, cooldown: Duration) -> Self {
        let name = name.into();
        Self {
            name: name.clone(),
            client: FailoverUpstreamClient::new(
                vec![FailoverUpstreamProvider::new(
                    name,
                    client,
                    None,
                    None,
                    JsonMap::new(),
                )],
                cooldown,
            ),
        }
    }
}

/// A compiled ad-hoc model route (G7): a request-model name/glob that maps to a
/// `RouteUpstreamProvider` and an optional upstream-model rewrite.
#[derive(Debug, Clone)]
pub struct ModelRouteSpec {
    /// Request-model name this route matches (literal or glob source).
    name: String,
    /// Compiled glob matcher (case-insensitive). `None` for a literal name,
    /// which is compared with `eq_ignore_ascii_case`.
    glob: Option<Regex>,
    /// Index into `RoutingUpstreamClient::route_providers`.
    route_provider_index: usize,
    /// Upstream model to send. `None` passes the request model through.
    upstream_model: Option<String>,
}

impl ModelRouteSpec {
    /// Build a route spec. `glob` is the pre-compiled matcher (from
    /// `config::ModelRoute`); `route_provider_index` indexes the matching
    /// `RouteUpstreamProvider`.
    pub fn new(
        name: impl Into<String>,
        glob: Option<Regex>,
        route_provider_index: usize,
        upstream_model: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            glob,
            route_provider_index,
            upstream_model,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RoutingUpstreamClient {
    providers: Vec<RoutingUpstreamProvider>,
    /// Synthetic providers backing ad-hoc model routes (G7), indexed by
    /// `ModelRouteSpec::route_provider_index`. Kept separate from catalog
    /// `providers` so routes never enter the `/v1/models` union.
    route_providers: Vec<RouteUpstreamProvider>,
    routes: Vec<ModelRouteSpec>,
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
    /// Ad-hoc routes (G7), cloned from the client each refresh. Matched purely
    /// by request-model name, independent of the live catalog, so routes still
    /// resolve when an upstream `/v1/models` fetch is unavailable.
    routes: Vec<ModelRouteSpec>,
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

/// Outcome of resolving a request model: either a catalog candidate (served by a
/// `RoutingUpstreamProvider`) or an ad-hoc route candidate (served by a
/// `RouteUpstreamProvider`). Routes slot strictly between an exact catalog id
/// and the canonical-key/default fallbacks (G7), so an exact model id always
/// beats a glob route.
#[derive(Debug, Clone)]
enum RoutingResolution {
    Catalog(RoutingModelCandidate),
    Route {
        route_provider_index: usize,
        model_id: String,
    },
}

#[derive(Debug, Clone)]
enum RoutingModelTarget {
    Primary,
    Fallback { failover_provider_index: usize },
}

/// Which `RoutingModelCatalog::resolve` rule matched a request model. Carried
/// out to the dispatch site purely for diagnostics: a `Default` match means the
/// requested model was NOT served by any upstream and we fell back to the first
/// catalog model — worth a WARN so an operator notices a model-name mismatch
/// (e.g. the loaded vLLM model differs from what clients request). The rest are
/// expected and log at INFO or below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchKind {
    /// Rule 1: exact catalog model id.
    ExactId,
    /// Rules 2-3: ad-hoc route name or glob.
    Route,
    /// Rule 4: unique canonical-key catalog match (case/punctuation normalized).
    CanonicalKey,
    /// Rule 5: no match; fell back to the first catalog model.
    Default,
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

/// Cheap case-insensitive scan for a potential image-URI marker (`data:` /
/// `http`) in serialized request bytes, so the log fast-path only pays the
/// redaction round-trip when an image URL might be present (G4 round-4 #3).
fn bytes_contain_image_uri(bytes: &[u8]) -> bool {
    bytes.windows(5).any(|w| w.eq_ignore_ascii_case(b"data:"))
        || bytes.windows(4).any(|w| w.eq_ignore_ascii_case(b"http"))
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
        // G4 round-4 #3: the JSONL request log is written to DISK and would
        // otherwise serialize raw `data:` image bytes / signed `image_url`s for
        // native-vision-passthrough / disabled-agent / missing-url /
        // tool_choice:"none" image requests (any path that does NOT strip). The
        // common no-image request serializes directly (format unchanged); only
        // when the serialized bytes contain an image-URI marker do we re-redact
        // via a CLONED JSON value through the shared redactor, so the on-disk log
        // never carries request image content.
        let mut payload = serde_json::to_vec(request).map_err(std::io::Error::other)?;
        if bytes_contain_image_uri(&payload) {
            let mut value = serde_json::to_value(request).map_err(std::io::Error::other)?;
            crate::vision::redact_image_uris_in_value(&mut value);
            payload = serde_json::to_vec(&value).map_err(std::io::Error::other)?;
        }
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
        Self::with_options(
            client,
            base_url,
            api_key,
            request_log_path,
            flatten_content,
            min_completion_tokens,
            default_max_sse_frame_bytes(),
        )
    }

    /// Construct with an explicit upstream SSE per-frame cap (G6). The simpler
    /// `new` delegates here with the default ceiling; callers that need to
    /// thread a configured/lowered cap (e.g. the DI root or a test) use this.
    #[allow(clippy::too_many_arguments)]
    pub fn with_options(
        client: reqwest::Client,
        base_url: Url,
        api_key: Option<String>,
        request_log_path: Option<PathBuf>,
        flatten_content: bool,
        min_completion_tokens: i64,
        max_sse_frame_bytes: usize,
    ) -> Self {
        Self {
            client,
            base_url,
            api_key,
            request_logger: request_log_path.map(UpstreamRequestLogger::new),
            flatten_content,
            min_completion_tokens: min_completion_tokens.max(1),
            max_sse_frame_bytes: max_sse_frame_bytes.max(1024),
            effort_policies: Arc::new(std::collections::BTreeMap::new()),
        }
    }

    /// Attach the per-backend-model reasoning-effort policies (built once from
    /// config). Threaded post-construction so the existing `with_options`
    /// signature is unchanged.
    pub fn with_effort_policies(
        mut self,
        policies: Arc<std::collections::BTreeMap<String, crate::config::ReasoningEffortPolicy>>,
    ) -> Self {
        self.effort_policies = policies;
        self
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
        // Strip the engine's reserved raw-effort marker BEFORE family injection,
        // logging, and the POST so it never reaches the wire, then apply this
        // FINAL model's reasoning_effort_map.
        let raw_effort = request
            .extra_body
            .remove(CANONICAL_REASONING_EFFORT_KEY)
            .and_then(|value| value.as_str().map(str::to_string));
        let effort_fragment =
            reasoning_effort_fragment(&self.effort_policies, &request.model, raw_effort.as_deref());
        // When the map places effort (in chat_template_kwargs), CLEAR the
        // top-level `reasoning_effort` BEFORE family injection: a mapped backend
        // reads the template knob, so a leftover top-level value would either be
        // ignored (GLM) or, worse, seed a CONTRADICTORY value via the DeepSeek
        // family setdefault. G3's estimate omits this field, so clearing it keeps
        // the pre-flight estimate a safe lower bound.
        if effort_fragment.is_some() {
            request.reasoning_effort = None;
        }
        apply_family_chat_template_kwargs(&mut request);
        if let Some(fragment) = effort_fragment {
            // Applied AFTER family so the map overrides family defaults; client
            // kwargs are re-asserted last inside the apply.
            apply_reasoning_effort_fragment(&mut request, &fragment);
        }
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
            return stream_success_response(response, self.max_sse_frame_bytes).await;
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
                return stream_success_response(retry_response, self.max_sse_frame_bytes).await;
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
                    redact_and_truncate_error_body(&retry_body, 500)
                )));
            }
            return Err(AppError::upstream(format!(
                "upstream chat failed with {retry_status}: {}",
                redact_and_truncate_error_body(&retry_body, 500)
            )));
        }

        Err(AppError::upstream(format!(
            "upstream chat failed with {status}: {}",
            redact_and_truncate_error_body(&body, 500)
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
                "upstream /models failed with {status}: {}",
                redact_and_truncate_error_body(&body, 500)
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

    /// The configured `upstream_model` rewrite of provider `index`, if any.
    /// `None` when out of range or the provider sends the request model
    /// unchanged. Used by G4 native-vision gating (round-2 #1).
    fn provider_upstream_model(&self, index: usize) -> Option<String> {
        self.providers
            .get(index)
            .and_then(|provider| provider.upstream_model.clone())
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
        states
            .get(provider_index)
            .and_then(|state| state.cooling_until)
            .is_none_or(|until| until <= now)
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
                        "upstream completions failed with {status}: {}",
                        redact_and_truncate_error_body(&body, 500)
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
        Self::with_routes(providers, Vec::new(), Vec::new())
    }

    /// Construct with ad-hoc model routes (G7). `route_providers` are the
    /// synthetic upstreams; `routes` map request-model names/globs to them and
    /// must reference valid `route_provider_index` values.
    pub fn with_routes(
        providers: Vec<RoutingUpstreamProvider>,
        route_providers: Vec<RouteUpstreamProvider>,
        routes: Vec<ModelRouteSpec>,
    ) -> Self {
        Self {
            providers,
            route_providers,
            routes,
            catalog: Arc::new(AsyncMutex::new(None)),
        }
    }

    /// Look up a synthetic route provider by index, mapping an out-of-range
    /// index (a wiring bug) to an internal error rather than a panic.
    fn route_provider(&self, index: usize) -> AppResult<&RouteUpstreamProvider> {
        self.route_providers
            .get(index)
            .ok_or_else(|| AppError::internal("resolved route provider index was out of range"))
    }

    /// Clone the request and apply the resolved upstream-model rewrite, logging
    /// once when the model actually changes. Shared by catalog and route
    /// dispatch so both honor the same rewrite + log behavior.
    fn routed_request(
        &self,
        request: &ChatCompletionRequest,
        model_id: &str,
        provider_name: &str,
        kind: MatchKind,
    ) -> ChatCompletionRequest {
        let mut routed_request = request.clone();
        if routed_request.model != model_id {
            log_model_resolution(&routed_request.model, model_id, provider_name, kind);
            routed_request.model = model_id.to_string();
        }
        routed_request
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

        // With ad-hoc routes (G7), an empty union is still a usable catalog:
        // routes resolve by name without a live model listing. Only error when
        // there are neither catalog models nor routes to dispatch to.
        if union_ids.is_empty() && self.routes.is_empty() {
            return Err(last_error.unwrap_or_else(|| {
                AppError::upstream("no models are currently available from configured upstreams")
            }));
        }

        Ok(RoutingModelCatalog {
            provider_catalogs,
            union_entries,
            union_ids,
            ids_by_key,
            routes: self.routes.clone(),
        })
    }
}

impl RoutingModelCatalog {
    /// Resolve a request model to a dispatch target. Precedence (G7):
    /// 1. exact catalog model id (an exact id always wins),
    /// 2. exact ad-hoc route name,
    /// 3. glob ad-hoc route name (first match wins on overlap),
    /// 4. unique canonical-key catalog match,
    /// 5. default (first model of the first non-empty provider catalog).
    ///
    /// Routes therefore slot strictly between an exact id and the
    /// canonical/default fallbacks; a glob never overrides an exact match.
    fn resolve(&self, requested_model: &str) -> Option<(RoutingResolution, MatchKind)> {
        let trimmed = requested_model.trim();
        if !trimmed.is_empty() {
            // 1. Exact catalog model id.
            for (provider_index, provider) in self.provider_catalogs.iter().enumerate() {
                if let Some(candidate) = provider
                    .candidates
                    .iter()
                    .find(|candidate| candidate.model_id == trimmed)
                {
                    debug_assert_eq!(candidate.provider_index, provider_index);
                    return Some((
                        RoutingResolution::Catalog(candidate.clone()),
                        MatchKind::ExactId,
                    ));
                }
            }

            // 2. Exact ad-hoc route name (case-insensitive), then 3. glob route.
            if let Some(route) = self.match_route(trimmed) {
                return Some((
                    RoutingResolution::Route {
                        route_provider_index: route.route_provider_index,
                        model_id: route
                            .upstream_model
                            .clone()
                            .unwrap_or_else(|| trimmed.to_string()),
                    },
                    MatchKind::Route,
                ));
            }

            // 4. Unique canonical-key catalog match.
            let key = canonical_model_key(trimmed);
            if let Some(candidates) = self.ids_by_key.get(&key)
                && let Some(model_id) = unique_candidate_model_id(candidates)
            {
                return candidates
                    .iter()
                    .find(|candidate| candidate.model_id == model_id)
                    .cloned()
                    .map(|candidate| {
                        (
                            RoutingResolution::Catalog(candidate),
                            MatchKind::CanonicalKey,
                        )
                    });
            }
        }

        // 5. Default catalog candidate.
        self.default_candidate()
            .map(|candidate| (RoutingResolution::Catalog(candidate), MatchKind::Default))
    }

    /// Match a request model against ad-hoc routes: an exact name (case
    /// insensitive) beats any glob; among globs the first declared wins.
    fn match_route(&self, requested_model: &str) -> Option<&ModelRouteSpec> {
        if let Some(route) = self
            .routes
            .iter()
            .find(|route| route.glob.is_none() && route.name.eq_ignore_ascii_case(requested_model))
        {
            return Some(route);
        }
        self.routes.iter().find(|route| {
            route
                .glob
                .as_ref()
                .is_some_and(|glob| glob.is_match(requested_model))
        })
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

/// Log a request-model rewrite at a level reflecting WHY it happened. A
/// `Default` match means the requested model was not served by any upstream —
/// the operator likely loaded a different model than clients ask for, so it goes
/// to WARN. Expected normalizations (canonical-key) and ad-hoc routes stay at
/// INFO. Callers invoke this only when the model actually changed.
fn log_model_resolution(requested: &str, resolved: &str, provider: &str, kind: MatchKind) {
    if requested == resolved {
        return;
    }
    if kind == MatchKind::Default {
        tracing::warn!(
            requested_model = %requested,
            routed_model = %resolved,
            provider = %provider,
            "requested model is not served by any configured upstream; falling back to the default catalog model"
        );
    } else {
        tracing::info!(
            requested_model = %requested,
            routed_model = %resolved,
            provider = %provider,
            "routed request model to upstream catalog model"
        );
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

    /// Every configured provider is a pre-first-chunk failover candidate, so the
    /// candidate model set is each provider's effective model — its
    /// `upstream_model` rewrite (matching `request_for_provider`) or the request
    /// model when it sends through unchanged (G4 round-2 #1). We enumerate ALL
    /// providers (not just currently-available ones): cooldown is transient, so
    /// a fallback that is non-native must still force strip+offload.
    async fn candidate_backend_models(&self, requested_model: &str) -> Vec<String> {
        self.providers
            .iter()
            .map(|provider| {
                provider
                    .upstream_model
                    .clone()
                    .unwrap_or_else(|| requested_model.to_string())
            })
            .collect()
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

    /// Candidate backend models for `requested_model` using the SAME route/
    /// catalog resolution as `stream_chat_completion` (G4 review #2 + round-2
    /// #1). A route resolves to one backend model. A catalog match dispatches to
    /// a routing provider: the PRIMARY target may fail over across that
    /// provider's whole failover chain (so all of its candidate models count),
    /// while a fallback/exposed-alias target serves only that single provider's
    /// model. A catalog-load failure yields an empty set, which the safe
    /// invariant treats as "unknown → strip+offload".
    async fn candidate_backend_models(&self, requested_model: &str) -> Vec<String> {
        let Ok(catalog) = self.load_catalog().await else {
            return Vec::new();
        };
        let Some((resolution, _kind)) = catalog.resolve(requested_model) else {
            return Vec::new();
        };
        match resolution {
            RoutingResolution::Route { model_id, .. } => vec![model_id],
            RoutingResolution::Catalog(candidate) => {
                let Some(provider) = self.providers.get(candidate.provider_index) else {
                    return Vec::new();
                };
                match candidate.target {
                    // Primary: the whole nested failover chain may serve.
                    RoutingModelTarget::Primary => {
                        provider
                            .client
                            .candidate_backend_models(&candidate.model_id)
                            .await
                    }
                    // Fallback/exposed-alias: only this one provider serves.
                    RoutingModelTarget::Fallback {
                        failover_provider_index,
                    } => provider
                        .failover_provider_model(failover_provider_index)
                        .map(|model| vec![model])
                        .unwrap_or_else(|| vec![candidate.model_id.clone()]),
                }
            }
        }
    }

    async fn stream_chat_completion_with_timeout(
        &self,
        request: &ChatCompletionRequest,
        request_timeout: Duration,
    ) -> AppResult<UpstreamStream> {
        let catalog = self.load_catalog().await.map_err(|err| {
            tracing::warn!(
                requested_model = %request.model,
                error = %err,
                "failed to load upstream model catalog (is the backend reachable?)"
            );
            err
        })?;
        let (resolution, match_kind) = catalog.resolve(&request.model).ok_or_else(|| {
            tracing::warn!(
                requested_model = %request.model,
                "upstream model catalog is empty; no model to serve (is the backend serving any model?)"
            );
            AppError::upstream("no models are currently available from configured upstreams")
        })?;
        if let RoutingResolution::Route {
            route_provider_index,
            model_id,
        } = &resolution
        {
            let provider = self.route_provider(*route_provider_index)?;
            let routed_request = self.routed_request(request, model_id, &provider.name, match_kind);
            return provider
                .client
                .stream_chat_completion_with_timeout(&routed_request, request_timeout)
                .await;
        }
        let RoutingResolution::Catalog(resolution) = resolution else {
            unreachable!("route resolution handled above");
        };
        let provider = self
            .providers
            .get(resolution.provider_index)
            .ok_or_else(|| {
                AppError::internal("resolved upstream provider index was out of range")
            })?;
        let routed_request =
            self.routed_request(request, &resolution.model_id, &provider.name, match_kind);
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
        let (resolution, match_kind) = catalog.resolve(&requested_model).ok_or_else(|| {
            tracing::warn!(
                requested_model = %requested_model,
                "upstream model catalog is empty; no model to serve (is the backend serving any model?)"
            );
            AppError::upstream("no models are currently available from configured upstreams")
        })?;
        if let RoutingResolution::Route {
            route_provider_index,
            model_id,
        } = &resolution
        {
            let provider = self.route_provider(*route_provider_index)?;
            log_model_resolution(&requested_model, model_id, &provider.name, match_kind);
            let body = proxy_body_with_model(body, model_id);
            return provider.client.proxy_completions(headers, body).await;
        }
        let RoutingResolution::Catalog(resolution) = resolution else {
            unreachable!("route resolution handled above");
        };
        let provider = self
            .providers
            .get(resolution.provider_index)
            .ok_or_else(|| {
                AppError::internal("resolved upstream provider index was out of range")
            })?;
        log_model_resolution(
            &requested_model,
            &resolution.model_id,
            &provider.name,
            match_kind,
        );
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

    async fn supported_model_catalog(&self) -> AppResult<Vec<UpstreamModelEntry>> {
        // Build from the cached union catalog directly (single snapshot) rather
        // than re-serializing `union_body()` through the default `list_models()`
        // path: the union ids are authoritative, and any context length is read
        // from the same merged entries.
        let catalog = self.load_catalog().await?;
        let limit_by_id: HashMap<String, i64> =
            extract_model_context_limits(&Value::Array(catalog.union_entries.clone()))
                .into_iter()
                .collect();
        Ok(catalog
            .union_ids
            .into_iter()
            .map(|id| {
                let context_limit = limit_by_id.get(&id).copied();
                UpstreamModelEntry { id, context_limit }
            })
            .collect())
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

/// Reserved `extra_body` key carrying the RAW canonical reasoning-effort level
/// (none/low/medium/high/xhigh/max) from the engine to this leaf. The leaf reads
/// it to apply the FINAL model's `reasoning_effort_map`, then REMOVES it so it
/// never reaches the wire or the request log.
pub const CANONICAL_REASONING_EFFORT_KEY: &str = "__llmconduit_canonical_reasoning_effort";

/// Resolve the reasoning-effort fragment for the FINAL provider `model` and the
/// raw client effort level, or `None` when the model has no policy or the level
/// (after defaulting) is not mapped. Lookup is exact, then canonical-key
/// (case/punctuation-insensitive), mirroring catalog/profile model matching.
fn reasoning_effort_fragment(
    policies: &std::collections::BTreeMap<String, crate::config::ReasoningEffortPolicy>,
    model: &str,
    raw_effort: Option<&str>,
) -> Option<Value> {
    let policy = policies.get(model).or_else(|| {
        // No exact id match: fall back to a canonical-key match, but ONLY when it
        // is unambiguous. Two profile names sharing a canonical key would make the
        // pick order-dependent, so require exactly one and otherwise apply no
        // policy (deterministic; the engine keeps its clamped top-level effort).
        let key = canonical_model_key(model);
        let mut matches = policies
            .iter()
            .filter(|(name, _)| canonical_model_key(name) == key)
            .map(|(_, policy)| policy);
        match (matches.next(), matches.next()) {
            (Some(policy), None) => Some(policy),
            _ => None,
        }
    })?;
    let level = raw_effort
        .map(str::trim)
        .filter(|level| !level.is_empty())
        .map(str::to_ascii_lowercase)
        .or_else(|| policy.default.clone())?;
    policy.map.get(&level).cloned()
}

/// Apply a resolved reasoning-effort `fragment` to the request at the leaf, with
/// precedence config < family < effort-map < client. The fragment is deep-merged
/// PREFER-SOURCE into `extra_body` (so it OVERRIDES configured/family defaults
/// already present in `chat_template_kwargs`, fixing the case where a static
/// configured default would otherwise block a per-request mapping), then the
/// client's own explicit `chat_template_kwargs` are re-asserted so an inbound
/// client value still wins. Call AFTER `apply_family_chat_template_kwargs`.
pub fn apply_reasoning_effort_fragment(request: &mut ChatCompletionRequest, fragment: &Value) {
    let Value::Object(fragment) = fragment else {
        return;
    };
    for (key, value) in fragment {
        match request.extra_body.get_mut(key) {
            Some(existing) => merge_json_value_prefer_source(existing, value),
            None => {
                request.extra_body.insert(key.clone(), value.clone());
            }
        }
    }
    // Re-assert the client's explicit chat_template_kwargs over the effort-map
    // fragment (precedence: client > effort-map).
    if let Some(client_kwargs) = &request.client_chat_template_kwargs
        && let Some(Value::Object(kwargs)) = request.extra_body.get_mut("chat_template_kwargs")
    {
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
async fn stream_success_response(
    response: reqwest::Response,
    max_sse_frame_bytes: usize,
) -> AppResult<UpstreamStream> {
    // Bound the upstream SSE read BEFORE `eventsource()` parses it (G6). The
    // `eventsource-stream` 0.2 parser accumulates bytes into an internal
    // `String`/`Vec` buffer and only flushes on an event boundary (a blank
    // line); a hostile/buggy upstream streaming an oversized or never-terminated
    // frame would grow that buffer without bound (OOM). `bounded_sse_byte_stream`
    // caps the bytes accumulated since the last boundary and surfaces a clean
    // `AppError` before the parser can over-accumulate. The 256 MiB request-body
    // cap in `http.rs` is inbound-only and does NOT cover this response path.
    let bounded = bounded_sse_byte_stream(response.bytes_stream(), max_sse_frame_bytes);
    let stream = bounded.eventsource().filter_map(|result| async move {
        match result {
            Ok(event) if event.data == "[DONE]" => None,
            Ok(event) => Some(parse_chat_completion_chunk(&event.data).map_err(|err| {
                AppError::upstream(format!(
                    "failed to parse upstream chat chunk: {err}; payload={}",
                    redact_and_truncate_error_body(&event.data, 500)
                ))
            })),
            // The bounded adapter surfaces the frame-cap rejection through the
            // transport-error channel as an already-formed `AppError` (its
            // `Display` carries the cap message); other transport errors are
            // wrapped here. Either way the model output is never silently
            // truncated — the stream ends in an error item.
            Err(err) => Some(Err(AppError::upstream(format!(
                "failed to read upstream SSE: {err}"
            )))),
        }
    });
    Ok(Box::pin(stream))
}

/// 8 MiB default upstream SSE per-frame ceiling. Mirrors
/// `config::default_max_sse_frame_bytes`; kept here so `ReqwestUpstreamClient::new`
/// (which does not take a cap) has a sane default without depending on config.
pub(crate) fn default_max_sse_frame_bytes() -> usize {
    8 * 1024 * 1024
}

/// Pure, synchronous per-frame byte-accounting guard for the upstream SSE read
/// (G6). Feed it each incoming byte chunk in order; it tracks the number of
/// bytes accumulated **since the last SSE event boundary** and returns an
/// `AppError` the moment that running count exceeds the cap — i.e. as soon as an
/// oversized or never-terminated (no blank-line) frame would force the
/// downstream `eventsource-stream` parser to over-buffer.
///
/// Kept pure (no async, no `reqwest`) so it is unit/integration-testable with
/// raw byte slices; `bounded_sse_byte_stream` is the thin async wrapper that
/// drives it over a real `bytes_stream()`.
#[derive(Debug)]
pub struct SseFrameGuard {
    max_frame_bytes: usize,
    /// INVARIANT: `since_boundary` = bytes of the current in-progress frame that
    /// are CONFIRMED not part of a pending boundary (every such byte counted
    /// exactly once). `carry` = the trailing <=3 bytes of the stream so far that
    /// form a (possibly empty) PREFIX of an SSE boundary and are therefore NOT
    /// yet charged: on the next chunk they either complete a boundary (→ reset,
    /// never charged) or are disambiguated as ordinary frame bytes (→ charged
    /// then). Holding the ambiguous tail uncharged is what makes the verdict
    /// chunking-INDEPENDENT.
    since_boundary: usize,
    /// Deferred boundary-prefix tail (`\n`, `\r`, `\r\n`, or `\r\n\r`). Fixed tiny
    /// window — never grows beyond 3 bytes; uncharged until disambiguated.
    carry: Vec<u8>,
    /// Set when the stream is currently INSIDE a maximal run of consecutive EOLs
    /// that began at a completed frame boundary (Codex round-4 LOW). After a blank
    /// line (boundary = two EOLs) any ADDITIONAL consecutive EOLs are extra empty
    /// lines that `eventsource-stream` dispatches as empty events / skips, so they
    /// belong to NO data frame and must be charged to neither. When this is true,
    /// a leading EOL on the next chunk continues that empty-line run (consumed,
    /// uncharged) rather than being charged into the next frame; the `carry` then
    /// holds the run's trailing partial EOL (a lone `\r` whose CR/CRLF nature is
    /// still ambiguous) instead of a boundary-prefix. Cleared the moment a
    /// non-EOL byte (real frame content) ends the run.
    in_eol_run: bool,
}

impl SseFrameGuard {
    /// Build a guard with the given per-frame ceiling (floored at 1 KiB so a
    /// misconfigured tiny cap cannot reject every normal frame).
    ///
    /// `in_eol_run` starts TRUE: the stream begins AS IF immediately after a frame
    /// boundary, so any LEADING EOLs (an empty/blank-line SSE event, or stray
    /// separators before the first `data:`) are an empty-line run charged to NO
    /// frame — exactly like extra blank lines BETWEEN frames (Codex round-4 LOW).
    /// Starting false instead charged a leading EOL into the first real frame, a
    /// false reject for a frame otherwise exactly at cap (Codex round-5 LOW). A
    /// stream that opens directly on real content ends the (zero-length) leading run
    /// at byte 0, so this is a no-op for the common case.
    pub fn new(max_frame_bytes: usize) -> Self {
        Self {
            max_frame_bytes: max_frame_bytes.max(1024),
            since_boundary: 0,
            carry: Vec::new(),
            in_eol_run: true,
        }
    }

    /// The effective (floored) per-frame cap this guard enforces.
    pub fn max_frame_bytes(&self) -> usize {
        self.max_frame_bytes
    }

    /// Account for one incoming chunk. Returns `Err` the moment ANY single SSE
    /// frame — the bytes between two boundaries, terminated or not — would exceed
    /// the cap (the caller must stop and surface the error — never silently
    /// truncate). Each boundary resets the running count, so a well-formed (even
    /// large-but-bounded-per-event) stream always passes.
    ///
    /// The scan searches `carry + chunk` (so a boundary straddling the chunk edge
    /// is detected). `carry` is the previously-DEFERRED boundary-prefix tail
    /// (uncharged), so the scan both charges it (when it turns out to be ordinary
    /// frame bytes) and re-derives a fresh deferred tail — all in one pass. This
    /// keeps the verdict independent of how the stream is split into chunks.
    /// See [`scan_frames_since_boundary`].
    pub fn accept(&mut self, chunk: &[u8]) -> Result<(), AppError> {
        self.scan(chunk, false)
    }

    /// Finalize accounting when the upstream byte stream ENDS. Any bytes still held
    /// in the deferred boundary-prefix carry could not be completed into a frame
    /// boundary (no more bytes will arrive), so a dangling single EOL is charged as
    /// part of the still-open, unterminated frame and a final cap check is emitted
    /// (Codex round-3 Finding 2: an unterminated frame must not slip past the cap by
    /// a trailing `\n`/`\r`/`\r\n`/`\r\n\r` just because EOF arrived before the carry
    /// was disambiguated). A trailing carry that is itself a complete boundary
    /// (`\n\r`, `\r\r`, `\r\n\r`, resolving the final CR at EOF) resets instead of
    /// being charged. Idempotent: after a successful call the carry is empty, so a
    /// second call is a no-op.
    pub fn finish(&mut self) -> Result<(), AppError> {
        self.scan(&[], true)
    }

    fn scan(&mut self, chunk: &[u8], at_eof: bool) -> Result<(), AppError> {
        let ScanState {
            since_boundary,
            carry,
            in_eol_run,
        } = scan_frames_since_boundary(
            ScanState {
                since_boundary: self.since_boundary,
                carry: std::mem::take(&mut self.carry),
                in_eol_run: self.in_eol_run,
            },
            chunk,
            self.max_frame_bytes,
            at_eof,
        )
        .map_err(|observed| {
            AppError::upstream(format!(
                "upstream SSE frame exceeded {} bytes before an event boundary \
                         (saw {observed}); rejecting to bound memory (G6)",
                self.max_frame_bytes
            ))
        })?;
        self.since_boundary = since_boundary;
        self.carry = carry;
        self.in_eol_run = in_eol_run;
        Ok(())
    }
}

/// Wrap an upstream byte stream so the bytes accumulated **between SSE event
/// boundaries** never exceed `max_frame_bytes` before being handed to the
/// `eventsource-stream` parser (G6 DoS guard).
///
/// SSE events are separated by a blank line (`\n\n`, `\r\n\r\n`, or `\r\r`). The
/// `eventsource-stream` parser buffers everything it receives until it sees such
/// a separator, so the only thing that can grow its buffer without bound is a
/// frame that never terminates (or a single oversized frame). The
/// [`SseFrameGuard`] tracks the byte count since the last separator and we reject
/// as soon as it exceeds the cap — *before* forwarding the offending chunk — so
/// the downstream parser buffer is itself bounded by `max_frame_bytes` (plus one
/// in-flight chunk).
///
/// The rejection is yielded as a `std::io::Error` so it travels the transport
/// (`EventStreamError::Transport`) channel of `eventsource()`; its message is the
/// `AppError`'s, and `stream_success_response` re-wraps it into an `AppError`.
/// Normal-sized streaming is untouched: each well-formed event resets the
/// counter at its boundary.
///
/// On stream END the adapter FINALIZES the guard ([`SseFrameGuard::finish`]):
/// any pending boundary-prefix carry is charged and a final cap check is emitted,
/// so an unterminated over-cap frame is rejected even if EOF arrives before a
/// trailing separator byte could be disambiguated (Codex round-3 Finding 2). This
/// is why a plain `.map` is insufficient — the adapter must be able to act on
/// end-of-stream — so it is a stateful `async_stream` that drives the guard and
/// emits one trailing error item when finalization trips the cap.
///
/// Cancellation is preserved: this is a lazy stream adapter that only advances
/// when polled. The caller's `tx.closed()`/timeout selects still cancel the
/// whole chain by dropping it; nothing here blocks or spawns. The raw `*.delta`
/// path and AppError-not-truncation contract are unchanged: every rejection still
/// travels the transport-error channel as an `std::io::Error` whose message is the
/// `AppError`'s, which `stream_success_response` re-wraps — output is never
/// silently truncated.
pub(crate) fn bounded_sse_byte_stream<S, B>(
    stream: S,
    max_frame_bytes: usize,
) -> impl Stream<Item = Result<Bytes, std::io::Error>>
where
    S: Stream<Item = Result<B, reqwest::Error>>,
    B: AsRef<[u8]>,
{
    async_stream::stream! {
        let mut guard = SseFrameGuard::new(max_frame_bytes);
        let mut stream = std::pin::pin!(stream);
        while let Some(result) = stream.next().await {
            let bytes = match result {
                Ok(bytes) => Bytes::copy_from_slice(bytes.as_ref()),
                Err(err) => {
                    yield Err(std::io::Error::other(format!(
                        "failed to read upstream SSE bytes: {err}"
                    )));
                    return;
                }
            };
            // Reject BEFORE forwarding so the parser never sees the over-cap bytes.
            if let Err(err) = guard.accept(bytes.as_ref()) {
                yield Err(std::io::Error::other(err.to_string()));
                return;
            }
            yield Ok(bytes);
        }
        // Upstream ended: charge any deferred carry and cap-check the (possibly
        // unterminated) final frame. A clean end stays clean; an over-cap dangling
        // frame surfaces as a trailing transport error rather than a silent EOF.
        if let Err(err) = guard.finish() {
            yield Err(std::io::Error::other(err.to_string()));
        }
    }
}

/// Length of the EOL token starting at `buf[i]`, tokenized **exactly like**
/// `eventsource-stream`'s `end-of-line = ( cr lf / cr / lf )` (CRLF matched
/// greedily, longest-first). Returns:
///   * `EolToken::Complete(len)` — a fully-determined EOL of `len` bytes;
///   * `EolToken::IncompleteCr` — `buf[i]` is a CR that is the LAST byte of `buf`
///     and `at_eof` is false, so we cannot yet tell `\r` (CR) from `\r\n` (CRLF);
///   * `EolToken::None` — `buf[i]` is not an EOL byte.
///
/// At end-of-stream (`at_eof`) a trailing lone CR is resolved as a 1-byte CR EOL,
/// because the parser will never receive the following byte that could make it a
/// CRLF (mirrors the parser leaving a trailing `\r` `Incomplete` forever).
enum EolToken {
    Complete(usize),
    IncompleteCr,
    None,
}

fn eol_token_at(buf: &[u8], i: usize, at_eof: bool) -> EolToken {
    match buf.get(i) {
        Some(b'\r') => match buf.get(i + 1) {
            Some(b'\n') => EolToken::Complete(2),    // CRLF, greedy.
            Some(_) => EolToken::Complete(1),        // lone CR proven by a following byte.
            None if at_eof => EolToken::Complete(1), // no more bytes: CR resolves to CR.
            None => EolToken::IncompleteCr,          // could still become CRLF.
        },
        Some(b'\n') => EolToken::Complete(1), // LF never coalesces forward.
        _ => EolToken::None,
    }
}

/// Carried byte-accounting state of the SSE frame guard between chunks. Bundled
/// into one value so the maximal-EOL-run flag (`in_eol_run`) travels alongside
/// the running count and the deferred-prefix carry without an ever-widening
/// tuple. See [`SseFrameGuard`] for the field invariants.
#[derive(Debug, Clone)]
struct ScanState {
    since_boundary: usize,
    carry: Vec<u8>,
    in_eol_run: bool,
}

/// Advance past a MAXIMAL run of complete EOL tokens in `buf` starting at `from`,
/// returning `(end, stop)` where `end` is the index just past the last complete
/// EOL consumed and `stop` says WHY the run ended:
///   * [`EolRunStop::Content`] — `buf[end]` is a non-EOL byte (real frame content);
///   * [`EolRunStop::BufferEnd`] — the run reached the end of `buf` cleanly (the
///     last token was a complete EOL); a leading EOL on the NEXT chunk continues it;
///   * [`EolRunStop::IncompleteCr`] — the run stopped on a trailing lone `\r` whose
///     CR-vs-CRLF nature is unresolved mid-stream (`!at_eof`); that `\r` is itself
///     another empty-line EOL and is deferred uncharged into the carry.
///
/// Every byte consumed here is an empty-line EOL that belongs to NO data frame, so
/// the caller charges none of them (Codex round-4 LOW).
fn eol_run_end(buf: &[u8], from: usize, at_eof: bool) -> (usize, EolRunStop) {
    let mut i = from;
    loop {
        match eol_token_at(buf, i, at_eof) {
            EolToken::Complete(len) => i += len,
            EolToken::IncompleteCr => return (i, EolRunStop::IncompleteCr),
            EolToken::None => {
                return if i >= buf.len() {
                    (i, EolRunStop::BufferEnd)
                } else {
                    (i, EolRunStop::Content)
                };
            }
        }
    }
}

enum EolRunStop {
    Content,
    BufferEnd,
    IncompleteCr,
}

/// Single robust pass that bounds EVERY SSE frame in `carry + chunk` and returns
/// the updated [`ScanState`] (running count, freshly-deferred tail, and whether we
/// ended inside an empty-line EOL run).
///
/// A frame boundary is a BLANK LINE = two consecutive `end-of-line`s, tokenized
/// exactly like the `eventsource-stream` parser (`end-of-line = cr lf / cr / lf`,
/// CRLF greedy). So the boundary byte-sequences are, by length: `\n\n`, `\n\r`,
/// `\r\r` (2); `\n\r\n`, `\r\n\n`, `\r\n\r`, `\r\r\n` (3); `\r\n\r\n` (4). The old
/// guard recognized only `\n\n`/`\r\r`/`\r\n\r\n` and so mis-detected the mixed
/// combos (Codex round-3 Finding 1).
///
/// `carry` is the tail DEFERRED by the previous call (uncharged). We rebuild
/// `buf = carry + chunk` so a boundary straddling the chunk edge is detected, then
/// walk it boundary by boundary:
///   * For each completed boundary, the bytes of `buf` since the current frame
///     started are now CONFIRMED frame bytes (a boundary follows them): charge
///     them to `since_boundary` and check the cap, then reset `since_boundary` to
///     0 for the next frame. (This naturally subsumes the old `carry` bytes — they
///     are charged here exactly once, the first time they are confirmed.)
///   * Immediately AFTER each boundary, consume the MAXIMAL run of additional
///     consecutive EOLs (Codex round-4 LOW): those are extra empty lines that the
///     parser dispatches as empty events / skips, so they belong to no frame and
///     resume scanning from the end of the run with `since_boundary` still 0. A run
///     that straddles the chunk edge is finished on the next chunk via `in_eol_run`
///     (a leading EOL there continues it, uncharged) so it is never charged.
///   * After the last boundary/run, the trailing segment is split: when `!at_eof`,
///     its longest suffix that is a PROPER PREFIX of a boundary is deferred
///     uncharged into the new carry and the remainder is charged; when `at_eof`
///     there is no future byte to disambiguate, so the entire trailing segment is
///     charged (a dangling single EOL is part of the still-open, unterminated
///     frame) — UNLESS we are still inside an EOL run, in which case a trailing EOL
///     is one more empty line and stays uncharged (Finding 2 vs. round-4 LOW: an
///     unterminated *frame* must be charged at EOF, but an inter-frame empty line
///     must not).
///
/// Correctness properties:
///   * Finding 1 — a trailing byte that merely STARTS a boundary is never charged
///     until the next chunk disambiguates it, so an ambiguous tail cannot trip the
///     cap (and the verdict does not depend on the chunk split).
///   * Finding 2 — a deferred boundary-prefix carry that never completes is charged
///     to the unterminated frame at EOF.
///   * Round-4 LOW — extra/empty blank-line EOLs are charged to no frame, with the
///     run consumed even when split across a chunk edge (carry = run tail,
///     `in_eol_run = true`).
///
/// Returns the new `ScanState`, or `Err(observed)` — the count that first exceeded
/// the cap — so the caller can format the error.
fn scan_frames_since_boundary(
    state: ScanState,
    chunk: &[u8],
    cap: usize,
    at_eof: bool,
) -> Result<ScanState, usize> {
    let ScanState {
        mut since_boundary,
        carry,
        in_eol_run,
    } = state;
    debug_assert!(
        carry.len() <= 3 && boundary_prefix_suffix_len(&carry) == carry.len(),
        "carry must be a pure boundary prefix of <=3 bytes"
    );
    let mut buf = Vec::with_capacity(carry.len() + chunk.len());
    buf.extend_from_slice(&carry);
    buf.extend_from_slice(chunk);

    // `seg_start` is the `buf` index where the current in-progress frame begins;
    // `scan` is how far we have searched for the next boundary.
    let mut seg_start = 0usize;
    let mut scan = 0usize;

    // If the previous chunk ended inside an empty-line EOL run, finish consuming it
    // FIRST: a leading EOL here is one more empty line (charged to nothing), not the
    // first byte of the next frame. Only when the run ends do we begin the frame.
    if in_eol_run {
        match eol_run_end(&buf, 0, at_eof) {
            (end, EolRunStop::IncompleteCr) => {
                // Still mid-run: defer the trailing lone `\r` (another empty-line
                // EOL whose CR/CRLF nature is unresolved) and stay in the run.
                let new_carry = buf[end..].to_vec();
                debug_assert!(new_carry.len() <= 1, "in-run carry is a lone CR");
                return Ok(ScanState {
                    since_boundary: 0,
                    carry: new_carry,
                    in_eol_run: true,
                });
            }
            (_end, EolRunStop::BufferEnd) => {
                // Run consumed the whole buffer cleanly; the next chunk's leading
                // EOLs (if any) continue it. Nothing is charged.
                return Ok(ScanState {
                    since_boundary: 0,
                    carry: Vec::new(),
                    in_eol_run: true,
                });
            }
            (end, EolRunStop::Content) => {
                // The run ended at real frame content: the next frame starts here,
                // and we fall through to the normal boundary scan below.
                seg_start = end;
                scan = end;
            }
        }
    }

    while let Some((bs, be)) = next_boundary(&buf, scan, at_eof) {
        // No boundary is ever double-counted: `scan`/`seg_start` only advance, so
        // each reported boundary starts at/after the current frame's start, and a
        // mid-stream `carry` is never itself a complete boundary (it was deferred
        // precisely because its trailing CR was an unresolved last byte, i.e.
        // `next_boundary(carry, 0, false) == None`). A boundary may now legitimately
        // END at `carry.len()` when the FIRST chunk byte merely RESOLVES that
        // trailing CR (e.g. carry `\r\r` + chunk `d` → boundary `[0,2)`), which is a
        // first detection, not a re-reset.
        debug_assert!(
            bs >= seg_start,
            "boundary start {bs} precedes frame start {seg_start} — double reset"
        );
        // Bytes [seg_start, bs) are now confirmed frame bytes (a boundary follows).
        let confirmed = bs.saturating_sub(seg_start);
        since_boundary = since_boundary.saturating_add(confirmed);
        if since_boundary > cap {
            return Err(since_boundary);
        }
        // Boundary terminates the frame: the count resets for the next frame. Then
        // consume any ADDITIONAL consecutive EOLs (extra empty lines) so their bytes
        // are charged to no frame (Codex round-4 LOW).
        since_boundary = 0;
        match eol_run_end(&buf, be, at_eof) {
            (end, EolRunStop::IncompleteCr) => {
                let new_carry = buf[end..].to_vec();
                debug_assert!(new_carry.len() <= 1, "in-run carry is a lone CR");
                return Ok(ScanState {
                    since_boundary: 0,
                    carry: new_carry,
                    in_eol_run: true,
                });
            }
            (_end, EolRunStop::BufferEnd) => {
                return Ok(ScanState {
                    since_boundary: 0,
                    carry: Vec::new(),
                    in_eol_run: true,
                });
            }
            (end, EolRunStop::Content) => {
                seg_start = end;
                scan = end;
            }
        }
    }

    // Trailing unterminated segment after the final boundary/run (or the whole
    // buffer if there was none). Mid-stream we defer its boundary-prefix suffix
    // uncharged and charge the rest; at EOF nothing more can arrive to complete a
    // boundary, so the whole segment is charged as part of the unterminated frame.
    let tail = &buf[seg_start..];
    let defer = if at_eof {
        0
    } else {
        boundary_prefix_suffix_len(tail)
    };
    let charged = tail.len() - defer;
    since_boundary = since_boundary.saturating_add(charged);
    if since_boundary > cap {
        return Err(since_boundary);
    }
    let new_carry = tail[charged..].to_vec();
    debug_assert!(new_carry.len() <= 3, "deferred carry must stay <=3 bytes");
    Ok(ScanState {
        since_boundary,
        carry: new_carry,
        in_eol_run: false,
    })
}

/// Length of the longest suffix of `buf` that is a PROPER prefix of an SSE
/// blank-line boundary (two `end-of-line`s) — i.e. bytes that might still grow
/// into / complete a boundary on the next chunk and so must be deferred
/// uncharged. With CRLF-greedy EOL tokenization the proper boundary prefixes,
/// longest-first, are:
///   * `\r\n\r` (3) — one CRLF EOL plus a pending CR (→ `\r\n\r\n` or `\r\n`+`\r`);
///   * `\r\n` (2) — one CRLF EOL, second EOL not yet seen;
///   * `\n\r` (2) — LF EOL plus a pending CR (→ `\n\r\n` or `\n`+`\r`);
///   * `\r\r` (2) — CR EOL plus a pending CR (→ `\r\r\n` or `\r`+`\r`);
///   * a lone trailing `\r` or `\n` (1).
///
/// A two-EOL boundary that is already COMPLETE and unambiguous (`\n\n`, `\r\n\n`,
/// `\n\r\n`, `\r\r\n`, `\r\n\r\n`) is consumed by [`next_boundary`] before the
/// tail is examined, so it never reaches here. The ambiguous-length boundaries
/// (`\n\r`, `\r\r`, `\r\n\r`) are deferred here precisely because a trailing CR
/// could still extend the separator — deferring them keeps the byte verdict
/// chunking-independent; they are resolved (as complete boundaries that reset, or
/// as charged frame bytes) on the next chunk or at EOF.
fn boundary_prefix_suffix_len(buf: &[u8]) -> usize {
    let n = buf.len();
    // 3-byte prefix `\r\n\r` of `\r\n\r\n`.
    if n >= 3 && &buf[n - 3..] == b"\r\n\r" {
        return 3;
    }
    // 2-byte ambiguous/partial prefixes: one EOL plus a pending CR, or a partial
    // CRLF, that could still complete or extend a boundary on the next chunk.
    if n >= 2 {
        let last2 = &buf[n - 2..];
        if last2 == b"\r\n" || last2 == b"\n\r" || last2 == b"\r\r" {
            return 2;
        }
    }
    // 1-byte prefix: a lone trailing `\r` (start of `\r\r`/`\r\n...`) or `\n`
    // (start of `\n\n`/`\n\r`).
    if n >= 1 && (buf[n - 1] == b'\r' || buf[n - 1] == b'\n') {
        return 1;
    }
    0
}

/// Find the next SSE blank-line boundary in `buf` at or after `from`, returning
/// its `(start, end)` byte range, or `None` if none completes. A boundary is two
/// consecutive `end-of-line`s, each tokenized greedily as `cr lf / cr / lf` (see
/// [`eol_token_at`]); the `(start, end)` range covers BOTH EOLs (so the bytes of
/// the separator itself are never charged to either adjacent frame). A trailing
/// lone CR that cannot yet be disambiguated (`!at_eof`) does not complete a
/// boundary — it is deferred into the carry instead.
fn next_boundary(buf: &[u8], from: usize, at_eof: bool) -> Option<(usize, usize)> {
    let n = buf.len();
    let mut i = from;
    while i < n {
        // First EOL of the candidate blank line.
        let first_len = match eol_token_at(buf, i, at_eof) {
            EolToken::Complete(len) => len,
            // A lone trailing CR mid-stream cannot start a confirmed boundary yet.
            EolToken::IncompleteCr => return None,
            EolToken::None => {
                i += 1;
                continue;
            }
        };
        // Second consecutive EOL → the line between them is empty → boundary.
        match eol_token_at(buf, i + first_len, at_eof) {
            EolToken::Complete(second_len) => {
                return Some((i, i + first_len + second_len));
            }
            // The second EOL is an unresolved trailing CR (mid-stream): the
            // boundary is not yet complete; defer (it lives in the carry).
            EolToken::IncompleteCr => return None,
            // First byte was an EOL but the next is ordinary content: not a blank
            // line. Resume scanning AFTER this EOL (the content may yet end in a
            // real boundary).
            EolToken::None => {
                i += first_len;
            }
        }
    }
    None
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

pub async fn collect_supported_model_catalog(
    response: reqwest::Response,
) -> AppResult<Vec<UpstreamModelEntry>> {
    let (_, body, _) = collect_models_response(response).await?;
    Ok(extract_supported_model_catalog(&body))
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

/// Parse the upstream model catalog (id + optional context length) from a
/// `/v1/models` body in a SINGLE pass. Preserves the existing id behavior: bare
/// string entries are ids with no context length; object entries take their
/// `id` and, if present, the first positive context-length field. Entries
/// without an `id` are skipped.
fn extract_supported_model_catalog(body: &Value) -> Vec<UpstreamModelEntry> {
    model_entries_from_body(body)
        .iter()
        .filter_map(|entry| match entry {
            Value::String(id) => Some(UpstreamModelEntry {
                id: id.clone(),
                context_limit: None,
            }),
            Value::Object(map) => {
                let id = map.get("id").and_then(Value::as_str)?;
                Some(UpstreamModelEntry {
                    id: id.to_string(),
                    context_limit: entry_context_limit(map),
                })
            }
            _ => None,
        })
        .collect()
}

/// First positive integer among the context-length keys the Anthropic
/// `/v1/models` reshape uses (`http.rs`): `max_input_tokens`, `context_length`,
/// `context_window`, `max_context_length`, `max_model_len`. Single source for
/// G3 context budgeting and the routing union parse.
fn entry_context_limit(map: &serde_json::Map<String, Value>) -> Option<i64> {
    [
        "max_input_tokens",
        "context_length",
        "context_window",
        "max_context_length",
        "max_model_len",
    ]
    .iter()
    .find_map(|key| map.get(*key).and_then(Value::as_i64).filter(|n| *n > 0))
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

/// Parse `(id, context_limit)` pairs from a `/v1/models` body for G3 budgeting,
/// omitting entries with no positive context length. Used by the routing client
/// to read context limits from its already-merged union entries.
fn extract_model_context_limits(body: &Value) -> Vec<(String, i64)> {
    model_entries_from_body(body)
        .iter()
        .filter_map(|entry| {
            let map = entry.as_object()?;
            let id = map.get("id").and_then(Value::as_str)?;
            Some((id.to_string(), entry_context_limit(map)?))
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

/// Redact image `data:`/signed URLs from an upstream RESPONSE error body, then
/// truncate it for an `AppError`/log message (G4 round-9 #2). A provider that
/// echoes a native-vision-passthrough / disabled-agent image request can mirror
/// the submitted `data:` bytes or a signed image URL back in its 4xx/5xx body;
/// without this they would leak through `response.failed` and failover logs
/// (AGENTS.md redact rule). Redaction runs BEFORE truncation so a split image
/// URI cannot survive at the truncation boundary.
fn redact_and_truncate_error_body(body: &str, max: usize) -> String {
    truncate_for_error(&crate::vision::redact_image_uris(body), max)
}

fn stringify_json_value(value: Value) -> Value {
    Value::String(serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string()))
}

#[cfg(test)]
mod tests {
    use super::Bytes;
    use super::ReqwestUpstreamClient;
    use super::UpstreamModelEntry;
    use super::UpstreamRequestLogger;
    use super::extract_model_context_limits;
    use super::extract_supported_model_catalog;
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

    fn glm_effort_policies()
    -> std::collections::BTreeMap<String, crate::config::ReasoningEffortPolicy> {
        std::collections::BTreeMap::from([(
            "GLM-5.2-NVFP4-MTP".to_string(),
            crate::config::ReasoningEffortPolicy {
                default: Some("max".to_string()),
                map: std::collections::BTreeMap::from([
                    (
                        "high".to_string(),
                        json!({"chat_template_kwargs": {"reasoning_effort": "high"}}),
                    ),
                    (
                        "max".to_string(),
                        json!({"chat_template_kwargs": {"reasoning_effort": "max"}}),
                    ),
                    (
                        "none".to_string(),
                        json!({"chat_template_kwargs": {"enable_thinking": false}}),
                    ),
                ]),
            },
        )])
    }

    #[test]
    fn reasoning_effort_fragment_resolves_level_default_and_canonical() {
        use super::reasoning_effort_fragment;
        let p = glm_effort_policies();
        let ctk_effort = |frag: Value| frag["chat_template_kwargs"]["reasoning_effort"].clone();
        // Exact id + explicit level.
        assert_eq!(
            ctk_effort(reasoning_effort_fragment(&p, "GLM-5.2-NVFP4-MTP", Some("high")).unwrap()),
            json!("high")
        );
        // Canonical-key match (case/punctuation differences).
        assert_eq!(
            ctk_effort(reasoning_effort_fragment(&p, "glm 5.2 nvfp4 mtp", Some("max")).unwrap()),
            json!("max")
        );
        // Raw absent -> policy default ("max").
        assert_eq!(
            ctk_effort(reasoning_effort_fragment(&p, "GLM-5.2-NVFP4-MTP", None).unwrap()),
            json!("max")
        );
        // Off.
        assert_eq!(
            reasoning_effort_fragment(&p, "GLM-5.2-NVFP4-MTP", Some("none")).unwrap()["chat_template_kwargs"]
                ["enable_thinking"],
            json!(false)
        );
        // Level not in the map -> None (engine keeps its clamped top-level effort).
        assert!(reasoning_effort_fragment(&p, "GLM-5.2-NVFP4-MTP", Some("medium")).is_none());
        // Unknown model -> None.
        assert!(reasoning_effort_fragment(&p, "other-model", Some("high")).is_none());

        // Ambiguous canonical match (two profiles, same canonical key, neither an
        // exact id match) -> no policy, deterministically.
        let mut ambiguous = glm_effort_policies();
        let dup = ambiguous["GLM-5.2-NVFP4-MTP"].clone();
        ambiguous.insert("glm5.2nvfp4mtp".to_string(), dup);
        assert!(reasoning_effort_fragment(&ambiguous, "GLM!5.2!NVFP4!MTP", Some("high")).is_none());
    }

    #[test]
    fn effort_fragment_overrides_config_default_but_client_wins() {
        use super::apply_reasoning_effort_fragment;
        let fragment = json!({"chat_template_kwargs": {"reasoning_effort": "high"}});

        // A configured chat_template_kwargs default is OVERRIDDEN by the map
        // (prefer-source), fixing the "config blocks the map" case.
        let mut request = family_request("GLM-5.2-NVFP4-MTP");
        request.extra_body.insert(
            "chat_template_kwargs".to_string(),
            json!({"reasoning_effort": "max", "sibling": 1}),
        );
        apply_reasoning_effort_fragment(&mut request, &fragment);
        assert_eq!(kwargs_of(&request)["reasoning_effort"], json!("high"));
        assert_eq!(kwargs_of(&request)["sibling"], json!(1));

        // An explicit CLIENT value wins over the map (client > effort-map).
        let mut request = family_request("GLM-5.2-NVFP4-MTP");
        request.client_chat_template_kwargs = Some(JsonMap::from_iter([(
            "reasoning_effort".to_string(),
            json!("max"),
        )]));
        apply_reasoning_effort_fragment(&mut request, &fragment);
        assert_eq!(kwargs_of(&request)["reasoning_effort"], json!("max"));
    }

    #[test]
    fn effort_fragment_survives_kimi_family_injection() {
        use super::apply_reasoning_effort_fragment;
        // Kimi injection wipes enable_thinking/reasoning_effort and forces
        // thinking=true; applying the fragment AFTER family re-asserts the map's
        // enable_thinking:false (fixes the Kimi-map-ignored finding).
        let mut request = family_request("kimi-k2-instruct");
        apply_family_chat_template_kwargs(&mut request);
        assert_eq!(kwargs_of(&request)["thinking"], json!(true));
        apply_reasoning_effort_fragment(
            &mut request,
            &json!({"chat_template_kwargs": {"enable_thinking": false}}),
        );
        assert_eq!(kwargs_of(&request)["enable_thinking"], json!(false));
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

    fn catalog_ids(entries: &[UpstreamModelEntry]) -> Vec<&str> {
        entries.iter().map(|entry| entry.id.as_str()).collect()
    }

    #[test]
    fn extract_supported_model_catalog_reads_standard_models_list() {
        let body = serde_json::json!({
            "object": "list",
            "data": [
                {"id": "glm-5.1"},
                {"id": "Qwen3.5"},
                "grok-4"
            ]
        });

        // Ids preserved, including the bare-string entry (context_limit None).
        assert_eq!(
            catalog_ids(&extract_supported_model_catalog(&body)),
            vec!["glm-5.1", "Qwen3.5", "grok-4"]
        );
        assert!(
            extract_supported_model_catalog(&body)
                .iter()
                .all(|entry| entry.context_limit.is_none())
        );
    }

    #[test]
    fn extract_supported_model_catalog_reads_models_array() {
        let body = serde_json::json!({"models": ["glm-5.1"]});
        assert_eq!(
            extract_supported_model_catalog(&body),
            vec![UpstreamModelEntry {
                id: "glm-5.1".to_string(),
                context_limit: None,
            }]
        );
    }

    #[test]
    fn extract_supported_model_catalog_reads_ids_and_context_limits_in_one_pass() {
        // Same snapshot yields ids AND limits: first positive context key wins,
        // and an entry with no positive context length keeps its id with a
        // `None` limit (budgeting no-ops for it, but it still resolves).
        let body = serde_json::json!({
            "data": [
                {"id": "a", "max_input_tokens": 1000, "context_length": 9999},
                {"id": "b", "context_window": 2048},
                {"id": "c", "context_length": 0},
                "bare",
            ]
        });

        assert_eq!(
            extract_supported_model_catalog(&body),
            vec![
                UpstreamModelEntry {
                    id: "a".to_string(),
                    context_limit: Some(1000)
                },
                UpstreamModelEntry {
                    id: "b".to_string(),
                    context_limit: Some(2048)
                },
                UpstreamModelEntry {
                    id: "c".to_string(),
                    context_limit: None
                },
                UpstreamModelEntry {
                    id: "bare".to_string(),
                    context_limit: None
                },
            ]
        );
    }

    #[test]
    fn extract_model_context_limits_reads_first_positive_key() {
        // Prefers `max_input_tokens`, then falls back through the alias keys;
        // skips entries with no positive context length (string ids, zero,
        // missing) so budgeting no-ops for them.
        let body = serde_json::json!({
            "data": [
                {"id": "a", "max_input_tokens": 1000, "context_length": 9999},
                {"id": "b", "context_window": 2048},
                {"id": "c", "max_model_len": 4096},
                {"id": "d", "context_length": 0},
                {"id": "e"},
                "bare-string-id",
            ]
        });

        let limits = extract_model_context_limits(&body);
        assert_eq!(
            limits,
            vec![
                ("a".to_string(), 1000),
                ("b".to_string(), 2048),
                ("c".to_string(), 4096),
            ]
        );
    }

    #[test]
    fn extract_model_context_limits_handles_models_array_and_empty() {
        let body = serde_json::json!({"models": [{"id": "x", "max_context_length": 32768}]});
        assert_eq!(
            extract_model_context_limits(&body),
            vec![("x".to_string(), 32768)]
        );
        assert!(extract_model_context_limits(&serde_json::json!({})).is_empty());
    }

    // --- G6 upstream SSE per-frame cap: boundary-detection internals ---

    #[test]
    fn next_boundary_tokenizes_every_eol_combo_like_the_parser() {
        // No boundary.
        assert_eq!(super::next_boundary(b"data: hi", 0, false), None);
        // Two consecutive EOLs = blank line = boundary; (start, end) spans BOTH
        // EOLs so separator bytes are charged to neither frame. All combos of
        // `cr lf / cr / lf` x `cr lf / cr / lf` (CRLF greedy) are recognized.
        // Followed by `b` so any trailing CR is disambiguated (not incomplete).
        assert_eq!(super::next_boundary(b"a\n\nb", 0, false), Some((1, 3))); // LF LF
        assert_eq!(super::next_boundary(b"a\r\rb", 0, false), Some((1, 3))); // CR CR
        assert_eq!(super::next_boundary(b"a\r\n\r\nb", 0, false), Some((1, 5))); // CRLF CRLF
        assert_eq!(super::next_boundary(b"a\r\n\nb", 0, false), Some((1, 4))); // CRLF LF
        assert_eq!(super::next_boundary(b"a\n\rb", 0, false), Some((1, 3))); // LF CR
        assert_eq!(super::next_boundary(b"a\r\n\rb", 0, false), Some((1, 4))); // CRLF CR
        assert_eq!(super::next_boundary(b"a\n\r\nb", 0, false), Some((1, 4))); // LF CRLF
        assert_eq!(super::next_boundary(b"a\r\r\nb", 0, false), Some((1, 4))); // CR CRLF
        // A trailing lone CR mid-stream is INCOMPLETE: it might still become CRLF,
        // so no boundary is reported until disambiguated (or EOF).
        assert_eq!(super::next_boundary(b"a\n\r", 0, false), None);
        assert_eq!(super::next_boundary(b"a\r\r", 0, false), None);
        // ...but at EOF the trailing CR resolves to a CR EOL and the boundary completes.
        assert_eq!(super::next_boundary(b"a\n\r", 0, true), Some((1, 3)));
        assert_eq!(super::next_boundary(b"a\r\r", 0, true), Some((1, 3)));
        // A single EOL followed by ordinary content is NOT a blank line; scanning
        // resumes and finds the real boundary later.
        assert_eq!(super::next_boundary(b"a\nb\n\nc", 0, false), Some((3, 5)));
        // The FIRST boundary at/after `from` is returned; `from` skips earlier ones.
        assert_eq!(super::next_boundary(b"a\n\nb\n\nc", 0, false), Some((1, 3)));
        assert_eq!(super::next_boundary(b"a\n\nb\n\nc", 3, false), Some((4, 6)));
    }

    /// Thin test shim: drive `scan_frames_since_boundary` with the legacy
    /// positional args and collapse the returned `ScanState` to the
    /// `(since_boundary, carry)` tuple the older assertions read. `in_eol_run`
    /// defaults to false on input (the round-4 LOW cases assert it explicitly via
    /// `scan_state`). Returns `Err(observed)` unchanged.
    fn scan(
        since: usize,
        carry: &[u8],
        chunk: &[u8],
        cap: usize,
        eof: bool,
    ) -> Result<(usize, Vec<u8>), usize> {
        scan_state(since, carry, false, chunk, cap, eof).map(|s| (s.since_boundary, s.carry))
    }

    /// Like [`scan`] but also threads/returns the `in_eol_run` flag so the
    /// round-4 LOW (maximal-EOL-run) cases can assert run state.
    fn scan_state(
        since: usize,
        carry: &[u8],
        in_eol_run: bool,
        chunk: &[u8],
        cap: usize,
        eof: bool,
    ) -> Result<super::ScanState, usize> {
        super::scan_frames_since_boundary(
            super::ScanState {
                since_boundary: since,
                carry: carry.to_vec(),
                in_eol_run,
            },
            chunk,
            cap,
            eof,
        )
    }

    #[test]
    fn scan_frames_charges_confirmed_frame_bytes_and_defers_prefix_tail() {
        let cap = 1024;
        // No carry, boundary mid-chunk: "ab" confirmed+reset, tail "cd" charged,
        // nothing deferred.
        assert_eq!(scan(0, b"", b"ab\n\ncd", cap, false), Ok((2, vec![])));
        // No boundary, no prefix tail: whole new chunk extends the frame and the
        // carried-in count; the (uncharged) carry "\r" is now confirmed & charged.
        assert_eq!(scan(5, b"\r", b"abc", cap, false), Ok((9, vec![])));
        // Boundary straddling carry/chunk edge: carry "\r\n" + chunk "\r\nz" =>
        // boundary completes & resets, tail "z" charged.
        assert_eq!(scan(7, b"\r\n", b"\r\nz", cap, false), Ok((1, vec![])));
        // A MIXED-separator boundary mid-chunk is recognized (Finding 1): carry-in
        // count 6, chunk "..\r\n\ncd" => the `\r\n\n` blank line resets (the `\r` is
        // NOT charged as a frame byte), tail "cd" charged.
        assert_eq!(scan(6, b"", b"\r\n\ncd", cap, false), Ok((2, vec![])));
        // A trailing boundary-PREFIX is DEFERRED, not charged (Finding 1): "ab"
        // charged, trailing "\r\n\r" held uncharged in the returned carry.
        assert_eq!(
            scan(0, b"", b"ab\r\n\r", cap, false),
            Ok((2, b"\r\n\r".to_vec()))
        );
        // A lone trailing "\n" is deferred (could start "\n\n"); count unchanged.
        assert_eq!(scan(3, b"", b"\n", cap, false), Ok((3, b"\n".to_vec())));
        // An ambiguous-length 2-byte prefix ("\n\r": LF + pending CR) is deferred
        // whole, mid-stream, so the verdict stays chunk-independent.
        assert_eq!(scan(3, b"", b"\n\r", cap, false), Ok((3, b"\n\r".to_vec())));
    }

    #[test]
    fn scan_frames_consumes_maximal_eol_run_after_boundary() {
        // Codex round-4 LOW: extra blank-line EOLs after a boundary belong to no
        // frame and must be charged to neither side.
        let cap = 4;
        // Three consecutive LFs: the first two are the boundary, the THIRD is an
        // extra empty line. "ab" charged+reset, the extra "\n" consumed (charged to
        // nothing), tail "cd" charged => since=2. The OLD code charged the extra
        // "\n" into "cd" (since=3).
        let s = scan_state(0, b"", false, b"ab\n\n\ncd", cap, false).expect("ok");
        assert_eq!(
            (s.since_boundary, s.carry, s.in_eol_run),
            (2, vec![], false)
        );
        // FOUR LFs (two boundaries' worth) collapse the same way: only "cd" counts.
        let s = scan_state(0, b"", false, b"ab\n\n\n\ncd", cap, false).expect("ok");
        assert_eq!(s.since_boundary, 2);
        // The exact Codex sequence: a frame at cap, then `\n\n\n`, then a second
        // frame of EXACTLY cap content. The extra `\n` must NOT be charged into the
        // second frame, so it stays at cap (accepted), not cap+1.
        let mut data = Vec::new();
        data.extend_from_slice(b"x".repeat(cap).as_slice());
        data.extend_from_slice(b"\n\n\n");
        data.extend_from_slice(b"y".repeat(cap).as_slice());
        let s = scan_state(0, b"", false, &data, cap, false).expect("at-cap second frame accepted");
        assert_eq!(s.since_boundary, cap);
        // Mixed run `\r\n\n` (boundary) + `\r\r` (two more empty-line EOLs): all
        // consumed, nothing charged from the run; tail "z" charged.
        let s = scan_state(0, b"", false, b"ab\r\n\n\r\rz", cap, false).expect("ok");
        assert_eq!((s.since_boundary, s.carry), (1, vec![]));
    }

    #[test]
    fn scan_frames_eol_run_straddling_chunk_edge_is_fully_consumed() {
        // The maximal-EOL run can straddle a chunk edge; it must still be consumed
        // (never charged), matching the in-chunk verdict (Codex round-4 LOW).
        let cap = 8;
        // Chunk 1 ends mid-run with a trailing lone `\r` (after boundary `\n\n` +
        // `\r`): the `\r` is deferred and we stay `in_eol_run`.
        let s = scan_state(0, b"", false, b"ab\n\n\r", cap, false).expect("ok");
        assert_eq!(
            (s.since_boundary, s.carry, s.in_eol_run),
            (0, b"\r".to_vec(), true)
        );
        // Chunk 2 resolves it as a CR EOL (`\r` + content): the run's `\r` is one
        // more empty line (NOT charged) and "cd" begins the next frame.
        let s = scan_state(0, b"\r", true, b"cd", cap, false).expect("ok");
        assert_eq!(
            (s.since_boundary, s.carry, s.in_eol_run),
            (2, vec![], false)
        );
        // If chunk 2 instead resolves it as CRLF (`\r\n`) followed by content, the
        // `\r\n` is still ONE empty-line EOL (uncharged) and "cd" begins the frame.
        let s = scan_state(0, b"\r", true, b"\ncd", cap, false).expect("ok");
        assert_eq!((s.since_boundary, s.in_eol_run), (2, false));
        // `in_eol_run` with a leading EOL that itself ends the chunk stays in-run.
        let s = scan_state(0, b"", true, b"\n", cap, false).expect("ok");
        assert_eq!((s.since_boundary, s.carry, s.in_eol_run), (0, vec![], true));
        // `in_eol_run` ending at EOF on a dangling `\r`: that final CR is one last
        // empty line, charged to nothing (NOT to a frame).
        let s = scan_state(0, b"\r", true, b"", cap, true).expect("ok");
        assert_eq!(s.since_boundary, 0);
    }

    #[test]
    fn scan_frames_finalizes_carry_on_eof() {
        let cap = 4;
        // EOF with a dangling single EOL carry charges it as the unterminated
        // frame's bytes (Finding 2): since=4 + carry "\n" => 5 > 4 => reject.
        assert_eq!(scan(cap, b"\n", b"", cap, true), Err(5));
        // EOF where the carry is itself a complete boundary ("\r\r", resolving the
        // final CR) resets instead of charging: the frame WAS terminated.
        assert_eq!(scan(3, b"\r\r", b"", cap, true), Ok((0, vec![])));
        // EOF with `\r\n\r` carry: that is `\r\n` EOL + a final CR EOL = boundary,
        // so it resets (no charge).
        assert_eq!(scan(3, b"\r\n\r", b"", cap, true), Ok((0, vec![])));
        // EOF with `\r\n` carry: one EOL, no second => unterminated frame; charge
        // both bytes. since=3 + 2 => 5 > 4 => reject.
        assert_eq!(scan(3, b"\r\n", b"", cap, true), Err(5));
    }

    #[test]
    fn scan_frames_caps_pre_boundary_segment() {
        let cap = 4;
        // A TERMINATED but oversized segment ("xxxxx" = 5 > 4) is rejected even
        // though the post-boundary tail is empty (Finding 1 sibling: confirmed
        // pre-boundary bytes are still capped).
        assert_eq!(scan(0, b"", b"xxxxx\n\n", cap, false), Err(5));
        // A pre-boundary segment that, added to the carried-in count, crosses the
        // cap is rejected before the reset (since=4, +"x" before "\n\n" => 5).
        assert_eq!(scan(cap, b"", b"x\n\n", cap, false), Err(5));
    }

    #[test]
    fn carry_completes_boundary_split_across_tiny_chunks() {
        // A `\r\n\r\n` separator arriving as "\r","\n","\r\n" must be detected.
        let mut guard = super::SseFrameGuard::new(1024);
        guard.accept(b"\r").expect("carry \\r");
        guard.accept(b"\n").expect("carry \\r\\n");
        // This completes \r\n\r\n across the chunk edge; the frame resets to 0.
        guard.accept(b"\r\n").expect("boundary completes");
        // Prove the reset: a near-cap frame now fits where it would not have if
        // the prior bytes had still been counted.
        let near_cap = vec![b'q'; 1024];
        guard
            .accept(&near_cap)
            .expect("fresh frame fits after multi-chunk boundary reset");
    }

    #[test]
    fn boundary_prefix_suffix_len_classifies_tails() {
        // Proper, incomplete/ambiguous boundary prefixes, longest-first. (In real
        // use the caller only ever passes a post-final-boundary tail, so an
        // unambiguous COMPLETE boundary like `\n\n` never reaches here.)
        assert_eq!(super::boundary_prefix_suffix_len(b"a\r\n\r"), 3); // CRLF + pending CR
        assert_eq!(super::boundary_prefix_suffix_len(b"a\r\n"), 2); // CRLF, 2nd EOL pending
        // Ambiguous-LENGTH 2-byte separators (one EOL + a pending CR) are deferred
        // whole: a following `\n` extends/redefines the boundary, so charging them
        // early would make the verdict depend on the chunk split.
        assert_eq!(super::boundary_prefix_suffix_len(b"a\n\r"), 2); // LF + pending CR
        assert_eq!(super::boundary_prefix_suffix_len(b"a\r\r"), 2); // CR + pending CR
        assert_eq!(super::boundary_prefix_suffix_len(b"a\r"), 1);
        assert_eq!(super::boundary_prefix_suffix_len(b"a\n"), 1);
        // A trailing `\r` is always a deferrable prefix (it may begin a `\r\r` or
        // `\r\n...` on the next chunk).
        assert_eq!(super::boundary_prefix_suffix_len(b"x\r"), 1);
        // Ordinary bytes defer nothing.
        assert_eq!(super::boundary_prefix_suffix_len(b"abc"), 0);
        assert_eq!(super::boundary_prefix_suffix_len(b""), 0);
    }

    #[test]
    fn guard_floors_tiny_cap_to_1kib() {
        let guard = super::SseFrameGuard::new(10);
        assert_eq!(guard.max_frame_bytes(), 1024);
    }

    #[tokio::test]
    async fn bounded_stream_passes_normal_then_errors_on_oversized() {
        use futures::StreamExt;
        // A normal small frame, then a chunk that blows the (floored) cap.
        // `Bytes` here is the re-exported `bytes::Bytes` already in scope; using
        // `Result<Bytes, reqwest::Error>` matches what `bytes_stream()` yields so
        // the adapter's generic bound is exercised exactly as in production.
        let chunks: Vec<Result<Bytes, reqwest::Error>> = vec![
            Ok(Bytes::from_static(b"data: ok\n\n")),
            Ok(Bytes::from(vec![b'x'; 2048])),
        ];
        let mut stream = Box::pin(super::bounded_sse_byte_stream(
            futures::stream::iter(chunks),
            1024,
        ));
        // First item passes through unchanged.
        let first = stream.next().await.expect("first item").expect("ok bytes");
        assert_eq!(first.as_ref(), b"data: ok\n\n");
        // Second item exceeds the cap and surfaces as a transport error.
        let err = stream
            .next()
            .await
            .expect("second item")
            .expect_err("oversized chunk errors");
        assert!(err.to_string().contains("exceeded"));
    }
}

#[cfg(test)]
mod resolve_match_kind_tests {
    use super::*;
    use std::collections::HashMap;

    /// Single-provider catalog serving exactly `model`, with the canonical-key
    /// index populated so rule-4 normalization is exercised too.
    fn catalog_serving(model: &str) -> RoutingModelCatalog {
        let candidate = RoutingModelCandidate {
            provider_index: 0,
            model_id: model.to_string(),
            target: RoutingModelTarget::Primary,
        };
        let mut ids_by_key = HashMap::new();
        ids_by_key.insert(canonical_model_key(model), vec![candidate.clone()]);
        RoutingModelCatalog {
            provider_catalogs: vec![RoutingProviderModelCatalog {
                candidates: vec![candidate],
            }],
            union_entries: Vec::new(),
            union_ids: vec![model.to_string()],
            ids_by_key,
            routes: Vec::new(),
        }
    }

    fn resolved_id(resolution: &RoutingResolution) -> &str {
        match resolution {
            RoutingResolution::Catalog(candidate) => &candidate.model_id,
            RoutingResolution::Route { model_id, .. } => model_id,
        }
    }

    #[test]
    fn exact_id_reports_exact_match() {
        let (resolution, kind) = catalog_serving("served-model")
            .resolve("served-model")
            .expect("resolves");
        assert_eq!(kind, MatchKind::ExactId);
        assert_eq!(resolved_id(&resolution), "served-model");
    }

    #[test]
    fn case_or_punctuation_variant_reports_canonical_key() {
        // Not an exact id (case differs), so it must normalize via the canonical
        // key rather than silently dropping to the default fallback.
        let (resolution, kind) = catalog_serving("served-model")
            .resolve("SERVED_MODEL")
            .expect("resolves");
        assert_eq!(kind, MatchKind::CanonicalKey);
        assert_eq!(resolved_id(&resolution), "served-model");
    }

    #[test]
    fn unknown_model_reports_default_fallback() {
        // The claude-relay parity case: an incoming `claude-opus-*` name that the
        // backend does not serve falls back to the first catalog model, tagged
        // Default so the dispatch site logs a WARN.
        let (resolution, kind) = catalog_serving("served-model")
            .resolve("claude-opus-4")
            .expect("falls back to default");
        assert_eq!(kind, MatchKind::Default);
        assert_eq!(resolved_id(&resolution), "served-model");
    }

    #[test]
    fn empty_catalog_resolves_to_none() {
        let empty = RoutingModelCatalog {
            provider_catalogs: Vec::new(),
            union_entries: Vec::new(),
            union_ids: Vec::new(),
            ids_by_key: HashMap::new(),
            routes: Vec::new(),
        };
        assert!(empty.resolve("anything").is_none());
    }
}
