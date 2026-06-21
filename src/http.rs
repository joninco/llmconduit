use crate::adapters::anthropic_to_responses;
use crate::adapters::chat_completions;
use crate::adapters::chat_completions::ChatCompletionCollector;
use crate::adapters::chat_completions::ChatCompletionStreamConverter;
use crate::adapters::responses_to_anthropic::AnthropicStreamCollector;
use crate::adapters::responses_to_anthropic::AnthropicStreamConverter;
use crate::dashboard_api::dashboard_catalog;
use crate::dashboard_api::dashboard_flow_detail;
use crate::dashboard_api::dashboard_flows;
use crate::dashboard_api::dashboard_metrics;
use crate::dashboard_api::dashboard_snapshot;
use crate::dashboard_api::dashboard_topology;
use crate::dashboard_auth::DashboardAuth;
use crate::dashboard_auth::MutationDenied;
use crate::dashboard_auth::MutationPolicy;
use crate::dashboard_auth::dashboard_login;
use crate::dashboard_auth::dashboard_logout;
use crate::dashboard_auth::require_session;
use crate::dashboard_ui::dashboard_asset;
use crate::dashboard_ui::dashboard_index;
use crate::dashboard_ws::dashboard_ws;
use crate::debug_ui::debug_app_js;
use crate::debug_ui::debug_index;
use crate::debug_ui::debug_ws;
use crate::engine::Gateway;
use crate::error::AppError;
use crate::error::AppResult;
use crate::models::anthropic::AnthropicRequest;
use crate::models::chat::ChatCompletionRequest;
use crate::models::responses::ResponsesRequest;
use crate::proxy_headers::header_name_eq;
use crate::proxy_headers::is_hop_by_hop_header;
use crate::upstream::collect_models_response;
use axum::Extension;
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::body::Bytes;
use axum::body::to_bytes;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::Request;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderName;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header;
use axum::middleware;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::response::Sse;
use axum::routing::MethodFilter;
use axum::routing::get;
use axum::routing::on;
use axum::routing::post;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

const API_LOG_BODY_LIMIT_BYTES: usize = 256 * 1024 * 1024;
const API_LOG_PAYLOAD_DUMP_LIMIT_BYTES: usize = 16 * 1024;
const API_LOG_PREVIEW_CHARS: usize = 160;
const UNKNOWN_MODEL_CREATED_AT: &str = "1970-01-01T00:00:00Z";

#[derive(Debug, Clone, Copy, Default)]
pub struct RouterOptions {
    pub with_debug_ui: bool,
    /// D7 startup gate: whether the protected `/debug` + `/dashboard` routes may
    /// be registered. `false` when the bind/secret configuration refuses them
    /// (e.g. non-loopback without a token + https origin). Independent of
    /// `with_debug_ui` so an operator sees a clear "refused to register" startup
    /// log rather than a silent 404. Only consulted when `with_debug_ui` is set.
    pub register_protected_routes: bool,
}

pub fn build_router(gateway: Arc<Gateway>, options: RouterOptions) -> Router {
    let router = Router::new()
        .route("/v1/responses", post(post_responses))
        .route("/v1/messages", post(post_messages))
        .route("/v1/messages", on(MethodFilter::HEAD, probe_messages))
        .route("/v1/messages", on(MethodFilter::OPTIONS, probe_messages))
        .route("/v1/chat/completions", post(post_chat_completions))
        .route("/v1/completions", post(post_completions))
        .route("/v1/models", get(get_models))
        .route("/health", get(get_health))
        .route("/", get(get_root));

    // D7: the debug UI + dashboard routes register only when `--with-debug-ui`
    // is set AND the startup decision permits it AND the env-built auth context
    // exists. All three hold or none of the protected routes appear (production
    // untouched; a misconfigured non-loopback server refuses rather than serving
    // transcripts/credentials in the clear).
    let router = match (
        options.with_debug_ui && options.register_protected_routes,
        gateway.dashboard_auth(),
    ) {
        (true, Some(auth)) => router.merge(protected_routes(auth)),
        _ => router,
    };

    router
        .fallback(api_not_found)
        .layer(middleware::from_fn_with_state(
            Arc::clone(&gateway),
            log_api_call,
        ))
        .with_state(gateway)
}

/// The D7-gated `/debug` + `/dashboard` sub-router (state `Arc<Gateway>`, merged
/// into the main router so it shares the outer `.with_state`).
///
/// Auth topology:
/// - `/dashboard/login` + `/dashboard/logout` — read the auth `Extension` (so
///   the handlers can sign/clear cookies) but are NOT behind `require_session`
///   (login is how you authenticate; logout must work for any state).
/// - `/dashboard` — auth `Extension` only; the shell handler serves the login
///   page vs. the SPA from `Option<AuthSession>` itself (no 401).
/// - `/dashboard/assets/{*path}` — public sub-resources (hashed, immutable); the
///   SPA shell behind them is already gated, and the asset bytes carry no
///   secrets.
/// - `/debug` + `/debug/app.js` — behind `require_session` (401 when unauthed).
/// - `/debug/ws` — self-gated inside the handler (cookie + `Origin` + `exp`); it
///   needs to OWN the rejection so the WS `Origin` check is authoritative.
///
/// The shared `Arc<DashboardAuth>` is attached as a request `Extension` scoped to
/// this sub-router so the middleware/handlers/extractors can read it
/// (`/debug/ws` reads it via `gateway.dashboard_auth()` instead).
fn protected_routes(auth: Arc<DashboardAuth>) -> Router<Arc<Gateway>> {
    // Routes requiring a valid session (401 when missing/expired/invalid). The
    // D13 `/dashboard/api/*` REST surface joins this group so every read AND the
    // kill mutation is 401'd for an unauthenticated caller BEFORE any handler work
    // (the kill's CSRF/mutation gate runs only for an authenticated request). The
    // handlers themselves stamp `no-store` + the dashboard security headers.
    let session_gated = Router::new()
        .route("/debug", get(debug_index))
        .route("/debug/app.js", get(debug_app_js))
        .route("/dashboard/api/flows", get(dashboard_flows))
        .route("/dashboard/api/flows/{id}", get(dashboard_flow_detail))
        .route("/dashboard/api/flows/{id}/kill", post(dashboard_flow_kill))
        .route("/dashboard/api/metrics", get(dashboard_metrics))
        .route("/dashboard/api/topology", get(dashboard_topology))
        .route("/dashboard/api/catalog", get(dashboard_catalog))
        .route("/dashboard/api/snapshot", get(dashboard_snapshot))
        .route_layer(middleware::from_fn(require_session));

    // Routes that read the auth context but manage their own access decision,
    // plus the self-gated WS and the public hashed assets.
    //
    // D7b: the dashboard data socket is a separate `/dashboard/ws` route carrying
    // the batched `DashboardFrame` envelope (Monitor/Usage/FlowStatus/MetricTick/
    // TopologyUpdate). Like `/debug/ws` it is SELF-gated inside the handler (cookie
    // + `Origin` allow-list + cookie-`exp` close, via D7a's `authenticate_ws`), so
    // it OWNS its rejection and the WS `Origin` check stays authoritative.
    let open = Router::new()
        .route("/dashboard", get(dashboard_index))
        .route("/dashboard/login", post(dashboard_login))
        .route("/dashboard/logout", post(dashboard_logout))
        .route("/debug/ws", get(debug_ws))
        .route("/dashboard/ws", get(dashboard_ws))
        .route("/dashboard/assets/{*path}", get(dashboard_asset));

    session_gated
        .merge(open)
        // Scope the auth context to ONLY the protected routes (not `/v1/*`).
        .layer(Extension(auth))
}

/// D6 — the outcome of a `POST /dashboard/api/flows/:id/kill` attempt, decoupled from
/// axum so the policy + abort logic is unit-testable against a MOCK [`MutationPolicy`]
/// (the spec's "compiles + tests against a mocked auth/CSRF gate"). The axum handler is
/// the only place that maps this to an HTTP status.
#[derive(Debug, PartialEq, Eq)]
pub enum FlowKillOutcome {
    /// A live token was found and cancelled → `200 OK`.
    Killed,
    /// No live flow for that `api_call_id` (unknown OR already finished) → `404`.
    NotFound,
    /// The mutation policy refused (mutations disabled, or CSRF missing/invalid) → the
    /// `MutationDenied` status (`403`). Carries the reason so the body can be precise.
    Denied(MutationDenied),
}

/// D6 — the pure kill core: authorize the mutation, then cancel the flow. Separated
/// from the axum handler so tests drive it with a mock `MutationPolicy` + a real
/// `Gateway` (no HTTP stack). CSRF/mutation gating runs FIRST (a refused mutation must
/// not even probe whether the id is live — no existence oracle for an unauthorized
/// caller); only an authorized request consults the AbortHub. `gateway.abort` is
/// idempotent, so a double-kill of a still-live flow simply re-cancels an
/// already-cancelled token (`true` both times until the guard removes it), and a kill
/// of a finished/unknown flow is `false` → 404.
pub fn flow_kill_outcome(
    policy: &dyn MutationPolicy,
    headers: &HeaderMap,
    gateway: &Gateway,
    api_call_id: &str,
) -> FlowKillOutcome {
    if let Err(denied) = policy.authorize_mutation(headers) {
        return FlowKillOutcome::Denied(denied);
    }
    if gateway.abort(api_call_id) {
        FlowKillOutcome::Killed
    } else {
        FlowKillOutcome::NotFound
    }
}

/// D6 — the `POST /dashboard/api/flows/:id/kill` handler. `:id` IS the flow's
/// `api_call_id` (the AbortHub key == the route param, no rekeying — spec decision), so
/// it cancels the live server-side stream: a `200` flips the flow's `CancellationToken`
/// and the engine's compose-with-`tx.closed()` sites surface `AppError::cancelled()`
/// (499) to the client while the L1 guard finalizes the record `Cancelled`; a `404`
/// means no live flow. The mutation+CSRF gate (`LLMCONDUIT_DASHBOARD_ALLOW_MUTATIONS` +
/// a double-submit CSRF token) is enforced via the shared `DashboardAuth`
/// [`MutationPolicy`] BEFORE any abort. Behind `require_session` when registered (D13),
/// so an unauthenticated caller is already 401'd before reaching here.
///
/// REGISTRATION is D13's job (this is the only mutation route in the phase; replay is
/// deferred). The handler is provided here so D6 ships the kill behavior + tests
/// independent of D13's route table (breaking the D6↔D13 cycle).
pub async fn dashboard_flow_kill(
    State(gateway): State<Arc<Gateway>>,
    Extension(auth): Extension<Arc<DashboardAuth>>,
    Path(api_call_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let response = match flow_kill_outcome(auth.as_ref(), &headers, gateway.as_ref(), &api_call_id)
    {
        // The 200 body MUST match the frozen `KillResponse {api_call_id, killed}`
        // (dashboard-frontend/src/api/types.ts) — the SPA decodes both fields.
        FlowKillOutcome::Killed => (
            StatusCode::OK,
            Json(serde_json::json!({"api_call_id": api_call_id, "killed": true})),
        )
            .into_response(),
        FlowKillOutcome::NotFound => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "no live flow for that id"})),
        )
            .into_response(),
        FlowKillOutcome::Denied(denied) => (
            denied.status(),
            Json(serde_json::json!({"error": denied.message()})),
        )
            .into_response(),
    };
    // Dashboard API responses are never cached (mutation result, auth-scoped).
    crate::dashboard_auth::no_store(response)
}

/// Whether a request is an instrumented inference flow (D1, incl. R1 #1): the
/// METHOD must be `POST` AND the path one of the three canonical inference entry
/// points. The method check matters because `/v1/messages` also serves HEAD/OPTIONS
/// probes — those (and any non-POST) must NOT open an orphan flow record.
/// `/v1/completions` is a raw upstream passthrough that bypasses the engine (never
/// instrumented); `/dashboard*`/`/debug*`/`/health`/`/`/`/v1/models` carry no flow.
fn is_flow_capture_request(method: &axum::http::Method, path: &str) -> bool {
    method == axum::http::Method::POST
        && matches!(
            path,
            "/v1/responses" | "/v1/messages" | "/v1/chat/completions"
        )
}

/// Whether `path` is a dashboard auth endpoint whose request body carries the
/// session secret (the login `{"token": ...}`; logout is bodyless but symmetric).
/// D7a R2 #1: the bare JSON key `token` is NOT in the global sensitive-key set
/// (too many legitimate `token` fields elsewhere), so the access token would leak
/// through the small-body `body_payload` dump. D7a R3 #1: a `body_sha256` + a
/// `body_bytes` length on a login body form an OFFLINE verification oracle — an
/// attacker with the logs can brute-force the token against the known digest and
/// length. So for these endpoints we suppress ALL body-derived fields (digest,
/// length, summary, AND payload), logging only non-body metadata.
fn is_dashboard_auth_path(path: &str) -> bool {
    matches!(path, "/dashboard/login" | "/dashboard/logout")
}

/// Body-derived tracing fields for the inbound-request log line. `None` for a
/// dashboard auth endpoint (D7a R3 #1): emitting the body length or its SHA-256
/// for a login body leaks an offline token-verification oracle, so an auth-path
/// request logs NO body-derived field at all (not the digest, length, summary,
/// nor — separately — the payload dump). `Some` for every other path carries the
/// length, hex digest, and the (already-redacted) summary.
struct BodyLogFields {
    bytes: usize,
    sha256: String,
    summary: String,
}

/// Compute the body-derived log fields for `path`/`body`, returning `None` for a
/// dashboard auth endpoint so the caller emits no body-derived field (D7a R3 #1
/// — the digest + length are a token-verification oracle).
fn body_log_fields(path: &str, body: &Bytes) -> Option<BodyLogFields> {
    if is_dashboard_auth_path(path) {
        return None;
    }
    Some(BodyLogFields {
        bytes: body.len(),
        sha256: hex::encode(Sha256::digest(body)),
        summary: summarize_api_body(path, body),
    })
}

async fn log_api_call(
    State(gateway): State<Arc<Gateway>>,
    request: Request,
    next: Next,
) -> Response {
    let api_call_id = format!("api_{}", Uuid::new_v4().simple());
    let method = request.method().clone();
    let uri = request.uri().clone();
    let headers = request.headers().clone();
    let started_at = Instant::now();

    let (mut parts, body) = request.into_parts();
    let body_bytes = match to_bytes(body, API_LOG_BODY_LIMIT_BYTES).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(
                api_call_id = %api_call_id,
                method = %method,
                path = %uri.path(),
                error = %err,
                "failed to read inbound API request body"
            );
            return (
                StatusCode::BAD_REQUEST,
                format!("failed to read request body: {err}"),
            )
                .into_response();
        }
    };

    // D7a R3 #1: for a dashboard auth endpoint (login/logout) NO body-derived
    // field may be logged — a `body_sha256` + `body_bytes` length on the login
    // body is an offline token-verification oracle. `body_log_fields` returns
    // `None` there so we emit only non-body metadata; every other path logs the
    // length, hex digest, and the redacted summary.
    let is_auth_path = is_dashboard_auth_path(uri.path());
    match body_log_fields(uri.path(), &body_bytes) {
        Some(fields) => tracing::info!(
            api_call_id = %api_call_id,
            method = %method,
            path = %uri.path(),
            query = uri.query().unwrap_or(""),
            content_type = %header_for_log(&headers, header::CONTENT_TYPE.as_str()),
            user_agent = %header_for_log(&headers, header::USER_AGENT.as_str()),
            anthropic_version = %header_for_log(&headers, "anthropic-version"),
            anthropic_beta = %header_for_log(&headers, "anthropic-beta"),
            openai_beta = %header_for_log(&headers, "openai-beta"),
            request_id = %header_for_log(&headers, "x-request-id"),
            authorization_present = headers.contains_key(header::AUTHORIZATION),
            x_api_key_present = headers.contains_key("x-api-key"),
            body_bytes = fields.bytes,
            body_sha256 = %fields.sha256,
            body_summary = %fields.summary,
            "inbound API request"
        ),
        // Auth endpoint: log only non-body metadata (no length, digest, summary).
        None => tracing::info!(
            api_call_id = %api_call_id,
            method = %method,
            path = %uri.path(),
            query = uri.query().unwrap_or(""),
            content_type = %header_for_log(&headers, header::CONTENT_TYPE.as_str()),
            user_agent = %header_for_log(&headers, header::USER_AGENT.as_str()),
            anthropic_version = %header_for_log(&headers, "anthropic-version"),
            anthropic_beta = %header_for_log(&headers, "anthropic-beta"),
            openai_beta = %header_for_log(&headers, "openai-beta"),
            request_id = %header_for_log(&headers, "x-request-id"),
            authorization_present = headers.contains_key(header::AUTHORIZATION),
            x_api_key_present = headers.contains_key("x-api-key"),
            "inbound API request"
        ),
    }
    // Never dump the auth-endpoint body (it carries the token, and even its
    // length/digest are an oracle — handled above).
    if !is_auth_path && body_bytes.len() <= API_LOG_PAYLOAD_DUMP_LIMIT_BYTES {
        tracing::info!(
            api_call_id = %api_call_id,
            method = %method,
            path = %uri.path(),
            body_payload = %payload_for_log(&body_bytes),
            "inbound API request payload"
        );
    }

    // D1 (incl. R1 #1/#6): only when the FlowStore is ENABLED AND this is an
    // instrumented inference flow (POST + whitelisted path) do we (a) stash the
    // `api_call_id` extension the engine reads to link `response_id → api_call_id`,
    // and (b) capture the inbound body + headers and open the record. Gating BOTH on
    // the same condition keeps the disabled production path zero-cost (no clone, no
    // extension insert) and prevents HEAD/OPTIONS probes or non-whitelisted paths
    // from opening an orphan record. Secrets (auth headers, `api_key`, image URIs)
    // are redacted INLINE by the serializer/header redactor — none persist here.
    // D3 L0: the RAII middleware guard. `None` for disabled-store / non-whitelisted
    // requests (zero overhead). When `Some`, it is held across `next.run`: if the
    // request never reaches the engine (an extractor/`Json` rejection, a layer panic
    // above the handler) the record is still `OpenL0` at the guard's `Drop`, which
    // CASes it to `Finalized` + `Failed("unhandled")` — no orphan stuck `Open`. If
    // the engine claimed it (`ClaimedL1`), the L0 `Drop` is inert and L1 owns
    // finalization.
    let _l0_guard =
        if gateway.flow_store().is_enabled() && is_flow_capture_request(&method, uri.path()) {
            parts
                .extensions
                .insert(crate::dashboard_flow::ApiCallId(api_call_id.clone()));
            let inbound_body = Some(crate::dashboard_flow::capture_body(&body_bytes));
            let headers_redacted = crate::dashboard_flow::redact_headers(&headers);
            gateway.flow_store().open(
                api_call_id.clone(),
                method.to_string(),
                uri.path().to_string(),
                headers_redacted,
                inbound_body,
            );
            gateway.flow_store().middleware_guard(&api_call_id)
        } else {
            None
        };

    let request = Request::from_parts(parts, Body::from(body_bytes));
    let response = next.run(request).await;
    // Per-request model-resolution audit: the handler tags the response with the
    // served model (and the requested model when it differs) via
    // `with_model_headers`; echo both here so every response record shows whether
    // a model fell back — un-throttled, unlike the engine WARN. `requested_model`
    // is empty when the requested model was served as-is.
    let served_model = header_for_log(response.headers(), "x-llmconduit-model").to_string();
    let requested_model = header_for_log(response.headers(), "x-llmconduit-requested").to_string();
    tracing::info!(
        api_call_id = %api_call_id,
        method = %method,
        path = %uri.path(),
        status = response.status().as_u16(),
        served_model = %served_model,
        requested_model = %requested_model,
        elapsed_ms = started_at.elapsed().as_millis(),
        "inbound API response prepared"
    );
    response
}

async fn api_not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

fn probe_response(allow: &str) -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        header::ALLOW,
        HeaderValue::try_from(allow).expect("valid header value"),
    );
    response
}

async fn probe_messages() -> Response {
    probe_response("POST, HEAD, OPTIONS")
}

async fn get_health() -> Response {
    (
        StatusCode::OK,
        Json(serde_json::json!({"status": "healthy"})),
    )
        .into_response()
}

async fn get_root() -> Response {
    (StatusCode::OK, Json(serde_json::json!({"status": "ok"}))).into_response()
}

fn header_for_log(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(compact_for_log)
        .unwrap_or_default()
}

fn summarize_api_body(path: &str, body: &Bytes) -> String {
    if body.is_empty() {
        return "empty".to_string();
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(value) => summarize_json_api_body(path, &value),
        Err(err) => {
            // Redact image URIs from the raw preview before logging (round-4 #2):
            // a non-JSON body could still embed a `data:`/signed image URL.
            let preview = crate::redaction::redact_image_uris(&String::from_utf8_lossy(body));
            format!(
                "non_json parse_error={} preview={}",
                compact_for_log(&err.to_string()),
                compact_for_log(&preview)
            )
        }
    }
}

fn payload_for_log(body: &Bytes) -> String {
    match serde_json::from_slice::<Value>(body) {
        Ok(mut value) => {
            redact_payload_secrets(&mut value);
            // G4 round-4 #2: an inbound body under the dump limit would otherwise
            // log raw `data:` image bytes / signed `image_url`s. Strip image URIs
            // from every remaining string via the shared redactor BEFORE
            // serializing, so no logged surface carries request image content.
            crate::redaction::redact_image_uris_in_value(&mut value);
            serde_json::to_string(&value)
                .unwrap_or_else(|_| "<failed to serialize json>".to_string())
        }
        Err(_) => {
            // Non-JSON body: still strip image URIs from the raw text so a
            // `data:`/signed URL in a malformed/odd payload is not logged raw.
            crate::redaction::redact_image_uris(&String::from_utf8_lossy(body))
        }
    }
}

fn redact_payload_secrets(value: &mut Value) {
    // Single sensitive-key authority lives in `crate::redaction` (D1 R1 #10); this
    // logging surface routes through it so the secret-key set has one definition.
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if crate::redaction::is_sensitive_payload_key(key) {
                    *value = Value::String("[redacted]".to_string());
                } else {
                    redact_payload_secrets(value);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_payload_secrets(value);
            }
        }
        _ => {}
    }
}

fn summarize_json_api_body(path: &str, value: &Value) -> String {
    let Some(map) = value.as_object() else {
        return format!("json_type={}", json_type(value));
    };

    let mut parts = Vec::new();
    parts.push(format!(
        "keys={}",
        summarized_list(map.keys().cloned().collect(), 24)
    ));
    append_common_json_fields(&mut parts, map);

    if path.contains("/messages") {
        append_anthropic_json_summary(&mut parts, map);
    } else if path.contains("/responses") {
        append_responses_json_summary(&mut parts, map);
    } else if path.contains("/chat/completions") || path.ends_with("/completions") {
        append_chat_json_summary(&mut parts, map);
    } else {
        append_generic_json_summary(&mut parts, map);
    }

    parts.join(" ")
}

fn append_common_json_fields(parts: &mut Vec<String>, map: &serde_json::Map<String, Value>) {
    for key in [
        "model",
        "stream",
        "max_tokens",
        "max_output_tokens",
        "max_completion_tokens",
        "store",
        "parallel_tool_calls",
        "temperature",
        "top_p",
    ] {
        append_scalar_field(parts, map, key);
    }
    append_typed_field(parts, map, "tool_choice");
    append_typed_field(parts, map, "thinking");
    append_typed_field(parts, map, "reasoning");
}

fn append_anthropic_json_summary(parts: &mut Vec<String>, map: &serde_json::Map<String, Value>) {
    append_anthropic_system_summary(parts, map.get("system"));
    if let Some(messages) = map.get("messages").and_then(Value::as_array) {
        parts.push(format!("messages={}", messages.len()));
        append_anthropic_message_summary(parts, messages);
    }
    if let Some(tools) = map.get("tools").and_then(Value::as_array) {
        append_tool_summary(parts, "tools", tools);
    }
    append_metadata_summary(parts, map.get("metadata"));
    append_array_len(parts, map, "stop_sequences");
}

fn append_responses_json_summary(parts: &mut Vec<String>, map: &serde_json::Map<String, Value>) {
    if let Some(instructions) = map.get("instructions").and_then(Value::as_str) {
        parts.push(format!(
            "instructions_chars={}",
            instructions.chars().count()
        ));
    }
    match map.get("input") {
        Some(Value::String(text)) => {
            parts.push("input=string".to_string());
            parts.push(format!("input_chars={}", text.chars().count()));
        }
        Some(Value::Array(items)) => {
            parts.push(format!("input_items={}", items.len()));
            append_responses_input_summary(parts, items);
        }
        Some(other) => {
            parts.push(format!("input_type={}", json_type(other)));
        }
        None => {}
    }
    if let Some(tools) = map.get("tools").and_then(Value::as_array) {
        append_tool_summary(parts, "tools", tools);
    }
    append_array_len(parts, map, "include");
    append_metadata_summary(parts, map.get("metadata"));
}

fn append_chat_json_summary(parts: &mut Vec<String>, map: &serde_json::Map<String, Value>) {
    if let Some(messages) = map.get("messages").and_then(Value::as_array) {
        parts.push(format!("messages={}", messages.len()));
        append_chat_message_summary(parts, messages);
    }
    if let Some(tools) = map.get("tools").and_then(Value::as_array) {
        append_tool_summary(parts, "tools", tools);
    }
    append_typed_field(parts, map, "response_format");
    append_typed_field(parts, map, "stream_options");
}

fn append_generic_json_summary(parts: &mut Vec<String>, map: &serde_json::Map<String, Value>) {
    if let Some(messages) = map.get("messages").and_then(Value::as_array) {
        parts.push(format!("messages={}", messages.len()));
        append_chat_message_summary(parts, messages);
    }
    match map.get("input") {
        Some(Value::String(text)) => {
            parts.push("input=string".to_string());
            parts.push(format!("input_chars={}", text.chars().count()));
        }
        Some(Value::Array(items)) => {
            parts.push(format!("input_items={}", items.len()));
        }
        _ => {}
    }
    if let Some(tools) = map.get("tools").and_then(Value::as_array) {
        append_tool_summary(parts, "tools", tools);
    }
}

fn append_anthropic_system_summary(parts: &mut Vec<String>, system: Option<&Value>) {
    match system {
        Some(Value::String(text)) => {
            parts.push("system=string".to_string());
            parts.push(format!("system_chars={}", text.chars().count()));
        }
        Some(Value::Array(blocks)) => {
            let mut text_chars = 0usize;
            let mut counts = BTreeMap::new();
            for block in blocks {
                let kind = typed_json_value(block);
                increment_count(&mut counts, kind);
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    text_chars += text.chars().count();
                }
            }
            parts.push(format!("system_blocks={}", blocks.len()));
            parts.push(format!("system_chars={text_chars}"));
            push_counts(parts, "system_block_types", &counts);
        }
        Some(other) => {
            parts.push(format!("system_type={}", json_type(other)));
        }
        None => {}
    }
}

fn append_anthropic_message_summary(parts: &mut Vec<String>, messages: &[Value]) {
    let mut roles = Vec::new();
    let mut content_counts = BTreeMap::new();
    let mut text_chars = 0usize;

    for message in messages {
        roles.push(
            message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
        );
        if let Some(content) = message.get("content") {
            accumulate_anthropic_content(content, &mut text_chars, &mut content_counts);
        }
    }

    parts.push(format!("message_roles={}", summarized_list(roles, 16)));
    parts.push(format!("message_text_chars={text_chars}"));
    push_counts(parts, "message_content", &content_counts);
}

fn append_chat_message_summary(parts: &mut Vec<String>, messages: &[Value]) {
    let mut roles = Vec::new();
    let mut content_counts = BTreeMap::new();
    let mut text_chars = 0usize;
    let mut tool_calls = 0usize;

    for message in messages {
        roles.push(
            message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
        );
        if let Some(content) = message.get("content") {
            accumulate_chat_content(content, &mut text_chars, &mut content_counts);
        }
        tool_calls += message
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
    }

    parts.push(format!("message_roles={}", summarized_list(roles, 16)));
    parts.push(format!("message_text_chars={text_chars}"));
    parts.push(format!("message_tool_calls={tool_calls}"));
    push_counts(parts, "message_content", &content_counts);
}

fn append_responses_input_summary(parts: &mut Vec<String>, items: &[Value]) {
    let mut roles = Vec::new();
    let mut item_counts = BTreeMap::new();
    let mut text_chars = 0usize;

    for item in items {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_else(|| {
            if item.get("role").is_some() {
                "message"
            } else {
                "unknown"
            }
        });
        increment_count(&mut item_counts, item_type.to_string());
        if let Some(role) = item.get("role").and_then(Value::as_str) {
            roles.push(role.to_string());
        }
        if let Some(content) = item.get("content") {
            accumulate_responses_content(content, &mut text_chars);
        }
    }

    parts.push(format!("input_roles={}", summarized_list(roles, 16)));
    parts.push(format!("input_text_chars={text_chars}"));
    push_counts(parts, "input_item_types", &item_counts);
}

fn accumulate_anthropic_content(
    content: &Value,
    text_chars: &mut usize,
    counts: &mut BTreeMap<String, usize>,
) {
    match content {
        Value::String(text) => {
            *text_chars += text.chars().count();
            increment_count(counts, "string".to_string());
        }
        Value::Array(blocks) => {
            for block in blocks {
                let kind = typed_json_value(block);
                increment_count(counts, kind.clone());
                match kind.as_str() {
                    "text" => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            *text_chars += text.chars().count();
                        }
                    }
                    "thinking" => {
                        if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                            *text_chars += text.chars().count();
                        }
                    }
                    "tool_result" => {
                        if let Some(nested) = block.get("content") {
                            accumulate_anthropic_content(nested, text_chars, counts);
                        }
                    }
                    _ => {}
                }
            }
        }
        other => {
            increment_count(counts, json_type(other).to_string());
        }
    }
}

fn accumulate_chat_content(
    content: &Value,
    text_chars: &mut usize,
    counts: &mut BTreeMap<String, usize>,
) {
    match content {
        Value::String(text) => {
            *text_chars += text.chars().count();
            increment_count(counts, "string".to_string());
        }
        Value::Array(parts) => {
            for part in parts {
                let kind = typed_json_value(part);
                increment_count(counts, kind.clone());
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    *text_chars += text.chars().count();
                }
            }
        }
        other => {
            increment_count(counts, json_type(other).to_string());
        }
    }
}

fn accumulate_responses_content(content: &Value, text_chars: &mut usize) {
    match content {
        Value::String(text) => {
            *text_chars += text.chars().count();
        }
        Value::Array(parts) => {
            for part in parts {
                for key in ["text", "input_text", "output_text"] {
                    if let Some(text) = part.get(key).and_then(Value::as_str) {
                        *text_chars += text.chars().count();
                    }
                }
            }
        }
        _ => {}
    }
}

fn append_tool_summary(parts: &mut Vec<String>, label: &str, tools: &[Value]) {
    let names = tools
        .iter()
        .filter_map(tool_name_for_summary)
        .collect::<Vec<_>>();
    parts.push(format!("{label}={}", tools.len()));
    if !names.is_empty() {
        parts.push(format!("{label}_names={}", summarized_list(names, 12)));
    }
}

fn tool_name_for_summary(tool: &Value) -> Option<String> {
    tool.get("name")
        .and_then(Value::as_str)
        .or_else(|| {
            tool.get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
        })
        .map(ToString::to_string)
}

fn append_metadata_summary(parts: &mut Vec<String>, metadata: Option<&Value>) {
    if let Some(Value::Object(map)) = metadata {
        parts.push(format!(
            "metadata_keys={}",
            summarized_list(map.keys().cloned().collect(), 16)
        ));
    }
}

fn append_array_len(parts: &mut Vec<String>, map: &serde_json::Map<String, Value>, key: &str) {
    if let Some(values) = map.get(key).and_then(Value::as_array) {
        parts.push(format!("{key}={}", values.len()));
    }
}

fn append_scalar_field(parts: &mut Vec<String>, map: &serde_json::Map<String, Value>, key: &str) {
    if let Some(value) = map.get(key).and_then(scalar_for_log) {
        parts.push(format!("{key}={value}"));
    }
}

fn append_typed_field(parts: &mut Vec<String>, map: &serde_json::Map<String, Value>, key: &str) {
    if let Some(value) = map.get(key) {
        parts.push(format!("{key}={}", typed_json_value(value)));
    }
}

fn typed_json_value(value: &Value) -> String {
    match value {
        Value::Object(map) => map
            .get("type")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| "object".to_string()),
        Value::Array(_) => "array".to_string(),
        Value::String(text) => compact_for_log(text),
        other => json_type(other).to_string(),
    }
}

fn scalar_for_log(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(compact_for_log(text)),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Null => Some("null".to_string()),
        Value::Array(_) | Value::Object(_) => None,
    }
}

fn push_counts(parts: &mut Vec<String>, label: &str, counts: &BTreeMap<String, usize>) {
    if counts.is_empty() {
        return;
    }
    let values = counts
        .iter()
        .map(|(key, count)| format!("{key}:{count}"))
        .collect::<Vec<_>>();
    parts.push(format!("{label}={}", summarized_list(values, 16)));
}

fn increment_count(counts: &mut BTreeMap<String, usize>, key: String) {
    *counts.entry(key).or_default() += 1;
}

fn summarized_list(mut values: Vec<String>, max: usize) -> String {
    let total = values.len();
    values.truncate(max);
    if total > max {
        values.push(format!("+{}", total - max));
    }
    format!("[{}]", values.join(","))
}

fn compact_for_log(value: &str) -> String {
    let mut compact = String::new();
    for ch in value.chars().take(API_LOG_PREVIEW_CHARS) {
        if ch.is_control() {
            compact.push(' ');
        } else {
            compact.push(ch);
        }
    }
    if value.chars().count() > API_LOG_PREVIEW_CHARS {
        compact.push_str("...");
    }
    compact
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

async fn post_responses(
    State(gateway): State<Arc<Gateway>>,
    api_call_id: Option<axum::Extension<crate::dashboard_flow::ApiCallId>>,
    Json(request): Json<ResponsesRequest>,
) -> AppResult<Response> {
    let requested = request.model.clone();
    let served = gateway.resolve_request_model(&request.model).await.0;
    let wants_stream = request.stream;
    let stream = gateway
        .stream_responses_with_api_call_id(request, api_call_id.map(|extension| extension.0.0))
        .await?;
    let response = if wants_stream {
        stream_responses_response(stream)
    } else {
        collect_responses_response(stream).await?
    };
    Ok(with_model_headers(response, &requested, &served))
}

async fn post_messages(
    State(gateway): State<Arc<Gateway>>,
    api_call_id: Option<axum::Extension<crate::dashboard_flow::ApiCallId>>,
    Json(request): Json<AnthropicRequest>,
) -> Response {
    let api_call_id = api_call_id.map(|extension| extension.0.0);
    match handle_post_messages(gateway, request, api_call_id).await {
        Ok(response) => response,
        Err(err) => anthropic_error_response(err),
    }
}

async fn post_chat_completions(
    State(gateway): State<Arc<Gateway>>,
    api_call_id: Option<axum::Extension<crate::dashboard_flow::ApiCallId>>,
    Json(request): Json<ChatCompletionRequest>,
) -> AppResult<Response> {
    let requested = request.model.clone();
    let model = gateway.resolve_request_model(&request.model).await.0;
    let wants_stream = request.stream;
    let include_usage = request
        .stream_options
        .as_ref()
        .is_some_and(|options| options.include_usage);
    // Decide BEFORE converting (which consumes the inbound request) whether the
    // Chat output converter must suppress `reasoning_content`. Suppression is
    // family-independent: a Chat client that did not request reasoning never
    // receives server-side chain-of-thought from ANY model (G2, Finding 1).
    let suppress_reasoning = gateway.chat_reasoning_suppressed(&request);
    let responses_request = chat_completions::convert_request(request)?;
    let stream = gateway
        .stream_responses_with_api_call_id(
            responses_request,
            api_call_id.map(|extension| extension.0.0),
        )
        .await?;

    let response = if wants_stream {
        stream_chat_completions_response(model.clone(), include_usage, suppress_reasoning, stream)
    } else {
        collect_chat_completions_response(model.clone(), suppress_reasoning, stream).await?
    };
    Ok(with_model_headers(response, &requested, &model))
}

async fn post_completions(
    State(gateway): State<Arc<Gateway>>,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<Response> {
    let response = gateway
        .upstream_client()
        .proxy_completions(headers, body)
        .await?;
    Ok(proxy_upstream_response(response))
}

async fn handle_post_messages(
    gateway: Arc<Gateway>,
    request: AnthropicRequest,
    api_call_id: Option<String>,
) -> AppResult<Response> {
    let requested = request.model.clone();
    let model = gateway.resolve_request_model(&request.model).await.0;
    let wants_stream = request.stream;
    let responses_request = anthropic_to_responses::convert_request(request)?;
    let stream = gateway
        .stream_responses_with_api_call_id(responses_request, api_call_id)
        .await?;

    let response = if wants_stream {
        stream_anthropic_response(model.clone(), stream)?
    } else {
        collect_anthropic_response(model.clone(), stream).await?
    };
    Ok(with_model_headers(response, &requested, &model))
}

/// Tag a response with the model that actually served it, so a model mismatch
/// (requested model not served → fell back to the loaded model) is visible
/// PER-REQUEST to anyone inspecting the response (`curl -v`, a proxy, or
/// response logging) without tailing the throttled engine WARN. The
/// `x-llmconduit-requested` header is added ONLY when the requested model
/// differs from the served one (the mismatch signal); an exact/canonical match
/// omits it to keep the common case quiet. The `log_api_call` middleware echoes
/// both headers into the per-request "response prepared" log line.
fn with_model_headers(mut response: Response, requested: &str, served: &str) -> Response {
    let headers = response.headers_mut();
    if !served.is_empty()
        && let Ok(value) = HeaderValue::from_str(served)
    {
        headers.insert(HeaderName::from_static("x-llmconduit-model"), value);
    }
    if !requested.is_empty()
        && !requested.eq_ignore_ascii_case(served)
        && let Ok(value) = HeaderValue::from_str(requested)
    {
        headers.insert(HeaderName::from_static("x-llmconduit-requested"), value);
    }
    response
}

fn stream_chat_completions_response(
    model: String,
    include_usage: bool,
    suppress_reasoning: bool,
    stream: ReceiverStream<crate::engine::SseEvent>,
) -> Response {
    let (tx, rx) = mpsc::channel(128);
    tokio::spawn(async move {
        let mut converter = ChatCompletionStreamConverter::with_reasoning_suppression(
            model,
            include_usage,
            suppress_reasoning,
        );
        let mut stream = std::pin::pin!(stream);
        'streaming: while let Some(event) = stream.next().await {
            let chat_events = converter.convert(&event);
            for chat_event in chat_events {
                if tx.send(chat_event).await.is_err() {
                    break 'streaming;
                }
            }
        }
    });

    let mapped = ReceiverStream::new(rx).map(|event| {
        Ok::<_, Infallible>(axum::response::sse::Event::default().data(event.to_sse_data()))
    });

    let mut response = Sse::new(mapped)
        .keep_alive(axum::response::sse::KeepAlive::new())
        .into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    response.headers_mut().insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    response
}

fn stream_anthropic_response(
    model: String,
    stream: ReceiverStream<crate::engine::SseEvent>,
) -> AppResult<Response> {
    let (tx, rx) = mpsc::channel(128);
    tokio::spawn(async move {
        let mut converter = AnthropicStreamConverter::new(model);
        let mut stream = std::pin::pin!(stream);
        while let Some(event) = stream.next().await {
            let anthropic_events = converter.convert(&event);
            for anthropic_event in anthropic_events {
                if tx.send(anthropic_event).await.is_err() {
                    return;
                }
            }
        }
        // The upstream event stream ended. If it never produced a
        // `response.completed` (engine error, dropped/stalled turn, aborted
        // web-search round-trip), emit a terminal `message_delta` +
        // `message_stop` so the client is not left hanging behind the SSE
        // keep-alive forever.
        for anthropic_event in converter.finalize() {
            if tx.send(anthropic_event).await.is_err() {
                return;
            }
        }
    });

    let mapped = ReceiverStream::new(rx).map(|event| {
        Ok::<_, Infallible>(
            axum::response::sse::Event::default()
                .event(event.sse_event_type())
                .data(event.to_json()),
        )
    });

    let mut response = Sse::new(mapped)
        .keep_alive(axum::response::sse::KeepAlive::new())
        .into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    response.headers_mut().insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    Ok(response)
}

fn proxy_upstream_response(response: reqwest::Response) -> Response {
    let status = response.status();
    let upstream_headers = response.headers().clone();
    let mut builder = Response::builder().status(status);
    if let Some(headers) = builder.headers_mut() {
        copy_proxy_response_headers(&upstream_headers, headers);
    }
    builder
        .body(Body::from_stream(response.bytes_stream()))
        .expect("valid upstream proxy response")
}

fn copy_proxy_response_headers(source: &HeaderMap, target: &mut HeaderMap) {
    for (name, value) in source {
        if should_proxy_response_header(name) {
            target.append(name.clone(), value.clone());
        }
    }
}

fn should_proxy_response_header(name: &HeaderName) -> bool {
    !is_hop_by_hop_header(name) && !header_name_eq(name, "content-length")
}

fn stream_responses_response(stream: ReceiverStream<crate::engine::SseEvent>) -> Response {
    let mapped = stream.map(|event| {
        Ok::<_, Infallible>(
            axum::response::sse::Event::default()
                .event(event.event)
                .data(event.data.to_string()),
        )
    });
    let mut response = Sse::new(mapped)
        .keep_alive(axum::response::sse::KeepAlive::new())
        .into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    response.headers_mut().insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    response
}

async fn collect_responses_response(
    stream: ReceiverStream<crate::engine::SseEvent>,
) -> AppResult<Response> {
    let mut final_payload: Option<Value> = None;
    let mut stream = std::pin::pin!(stream);
    while let Some(event) = stream.next().await {
        match event.event.as_str() {
            "response.completed" | "response.incomplete" => {
                final_payload = event.data.get("response").cloned();
            }
            "response.failed" => {
                let message = event
                    .data
                    .get("response")
                    .and_then(|response| response.get("error"))
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("upstream request failed");
                return Err(AppError::upstream(message));
            }
            _ => {}
        }
    }

    match final_payload {
        Some(payload) => Ok(Json(payload).into_response()),
        None => Err(AppError::upstream(
            "stream ended before a final response resource was emitted",
        )),
    }
}

async fn collect_chat_completions_response(
    model: String,
    suppress_reasoning: bool,
    stream: ReceiverStream<crate::engine::SseEvent>,
) -> AppResult<Response> {
    let mut collector =
        ChatCompletionCollector::with_reasoning_suppression(model, suppress_reasoning);
    let mut stream = std::pin::pin!(stream);
    while let Some(event) = stream.next().await {
        collector.process(&event);
    }
    Ok(Json(collector.into_response()?).into_response())
}

async fn collect_anthropic_response(
    model: String,
    stream: ReceiverStream<crate::engine::SseEvent>,
) -> AppResult<Response> {
    let mut collector = AnthropicStreamCollector::new(model);
    let mut stream = std::pin::pin!(stream);
    while let Some(event) = stream.next().await {
        collector.process(&event);
    }
    match collector.into_response() {
        Ok(msg) => Ok(Json(msg).into_response()),
        Err(err) => Ok(anthropic_error_response(AppError::upstream(err.message))),
    }
}

fn anthropic_error_response(err: AppError) -> Response {
    let status = err.status_code();
    let error_type = match err.status_code() {
        axum::http::StatusCode::BAD_REQUEST => "invalid_request_error",
        axum::http::StatusCode::CONFLICT => "invalid_request_error",
        _ => "api_error",
    };
    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": error_type,
            "message": err.to_string(),
        }
    });
    (status, Json(body)).into_response()
}

#[derive(Debug, Default, Deserialize)]
struct ModelsListQuery {
    after_id: Option<String>,
    before_id: Option<String>,
    limit: Option<String>,
}

async fn get_models(
    headers: HeaderMap,
    Query(query): Query<ModelsListQuery>,
    State(gateway): State<Arc<Gateway>>,
) -> AppResult<Response> {
    let anthropic_models = is_anthropic_models_request(&headers);
    let response = gateway.upstream_client().list_models().await?;
    let (status, body, etag) = collect_models_response(response).await?;
    let body = if anthropic_models {
        transform_models_response_for_anthropic(body, &query)?
    } else {
        body
    };
    let mut headers = HeaderMap::new();
    if !anthropic_models && let Some(etag) = etag {
        headers.insert(
            http::header::ETAG,
            HeaderValue::from_str(&etag)
                .map_err(|err| AppError::internal(format!("invalid ETag header: {err}")))?,
        );
    }
    Ok((status, headers, Json(body)).into_response())
}

fn is_anthropic_models_request(headers: &HeaderMap) -> bool {
    headers.contains_key("anthropic-version") || headers.contains_key("anthropic-beta")
}

fn transform_models_response_for_anthropic(
    body: Value,
    query: &ModelsListQuery,
) -> AppResult<Value> {
    if query.after_id.is_some() && query.before_id.is_some() {
        return Err(AppError::bad_request(
            "after_id and before_id cannot both be specified",
        ));
    }

    let limit = parse_anthropic_models_limit(query.limit.as_deref())?;
    let models = extract_model_entries(&body)
        .into_iter()
        .filter_map(|entry| anthropic_model_entry(&entry))
        .collect::<Vec<_>>();

    let (page, has_more) = page_anthropic_models(&models, query, limit)?;
    let first_id = page
        .first()
        .and_then(model_id_from_value)
        .map(Value::String)
        .unwrap_or(Value::Null);
    let last_id = page
        .last()
        .and_then(model_id_from_value)
        .map(Value::String)
        .unwrap_or(Value::Null);

    Ok(serde_json::json!({
        "data": page,
        "first_id": first_id,
        "has_more": has_more,
        "last_id": last_id,
    }))
}

fn parse_anthropic_models_limit(limit: Option<&str>) -> AppResult<usize> {
    match limit {
        Some(raw) => {
            let parsed = raw
                .parse::<usize>()
                .map_err(|_| AppError::bad_request("limit must be an integer from 1 to 1000"))?;
            if !(1..=1000).contains(&parsed) {
                return Err(AppError::bad_request("limit must be between 1 and 1000"));
            }
            Ok(parsed)
        }
        None => Ok(20),
    }
}

fn extract_model_entries(body: &Value) -> Vec<Value> {
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

fn anthropic_model_entry(entry: &Value) -> Option<Value> {
    match entry {
        Value::String(id) => {
            let caps = infer_capabilities_from_model_id(id);
            Some(build_anthropic_model_entry(
                id,
                id,
                UNKNOWN_MODEL_CREATED_AT,
                None,
                None,
                Some(&caps),
            ))
        }
        Value::Object(map) => {
            let id = map.get("id").and_then(Value::as_str)?;
            let display_name = map
                .get("display_name")
                .and_then(Value::as_str)
                .or_else(|| map.get("id").and_then(Value::as_str))
                .unwrap_or(id);
            let created_at =
                parse_created_at(map).unwrap_or_else(|| UNKNOWN_MODEL_CREATED_AT.to_string());

            let max_input_tokens = map
                .get("max_input_tokens")
                .or_else(|| map.get("context_length"))
                .or_else(|| map.get("context_window"))
                .or_else(|| map.get("max_context_length"))
                .or_else(|| map.get("max_model_len"));
            let max_tokens = map
                .get("max_tokens")
                .or_else(|| map.get("max_output_tokens"));
            let capabilities = map
                .get("capabilities")
                .filter(|value| value.is_object())
                .cloned()
                .unwrap_or_else(|| infer_capabilities_from_model_id(id));

            Some(build_anthropic_model_entry(
                id,
                display_name,
                &created_at,
                max_input_tokens,
                max_tokens,
                Some(&capabilities),
            ))
        }
        _ => None,
    }
}

/// Parse a creation timestamp from common upstream formats.
///
/// - `created_at` → ISO 8601 string (passed through)
/// - `created`    → Unix epoch integer / float (⇒ ISO 8601 string)
fn parse_created_at(map: &serde_json::Map<String, Value>) -> Option<String> {
    match map.get("created_at").and_then(Value::as_str) {
        Some(iso) => Some(iso.to_string()),
        None => {
            let epoch = map.get("created").and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_f64().and_then(|f| (f as u64).checked_add(0)))
            })?;
            chrono::DateTime::from_timestamp(epoch as i64, 0)
                .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        }
    }
}

fn infer_capabilities_from_model_id(_id: &str) -> Value {
    default_anthropic_model_capabilities()
}

fn build_anthropic_model_entry(
    id: &str,
    display_name: &str,
    created_at: &str,
    max_input_tokens: Option<&Value>,
    max_tokens: Option<&Value>,
    capabilities: Option<&Value>,
) -> Value {
    serde_json::json!({
        "id": id,
        "capabilities": capabilities
            .filter(|value| value.is_object())
            .cloned()
            .unwrap_or_else(default_anthropic_model_capabilities),
        "created_at": created_at,
        "display_name": display_name,
        "max_input_tokens": numeric_field_or_zero(max_input_tokens),
        "max_tokens": numeric_field_or_zero(max_tokens),
        "type": "model",
    })
}

fn numeric_field_or_zero(value: Option<&Value>) -> Value {
    value
        .and_then(Value::as_u64)
        .map(|number| serde_json::json!(number))
        .unwrap_or_else(|| serde_json::json!(0))
}

fn default_anthropic_model_capabilities() -> Value {
    let unsupported = || serde_json::json!({ "supported": false });
    serde_json::json!({
        "batch": unsupported(),
        "citations": unsupported(),
        "code_execution": unsupported(),
        "context_management": {
            "clear_thinking_20251015": unsupported(),
            "clear_tool_uses_20250919": unsupported(),
            "compact_20260112": unsupported(),
            "supported": false
        },
        "effort": {
            "high": unsupported(),
            "low": unsupported(),
            "max": unsupported(),
            "medium": unsupported(),
            "supported": false
        },
        "image_input": unsupported(),
        "pdf_input": unsupported(),
        "structured_outputs": unsupported(),
        "thinking": {
            "supported": false,
            "types": {
                "adaptive": unsupported(),
                "enabled": unsupported()
            }
        }
    })
}

fn page_anthropic_models(
    models: &[Value],
    query: &ModelsListQuery,
    limit: usize,
) -> AppResult<(Vec<Value>, bool)> {
    if let Some(before_id) = query.before_id.as_deref() {
        let end = model_index(models, before_id)?;
        let start = end.saturating_sub(limit);
        return Ok((models[start..end].to_vec(), start > 0));
    }

    let start = match query.after_id.as_deref() {
        Some(after_id) => model_index(models, after_id)? + 1,
        None => 0,
    };
    let end = (start + limit).min(models.len());
    Ok((models[start..end].to_vec(), end < models.len()))
}

fn model_index(models: &[Value], id: &str) -> AppResult<usize> {
    models
        .iter()
        .position(|model| model_id_from_value(model).as_deref() == Some(id))
        .ok_or_else(|| AppError::bad_request(format!("model cursor not found: {id}")))
}

fn model_id_from_value(model: &Value) -> Option<String> {
    model
        .as_object()
        .and_then(|map| map.get("id"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::body_log_fields;
    use super::should_proxy_response_header;
    use axum::body::Bytes;
    use axum::http::HeaderName;
    use sha2::Digest as _;

    /// D7a R3 #1 (REGRESSION): a `/dashboard/login` body must yield NO
    /// body-derived log field. Emitting `body_sha256` + `body_bytes` (length) for
    /// the login body is an offline token-verification oracle — an attacker with
    /// the logs can brute-force the token against the digest and the known length.
    /// So `body_log_fields` returns `None`, and the request line that follows it
    /// carries NEITHER the token NOR its SHA-256 NOR the body length.
    #[test]
    fn auth_path_body_emits_no_body_derived_log_fields() {
        let token = "s3cret-login-token";
        let body = Bytes::from(format!(r#"{{"token":"{token}"}}"#));
        let token_sha = hex::encode(sha2::Sha256::digest(token.as_bytes()));
        let body_sha = hex::encode(sha2::Sha256::digest(&body));

        // The login endpoint suppresses every body-derived field.
        assert!(
            body_log_fields("/dashboard/login", &body).is_none(),
            "login body must produce no body-derived log fields (token oracle)"
        );
        // Logout is symmetric (bodyless, but the same path class).
        assert!(body_log_fields("/dashboard/logout", &Bytes::new()).is_none());

        // A normal inference path still logs the length + digest + summary, and
        // that digest is over the body (never resembles the bare-token digest).
        let normal =
            body_log_fields("/v1/messages", &body).expect("non-auth path logs body-derived fields");
        assert_eq!(normal.bytes, body.len());
        assert_eq!(normal.sha256, body_sha);
        // Sanity: the body digest is not the standalone token digest, so even the
        // normal path never logs a digest of the bare token.
        assert_ne!(normal.sha256, token_sha);
    }

    /// The full RFC 7230 §6.1 hop-by-hop set; must match the canonical list and
    /// the request-direction parity test in `upstream.rs`.
    const HOP_BY_HOP: [&str; 8] = [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ];

    #[test]
    fn response_direction_strips_full_hop_by_hop_set() {
        for header in HOP_BY_HOP {
            let name = HeaderName::from_bytes(header.as_bytes()).unwrap();
            assert!(
                !should_proxy_response_header(&name),
                "response proxy must strip hop-by-hop header {header}",
            );
        }
    }

    #[test]
    fn response_direction_strips_content_length() {
        let name = HeaderName::from_static("content-length");
        assert!(!should_proxy_response_header(&name));
    }

    #[test]
    fn response_direction_passes_representative_passthrough_header() {
        let name = HeaderName::from_static("content-type");
        assert!(should_proxy_response_header(&name));
    }
}
