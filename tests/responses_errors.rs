mod common;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use llmconduit::config::Config;
use llmconduit::engine::Gateway;
use llmconduit::error::AppError;
use llmconduit::monitor::MonitorHub;
use llmconduit::replay::ReplayStore;
use llmconduit::upstream::ReqwestUpstreamClient;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SENTINEL: &str = "responses-error-secret-6f3b8d9a";

#[derive(Debug, Clone, Copy)]
struct ExpectedError {
    status: StatusCode,
    kind: &'static str,
    param: Option<&'static str>,
    code: &'static str,
}

impl ExpectedError {
    const fn client(status: StatusCode, param: Option<&'static str>, code: &'static str) -> Self {
        Self {
            status,
            kind: "invalid_request_error",
            param,
            code,
        }
    }

    const fn server(status: StatusCode, code: &'static str) -> Self {
        Self {
            status,
            kind: "server_error",
            param: None,
            code,
        }
    }
}

async fn post_raw(app: Router, content_type: &str, body: impl Into<Body>) -> Response {
    app.oneshot(
        Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header(header::CONTENT_TYPE, content_type)
            .body(body.into())
            .expect("Responses request"),
    )
    .await
    .expect("router response")
}

async fn post_json(app: Router, body: Value) -> Response {
    post_raw(
        app,
        "application/json",
        serde_json::to_vec(&body).expect("serialize request"),
    )
    .await
}

async fn assert_openai_error(response: Response, expected: ExpectedError, case: &str) -> Value {
    assert_eq!(response.status(), expected.status, "{case}");
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json"),
        "{case}"
    );
    let bytes = to_bytes(response.into_body(), 128 * 1024)
        .await
        .expect("read error body");
    let rendered = String::from_utf8(bytes.to_vec()).expect("UTF-8 error body");
    assert!(
        !rendered.contains(SENTINEL),
        "{case} leaked sentinel: {rendered}"
    );
    let body: Value = serde_json::from_str(&rendered).expect("OpenAI JSON error");
    let error = body["error"]
        .as_object()
        .unwrap_or_else(|| panic!("{case} missing error object: {body}"));
    for field in ["message", "type", "param", "code"] {
        assert!(
            error.contains_key(field),
            "{case} missing error.{field}: {body}"
        );
    }
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty()),
        "{case} has no public message: {body}"
    );
    assert_eq!(error["type"], expected.kind, "{case}: {body}");
    match expected.param {
        Some(param) => assert_eq!(error["param"], param, "{case}: {body}"),
        None => assert!(error["param"].is_null(), "{case}: {body}"),
    }
    assert_eq!(error["code"], expected.code, "{case}: {body}");
    body
}

fn reqwest_responses_app(config: Config) -> Router {
    let upstream = ReqwestUpstreamClient::new(
        reqwest::Client::new(),
        config.upstream_base_url.clone(),
        None,
        None,
        config.flatten_content,
        config.min_completion_tokens,
    )
    .with_request_timeout(config.request_timeout);
    let vision: Arc<dyn llmconduit::vision::VisionClient> = Arc::new(
        llmconduit::vision::ReqwestVisionClient::new(reqwest::Client::new(), &config),
    );
    let image_cache = Arc::new(llmconduit::vision::ImageCache::from_config(&config));
    // No environment-derived API auth is attached here: this harness exercises
    // only the Responses wire contract and must be deterministic under any
    // developer shell environment.
    let gateway = Arc::new(Gateway::new(
        config,
        ReplayStore::new(8),
        Arc::new(upstream),
        Arc::new(common::MockSearch::default()),
        vision,
        image_cache,
        MonitorHub::disabled(),
        None,
        llmconduit::dashboard_flow::DashboardFlowStore::disabled(),
    ));
    llmconduit::build_app_from_gateway(gateway)
}

async fn mount_strict_model_catalog(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{
                "id": "glm-5.1",
                "object": "model",
                "created": 0,
                "owned_by": "responses-errors-test"
            }]
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn responses_request_errors_have_complete_openai_shape() {
    let upstream = common::MockUpstream::default();
    upstream.set_supported_models(["glm-5.1"]).await;
    let app = llmconduit::build_app_from_gateway(common::test_gateway(
        upstream.clone(),
        common::MockSearch::default(),
    ));

    assert_openai_error(
        post_raw(
            app.clone(),
            "application/json",
            format!(r#"{{"model":"glm-5.1","input":"{SENTINEL}""#),
        )
        .await,
        ExpectedError::client(StatusCode::BAD_REQUEST, None, "invalid_json"),
        "malformed JSON",
    )
    .await;

    assert_openai_error(
        post_raw(
            app.clone(),
            "text/plain",
            json!({ "model": "glm-5.1", "input": SENTINEL }).to_string(),
        )
        .await,
        ExpectedError::client(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            None,
            "unsupported_media_type",
        ),
        "wrong media type",
    )
    .await;

    assert_openai_error(
        post_json(
            app.clone(),
            json!({
                "model": "glm-5.1",
                "input": SENTINEL,
                "parallel_tool_calls": "yes"
            }),
        )
        .await,
        ExpectedError::client(
            StatusCode::BAD_REQUEST,
            Some("parallel_tool_calls"),
            "invalid_type",
        ),
        "wrong field type",
    )
    .await;

    assert_openai_error(
        post_json(
            app.clone(),
            json!({ "model": "glm-5.1", "input": SENTINEL, "temperature": 2.5 }),
        )
        .await,
        ExpectedError::client(
            StatusCode::BAD_REQUEST,
            Some("temperature"),
            "invalid_value",
        ),
        "invalid sampling value",
    )
    .await;

    assert_openai_error(
        post_json(
            app.clone(),
            json!({ "model": "glm-5.1", "input": SENTINEL, "background": true }),
        )
        .await,
        ExpectedError::client(
            StatusCode::BAD_REQUEST,
            Some("background"),
            "unsupported_parameter",
        ),
        "unsupported standard field",
    )
    .await;

    assert_openai_error(
        post_json(app, json!({ "model": "unknown-model", "input": SENTINEL })).await,
        ExpectedError::client(StatusCode::NOT_FOUND, Some("model"), "model_not_found"),
        "unknown model",
    )
    .await;

    assert!(
        upstream.requests().await.is_empty(),
        "request-shape errors must fail before generation dispatch"
    );
}

#[tokio::test]
async fn responses_inbound_body_limit_is_openai_shaped() {
    let mut config = common::test_config();
    config.max_request_body_bytes = 64;
    let app = llmconduit::build_app_from_gateway(common::test_gateway_with_config(
        common::MockUpstream::default(),
        common::MockSearch::default(),
        config,
    ));
    let oversized = json!({
        "model": "glm-5.1",
        "input": format!("{SENTINEL}-{}", "x".repeat(256))
    })
    .to_string();

    assert_openai_error(
        post_raw(app, "application/json", oversized).await,
        ExpectedError::client(StatusCode::PAYLOAD_TOO_LARGE, None, "request_too_large"),
        "inbound body limit",
    )
    .await;
}

#[tokio::test]
async fn responses_upstream_status_errors_are_sanitized_and_openai_shaped() {
    let cases = [
        (
            400_u16,
            ExpectedError::client(StatusCode::BAD_REQUEST, None, "invalid_request_error"),
        ),
        (
            413,
            ExpectedError::client(StatusCode::PAYLOAD_TOO_LARGE, None, "request_too_large"),
        ),
        (
            415,
            ExpectedError::client(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                None,
                "unsupported_media_type",
            ),
        ),
        (
            422,
            ExpectedError::client(
                StatusCode::UNPROCESSABLE_ENTITY,
                None,
                "unprocessable_entity",
            ),
        ),
        (
            500,
            ExpectedError::server(StatusCode::BAD_GATEWAY, "upstream_error"),
        ),
        (
            503,
            ExpectedError::server(StatusCode::BAD_GATEWAY, "upstream_error"),
        ),
    ];

    for (upstream_status, expected) in cases {
        let server = MockServer::start().await;
        mount_strict_model_catalog(&server).await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(upstream_status).set_body_json(json!({
                "error": {
                    "message": format!("raw upstream body containing {SENTINEL}"),
                    "api_key": SENTINEL
                }
            })))
            .mount(&server)
            .await;

        let mut config = common::test_config();
        config.upstream_base_url = format!("{}/v1", server.uri())
            .parse()
            .expect("upstream URL");
        let app = reqwest_responses_app(config);
        assert_openai_error(
            post_json(
                app,
                json!({
                    "model": "glm-5.1",
                    "stream": false,
                    "store": false,
                    "input": SENTINEL
                }),
            )
            .await,
            expected,
            &format!("upstream HTTP {upstream_status}"),
        )
        .await;

        let requests = server
            .received_requests()
            .await
            .expect("recorded upstream requests");
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.method.as_str() == "GET")
                .count(),
            1,
            "upstream HTTP {upstream_status} must use the strict catalog"
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.method.as_str() == "POST")
                .count(),
            1,
            "upstream HTTP {upstream_status} must dispatch exactly once"
        );
    }
}

#[tokio::test]
async fn responses_internal_failure_is_500_and_never_leaks_detail() {
    let upstream = common::MockUpstream::default();
    upstream.set_supported_models(["glm-5.1"]).await;
    upstream
        .push_response(vec![Err(AppError::internal(format!(
            "internal diagnostic containing {SENTINEL}"
        )))])
        .await;
    let app = llmconduit::build_app_from_gateway(common::test_gateway(
        upstream,
        common::MockSearch::default(),
    ));

    assert_openai_error(
        post_json(
            app,
            json!({
                "model": "glm-5.1",
                "stream": false,
                "store": false,
                "input": SENTINEL
            }),
        )
        .await,
        ExpectedError::server(StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
        "internal generation failure",
    )
    .await;
}
