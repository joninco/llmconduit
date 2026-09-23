use super::*;
use crate::config::{PersistedConfig, PersistedModelRoute};
use crate::http::RouterOptions;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const OAUTH: &str = "Bearer subscription-test-secret";
const MODEL: &str = "claude-fable-5-1";

fn persisted() -> PersistedConfig {
    PersistedConfig {
        upstream_base_url: "http://127.0.0.1:1/v1".into(),
        anthropic_passthrough: Some(PersistedAnthropicPassthrough {
            upstream_origin: "https://api.anthropic.com".into(),
            rules: vec![PersistedPassthroughRule {
                model: Some("claude-fable-*".into()),
                headers: BTreeMap::new(),
            }],
        }),
        ..PersistedConfig::default()
    }
}

fn request(endpoint: &str, body: impl Into<Body>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(endpoint)
        .header("content-type", "application/json")
        .header("authorization", OAUTH)
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "oauth-2025-04-20,future-extension")
        .body(body.into())
        .unwrap()
}

fn body(model: &str) -> String {
    json!({"model":model,"max_tokens":32,"stream":false,
        "messages":[{"role":"user","content":"hello"}]})
    .to_string()
}

fn test_proxy(config: &Config, origin: &str) -> AnthropicProxy {
    let mut proxy = AnthropicProxy::new(config.anthropic_passthrough.clone().unwrap(), config);
    // Only unit tests can substitute a plain HTTP origin. Runtime configuration
    // cannot enable OAuth forwarding to a local or arbitrary remote service.
    proxy.config.origin = Url::parse(origin).unwrap();
    proxy
}

fn app(config: PersistedConfig, origin: &str) -> axum::Router {
    let config = Config::from_persisted(&config).unwrap();
    let proxy = Arc::new(test_proxy(&config, origin));
    let (app, gateway) = crate::build_app_with_gateway(config);
    drop(app);
    crate::http::build_router_with_proxy(gateway, RouterOptions::default(), Some(proxy))
}

async fn bytes(response: Response) -> Bytes {
    axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap()
}

#[test]
fn configuration_is_opt_in_and_validates_trust_and_selectors() {
    assert!(
        Config::from_persisted(&PersistedConfig::default())
            .unwrap()
            .anthropic_passthrough
            .is_none()
    );
    let config = persisted();
    let yaml = serde_yaml::to_string(&config).unwrap();
    let decoded: PersistedConfig = serde_yaml::from_str(&yaml).unwrap();
    assert!(config.anthropic_passthrough == decoded.anthropic_passthrough);
    let example: PersistedConfig = serde_yaml::from_str(include_str!(
        "../../docs/anthropic-subscription.example.yaml"
    ))
    .unwrap();
    let example = Config::from_persisted(&example).expect("documented configuration is valid");
    assert!(example.anthropic_passthrough.is_some());
    assert_eq!(example.model_routes.len(), 2);
    for origin in [
        "http://api.anthropic.com",
        "https://evil.example",
        "https://api.anthropic.com.evil.example",
        "https://api.anthropic.com:8443",
        "https://api.anthropic.com/v1",
        "https://secret@api.anthropic.com",
        "https://api.anthropic.com?key=secret",
        "https://api.anthropic.com#secret",
        "not a URL secret",
    ] {
        let mut config = persisted();
        config
            .anthropic_passthrough
            .as_mut()
            .unwrap()
            .upstream_origin = origin.into();
        let error = Config::from_persisted(&config).expect_err("untrusted origin rejected");
        assert!(!error.contains("secret"));
    }
    for rule in [
        PersistedPassthroughRule {
            model: None,
            headers: BTreeMap::new(),
        },
        PersistedPassthroughRule {
            model: Some(" ".into()),
            headers: BTreeMap::new(),
        },
        PersistedPassthroughRule {
            model: Some("claude-[".into()),
            headers: BTreeMap::new(),
        },
        PersistedPassthroughRule {
            model: None,
            headers: [("authorization".into(), OAUTH.into())].into(),
        },
        PersistedPassthroughRule {
            model: None,
            headers: [("x-claude-code-request-class".into(), "\nsecret".into())].into(),
        },
        PersistedPassthroughRule {
            model: None,
            headers: [
                ("x-llmconduit-route".into(), "one".into()),
                ("X-Llmconduit-Route".into(), "two".into()),
            ]
            .into(),
        },
    ] {
        let mut config = persisted();
        config.anthropic_passthrough.as_mut().unwrap().rules = vec![rule];
        let error = Config::from_persisted(&config).expect_err("invalid selector rejected");
        assert!(!error.contains("secret"));
    }
    // An omitted or empty `rules` list is valid and selects the Anthropic
    // first-party model families, so fable, opus, and haiku use the subscription
    // and every other model string reaches the local routes.
    let mut config = persisted();
    config.anthropic_passthrough.as_mut().unwrap().rules.clear();
    let config = Config::from_persisted(&config).expect("empty rules select the families");
    let passthrough = config.anthropic_passthrough.as_ref().unwrap();
    let uri: Uri = "/v1/messages".parse().unwrap();
    let headers = HeaderMap::new();
    for (model, expected) in [
        ("claude-fable-5-1", Some(0)),
        ("claude-opus-5-5", Some(1)),
        ("claude-haiku-4-5", Some(2)),
        ("claude-sonnet-5", None),
        ("deepseek-v4.1-flash", None),
    ] {
        assert_eq!(
            passthrough.matching_rule(&Method::POST, &uri, &headers, body(model).as_bytes()),
            expected,
            "model {model}"
        );
    }
    // An unknown key is still rejected, so a misspelled selector cannot be
    // silently ignored.
    assert!(
        serde_yaml::from_str::<PersistedAnthropicPassthrough>(
            "upstream_origin: https://api.anthropic.com\nrules: []\napi_key: secret\n"
        )
        .is_err()
    );
}

#[test]
fn model_and_header_rules_are_explicit_and_endpoint_scoped() {
    let mut config = persisted();
    config.anthropic_passthrough.as_mut().unwrap().rules = vec![
        PersistedPassthroughRule {
            model: Some("claude-fable-*".into()),
            headers: [("X-Claude-Code-Request-Class".into(), "main".into())].into(),
        },
        PersistedPassthroughRule {
            model: None,
            headers: [("x-llmconduit-route".into(), "anthropic".into())].into(),
        },
    ];
    let config = Config::from_persisted(&config)
        .unwrap()
        .anthropic_passthrough
        .unwrap();
    let mut headers = HeaderMap::new();
    let uri: Uri = "/v1/messages?beta=true".parse().unwrap();
    assert_eq!(
        config.matching_rule(&Method::POST, &uri, &headers, body(MODEL).as_bytes()),
        None
    );
    headers.insert(
        "x-claude-code-request-class",
        HeaderValue::from_static("main"),
    );
    assert_eq!(
        config.matching_rule(&Method::POST, &uri, &headers, body(MODEL).as_bytes()),
        Some(0)
    );
    assert_eq!(
        config.matching_rule(
            &Method::POST,
            &uri,
            &headers,
            body("claude-opus-4").as_bytes()
        ),
        None
    );
    assert_eq!(
        config.matching_rule(&Method::GET, &uri, &headers, body(MODEL).as_bytes()),
        None
    );
    assert_eq!(
        config.matching_rule(
            &Method::POST,
            &"/v1/chat/completions".parse().unwrap(),
            &headers,
            body(MODEL).as_bytes()
        ),
        None
    );
    assert_eq!(
        config.matching_rule(
            &Method::POST,
            &"/v1/messages/count_tokens".parse().unwrap(),
            &headers,
            body(MODEL).as_bytes()
        ),
        Some(0)
    );
    assert_eq!(
        config.matching_rule(
            &Method::POST,
            &uri,
            &headers,
            br#"{"model":"claude-fable-5-1","model":"claude-opus-4"}"#
        ),
        None
    );
    headers.append(
        "x-claude-code-request-class",
        HeaderValue::from_static("main"),
    );
    assert_eq!(
        config.matching_rule(&Method::POST, &uri, &headers, body(MODEL).as_bytes()),
        None
    );
    headers.insert("x-llmconduit-route", HeaderValue::from_static("anthropic"));
    assert_eq!(
        config.matching_rule(
            &Method::POST,
            &uri,
            &headers,
            b"native validation belongs upstream"
        ),
        Some(1)
    );
}

#[test]
fn local_routes_do_not_suppress_anthropic_families() {
    // A fable, opus, or haiku model uses the subscription even when an ad-hoc
    // route names the same model, so the family set decides rather than the route
    // table. Every other model string reaches the local routes.
    let mut config = persisted();
    config.anthropic_passthrough.as_mut().unwrap().rules.clear();
    config.model_routes.upsert(
        "claude-opus-*".into(),
        PersistedModelRoute {
            upstream_base_url: Some("http://127.0.0.1:8000/v1".into()),
            upstream_model: Some("deepseek-ai/DeepSeek-V4.1-Flash".into()),
        },
    );
    let config = Config::from_persisted(&config).unwrap();
    let passthrough = config.anthropic_passthrough.as_ref().unwrap();
    let uri: Uri = "/v1/messages".parse().unwrap();
    let headers = HeaderMap::new();
    assert!(config.matches_model_route("claude-opus-5-5"));
    assert_eq!(
        passthrough.matching_rule(
            &Method::POST,
            &uri,
            &headers,
            body("claude-opus-5-5").as_bytes()
        ),
        Some(1)
    );
    assert_eq!(
        passthrough.matching_rule(
            &Method::POST,
            &uri,
            &headers,
            body("deepseek-v4.1-flash").as_bytes()
        ),
        None
    );
}

#[test]
fn header_only_rules_never_parse_the_model() {
    // A rule that keys on headers alone still selects an opaque body, so the
    // request model is parsed only when a rule keys on it.
    let mut config = persisted();
    config.anthropic_passthrough.as_mut().unwrap().rules = vec![PersistedPassthroughRule {
        model: None,
        headers: [("x-llmconduit-route".into(), "anthropic".into())].into(),
    }];
    let config = Config::from_persisted(&config).unwrap();
    let passthrough = config.anthropic_passthrough.as_ref().unwrap();
    let uri: Uri = "/v1/messages".parse().unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("x-llmconduit-route", HeaderValue::from_static("anthropic"));
    assert_eq!(
        passthrough.matching_rule(
            &Method::POST,
            &uri,
            &headers,
            b"native validation belongs upstream"
        ),
        Some(0)
    );
}

#[tokio::test]
async fn model_family_selects_subscription_or_local_route() {
    let anthropic = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"id":"msg_native","type":"message","role":"assistant","model":"claude-fable-5-1","content":[{"type":"text","text":"native reply"}],"stop_reason":"end_turn","usage":{"input_tokens":5,"output_tokens":2}}"#,
            "application/json",
        ))
        .mount(&anthropic)
        .await;
    let local = local_model_server().await;
    // No rules: fable, opus, and haiku use the subscription. The local route
    // claims a non-Anthropic alias, which is what an agent declares to reach a
    // local backend.
    let mut config = persisted();
    config.anthropic_passthrough.as_mut().unwrap().rules.clear();
    config.model_routes.upsert(
        "deepseek-*".into(),
        PersistedModelRoute {
            upstream_base_url: Some(format!("{}/v1", local.uri())),
            upstream_model: Some("deepseek-ai/DeepSeek-V4.1-Flash".into()),
        },
    );
    let router = app(config, &anthropic.uri());
    for (model, expected) in [
        ("claude-fable-5-1", "native reply"),
        ("deepseek-v4.1-flash", "local reply"),
    ] {
        let response = router
            .clone()
            .oneshot(request("/v1/messages", body(model)))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let payload: serde_json::Value = serde_json::from_slice(&bytes(response).await).unwrap();
        assert_eq!(payload["content"][0]["text"], expected, "model {model}");
    }
    // Each destination saw exactly its own request.
    assert_eq!(anthropic.received_requests().await.unwrap().len(), 1);
    assert_eq!(local.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn preserves_request_bytes_query_vendor_headers_and_native_response() {
    let upstream = MockServer::start().await;
    let response_body =
        b"{ \"future_response\": true, \"usage\": {\"cache_read_input_tokens\": 42} }\n";
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_bytes(response_body)
                .insert_header("content-type", "application/json")
                .insert_header("request-id", "req_native")
                .insert_header("anthropic-ratelimit-requests-remaining", "99"),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let raw = b"{ \"model\":\"claude-fable-5-1\", \"future_field\":{\"native\":true}, \"max_tokens\":123, \"system\":\"unchanged\" }\n";
    let mut req = request(
        "/v1/messages?beta=true&future=a%2Fb&future=x+y",
        Bytes::from_static(raw),
    );
    req.headers_mut()
        .insert("host", HeaderValue::from_static("attacker.example"));
    req.headers_mut()
        .insert("cookie", HeaderValue::from_static("gateway=secret"));
    req.headers_mut()
        .insert("x-api-key", HeaderValue::from_static("gateway-secret"));
    req.headers_mut().insert(
        "proxy-authorization",
        HeaderValue::from_static("Basic secret"),
    );
    req.headers_mut().insert(
        "connection",
        HeaderValue::from_static("x-stainless-hop, keep-alive"),
    );
    req.headers_mut()
        .insert("x-stainless-hop", HeaderValue::from_static("strip"));
    req.headers_mut()
        .insert("x-stainless-lang", HeaderValue::from_static("js"));
    req.headers_mut()
        .append("anthropic-beta", HeaderValue::from_static("another-beta"));
    req.headers_mut().insert(
        "x-claude-code-agent-type",
        HeaderValue::from_static("teammate"),
    );
    req.headers_mut().insert(
        "x-llmconduit-route",
        HeaderValue::from_static("private-hint"),
    );
    let mut config = persisted();
    config.system_prompt_prefix = Some("must not be injected".into());
    config.upstream_model = Some("must-not-rewrite".into());
    config
        .upstream_chat_kwargs
        .insert("temperature".into(), json!(0.1));
    let response = app(config, &upstream.uri()).oneshot(req).await.unwrap();
    assert_eq!(response.status(), 201);
    assert_eq!(response.headers()["request-id"], "req_native");
    assert_eq!(
        response.headers()["anthropic-ratelimit-requests-remaining"],
        "99"
    );
    assert_eq!(bytes(response).await.as_ref(), response_body);
    let requests = upstream.received_requests().await.unwrap();
    let received = &requests[0];
    assert_eq!(received.body, raw);
    assert_eq!(
        received.url.query(),
        Some("beta=true&future=a%2Fb&future=x+y")
    );
    assert_eq!(received.headers["authorization"], OAUTH);
    assert_eq!(received.headers["anthropic-version"], "2023-06-01");
    assert_eq!(received.headers.get_all("anthropic-beta").iter().count(), 2);
    assert_eq!(received.headers["x-stainless-lang"], "js");
    assert_eq!(received.headers["x-claude-code-agent-type"], "teammate");
    assert_ne!(received.headers["host"], "attacker.example");
    for name in [
        "cookie",
        "x-api-key",
        "proxy-authorization",
        "connection",
        "x-stainless-hop",
        "x-llmconduit-route",
    ] {
        assert!(!received.headers.contains_key(name), "leaked {name}");
    }
}

#[tokio::test]
async fn count_tokens_and_http_errors_remain_native_without_retry_or_fallback() {
    let upstream = MockServer::start().await;
    for status in [400, 401, 403, 429, 500, 529] {
        upstream.reset().await;
        let raw = format!("native error {status}\n");
        Mock::given(method("POST"))
            .and(path("/v1/messages/count_tokens"))
            .respond_with(
                ResponseTemplate::new(status)
                    .set_body_string(&raw)
                    .insert_header("retry-after", "19")
                    .insert_header("request-id", "req_error"),
            )
            .expect(1)
            .mount(&upstream)
            .await;
        let response = app(persisted(), &upstream.uri())
            .oneshot(request("/v1/messages/count_tokens?beta=true", body(MODEL)))
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status);
        assert_eq!(response.headers()["retry-after"], "19");
        assert_eq!(response.headers()["request-id"], "req_error");
        assert_eq!(bytes(response).await, raw);
        upstream.verify().await;
    }
    upstream.reset().await;
    Mock::given(path("/v1/messages/count_tokens"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("{\"input_tokens\":9876}", "application/json"),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let response = app(persisted(), &upstream.uri())
        .oneshot(request("/v1/messages/count_tokens", body(MODEL)))
        .await
        .unwrap();
    assert_eq!(bytes(response).await, "{\"input_tokens\":9876}");
}

#[tokio::test]
async fn redirects_never_forward_oauth_to_another_destination() {
    let upstream = MockServer::start().await;
    let other = MockServer::start().await;
    Mock::given(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/steal", other.uri()))
                .set_body_string("redirect"),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let response = app(persisted(), &upstream.uri())
        .oneshot(request("/v1/messages", body(MODEL)))
        .await
        .unwrap();
    assert_eq!(response.status(), 307);
    assert_eq!(
        response.headers()["location"],
        format!("{}/steal", other.uri())
    );
    assert_eq!(bytes(response).await, "redirect");
    assert!(other.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn subscription_bearer_is_required_and_body_limit_still_applies() {
    let upstream = MockServer::start().await;
    let router = app(persisted(), &upstream.uri());
    for value in [None, Some("Basic secret"), Some("Bearer ")] {
        let mut req = request("/v1/messages", body(MODEL));
        req.headers_mut().remove("authorization");
        req.headers_mut()
            .insert("x-api-key", HeaderValue::from_static("not-an-oauth-token"));
        if let Some(value) = value {
            req.headers_mut()
                .insert("authorization", value.parse().unwrap());
        }
        let response = router.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(!String::from_utf8_lossy(&bytes(response).await).contains("secret"));
    }
    let mut req = request("/v1/messages", body(MODEL));
    req.headers_mut()
        .append("authorization", HeaderValue::from_static(OAUTH));
    assert_eq!(router.oneshot(req).await.unwrap().status(), 401);
    let mut config = persisted();
    config.max_request_body_bytes = 1024;
    let router = app(config, &upstream.uri());
    assert_eq!(
        router
            .clone()
            .oneshot(request("/v1/messages", body(MODEL) + &" ".repeat(2048)))
            .await
            .unwrap()
            .status(),
        413
    );
    let mut req = request("/v1/messages", Body::empty());
    req.headers_mut()
        .insert("content-length", HeaderValue::from_static("10000000"));
    assert_eq!(router.oneshot(req).await.unwrap().status(), 413);
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[test]
fn hop_by_hop_response_headers_and_connection_tokens_are_removed() {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("connection", "X-Hop, keep-alive"),
        ("x-hop", "private"),
        ("keep-alive", "timeout=5"),
        ("transfer-encoding", "chunked"),
        ("trailer", "x-checksum"),
        ("content-type", "text/event-stream"),
        ("content-encoding", "gzip"),
        ("content-length", "20"),
    ] {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    headers.append("connection", HeaderValue::from_static("x-another-hop"));
    headers.insert("x-another-hop", HeaderValue::from_static("private"));
    let result = response_headers(&headers);
    assert_eq!(result.len(), 3);
    assert_eq!(result["content-type"], "text/event-stream");
    assert_eq!(result["content-encoding"], "gzip");
    assert_eq!(result["content-length"], "20");
}

async fn local_model_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":[{"id":"deepseek-ai/DeepSeek-V4.1-Flash"}]})),
        )
        .mount(&server)
        .await;
    let sse = [
        json!({"id":"chat-local","choices":[{"index":0,"delta":{"role":"assistant","content":"local reply"},"finish_reason":null}]}),
        json!({"id":"chat-local","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}}),
    ].iter().map(|chunk| format!("data: {chunk}\n\n")).collect::<String>() + "data: [DONE]\n\n";
    Mock::given(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn local_aliases_keep_translation_and_never_receive_inbound_credentials() {
    let anthropic = MockServer::start().await;
    let opus = local_model_server().await;
    let haiku = local_model_server().await;
    let mut config = persisted();
    for (alias, server) in [("local-opus", &opus), ("local-haiku", &haiku)] {
        config.model_routes.upsert(
            alias.into(),
            PersistedModelRoute {
                upstream_base_url: Some(format!("{}/v1", server.uri())),
                upstream_model: Some("deepseek-ai/DeepSeek-V4.1-Flash".into()),
            },
        );
    }
    let router = app(config, &anthropic.uri());
    for (model, server) in [("local-opus", &opus), ("local-haiku", &haiku)] {
        let mut req = request("/v1/messages", body(model));
        req.headers_mut()
            .insert("x-api-key", HeaderValue::from_static("gateway-test-secret"));
        req.headers_mut().insert(
            "cookie",
            HeaderValue::from_static("session=cookie-test-secret"),
        );
        let response = router.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), 200);
        let response: serde_json::Value = serde_json::from_slice(&bytes(response).await).unwrap();
        assert_eq!(response["type"], "message");
        assert_eq!(response["content"][0]["text"], "local reply");
        let requests = server.received_requests().await.unwrap();
        let chat = requests
            .iter()
            .find(|request| request.url.path() == "/v1/chat/completions")
            .unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&chat.body).unwrap();
        assert_eq!(payload["model"], "deepseek-ai/DeepSeek-V4.1-Flash");
        for received in &requests {
            for name in [
                "authorization",
                "x-api-key",
                "cookie",
                "proxy-authorization",
            ] {
                assert!(
                    !received.headers.contains_key(name),
                    "local provider received {name}"
                );
            }
            assert!(!String::from_utf8_lossy(&received.body).contains("test-secret"));
        }
    }
    assert!(anthropic.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn disabled_passthrough_retains_existing_fable_translation() {
    let server = local_model_server().await;
    let config = PersistedConfig {
        upstream_base_url: format!("{}/v1", server.uri()),
        upstream_model: Some("deepseek-ai/DeepSeek-V4.1-Flash".into()),
        ..PersistedConfig::default()
    };
    let response = crate::build_app(Config::from_persisted(&config).unwrap())
        .oneshot(request("/v1/messages", body(MODEL)))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(String::from_utf8_lossy(&bytes(response).await).contains("local reply"));
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| request.url.path() == "/v1/chat/completions")
    );
}

#[tokio::test]
async fn proxy_does_not_bypass_gateway_api_authentication() {
    let upstream = MockServer::start().await;
    let config = Config::from_persisted(&persisted()).unwrap();
    let proxy = Arc::new(test_proxy(&config, &upstream.uri()));
    let (router, gateway) = crate::build_app_with_gateway(config);
    drop(router);
    let gateway = Arc::try_unwrap(gateway)
        .ok()
        .expect("unshared test gateway")
        .with_api_auth(Some(Arc::new(crate::api_auth::ApiAuth::new(
            "gateway-only-secret",
        ))));
    let router = crate::http::build_router_with_proxy(
        Arc::new(gateway),
        RouterOptions::default(),
        Some(proxy),
    );
    let response = router
        .oneshot(request("/v1/messages", body(MODEL)))
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

struct Server {
    origin: String,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start(app: axum::Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { origin, task }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Dropped(Arc<tokio::sync::Notify>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

#[tokio::test]
async fn sse_bytes_pings_and_cancellation_pass_through_without_buffering() {
    let release = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(tokio::sync::Notify::new());
    let first = b": keepalive\r\nevent: ping\r\ndata: {\"type\":\"ping\"}\r\n\r\n";
    let second = b"event: future_event\ndata: {\"opaque\":\"\xe2\x98\x83\"}\n\n";
    let upstream = Server::start(axum::Router::new().route(
        "/v1/messages",
        axum::routing::post({
            let release = release.clone();
            let dropped = dropped.clone();
            move || {
                let release = release.clone();
                let guard = Dropped(dropped.clone());
                async move {
                    let stream = async_stream::stream! {
                        let _guard = guard;
                        yield Ok::<_, std::io::Error>(Bytes::from_static(first));
                        release.notified().await;
                        yield Ok(Bytes::from_static(second));
                        std::future::pending::<()>().await;
                    };
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .header("request-id", "req_stream")
                        .body(Body::from_stream(stream))
                        .unwrap()
                }
            }
        }),
    ))
    .await;
    let gateway = Server::start(app(persisted(), &upstream.origin)).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = tokio::time::timeout(
        Duration::from_secs(3),
        client
            .post(format!("{}/v1/messages", gateway.origin))
            .header("authorization", OAUTH)
            .header("content-type", "application/json")
            .body(body(MODEL))
            .send(),
    )
    .await
    .expect("headers must arrive before stream finishes")
    .unwrap();
    assert_eq!(response.headers()["request-id"], "req_stream");
    let mut stream = response.bytes_stream();
    let mut received = Vec::new();
    while received.len() < first.len() {
        received.extend_from_slice(
            &tokio::time::timeout(Duration::from_secs(3), stream.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
        );
    }
    assert_eq!(received, first);
    release.notify_one();
    while received.len() < first.len() + second.len() {
        received.extend_from_slice(
            &tokio::time::timeout(Duration::from_secs(3), stream.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
        );
    }
    assert_eq!(received, [first.as_slice(), second.as_slice()].concat());
    drop(stream);
    tokio::time::timeout(Duration::from_secs(3), dropped.notified())
        .await
        .expect("client disconnect must drop the upstream stream");
}

#[tokio::test]
async fn disconnect_before_response_headers_cancels_the_upstream_request() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(tokio::sync::Notify::new());
    let upstream = Server::start(axum::Router::new().route(
        "/v1/messages",
        axum::routing::post({
            let entered = entered.clone();
            let dropped = dropped.clone();
            move || {
                let entered = entered.clone();
                let guard = Dropped(dropped.clone());
                async move {
                    let _guard = guard;
                    entered.notify_one();
                    std::future::pending::<Response>().await
                }
            }
        }),
    ))
    .await;
    let gateway = Server::start(app(persisted(), &upstream.origin)).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let pending = tokio::spawn(
        client
            .post(format!("{}/v1/messages", gateway.origin))
            .header("authorization", OAUTH)
            .header("content-type", "application/json")
            .body(body(MODEL))
            .send(),
    );
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(3), dropped.notified())
        .await
        .expect("disconnect while waiting for headers must cancel the upstream handler");
}

#[tokio::test]
async fn transport_failures_are_sanitized_and_stream_failures_are_not_rewritten() {
    let response = app(persisted(), "http://127.0.0.1:1")
        .oneshot(request(
            "/v1/messages?secret=query-test-secret",
            body(MODEL),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    assert!(!String::from_utf8_lossy(&bytes(response).await).contains("test-secret"));

    let release = Arc::new(tokio::sync::Notify::new());
    let upstream = Server::start(axum::Router::new().route(
        "/v1/messages",
        axum::routing::post({
            let release = release.clone();
            move || {
                let release = release.clone();
                async move {
                    let stream = async_stream::stream! {
                        yield Ok(Bytes::from_static(b": ping\n\n"));
                        release.notified().await;
                        yield Err::<Bytes, _>(std::io::Error::other("upstream failure"));
                    };
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(Body::from_stream(stream))
                        .unwrap()
                }
            }
        }),
    ))
    .await;
    let response = app(persisted(), &upstream.origin)
        .oneshot(request("/v1/messages", body(MODEL)))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut response = response.into_body();
    assert_eq!(
        response
            .frame()
            .await
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap(),
        ": ping\n\n"
    );
    release.notify_one();
    let error = tokio::time::timeout(Duration::from_secs(3), response.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.to_string(), "Anthropic passthrough stream failed");
}

#[tokio::test]
async fn header_selected_requests_and_encoded_responses_remain_opaque() {
    let upstream = MockServer::start().await;
    // An intentionally opaque encoding catches accidental decompression or
    // JSON parsing on either side of a header-selected route.
    let opaque = [0x1f, 0x8b, 0x08, 0x00, 0xff, 0x00];
    Mock::given(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(opaque)
                .insert_header("content-encoding", "gzip")
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let mut config = persisted();
    config.anthropic_passthrough.as_mut().unwrap().rules = vec![PersistedPassthroughRule {
        model: None,
        headers: [("x-llmconduit-route".into(), "anthropic".into())].into(),
    }];
    let mut req = request("/v1/messages", Bytes::copy_from_slice(&opaque));
    req.headers_mut()
        .insert("content-encoding", HeaderValue::from_static("gzip"));
    req.headers_mut()
        .insert("x-llmconduit-route", HeaderValue::from_static("anthropic"));
    let response = app(config, &upstream.uri()).oneshot(req).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-encoding"], "gzip");
    assert_eq!(bytes(response).await.as_ref(), opaque);
    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received[0].body, opaque);
    assert_eq!(received[0].headers["content-encoding"], "gzip");
}

#[derive(Clone)]
struct LogBuffer(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn passthrough_uses_metadata_only_even_with_payload_logging_and_capture_enabled() {
    let upstream = MockServer::start().await;
    Mock::given(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string("response-body-private-marker"))
        .expect(1)
        .mount(&upstream)
        .await;
    let capture =
        std::env::temp_dir().join(format!("llmconduit-proxy-test-{}", uuid::Uuid::new_v4()));
    let mut config = persisted();
    config.api_log_body_mode = crate::config::LogBodyMode::RedactedPayload;
    config.turn_capture_dir = Some(capture.to_string_lossy().into_owned());
    let router = app(config, &upstream.uri());
    let logs = LogBuffer(Arc::default());
    // Parallel tests register these same callsites without a default subscriber.
    // Two live dispatchers keep tracing's global callsite cache from assuming
    // one thread's default subscriber is shared by every test thread.
    let _other_dispatcher = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer({
            let logs = logs.clone();
            move || logs.clone()
        })
        .finish();
    let mut req = request(
        "/v1/messages?private=query-private-marker",
        json!({
            "model":MODEL,"opaque":"request-body-private-marker"
        })
        .to_string(),
    );
    req.headers_mut().insert(
        "x-request-id",
        HeaderValue::from_static("header-private-marker"),
    );
    // Keep the dispatcher installed across polls on this single-thread runtime;
    // the body and HTTP tasks use the same diagnostic sink as the handler.
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let response = router.oneshot(req).await.unwrap();
    assert_eq!(bytes(response).await, "response-body-private-marker");
    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains("Anthropic passthrough response prepared"),
        "captured diagnostics: {logs}"
    );
    for secret in [
        "subscription-test-secret",
        "request-body-private-marker",
        "response-body-private-marker",
        "header-private-marker",
        "query-private-marker",
    ] {
        assert!(!logs.contains(secret), "diagnostics leaked {secret}");
    }
    if capture.exists() {
        assert_eq!(std::fs::read_dir(&capture).unwrap().count(), 0);
        std::fs::remove_dir(capture).unwrap();
    }
}
