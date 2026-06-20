use crate::config::merge_json_maps;
use crate::error::AppError;
use crate::error::AppResult;
use crate::error::FailoverDisposition;
use crate::models::chat::ChatCompletionChunk;
use crate::models::chat::ChatCompletionRequest;
use crate::sse_guard::bounded_sse_byte_stream;
use crate::sse_guard::default_max_sse_frame_bytes;
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

/// One pre-first-chunk serving backend: its FINAL model id (after any
/// routing/route/exposed-alias + per-provider `upstream_model` rewrite — no
/// further remap) and the context-window length the upstream reports for it,
/// for G3 pre-flight budgeting (T9). `context_limit: None` ⇒ the upstream did
/// not report a window for this model (budgeting no-ops for it).
#[derive(Debug, Clone)]
pub struct BackendCandidate {
    pub model: String,
    pub context_limit: Option<i64>,
}

/// Typed backend-candidate plan: the routing/failover layer's answer to "which
/// backend models could serve this request pre-first-chunk, and what context
/// window does each report?" G4 native-vision gating consumes the candidate
/// MODELS instead of re-deriving the set in the engine (T2); G3 pre-flight
/// budgeting consumes the candidate CONTEXT LIMITS (conservative MIN — the
/// strictest window across the failover chain — so a failover to a smaller
/// model cannot overflow; unknown ⇒ no-op) instead of budgeting against the
/// pre-routing `resolved_model` (T9). The `genuine` signal — whether the
/// request model truly resolved vs. fell back to a catalog default — is
/// ENGINE-side (a byproduct of `normalize_upstream_model`, not a re-derived
/// ladder; see `backend_is_native_vision`).
///
/// Empty `candidates` ⇒ unknown candidate set (catalog-load failure): the gate
/// treats it as strip+offload, budgeting no-ops.
#[derive(Debug, Clone)]
pub struct BackendCandidatePlan {
    pub candidates: Vec<BackendCandidate>,
}

#[async_trait]
pub trait UpstreamClient: Send + Sync {
    async fn stream_chat_completion(
        &self,
        request: &BackendChatRequest,
    ) -> AppResult<UpstreamStream>;
    async fn stream_chat_completion_with_timeout(
        &self,
        request: &BackendChatRequest,
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
    /// Default impl: thin projection over
    /// [`backend_candidate_plan`](Self::backend_candidate_plan) — the single
    /// source of truth since T2. A single-provider passthrough sends the request
    /// model unchanged, so the only candidate is `requested_model` itself.
    async fn candidate_backend_models(&self, requested_model: &str) -> Vec<String> {
        self.backend_candidate_plan(requested_model)
            .await
            .candidates
            .into_iter()
            .map(|candidate| candidate.model)
            .collect()
    }

    /// Typed backend-candidate plan for `requested_model` — the routing/failover
    /// layer's candidate set (model + per-candidate context limit) for G4
    /// native-vision gating (T2) and G3 pre-flight budgeting (T9). The `genuine`
    /// signal is engine-side; this method returns only `candidates`.
    ///
    /// Default impl: a single-provider passthrough sends the request model
    /// unchanged, so the only candidate is `requested_model` itself with no
    /// known context limit (budgeting no-ops). Routing/failover clients override
    /// to fold in the failover chain + per-provider context limits.
    async fn backend_candidate_plan(&self, requested_model: &str) -> BackendCandidatePlan {
        BackendCandidatePlan {
            candidates: vec![BackendCandidate {
                model: requested_model.to_string(),
                context_limit: None,
            }],
        }
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
    /// Per-backend-model finalization policies (effort map, `template_family`
    /// override, `upstream_chat_kwargs`), keyed by resolved model id. Applied at
    /// this leaf because it is the single point that sees the FINAL provider
    /// model after routing/failover/exposed-alias remap (T1). Shared (cheap
    /// clone) across all providers; empty when no profile defines any.
    finalization_policies: BackendFinalizationPolicies,
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
    /// Context-window length keyed by union model id, parsed from the SAME
    /// first-seen `/v1/models` entry that populated `union_entries`/`union_ids`.
    /// `supported_model_catalog` reads it directly so the union catalog is parsed
    /// once at refresh rather than reparsed from `union_entries` per call.
    union_context_limit_by_id: HashMap<String, i64>,
    ids_by_key: HashMap<String, Vec<RoutingModelCandidate>>,
    /// Ad-hoc routes (G7), cloned from the client each refresh. Matched purely
    /// by request-model name, independent of the live catalog, so routes still
    /// resolve when an upstream `/v1/models` fetch is unavailable.
    routes: Vec<ModelRouteSpec>,
}

#[derive(Debug, Clone, Default)]
struct RoutingProviderModelCatalog {
    candidates: Vec<RoutingModelCandidate>,
    /// Per-model context-window length for this provider's catalog, keyed by
    /// model id (parsed from the SAME `/v1/models` snapshot as `candidates`).
    /// G3 pre-flight budgeting reads this via `backend_candidate_plan` so it
    /// budgets against the REAL per-provider window (T9), not the pre-routing
    /// union default.
    context_limit_by_id: HashMap<String, i64>,
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
            crate::redaction::redact_image_uris_in_value(&mut value);
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
            finalization_policies: BackendFinalizationPolicies::default(),
        }
    }

    /// Attach the per-backend-model finalization policies (effort map,
    /// `template_family` override, `upstream_chat_kwargs`; built once from
    /// config). Threaded post-construction so the `with_options` signature is
    /// unchanged.
    pub(crate) fn with_finalization_policies(
        mut self,
        policies: BackendFinalizationPolicies,
    ) -> Self {
        self.finalization_policies = policies;
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

    /// Append `request` to the JSONL request log (when configured), then POST it.
    /// Every actual upstream chat POST goes through here so the log records both
    /// the original request and the G1 shrink-and-retry — otherwise the retry's
    /// reduced budget would be invisible to the JSONL log and `analyze-log`. A
    /// log-write failure is logged and the POST proceeds (logging is observability,
    /// not a hard dependency of serving the request).
    async fn logged_send_chat_request(
        &self,
        url: &Url,
        request: &ChatCompletionRequest,
    ) -> AppResult<reqwest::Response> {
        if let Some(ref logger) = self.request_logger
            && let Err(err) = logger.log(request).await
        {
            tracing::warn!(
                path = %logger.path.display(),
                error = %err,
                "failed to append upstream request log"
            );
        }
        self.send_chat_request(url, request).await
    }
}

#[async_trait]
impl UpstreamClient for ReqwestUpstreamClient {
    async fn stream_chat_completion(
        &self,
        backend: &BackendChatRequest,
    ) -> AppResult<UpstreamStream> {
        let url = self.endpoint_url("chat/completions")?;
        // Finalize at THIS leaf: the single point that sees the FINAL provider
        // model after routing/failover/exposed-alias remap, with provider kwargs
        // already merged by `request_for_provider`. Resolves the per-model
        // `upstream_chat_kwargs` (global base + per-model) + `template_family`
        // override from policies keyed by `request.model`, maps/clamps reasoning
        // effort, and injects family `chat_template_kwargs` (precedence: config <
        // family < effort-map < client; the wrapper's `client_chat_template_kwargs`
        // is re-asserted last so an explicit client value still wins).
        let mut backend = backend.clone();
        finalize_request_for_backend(&mut backend, &self.finalization_policies);
        let request = sanitize_chat_request(backend.request, self.flatten_content);

        // First attempt. On a non-2xx whose body indicates a context/completion
        // token-limit overflow, shrink `max_completion_tokens` and retry ONCE.
        // This happens before any SSE chunk is parsed/yielded downstream, so it
        // can never duplicate already-streamed tokens, and it stays inside the
        // leaf client so the failover/routing layers never see a context-limit
        // error as a provider failure (it is a same-provider shrink-and-retry).
        let response = self.logged_send_chat_request(&url, &request).await?;
        let status = response.status();
        if status.is_success() {
            return stream_success_response(response, self.max_sse_frame_bytes).await;
        }

        let body = response.text().await.unwrap_or_default();
        if let Some(retry) = classify_context_overflow(
            &body,
            self.min_completion_tokens,
            Some(estimate_leaf_input_tokens(&request)),
        ) {
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
            let retry_response = self.logged_send_chat_request(&url, &retried).await?;
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
            if classify_context_overflow(
                &retry_body,
                self.min_completion_tokens,
                Some(estimate_leaf_input_tokens(&retried)),
            )
            .is_some()
            {
                return Err(AppError::upstream_with_disposition(
                    format!(
                        "upstream context-window overflow persisted after shrink-and-retry; \
                         failed with {retry_status}: {}",
                        redact_and_truncate_error_body(&retry_body, 500)
                    ),
                    FailoverDisposition::Terminal,
                ));
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
        backend: &BackendChatRequest,
    ) -> BackendChatRequest {
        let mut request = backend.request.clone();
        if let Some(model) = &provider.upstream_model {
            request.model = model.clone();
        }
        merge_fallback_chat_kwargs(&mut request, &provider.upstream_chat_kwargs);
        BackendChatRequest {
            request,
            client_chat_template_kwargs: backend.client_chat_template_kwargs.clone(),
        }
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
        backend: &BackendChatRequest,
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
            backend,
            request_timeout,
        )
        .await
    }

    async fn stream_chat_completion_with_provider_indices(
        &self,
        provider_indices: Vec<usize>,
        backend: &BackendChatRequest,
        request_timeout: Duration,
    ) -> AppResult<UpstreamStream> {
        let mut last_error = None;
        for provider_index in provider_indices {
            let provider = &self.providers[provider_index];
            let provider_request = Self::request_for_provider(provider, backend);
            let stream = match provider
                .client
                .stream_chat_completion(&provider_request)
                .await
            {
                Ok(stream) => stream,
                Err(err) if err.failover_disposition() == FailoverDisposition::Terminal => {
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
    /// dispatch so both honor the same rewrite + log behavior. The wrapper's
    /// `client_chat_template_kwargs` is preserved across the rewrite.
    fn routed_request(
        &self,
        backend: &BackendChatRequest,
        model_id: &str,
        provider_name: &str,
        kind: MatchKind,
    ) -> BackendChatRequest {
        let mut routed_request = backend.request.clone();
        if routed_request.model != model_id {
            log_model_resolution(&routed_request.model, model_id, provider_name, kind);
            routed_request.model = model_id.to_string();
        }
        BackendChatRequest {
            request: routed_request,
            client_chat_template_kwargs: backend.client_chat_template_kwargs.clone(),
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
        let mut union_context_limit_by_id: HashMap<String, i64> = HashMap::new();
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
            let mut provider_context_limits: HashMap<String, i64> = HashMap::new();
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
                    &mut provider_context_limits,
                    &mut union_entries,
                    &mut union_ids,
                    &mut union_context_limit_by_id,
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
                    &mut provider_context_limits,
                    &mut union_entries,
                    &mut union_ids,
                    &mut union_context_limit_by_id,
                    &mut ids_by_key,
                    &mut seen_union_ids,
                );
            }
            provider_catalogs.push(RoutingProviderModelCatalog {
                candidates: provider_candidates,
                context_limit_by_id: provider_context_limits,
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
            union_context_limit_by_id,
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
    context_limit_by_id: &mut HashMap<String, i64>,
    union_entries: &mut Vec<Value>,
    union_ids: &mut Vec<String>,
    union_context_limit_by_id: &mut HashMap<String, i64>,
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
    // Preserve the per-provider context limit for G3 budgeting (T9). Synthetic
    // bare-string entries (no object body) carry no limit ⇒ None.
    if let Value::Object(map) = &entry
        && let Some(limit) = entry_context_limit(map)
    {
        context_limit_by_id.insert(model_id.clone(), limit);
    }
    ids_by_key.entry(key).or_default().push(candidate);
    if seen_union_ids.insert(model_id.clone()) {
        // Capture the first-seen entry's context limit alongside the union id,
        // so the union catalog is parsed once here instead of being reparsed
        // from `union_entries` in `supported_model_catalog`. Reads the SAME
        // `entry_context_limit` over the SAME first-seen entry.
        if let Value::Object(map) = &entry
            && let Some(limit) = entry_context_limit(map)
        {
            union_context_limit_by_id.insert(model_id.clone(), limit);
        }
        union_ids.push(model_id);
        union_entries.push(entry);
    }
}

#[async_trait]
impl UpstreamClient for FailoverUpstreamClient {
    async fn stream_chat_completion(
        &self,
        backend: &BackendChatRequest,
    ) -> AppResult<UpstreamStream> {
        self.stream_chat_completion_with_timeout(backend, Duration::from_secs(60))
            .await
    }

    async fn stream_chat_completion_with_timeout(
        &self,
        backend: &BackendChatRequest,
        request_timeout: Duration,
    ) -> AppResult<UpstreamStream> {
        let provider_indices = self.available_provider_indices();
        if provider_indices.is_empty() {
            return Err(self.cooldown_error());
        }
        self.stream_chat_completion_with_provider_indices(
            provider_indices,
            backend,
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

    /// Typed plan: a failover chain's candidate set is each provider's
    /// effective model — its `upstream_model` rewrite (matching
    /// `request_for_provider`) or the request model when it sends through
    /// unchanged (G4 round-2 #1). We enumerate ALL providers (not just
    /// currently-available ones): cooldown is transient, so a fallback that is
    /// non-native must still force strip+offload. Context limits are `None`
    /// (a failover chain does not load per-provider `/v1/models` catalogs, so
    /// G3 budgeting no-ops — matching pre-T9 behavior). `candidate_backend_models`
    /// (trait default) projects from this.
    async fn backend_candidate_plan(&self, requested_model: &str) -> BackendCandidatePlan {
        let candidates = self
            .providers
            .iter()
            .map(|provider| BackendCandidate {
                model: provider
                    .upstream_model
                    .clone()
                    .unwrap_or_else(|| requested_model.to_string()),
                context_limit: None,
            })
            .collect();
        BackendCandidatePlan { candidates }
    }
}

#[async_trait]
impl UpstreamClient for RoutingUpstreamClient {
    async fn stream_chat_completion(
        &self,
        backend: &BackendChatRequest,
    ) -> AppResult<UpstreamStream> {
        self.stream_chat_completion_with_timeout(backend, Duration::from_secs(60))
            .await
    }

    /// Typed backend-candidate plan using the SAME route/catalog resolution as
    /// `stream_chat_completion` (G4 review #2 + round-2 #1). Each candidate
    /// carries its per-provider context limit for G3 budgeting (T9); the
    /// `genuine` signal is engine-side (see `BackendCandidatePlan`).
    ///
    /// A route resolves to one backend model (route providers are synthetic
    /// single-model upstreams with no `/v1/models` context window ⇒ `None`).
    /// A catalog match dispatches to a routing provider: the PRIMARY target may
    /// fail over across that provider's whole nested failover chain (so all of
    /// its candidate models count), while a fallback/exposed-alias target serves
    /// only that single provider's model. Per-provider context limits come from
    /// the routing catalog's `context_limit_by_id` (populated from the SAME
    /// `/v1/models` snapshot as the candidate ids); nested-failover models not
    /// in this provider's catalog carry `None` (G3 no-ops for them, same as
    /// pre-T9). A catalog-load failure or empty catalog yields an empty candidate
    /// set, which the safe invariant treats as "unknown → strip+offload" and
    /// budgeting treats as no-op.
    async fn backend_candidate_plan(&self, requested_model: &str) -> BackendCandidatePlan {
        let Ok(catalog) = self.load_catalog().await else {
            return BackendCandidatePlan {
                candidates: Vec::new(),
            };
        };
        let Some((resolution, _kind)) = catalog.resolve(requested_model) else {
            return BackendCandidatePlan {
                candidates: Vec::new(),
            };
        };
        let candidates = match resolution {
            RoutingResolution::Route { model_id, .. } => vec![BackendCandidate {
                model: model_id,
                context_limit: None,
            }],
            RoutingResolution::Catalog(candidate) => {
                let Some(provider) = self.providers.get(candidate.provider_index) else {
                    return BackendCandidatePlan {
                        candidates: Vec::new(),
                    };
                };
                // Per-provider context limits come from the SELECTED routing
                // provider's primary `/v1/models` snapshot. They are keyed by
                // the primary's OWN model ids, so the limit is authoritative
                // ONLY for the primary's own model (`candidate.model_id`).
                // Nested-failover / exposed-fallback models are served by OTHER
                // upstreams whose `/v1/models` is not loaded here; looking them
                // up in the primary's map could borrow the WRONG window when an
                // id coincidentally matches, so they carry `None` (G3 no-ops for
                // them — T9 R2 HIGH fix).
                let primary_limits =
                    &catalog.provider_catalogs[candidate.provider_index].context_limit_by_id;
                let primary_limit = primary_limits.get(&candidate.model_id).copied();
                match candidate.target {
                    // Primary: the whole nested failover chain may serve. Only
                    // Primary: the whole nested failover chain may serve. Only
                    // the chain's FIRST candidate (index 0 — the selected
                    // provider's own model, whose `/v1/models` snapshot populates
                    // `primary_limits`) carries `primary_limit`. All later chain
                    // candidates are nested-failover models served by OTHER
                    // upstreams whose `/v1/models` is not loaded here ⇒ `None`
                    // (G3 no-ops for them — never borrow the primary's window for
                    // a different provider, even if the model id matches). This
                    // is provider-identity scoping, not model-string scoping
                    // (T9 R3 HIGH fix).
                    RoutingModelTarget::Primary => provider
                        .client
                        .candidate_backend_models(&candidate.model_id)
                        .await
                        .into_iter()
                        .enumerate()
                        .map(|(index, model)| BackendCandidate {
                            context_limit: (index == 0).then_some(primary_limit).flatten(),
                            model,
                        })
                        .collect(),
                    // Fallback/exposed-alias: served by a nested failover
                    // provider whose own `/v1/models` is not loaded here ⇒ None
                    // (G3 no-ops; the failover provider's window is unknown at
                    // this layer, so no false cap / no wrong-window borrow).
                    RoutingModelTarget::Fallback {
                        failover_provider_index,
                    } => {
                        let model = provider
                            .failover_provider_model(failover_provider_index)
                            .unwrap_or_else(|| candidate.model_id.clone());
                        vec![BackendCandidate {
                            context_limit: None,
                            model,
                        }]
                    }
                }
            }
        };
        BackendCandidatePlan { candidates }
    }

    async fn stream_chat_completion_with_timeout(
        &self,
        backend: &BackendChatRequest,
        request_timeout: Duration,
    ) -> AppResult<UpstreamStream> {
        let catalog = self.load_catalog().await.map_err(|err| {
            tracing::warn!(
                requested_model = %backend.request.model,
                error = %err,
                "failed to load upstream model catalog (is the backend reachable?)"
            );
            err
        })?;
        let (resolution, match_kind) = catalog.resolve(&backend.request.model).ok_or_else(|| {
            tracing::warn!(
                requested_model = %backend.request.model,
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
            let routed_request = self.routed_request(backend, model_id, &provider.name, match_kind);
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
            self.routed_request(backend, &resolution.model_id, &provider.name, match_kind);
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
        // from `union_context_limit_by_id`, parsed once at catalog refresh from
        // the same first-seen entries.
        let catalog = self.load_catalog().await?;
        Ok(catalog
            .union_ids
            .into_iter()
            .map(|id| {
                let context_limit = catalog.union_context_limit_by_id.get(&id).copied();
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

/// Resolved per-model finalization policies, built ONCE from config and shared
/// (cheap `Arc` clone) across all leaf clients. Each map is keyed by the FINAL
/// resolved model id (profile name); the leaf looks up the policy for the model
/// it actually POSTs to (post routing/failover/exposed-alias remap), so a
/// routed/failover target gets its OWN model's family/kwargs/effort vocabulary
/// rather than the request alias's (T1).
///
/// This is the typed successor to the engine's pre-routing resolution: the
/// engine no longer threads `template_family` / `upstream_chat_kwargs` down the
/// wire DTO, and `ChatCompletionRequest` carries no `#[serde(skip)]` side-channel
/// fields. The leaf is the single point that knows the FINAL `request.model`.
#[derive(Clone, Default, Debug)]
pub struct BackendFinalizationPolicies {
    /// Per-model reasoning-effort policy (`reasoning_effort_map` + default).
    pub effort: Arc<std::collections::BTreeMap<String, crate::config::ReasoningEffortPolicy>>,
    /// Per-model `template_family` override (normalized `kimi`/`deepseek`).
    pub template_family: Arc<std::collections::BTreeMap<String, String>>,
    /// GLOBAL `template_family` fallback (normalized), applied when no per-model
    /// policy matches the FINAL model.
    pub global_template_family: Option<String>,
    /// Per-model extends-merged `upstream_chat_kwargs`.
    pub upstream_chat_kwargs: Arc<std::collections::BTreeMap<String, JsonMap<String, Value>>>,
    /// GLOBAL `upstream_chat_kwargs` (base layer), merged under the per-model
    /// policy at the leaf.
    pub global_upstream_chat_kwargs: Arc<JsonMap<String, Value>>,
}

impl BackendFinalizationPolicies {
    /// Build the leaf finalization policies from config: effort map,
    /// `template_family` override, and `upstream_chat_kwargs` (global base +
    /// per-model). Built once at startup and shared (cheap `Arc` clone) across
    /// all leaf clients. `pub` so the test harness builds the same policies the
    /// production leaf receives (T1).
    pub fn from_config(config: &crate::config::Config) -> Self {
        Self {
            effort: Arc::new(config.reasoning_effort_policies()),
            template_family: Arc::new(config.template_family_policies()),
            global_template_family: config.global_template_family(),
            upstream_chat_kwargs: Arc::new(config.upstream_chat_kwargs_policies()),
            global_upstream_chat_kwargs: Arc::new(config.global_upstream_chat_kwargs().clone()),
        }
    }

    /// Resolve the `template_family` override for the FINAL provider `model`:
    /// the per-model policy wins (exact then canonical-key match, mirroring
    /// `Config::model_profile` / `reasoning_effort_fragment`), else the global
    /// fallback. `None` means sniff the model id instead.
    fn resolve_family_override(&self, model: &str) -> Option<String> {
        policy_for_model(&self.template_family, model)
            .cloned()
            .or_else(|| self.global_template_family.clone())
    }

    /// The extends-merged `upstream_chat_kwargs` for the FINAL provider `model`:
    /// the per-model policy (exact then canonical-key match) layered over the
    /// global base (per-model wins on conflict). Empty when neither applies.
    fn resolve_chat_kwargs(&self, model: &str) -> JsonMap<String, Value> {
        let mut merged = (*self.global_upstream_chat_kwargs).clone();
        if let Some(per_model) = policy_for_model(&self.upstream_chat_kwargs, model) {
            merge_json_maps(&mut merged, per_model);
        }
        merged
    }
}
/// Look up a per-model policy in `map` for the FINAL provider `model` with the
/// SAME semantics as `Config::model_profile` and `RoutingModelCatalog` model
/// matching: exact (case-sensitive) id first, then a canonical-key match
/// (case/punctuation-insensitive) ONLY when unambiguous (exactly one profile
/// shares that canonical key — two would make the pick order-dependent). `None`
/// when no policy applies. Keeps the leaf's per-model policy lookup consistent
/// with how profiles are matched everywhere else (T1).
fn policy_for_model<'a, V>(
    map: &'a std::collections::BTreeMap<String, V>,
    model: &str,
) -> Option<&'a V> {
    if let Some(policy) = map.get(model) {
        return Some(policy);
    }
    let key = canonical_model_key(model);
    let mut matches = map
        .iter()
        .filter(|(name, _)| canonical_model_key(name) == key)
        .map(|(_, policy)| policy);
    match (matches.next(), matches.next()) {
        (Some(policy), None) => Some(policy),
        _ => None,
    }
}

/// Leaf-boundary wrapper carrying finalization metadata that does NOT belong on
/// the wire DTO `ChatCompletionRequest`. Built at the leaf boundary (in the
/// engine, before dispatch) from per-model policies keyed by the FINAL
/// `request.model` after routing/failover rewrite. Never crosses a serde
/// boundary (no serde derives; it is not serialized to the wire).
///
/// `pub` only because the `pub UpstreamClient` trait's `stream_chat_completion`
/// takes it (and `Gateway::upstream_client()` returns `Arc<dyn UpstreamClient>`
/// publicly, so the trait is `pub`). The crate is a binary gateway with no
/// external consumer; the wrapper is an internal seam, not a public API surface.
///
/// `client_chat_template_kwargs` is the client's EXPLICIT `chat_template_kwargs`
/// (from the inbound request `extra_body`), captured PRE-MERGE. It is NOT
/// re-derivable at the leaf: by the time the leaf runs, `request.extra_body[
/// "chat_template_kwargs"]` already contains global+profile+provider merges, and
/// re-asserting that blend over the forced family default would regress leak
/// prevention (a provider/global `thinking:false` would clobber Kimi's forced
/// `thinking:true`). Threading the pure client value lets the leaf re-overlay
/// ONLY the client's keys so an explicit client value still wins over the family
/// default (precedence: config < family < effort-map < client).
#[derive(Debug, Clone)]
pub struct BackendChatRequest {
    pub request: ChatCompletionRequest,
    pub client_chat_template_kwargs: Option<JsonMap<String, Value>>,
}

impl BackendChatRequest {
    /// Wrap a wire request with the client's explicit `chat_template_kwargs`
    /// (captured pre-merge by the engine). The leaf resolves family/kwargs from
    /// policies keyed by `request.model` inside `finalize_request_for_backend`.
    pub fn new(
        request: ChatCompletionRequest,
        client_chat_template_kwargs: Option<JsonMap<String, Value>>,
    ) -> Self {
        Self {
            request,
            client_chat_template_kwargs,
        }
    }
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

/// Finalize the chat request the backend will receive, at the leaf — the single
/// point that knows the FINAL provider `model` after routing/failover/exposed-
/// alias remap. Resolves and applies that model's `upstream_chat_kwargs` (global
/// base + per-model policy, request-wins), its `template_family` override (per-
/// model policy else global), its reasoning-effort policy, then injects family
/// `chat_template_kwargs`. `request.reasoning_effort` arrives RAW (lowering does
/// not clamp); here it is either MAPPED or clamped.
///
/// MAPPED: the per-model `reasoning_effort_map` produces a fragment, so the
/// top-level field is CLEARED (the fragment relays effort via
/// chat_template_kwargs; a leftover top-level value would be ignored by GLM or
/// seed a contradictory DeepSeek setdefault), then the fragment is applied AFTER
/// family so it overrides family defaults.
///
/// UNMAPPED: clamped to the OpenAI-compatible `none`/`low`/`high` vocabulary.
///
/// The G3 estimate omits `reasoning_effort` entirely, so neither clearing nor
/// clamping here perturbs the pre-flight lower bound.
///
/// `pub` so the integration-test mock upstreams mirror the production leaf.
pub fn finalize_request_for_backend(
    backend: &mut BackendChatRequest,
    policies: &BackendFinalizationPolicies,
) {
    let request = &mut backend.request;
    // 1. Per-model `upstream_chat_kwargs` (global base + per-model policy),
    //    gap-filling into `extra_body` (request-wins: keys already set by the
    //    engine's request-extra merge or by `request_for_provider`'s provider
    //    kwargs are preserved). This replaces the engine's pre-routing
    //    `build_upstream_extra_body` defaults (T1): the leaf now resolves kwargs
    //    against the FINAL model, so a routed/failover cross-family target gets
    //    its OWN kwargs, not the alias's.
    merge_upstream_chat_kwargs(request, &policies.resolve_chat_kwargs(&request.model));
    // 2. Reasoning effort: map (→ fragment, top-level cleared) or clamp.
    let fragment = reasoning_effort_fragment(
        &policies.effort,
        &request.model,
        request.reasoning_effort.as_deref(),
    );
    request.reasoning_effort = if fragment.is_some() {
        None
    } else {
        clamp_reasoning_effort(request.reasoning_effort.as_deref())
    };
    // 3. Family kwargs from the FINAL model + resolved override.
    apply_family_chat_template_kwargs(backend, policies);
    // 4. Effort fragment after family (effort-map > family default).
    if let Some(fragment) = fragment {
        apply_reasoning_effort_fragment(backend, &fragment);
    }
}

/// Merge per-model `upstream_chat_kwargs` `defaults` into the request
/// `extra_body` with REQUEST-WINS semantics, mirroring `merge_fallback_chat_kwargs`
/// (provider kwargs): a key already explicitly set on the request (typed field
/// or `extra_body`) is preserved; a configured default fills the gap. Deep-merge
/// nested objects so a configured object composes with a request object rather
/// than clobbering sibling keys.
fn merge_upstream_chat_kwargs(
    request: &mut ChatCompletionRequest,
    defaults: &JsonMap<String, Value>,
) {
    // Max-token aliases (`max_tokens`/`max_output_tokens`/`max_completion_tokens`)
    // are ONE logical knob. If the client expressed ANY of them (typed
    // `max_output_tokens` OR an alias in `request.extra_body`), skip ALL of them
    // from the configured defaults so a stale config alias cannot land alongside
    // the explicit request value and shadow it (mirrors the engine's pre-T1
    // `remove_defaults_shadowed_by_request_extra`).
    let max_token_requested = request.max_output_tokens.is_some()
        || MAX_TOKEN_ALIAS_KEYS
            .iter()
            .any(|alias| request.extra_body.contains_key(*alias));
    for (key, value) in defaults {
        if max_token_requested && MAX_TOKEN_ALIAS_KEYS.contains(&key.as_str()) {
            continue;
        }
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

/// Clamp a raw reasoning-effort level to the OpenAI-compatible vocabulary a
/// mapless backend understands: `none`/`low` pass through, everything else
/// (`medium`/`high`/`xhigh`/`max`/unknown) collapses to `high`. This is the
/// model-agnostic default for backends without a `reasoning_effort_map`.
fn clamp_reasoning_effort(effort: Option<&str>) -> Option<String> {
    let level = effort.map(str::trim).filter(|level| !level.is_empty())?;
    let clamped = match level.to_ascii_lowercase().as_str() {
        "none" => "none",
        "low" => "low",
        _ => "high",
    };
    Some(clamped.to_string())
}

/// Resolve the reasoning-effort fragment for the FINAL provider `model` and the
/// raw client effort level, or `None` when the model has no policy or the level
/// (after defaulting) is not mapped. Lookup is exact, then canonical-key
/// (case/punctuation-insensitive), mirroring catalog/profile model matching.
fn reasoning_effort_fragment(
    policies: &std::collections::BTreeMap<String, crate::config::ReasoningEffortPolicy>,
    model: &str,
    raw_effort: Option<&str>,
) -> Option<Value> {
    let policy = policy_for_model(policies, model)?;
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
pub fn apply_reasoning_effort_fragment(backend: &mut BackendChatRequest, fragment: &Value) {
    let Value::Object(fragment) = fragment else {
        return;
    };
    let request = &mut backend.request;
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
    if let Some(client_kwargs) = &backend.client_chat_template_kwargs
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
/// from the FINAL provider model + resolved `template_family` override. Composes
/// with (does not clobber) the already-merged configured/provider
/// `upstream_chat_kwargs`, and re-overlays the client's explicit
/// `chat_template_kwargs` last so an explicit request value still WINS over a
/// forced family default. A no-op when the family is unrecognized.
///
/// Public so the integration test harness's mock upstream can mirror the
/// production leaf and record the request the backend would actually receive.
pub fn apply_family_chat_template_kwargs(
    backend: &mut BackendChatRequest,
    policies: &BackendFinalizationPolicies,
) {
    let family_override = policies.resolve_family_override(&backend.request.model);
    let Some(family) = detect_model_family(&backend.request.model, family_override.as_deref())
    else {
        return;
    };
    let request = &mut backend.request;
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
    if let Some(client_kwargs) = &backend.client_chat_template_kwargs {
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
        "stop" => request.stop.is_some(),
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

/// Coarse, deterministic CONSERVATIVE lower-bound estimate of the input tokens
/// the leaf POSTs for `request`, mirroring `engine::estimate_input_tokens`.
/// Used only by the G1 reactive retry's `COMPLETION_LIMIT_RE` shape (an
/// output-only `max_model_len` error that reports the context window but NOT
/// the prompt size); without it the retry budget would be `ctx_limit - margin`,
/// ignoring the prompt and re-overflowing. `ceil(serialized_bytes / 4)` is the
/// same ~4-bytes-per-token heuristic the engine uses — not a tokenizer. It
/// runs on the sanitized, post-`finalize_request_for_backend` request, so it
/// accounts for any family/effort kwargs merged at the leaf. The other overflow
/// shapes extract the real input count from the error body and ignore this.
fn estimate_leaf_input_tokens(request: &ChatCompletionRequest) -> i64 {
    let bytes = serde_json::to_vec(request).map(|v| v.len()).unwrap_or(0);
    bytes.div_ceil(4) as i64
}

/// Parse a non-streaming upstream error body for a context/completion
/// token-limit overflow and compute the reduced completion budget for a retry.
///
/// Returns `None` for any error text that is not a recognizable token-limit
/// overflow, so the original error is surfaced unchanged (no retry).
///
/// `estimated_input_tokens` supplies the prompt size for the completion-only
/// (`max_model_len`) shape, which does not itself report the input size; the
/// leaf upstream path passes a local estimate from the sanitized request (see
/// [`estimate_leaf_input_tokens`]).
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

/// `pub` so the G3 test oracle (`tests/port_server.rs`) can independently
/// normalize a recorded request through the SAME terminal leaf transform the
/// estimator uses, without calling the production estimator (T9: breaks the
/// self-referential oracle, G3 MEDIUM #19).
pub fn sanitize_chat_request(
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
    truncate_for_error(&crate::redaction::redact_image_uris(body), max)
}

fn stringify_json_value(value: Value) -> Value {
    Value::String(serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string()))
}

#[cfg(test)]
mod tests {
    use super::ReqwestUpstreamClient;
    use super::UpstreamModelEntry;
    use super::UpstreamRequestLogger;
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

    use super::BackendChatRequest;
    use super::BackendFinalizationPolicies;
    use super::FailoverUpstreamClient;
    use super::FailoverUpstreamProvider;
    use super::ModelFamily;
    use super::apply_family_chat_template_kwargs;
    use super::detect_model_family;
    use super::merge_fallback_chat_kwargs;
    use super::merge_upstream_chat_kwargs;
    use super::write_family_kwargs;
    use serde_json::Map as JsonMap;
    use serde_json::json;
    use std::sync::Arc;

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

    /// Wrap a family-request with optional client kwargs into the leaf wrapper.
    fn family_backend(
        model: &str,
        client_kwargs: Option<JsonMap<String, Value>>,
    ) -> BackendChatRequest {
        BackendChatRequest::new(family_request(model), client_kwargs)
    }

    /// Empty finalization policies (no effort map, no family override, no
    /// per-model kwargs). The leaf's unmapped/clamp path.
    fn empty_policies() -> BackendFinalizationPolicies {
        BackendFinalizationPolicies::default()
    }

    /// GLM effort-map finalization policies (no family override, no kwargs).
    fn glm_backend_policies() -> BackendFinalizationPolicies {
        BackendFinalizationPolicies {
            effort: Arc::new(glm_effort_policies()),
            ..Default::default()
        }
    }

    /// Finalization policies carrying a per-model `template_family` override.
    fn family_policies(per_model: &[(&str, &str)]) -> BackendFinalizationPolicies {
        BackendFinalizationPolicies {
            template_family: Arc::new(
                per_model
                    .iter()
                    .map(|(m, f)| (m.to_string(), f.to_string()))
                    .collect(),
            ),
            ..Default::default()
        }
    }

    /// Finalization policies carrying a global `upstream_chat_kwargs` base.
    fn global_kwargs_policies(kwargs: JsonMap<String, Value>) -> BackendFinalizationPolicies {
        BackendFinalizationPolicies {
            global_upstream_chat_kwargs: Arc::new(kwargs),
            ..Default::default()
        }
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
        let mut backend = family_backend("GLM-5.2-NVFP4-MTP", None);
        backend.request.extra_body.insert(
            "chat_template_kwargs".to_string(),
            json!({"reasoning_effort": "max", "sibling": 1}),
        );
        apply_reasoning_effort_fragment(&mut backend, &fragment);
        assert_eq!(
            kwargs_of(&backend.request)["reasoning_effort"],
            json!("high")
        );
        assert_eq!(kwargs_of(&backend.request)["sibling"], json!(1));

        // An explicit CLIENT value wins over the map (client > effort-map).
        let mut backend = family_backend(
            "GLM-5.2-NVFP4-MTP",
            Some(JsonMap::from_iter([(
                "reasoning_effort".to_string(),
                json!("max"),
            )])),
        );
        apply_reasoning_effort_fragment(&mut backend, &fragment);
        assert_eq!(
            kwargs_of(&backend.request)["reasoning_effort"],
            json!("max")
        );
    }

    #[test]
    fn effort_fragment_survives_kimi_family_injection() {
        use super::apply_reasoning_effort_fragment;
        // Kimi injection wipes enable_thinking/reasoning_effort and forces
        // thinking=true; applying the fragment AFTER family re-asserts the map's
        // enable_thinking:false (fixes the Kimi-map-ignored finding).
        let mut backend = family_backend("kimi-k2-instruct", None);
        apply_family_chat_template_kwargs(&mut backend, &empty_policies());
        assert_eq!(kwargs_of(&backend.request)["thinking"], json!(true));
        apply_reasoning_effort_fragment(
            &mut backend,
            &json!({"chat_template_kwargs": {"enable_thinking": false}}),
        );
        assert_eq!(kwargs_of(&backend.request)["enable_thinking"], json!(false));
    }

    #[test]
    fn clamp_reasoning_effort_collapses_to_openai_vocabulary() {
        use super::clamp_reasoning_effort;
        assert_eq!(clamp_reasoning_effort(None), None);
        assert_eq!(clamp_reasoning_effort(Some("")), None);
        assert_eq!(
            clamp_reasoning_effort(Some("none")).as_deref(),
            Some("none")
        );
        assert_eq!(clamp_reasoning_effort(Some("low")).as_deref(), Some("low"));
        for raw in ["medium", "high", "xhigh", "max", "unknown"] {
            assert_eq!(
                clamp_reasoning_effort(Some(raw)).as_deref(),
                Some("high"),
                "{raw} collapses to high"
            );
        }
        assert_eq!(
            clamp_reasoning_effort(Some("  XHigh ")).as_deref(),
            Some("high")
        );
    }

    #[test]
    fn finalize_unmapped_model_clamps_top_level_effort() {
        use super::finalize_request_for_backend;
        let mut backend = family_backend("some-plain-model", None);
        backend.request.reasoning_effort = Some("xhigh".to_string());
        finalize_request_for_backend(&mut backend, &empty_policies());
        // No policy -> clamp; no family -> no chat_template_kwargs injected.
        assert_eq!(backend.request.reasoning_effort.as_deref(), Some("high"));
        assert!(
            !backend
                .request
                .extra_body
                .contains_key("chat_template_kwargs")
        );
    }

    #[test]
    fn finalize_mapped_model_clears_top_level_and_maps_to_chat_template_kwargs() {
        use super::finalize_request_for_backend;
        let mut backend = family_backend("GLM-5.2-NVFP4-MTP", None);
        backend.request.reasoning_effort = Some("max".to_string());
        finalize_request_for_backend(&mut backend, &glm_backend_policies());
        assert_eq!(backend.request.reasoning_effort, None);
        assert_eq!(
            kwargs_of(&backend.request)["reasoning_effort"],
            json!("max")
        );
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
        let mut backend = family_backend("kimi-k2-instruct", None);
        apply_family_chat_template_kwargs(&mut backend, &empty_policies());
        let kwargs = kwargs_of(&backend.request);
        assert_eq!(kwargs["thinking"], json!(true));
        assert_eq!(kwargs["preserve_thinking"], json!(true));
    }

    /// Finding 1 (negative): a DeepSeek FINAL provider model does NOT get the
    /// Kimi `thinking` knob — the wrong-family kwargs cannot leak across.
    #[test]
    fn deepseek_final_provider_model_does_not_get_kimi_kwargs() {
        let mut backend = family_backend("deepseek-v3", None);
        backend.request.reasoning_effort = Some("high".to_string());
        apply_family_chat_template_kwargs(&mut backend, &empty_policies());
        let kwargs = kwargs_of(&backend.request);
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
        let base = family_backend("glm-5.1", None);
        let mut provider_request = FailoverUpstreamClient::request_for_provider(&provider, &base);
        assert_eq!(provider_request.request.model, "kimi-k2-instruct");
        // Leaf injects family from the FINAL (remapped) model.
        apply_family_chat_template_kwargs(&mut provider_request, &empty_policies());
        let kwargs = kwargs_of(&provider_request.request);
        assert_eq!(kwargs["thinking"], json!(true));
        assert_eq!(kwargs["preserve_thinking"], json!(true));
        assert_eq!(kwargs["configured_only"], json!(true));
    }

    /// Finding 1: an explicit client `chat_template_kwargs` value still WINS
    /// over the forced family default, while non-conflicting forced keys remain.
    #[test]
    fn client_chat_template_kwargs_win_over_forced_family_default() {
        let mut backend = family_backend(
            "kimi-k2",
            Some(
                json!({ "thinking": false, "custom": 1 })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        );
        // The engine bakes the client value into extra_body; mirror that.
        backend.request.extra_body.insert(
            "chat_template_kwargs".to_string(),
            json!({ "thinking": false, "custom": 1 }),
        );
        apply_family_chat_template_kwargs(&mut backend, &empty_policies());
        let kwargs = kwargs_of(&backend.request);
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
        let mut backend = family_backend(
            "kimi-k2-instruct",
            Some(
                json!({ "mm_processor_kwargs": { "shared": "client", "from_client": 1 } })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        );
        // Configured/provider deep-merge already produced a nested object with
        // two siblings under `mm_processor_kwargs`.
        backend.request.extra_body.insert(
            "chat_template_kwargs".to_string(),
            json!({ "mm_processor_kwargs": { "from_config": true, "shared": "config" } }),
        );
        apply_family_chat_template_kwargs(&mut backend, &empty_policies());
        let nested = kwargs_of(&backend.request)["mm_processor_kwargs"]
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
        assert_eq!(kwargs_of(&backend.request)["thinking"], json!(true));
    }

    /// Unrecognized family => no injection at all.
    #[test]
    fn unrecognized_final_model_injects_nothing() {
        let mut backend = family_backend("glm-5.1", None);
        apply_family_chat_template_kwargs(&mut backend, &empty_policies());
        assert!(
            !backend
                .request
                .extra_body
                .contains_key("chat_template_kwargs")
        );
    }

    /// T1: the leaf resolves the `template_family` override from the FINAL
    /// provider model's policy (per-model, else global). A per-model policy
    /// forces a family the model NAME does not sniff, so the right family kwargs
    /// apply to the model the provider actually receives — not the alias's.
    #[test]
    fn leaf_resolves_family_from_per_model_policy_on_final_model() {
        // An opaque-named model with a per-model `deepseek` policy gets DeepSeek
        // kwargs even though the name sniffs nothing.
        let policies = family_policies(&[("opaque-target", "deepseek")]);
        let mut backend = family_backend("opaque-target", None);
        apply_family_chat_template_kwargs(&mut backend, &policies);
        let kwargs = kwargs_of(&backend.request);
        assert_eq!(kwargs["enable_thinking"], json!(true));
        assert!(
            kwargs.get("thinking").is_none(),
            "no Kimi knob for deepseek"
        );

        // An alias's policy does NOT bleed onto a different final model: the
        // final model has its own policy, which wins.
        let policies = family_policies(&[("glm-alias", "kimi"), ("opaque-target", "deepseek")]);
        let mut backend = family_backend("opaque-target", None);
        apply_family_chat_template_kwargs(&mut backend, &policies);
        let kwargs = kwargs_of(&backend.request);
        assert_eq!(kwargs["enable_thinking"], json!(true));
        assert!(
            kwargs.get("thinking").is_none(),
            "alias's kimi override must not bleed onto the deepseek target"
        );
    }

    /// T1 (R1 F1): per-model family policy lookup is case-insensitive (exact
    /// then canonical-key), mirroring `Config::model_profile`. A profile keyed
    /// `Opaque-Target` matches a FINAL model `opaque-target`.
    #[test]
    fn leaf_family_policy_lookup_is_case_insensitive() {
        let policies = family_policies(&[("Opaque-Target", "deepseek")]);
        let mut backend = family_backend("opaque-target", None);
        apply_family_chat_template_kwargs(&mut backend, &policies);
        let kwargs = kwargs_of(&backend.request);
        assert_eq!(
            kwargs["enable_thinking"],
            json!(true),
            "mixed-case profile key must match lowercase final model"
        );
    }

    /// T1: the GLOBAL `template_family` fallback applies when no per-model
    /// policy matches the FINAL model.
    #[test]
    fn leaf_global_family_fallback_when_no_per_model_policy() {
        let policies = BackendFinalizationPolicies {
            global_template_family: Some("kimi".to_string()),
            ..Default::default()
        };
        let mut backend = family_backend("opaque-no-profile", None);
        apply_family_chat_template_kwargs(&mut backend, &policies);
        let kwargs = kwargs_of(&backend.request);
        assert_eq!(kwargs["thinking"], json!(true));
        assert_eq!(kwargs["preserve_thinking"], json!(true));
    }

    /// T1 (R1 F2): max-token aliases are one logical knob. If the client sent
    /// `max_completion_tokens` via `extra_body`, the leaf must NOT insert a
    /// configured `max_tokens` default alongside it (would shadow the explicit
    /// request value). Mirrors the engine's pre-T1
    /// `remove_defaults_shadowed_by_request_extra`.
    #[test]
    fn leaf_kwargs_skip_max_token_aliases_when_request_has_one() {
        use super::finalize_request_for_backend;
        let mut defaults = JsonMap::new();
        defaults.insert("max_tokens".to_string(), json!(8192));
        let policies = global_kwargs_policies(defaults);

        // Client expressed `max_completion_tokens` via extra_body (typed field
        // absent). The configured `max_tokens` default must NOT be inserted.
        let mut backend = family_backend("plain-model", None);
        backend
            .request
            .extra_body
            .insert("max_completion_tokens".to_string(), json!(2048));
        finalize_request_for_backend(&mut backend, &policies);
        assert!(
            !backend.request.extra_body.contains_key("max_tokens"),
            "configured max_tokens must be shadowed by the request's max_completion_tokens alias: {:?}",
            backend.request.extra_body
        );
        assert_eq!(
            backend.request.extra_body["max_completion_tokens"],
            json!(2048),
            "request alias preserved"
        );
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
        // Same snapshot yields ids AND limits. Each of the FIVE alias keys gets
        // its own ISOLATED entry where it is the ONLY positive limit, so deleting
        // any single alias from `entry_context_limit` flips that entry's limit to
        // `None` and fails this test. A separate precedence entry locks the
        // search ORDER (`max_input_tokens` > `context_length` > `context_window`
        // > `max_context_length` > `max_model_len`). An entry with no positive
        // context length (zero, missing, or a bare string) keeps its id with a
        // `None` limit (budgeting no-ops for it, but it still resolves).
        let body = serde_json::json!({
            // Isolated positive coverage: one entry per alias key.
            "data": [
                {"id": "k_max_input_tokens", "max_input_tokens": 1000},
                {"id": "k_context_length", "context_length": 8192},
                {"id": "k_context_window", "context_window": 2048},
                {"id": "k_max_context_length", "max_context_length": 32768},
                {"id": "k_max_model_len", "max_model_len": 4096},
                // Precedence: every key positive but DIFFERENT, so the winner
                // pins the ordering (a lower-priority key winning would change
                // this value). `max_input_tokens` must win.
                {
                    "id": "precedence",
                    "max_input_tokens": 11,
                    "context_length": 22,
                    "context_window": 33,
                    "max_context_length": 44,
                    "max_model_len": 55,
                },
                // No positive limit -> None (still resolves as an id).
                {"id": "zero", "context_length": 0},
                {"id": "missing"},
                "bare",
            ]
        });

        assert_eq!(
            extract_supported_model_catalog(&body),
            vec![
                UpstreamModelEntry {
                    id: "k_max_input_tokens".to_string(),
                    context_limit: Some(1000)
                },
                UpstreamModelEntry {
                    id: "k_context_length".to_string(),
                    context_limit: Some(8192)
                },
                UpstreamModelEntry {
                    id: "k_context_window".to_string(),
                    context_limit: Some(2048)
                },
                UpstreamModelEntry {
                    id: "k_max_context_length".to_string(),
                    context_limit: Some(32768)
                },
                UpstreamModelEntry {
                    id: "k_max_model_len".to_string(),
                    context_limit: Some(4096)
                },
                UpstreamModelEntry {
                    id: "precedence".to_string(),
                    context_limit: Some(11)
                },
                UpstreamModelEntry {
                    id: "zero".to_string(),
                    context_limit: None
                },
                UpstreamModelEntry {
                    id: "missing".to_string(),
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
    fn extract_supported_model_catalog_handles_empty_body() {
        assert!(extract_supported_model_catalog(&serde_json::json!({})).is_empty());
    }

    /// A typed `stop` on the request counts as request-set, so a configured
    /// `stop` default must NOT gap-fill into `extra_body["stop"]`. Otherwise the
    /// wire would carry BOTH a typed `stop` and an `extra_body["stop"]` key
    /// (duplicate/conflicting), breaking request-wins. Covers the Anthropic path
    /// (stop_sequences → typed stop) sharing this merge with configured kwargs.
    #[test]
    fn merge_upstream_chat_kwargs_does_not_shadow_typed_stop() {
        let mut request = family_request("m");
        request.stop = Some(vec!["STOP".to_string()]);
        let defaults = JsonMap::from_iter([("stop".to_string(), json!(["CONFIGURED"]))]);

        merge_upstream_chat_kwargs(&mut request, &defaults);

        assert_eq!(request.stop, Some(vec!["STOP".to_string()]));
        assert!(!request.extra_body.contains_key("stop"));
    }

    /// Same guard for the provider-fallback kwargs merge path.
    #[test]
    fn merge_fallback_chat_kwargs_does_not_shadow_typed_stop() {
        let mut request = family_request("m");
        request.stop = Some(vec!["STOP".to_string()]);
        let defaults = JsonMap::from_iter([("stop".to_string(), json!(["CONFIGURED"]))]);

        merge_fallback_chat_kwargs(&mut request, &defaults);

        assert_eq!(request.stop, Some(vec!["STOP".to_string()]));
        assert!(!request.extra_body.contains_key("stop"));
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
                context_limit_by_id: HashMap::new(),
            }],
            union_entries: Vec::new(),
            union_ids: vec![model.to_string()],
            union_context_limit_by_id: HashMap::new(),
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
            union_context_limit_by_id: HashMap::new(),
            ids_by_key: HashMap::new(),
            routes: Vec::new(),
        };
        assert!(empty.resolve("anything").is_none());
    }
}
