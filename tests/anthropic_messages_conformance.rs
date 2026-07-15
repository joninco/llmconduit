//! End-to-end request conformance for the Anthropic Messages ingress.
//!
//! Existing converter tests cover individual transformations and the Anthropic
//! egress state machine. This suite keeps realistic *whole request* shapes in
//! one place and drives them through Axum -> Anthropic deserialization ->
//! canonical Responses -> Chat lowering -> the recorded upstream request.
//! Fixtures are synthetic and contain no captured prompts or credentials.

mod common;

use axum::Router;
use axum::body::Body;
use axum::body::Bytes;
use axum::http::Method;
use axum::http::Request;
use axum::http::StatusCode;
use common::MockSearch;
use common::MockUpstream;
use common::content_chunk;
use common::parse_anthropic_sse_events;
use common::test_config;
use common::test_gateway_with_config;
use llmconduit::api_auth::ApiAuth;
use llmconduit::config::Config;
use llmconduit::config::ModelProfile;
use llmconduit::models::chat::ChatCompletionRequest;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

const FULL_REQUEST: &str = include_str!("fixtures/anthropic/full_claude_code_request.json");
const MIXED_HISTORY: &str = include_str!("fixtures/anthropic/mixed_history_request.json");
const TOOL_TEMPLATES: &str = include_str!("fixtures/anthropic/claude_code_tool_templates.json");

struct Harness {
    app: Router,
    upstream: MockUpstream,
}

impl Harness {
    async fn generation(config: Config) -> Self {
        Self::generation_with_text(config, "synthetic-ok").await
    }

    async fn generation_with_text(config: Config, text: &str) -> Self {
        let upstream = MockUpstream::default();
        upstream
            .push_response(vec![Ok(content_chunk("chat-conformance", text))])
            .await;
        Self::with_upstream(config, upstream)
    }

    async fn tokenizer(config: Config, count: u64) -> Self {
        let upstream = MockUpstream::default();
        upstream.set_token_count(Some(count)).await;
        Self::with_upstream(config, upstream)
    }

    fn authenticated(config: Config, token: &str) -> Self {
        let upstream = MockUpstream::default();
        let gateway = test_gateway_with_config(upstream.clone(), MockSearch::default(), config);
        let gateway = match Arc::try_unwrap(gateway) {
            Ok(gateway) => gateway,
            Err(_) => panic!("test gateway unexpectedly has multiple owners"),
        };
        let gateway = Arc::new(gateway.with_api_auth(Some(Arc::new(ApiAuth::new(token)))));
        let app = llmconduit::build_app_from_gateway(gateway);
        Self { app, upstream }
    }

    fn with_upstream(config: Config, upstream: MockUpstream) -> Self {
        let gateway = test_gateway_with_config(upstream.clone(), MockSearch::default(), config);
        let app = llmconduit::build_app_from_gateway(gateway);
        Self { app, upstream }
    }
}

#[derive(Debug)]
struct ResponseSnapshot {
    status: StatusCode,
    content_type: Option<String>,
    body: Vec<u8>,
}

impl ResponseSnapshot {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|error| {
            panic!(
                "response was not JSON ({error}): {}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    fn text(&self) -> String {
        String::from_utf8(self.body.clone()).expect("UTF-8 response")
    }
}

async fn send_raw(
    app: &Router,
    method: Method,
    path: &str,
    content_type: Option<&str>,
    body: impl Into<Body>,
) -> ResponseSnapshot {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(content_type) = content_type {
        request = request.header("content-type", content_type);
    }
    send_request(
        app,
        request
            .header("anthropic-version", "2023-06-01")
            .body(body.into())
            .expect("request"),
    )
    .await
}

async fn send_request(app: &Router, request: Request<Body>) -> ResponseSnapshot {
    let response = app.clone().oneshot(request).await.expect("router response");
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    let body = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .expect("response body")
        .to_vec();
    ResponseSnapshot {
        status,
        content_type,
        body,
    }
}

async fn post_json(app: &Router, path: &str, body: &Value) -> ResponseSnapshot {
    send_raw(
        app,
        Method::POST,
        path,
        Some("application/json"),
        body.to_string(),
    )
    .await
}

fn fixture(source: &str) -> Value {
    serde_json::from_str(source).expect("valid synthetic fixture")
}

fn basic_request() -> Value {
    json!({
        "model": "claude-opus-4-8",
        "max_tokens": 256,
        "stream": false,
        "messages": [{"role": "user", "content": "hello"}]
    })
}

fn tool_catalog(count: usize) -> Vec<Value> {
    let fixture = fixture(TOOL_TEMPLATES);
    let templates = fixture["templates"]
        .as_array()
        .expect("tool templates array");
    assert!(!templates.is_empty());

    (0..count)
        .map(|index| {
            let template = &templates[index % templates.len()];
            let slug = template["slug"].as_str().expect("template slug");
            let mut tool = template.clone();
            let object = tool.as_object_mut().expect("tool template object");
            object.remove("slug");
            object.insert(
                "name".to_string(),
                Value::String(format!("tool_{index:03}_{slug}")),
            );
            object.insert("cache_control".to_string(), json!({"type": "ephemeral"}));
            if index % 11 == 0 {
                object.remove("description");
            }
            tool
        })
        .collect()
}

fn full_request(tool_count: usize, message_count: usize) -> Value {
    assert!(message_count > 0);
    let mut request = fixture(FULL_REQUEST);
    request["tools"] = Value::Array(tool_catalog(tool_count));
    if message_count > 1 {
        request["messages"] = Value::Array(
            (0..message_count)
                .map(|index| {
                    let role = if index % 2 == 0 { "user" } else { "assistant" };
                    if index % 3 == 0 {
                        json!({
                            "role": role,
                            "content": [{
                                "type": "text",
                                "text": format!("synthetic history item {index}")
                            }]
                        })
                    } else {
                        json!({
                            "role": role,
                            "content": format!("synthetic history item {index}")
                        })
                    }
                })
                .collect(),
        );
    }
    request
}

fn expected_tools(request: &Value) -> BTreeMap<String, (String, Value)> {
    request["tools"]
        .as_array()
        .expect("request tools")
        .iter()
        .map(|tool| {
            (
                tool["name"].as_str().expect("tool name").to_string(),
                (
                    tool.get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    tool["input_schema"].clone(),
                ),
            )
        })
        .collect()
}

fn assert_tool_catalog_preserved(request: &Value, upstream: &ChatCompletionRequest) {
    let expected = expected_tools(request);
    let actual = upstream.tools.as_ref().expect("forwarded tools");
    assert_eq!(actual.len(), expected.len());

    let names: Vec<&str> = actual
        .iter()
        .map(|tool| tool.function.name.as_str())
        .collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(
        names, sorted,
        "Anthropic tools must lower deterministically"
    );

    for tool in actual {
        assert_eq!(tool.kind, "function");
        let (description, schema) = expected
            .get(&tool.function.name)
            .unwrap_or_else(|| panic!("unexpected tool {}", tool.function.name));
        assert_eq!(&tool.function.description, description);
        assert_eq!(tool.function.parameters.as_ref(), Some(schema));
        assert!(
            !tool.function.strict,
            "Anthropic client tool {} must remain non-strict",
            tool.function.name
        );
    }
}

fn assert_anthropic_error(snapshot: &ResponseSnapshot, status: StatusCode, case: &str) {
    assert_anthropic_error_type(snapshot, status, "invalid_request_error", case);
}

fn assert_anthropic_error_type(
    snapshot: &ResponseSnapshot,
    status: StatusCode,
    error_type: &str,
    case: &str,
) {
    assert_eq!(
        snapshot.status,
        status,
        "case {case}: {}",
        String::from_utf8_lossy(&snapshot.body)
    );
    assert!(
        snapshot
            .content_type
            .as_deref()
            .is_some_and(|value| value.starts_with("application/json")),
        "case {case}: missing JSON content type: {:?}",
        snapshot.content_type
    );
    let body = snapshot.json();
    assert_eq!(body["type"], "error", "case {case}: {body}");
    assert_eq!(body["error"]["type"], error_type, "case {case}: {body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| !message.trim().is_empty()),
        "case {case}: {body}"
    );
}

#[tokio::test]
async fn anthropic_minimal_streaming_and_nonstreaming_requests_are_wire_conformant() {
    for stream in [false, true] {
        let harness = Harness::generation(test_config()).await;
        let mut request = basic_request();
        request["stream"] = Value::Bool(stream);
        request["messages"] = if stream {
            json!([{
                "role": "user",
                "content": [{"type": "text", "text": "hello in blocks"}]
            }])
        } else {
            json!([{"role": "user", "content": "hello as a string"}])
        };

        let response = post_json(&harness.app, "/v1/messages", &request).await;
        assert_eq!(response.status, StatusCode::OK);
        let recorded = harness.upstream.requests().await;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].model, "claude-opus-4-8");
        assert!(recorded[0].stream, "upstream transport is always streaming");

        if stream {
            assert!(
                response
                    .content_type
                    .as_deref()
                    .is_some_and(|value| value.starts_with("text/event-stream"))
            );
            let events = parse_anthropic_sse_events(&response.text());
            llmconduit::adapters::responses_to_anthropic::conformance::assert_sse_conformant(
                &events,
                llmconduit::adapters::responses_to_anthropic::conformance::Surface::TextOnly,
            );
        } else {
            let body = response.json();
            assert_eq!(body["type"], "message");
            assert_eq!(body["role"], "assistant");
            assert_eq!(body["content"][0]["type"], "text");
            assert_eq!(body["content"][0]["text"], "synthetic-ok");
            assert_eq!(body["stop_reason"], "end_turn");
        }
    }
}

#[tokio::test]
async fn anthropic_claude_code_scale_catalogs_preserve_every_schema() {
    // 73 and 90 are the common observed Claude Code catalog sizes. The final
    // case also exercises the largest observed catalog/history dimensions.
    for (tool_count, message_count) in [(73, 1), (90, 3), (104, 235)] {
        let request = full_request(tool_count, message_count);
        let harness = Harness::generation(test_config()).await;
        let response = post_json(&harness.app, "/v1/messages", &request).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{tool_count} tools/{message_count} messages: {}",
            response.text()
        );

        let recorded = harness.upstream.requests().await;
        assert_eq!(recorded.len(), 1);
        let upstream = &recorded[0];
        assert_tool_catalog_preserved(&request, upstream);
        assert_eq!(upstream.max_output_tokens, Some(8192));
        assert_eq!(upstream.temperature, Some(0.25));
        assert_eq!(upstream.top_p, Some(0.9));
        assert_eq!(upstream.stop, Some(vec!["<synthetic-stop>".to_string()]));
        assert_eq!(upstream.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(
            upstream.parallel_tool_calls,
            Some(false),
            "Claude Code's disable_parallel_tool_use must survive lowering"
        );
        assert_eq!(upstream.tool_choice, Some(json!("auto")));
        assert_eq!(
            upstream.messages.len(),
            message_count + 1,
            "one joined system instruction plus the requested history"
        );
    }
}

#[tokio::test]
async fn anthropic_count_tokens_accepts_the_same_large_schema_catalog() {
    let request = full_request(73, 9);
    let harness = Harness::tokenizer(test_config(), 4242).await;
    let response = post_json(&harness.app, "/v1/messages/count_tokens", &request).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.json(), json!({"input_tokens": 4242}));

    let recorded = harness.upstream.requests().await;
    assert_eq!(recorded.len(), 1);
    let upstream = &recorded[0];
    assert_tool_catalog_preserved(&request, upstream);
    assert!(!upstream.stream);
    assert_eq!(
        upstream.max_output_tokens, None,
        "count_tokens must not include the requested completion budget"
    );
    assert_eq!(upstream.reasoning_effort.as_deref(), Some("high"));
}

#[tokio::test]
async fn anthropic_mixed_tool_and_thinking_history_preserves_order_and_identity() {
    let request = fixture(MIXED_HISTORY);
    let harness = Harness::generation(test_config()).await;
    let response = post_json(&harness.app, "/v1/messages", &request).await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text());

    let recorded = harness.upstream.requests().await;
    assert_eq!(recorded.len(), 1);
    let upstream = &recorded[0];
    assert_tool_catalog_preserved(&request, upstream);

    let calls = upstream
        .messages
        .iter()
        .filter_map(|message| message.tool_calls.as_ref())
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].id.as_deref(), Some("toolu_read_1"));
    assert_eq!(calls[0].function.name.as_deref(), Some("read_resource"));
    assert_eq!(calls[1].id.as_deref(), Some("toolu_write_1"));
    assert_eq!(calls[1].function.name.as_deref(), Some("write_resource"));

    let results = upstream
        .messages
        .iter()
        .filter(|message| message.role == "tool")
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].tool_call_id.as_deref(), Some("toolu_read_1"));
    assert_eq!(
        results[0].content.as_ref().and_then(Value::as_str),
        Some("synthetic contents")
    );
    assert_eq!(results[1].tool_call_id.as_deref(), Some("toolu_write_1"));
    assert_eq!(
        results[1].content.as_ref().and_then(Value::as_str),
        Some("write failed\nsynthetic permission error")
    );

    let signed = upstream
        .messages
        .iter()
        .find_map(|message| message.thinking.as_ref())
        .expect("signed thinking history");
    assert_eq!(signed.content, "A short synthetic rationale.");
    assert_eq!(
        signed.signature.as_deref(),
        Some("synthetic-provider-signature")
    );
    let serialized = serde_json::to_string(upstream).expect("serialize upstream request");
    assert!(serialized.contains("A synthetic inline system note."));
    assert!(
        !serialized.contains("opaque-synthetic-data"),
        "redacted thinking must stay opaque and must not reach the chat backend"
    );
}

#[tokio::test]
async fn anthropic_tool_choice_variants_lower_to_chat_equivalents() {
    let cases = [
        ("omitted", None, json!("auto"), None),
        ("auto", Some(json!({"type": "auto"})), json!("auto"), None),
        (
            "auto parallel enabled",
            Some(json!({"type": "auto", "disable_parallel_tool_use": false})),
            json!("auto"),
            Some(true),
        ),
        ("any", Some(json!({"type": "any"})), json!("required"), None),
        (
            "any parallel disabled",
            Some(json!({"type": "any", "disable_parallel_tool_use": true})),
            json!("required"),
            Some(false),
        ),
        ("none", Some(json!({"type": "none"})), json!("none"), None),
        (
            "named",
            Some(json!({"type": "tool", "name": "lookup"})),
            json!({"type": "function", "function": {"name": "lookup"}}),
            None,
        ),
    ];

    for (name, choice, expected, expected_parallel) in cases {
        let mut request = basic_request();
        request["tools"] = json!([{
            "name": "lookup",
            "input_schema": {
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {}
            }
        }]);
        if let Some(choice) = choice {
            request["tool_choice"] = choice;
        }
        let harness = Harness::generation(test_config()).await;
        let response = post_json(&harness.app, "/v1/messages", &request).await;
        assert_eq!(response.status, StatusCode::OK, "case {name}");
        let recorded = harness.upstream.requests().await;
        assert_eq!(
            recorded[0].tool_choice.as_ref(),
            Some(&expected),
            "case {name}"
        );
        assert_eq!(
            recorded[0].parallel_tool_calls, expected_parallel,
            "case {name}"
        );
    }
}

#[tokio::test]
async fn anthropic_strict_tools_preserve_strict_schema_enforcement() {
    let request = json!({
        "model": "claude-opus-4-8",
        "max_tokens": 256,
        "stream": false,
        "messages": [{"role": "user", "content": "Use the synthetic tool."}],
        "tools": [{
            "name": "strict_lookup",
            "description": "Look up a synthetic record.",
            "strict": true,
            "input_schema": {
                "type": "object",
                "properties": {
                    "location": {"type": "string"},
                    "unit": {"type": "string", "enum": ["celsius", "fahrenheit"]}
                },
                "required": ["location"],
                "additionalProperties": false
            }
        }]
    });
    let harness = Harness::generation(test_config()).await;
    let response = post_json(&harness.app, "/v1/messages", &request).await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    let recorded = harness.upstream.requests().await;
    let tool = &recorded[0].tools.as_ref().expect("forwarded tool")[0];
    assert!(tool.function.strict);
    assert_eq!(
        tool.function.parameters.as_ref(),
        Some(&request["tools"][0]["input_schema"])
    );
}

#[tokio::test]
async fn anthropic_structured_output_and_controls_lower_end_to_end() {
    let schema = json!({
        "type": "object",
        "properties": {
            "answer": {"type": "string"},
            "confidence": {"type": "number", "minimum": 0, "maximum": 1}
        },
        "required": ["answer"],
        "additionalProperties": false
    });
    let request = json!({
        "model": "claude-opus-4-8",
        "max_tokens": 512,
        "stream": false,
        "system": [
            {"type": "text", "text": "Return structured data.", "cache_control": {"type": "ephemeral"}},
            {"type": "text", "text": "Use the requested schema."}
        ],
        "messages": [{"role": "user", "content": "Give a synthetic answer."}],
        "thinking": {"type": "adaptive"},
        "output_config": {
            "effort": "max",
            "format": {
                "type": "json_schema",
                "name": "synthetic_answer",
                "description": "A synthetic structured answer.",
                "strict": true,
                "schema": schema.clone()
            }
        },
        "temperature": 0.5,
        "top_p": 0.8,
        "stop_sequences": ["END"],
        "metadata": {"user_id": "synthetic-user"}
    });
    let harness =
        Harness::generation_with_text(test_config(), r#"{"answer":"synthetic-ok"}"#).await;
    let response = post_json(&harness.app, "/v1/messages", &request).await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text());

    let recorded = harness.upstream.requests().await;
    let upstream = &recorded[0];
    assert_eq!(upstream.max_output_tokens, Some(512));
    assert_eq!(
        upstream.reasoning_effort.as_deref(),
        Some("high"),
        "the generic chat leaf clamps Anthropic's max effort to its supported ceiling"
    );
    assert_eq!(upstream.temperature, Some(0.5));
    assert_eq!(upstream.top_p, Some(0.8));
    assert_eq!(upstream.stop, Some(vec!["END".to_string()]));
    assert_eq!(
        upstream.response_format,
        Some(json!({
            "type": "json_schema",
            "json_schema": {
                "name": "synthetic_answer",
                "description": "A synthetic structured answer.",
                "strict": true,
                "schema": schema
            }
        }))
    );
    assert_eq!(
        upstream.messages[0]
            .content
            .as_ref()
            .and_then(Value::as_str),
        Some("Return structured data.\nUse the requested schema.")
    );
}

#[tokio::test]
async fn anthropic_image_source_variants_reach_only_native_vision_upstream() {
    let mut config = test_config();
    config.model_profiles = BTreeMap::from([(
        "claude-opus-4-8".to_string(),
        ModelProfile {
            native_vision: Some(true),
            ..Default::default()
        },
    )]);
    let request = json!({
        "model": "claude-opus-4-8",
        "max_tokens": 256,
        "stream": false,
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "Inspect synthetic images."},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "c3ludGhldGlj"}},
                {"type": "image", "source": {"type": "url", "url": "https://example.invalid/synthetic.png"}},
                {"type": "image", "source": {"type": "file", "file_id": "file_synthetic_image"}}
            ]
        }]
    });
    let harness = Harness::generation(config).await;
    let response = post_json(&harness.app, "/v1/messages", &request).await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text());

    let recorded = harness.upstream.requests().await;
    assert_eq!(
        recorded[0].messages[0].content,
        Some(json!([
            {"type": "text", "text": "Inspect synthetic images."},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,c3ludGhldGlj"}},
            {"type": "image_url", "image_url": {"url": "https://example.invalid/synthetic.png"}},
            {"type": "input_image", "file_id": "file_synthetic_image"}
        ]))
    );
}

#[tokio::test]
async fn anthropic_zero_max_tokens_prewarm_is_explicitly_unsupported() {
    let mut request = basic_request();
    request["max_tokens"] = json!(0);
    request["stream"] = json!(false);
    let harness = Harness::generation(test_config()).await;

    let response = post_json(&harness.app, "/v1/messages", &request).await;

    assert_anthropic_error(
        &response,
        StatusCode::BAD_REQUEST,
        "zero-token cache prewarm",
    );
    let message = response.json()["error"]["message"]
        .as_str()
        .expect("error message")
        .to_string();
    assert!(message.contains("max_tokens"), "{message}");
    assert!(message.contains("prewarming"), "{message}");
    assert!(harness.upstream.requests().await.is_empty());
}

#[tokio::test]
async fn anthropic_messages_auth_failures_use_anthropic_error_types() {
    let body = basic_request().to_string();
    for path in ["/v1/messages", "/v1/messages/count_tokens"] {
        for credential in [None, Some("Bearer wrong-token")] {
            let harness = Harness::authenticated(test_config(), "dedicated-test-token");
            let mut request = Request::builder()
                .method(Method::POST)
                .uri(path)
                .header("content-type", "application/json")
                .header("anthropic-version", "2023-06-01");
            if let Some(credential) = credential {
                request = request.header("authorization", credential);
            }
            let response = send_request(
                &harness.app,
                request.body(Body::from(body.clone())).expect("request"),
            )
            .await;
            assert_anthropic_error_type(
                &response,
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                &format!("{credential:?} on {path}"),
            );
            assert!(harness.upstream.requests().await.is_empty());
        }
    }
}

#[tokio::test]
async fn anthropic_messages_body_limit_failures_use_anthropic_error_types() {
    for path in ["/v1/messages", "/v1/messages/count_tokens"] {
        for declared_length in [true, false] {
            let mut config = test_config();
            config.max_request_body_bytes = 256;
            let harness = Harness::generation(config).await;
            let mut request = Request::builder()
                .method(Method::POST)
                .uri(path)
                .header("content-type", "application/json")
                .header("anthropic-version", "2023-06-01");
            let body = if declared_length {
                request = request.header("content-length", "4096");
                Body::from("{}")
            } else {
                Body::from("x".repeat(1024))
            };
            let request = request.body(body).expect("request");
            if !declared_length {
                assert!(request.headers().get("content-length").is_none());
            }

            let response = send_request(&harness.app, request).await;
            assert_anthropic_error_type(
                &response,
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                &format!("declared_length={declared_length} on {path}"),
            );
            assert!(harness.upstream.requests().await.is_empty());
        }
    }
}

#[tokio::test]
async fn anthropic_messages_broken_bodies_use_anthropic_error_types() {
    for path in ["/v1/messages", "/v1/messages/count_tokens"] {
        let harness = Harness::generation(test_config()).await;
        let stream = futures::stream::iter(vec![
            Ok::<_, std::io::Error>(Bytes::from_static(b"partial")),
            Err(std::io::Error::other("synthetic connection reset")),
        ]);
        let request = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .body(Body::from_stream(stream))
            .expect("request");

        let response = send_request(&harness.app, request).await;
        assert_anthropic_error(
            &response,
            StatusCode::BAD_REQUEST,
            &format!("broken body on {path}"),
        );
        assert!(harness.upstream.requests().await.is_empty());
    }
}

fn semantic_error_cases() -> Vec<(&'static str, Value)> {
    let mut cases = Vec::new();

    let mut request = basic_request();
    request["top_k"] = json!(40);
    cases.push(("top_k", request));

    let mut request = basic_request();
    request["max_tokens"] = json!(9_223_372_036_854_775_808_u64);
    cases.push(("max_tokens overflow", request));

    let mut request = basic_request();
    request["temperature"] = json!(2.1);
    cases.push(("temperature", request));

    let mut request = basic_request();
    request["top_p"] = json!(1.1);
    cases.push(("top_p", request));

    let mut request = basic_request();
    request["stop_sequences"] = json!(["a", "b", "c", "d", "e"]);
    cases.push(("too many stop sequences", request));

    let mut request = basic_request();
    request["metadata"] = json!("not-an-object");
    cases.push(("metadata type", request));

    let mut request = basic_request();
    request["messages"] = json!([{"role": "tool", "content": "invalid role"}]);
    cases.push(("invalid role", request));

    let mut request = basic_request();
    request["messages"] = json!([{
        "role": "user",
        "content": [{"type": "image", "source": {"type": "base64", "media_type": "image/png"}}]
    }]);
    cases.push(("missing base64 image data", request));

    let mut request = basic_request();
    request["tools"] = json!([{"name": "bad name", "input_schema": {"type": "object"}}]);
    cases.push(("invalid tool name", request));

    let mut request = basic_request();
    request["tools"] = json!([{"name": "lookup", "input_schema": {"type": 42}}]);
    cases.push(("malformed known schema keyword", request));

    let mut request = basic_request();
    request["tools"] = json!([
        {"name": "lookup", "input_schema": {"type": "object"}},
        {"name": "lookup", "input_schema": {"type": "object"}}
    ]);
    cases.push(("duplicate tool names", request));

    let mut request = basic_request();
    request["tools"] = json!([{"name": "lookup", "input_schema": {"type": "object"}}]);
    request["tool_choice"] = json!({"type": "tool", "name": "missing"});
    cases.push(("missing named tool", request));

    let mut request = basic_request();
    request["tool_choice"] = json!({"type": "any"});
    cases.push(("required choice without tools", request));

    let mut request = basic_request();
    request["output_config"] = json!({"format": {"type": "json_schema"}});
    cases.push(("structured output missing schema", request));

    cases
}

#[tokio::test]
async fn anthropic_semantic_error_corpus_is_rejected_before_dispatch_on_both_routes() {
    for (case, request) in semantic_error_cases() {
        for path in ["/v1/messages", "/v1/messages/count_tokens"] {
            let harness = if path.ends_with("count_tokens") {
                Harness::tokenizer(test_config(), 123).await
            } else {
                Harness::generation(test_config()).await
            };
            let response = post_json(&harness.app, path, &request).await;
            assert_anthropic_error(
                &response,
                StatusCode::BAD_REQUEST,
                &format!("{case} on {path}"),
            );
            assert_eq!(
                harness.upstream.requests().await.len(),
                0,
                "case {case} on {path} reached the upstream"
            );
        }
    }
}

#[tokio::test]
async fn anthropic_decode_error_corpus_never_dispatches() {
    let cases = [
        ("malformed JSON", "{"),
        ("wrong root", "[]"),
        ("missing messages", r#"{"model":"claude-opus-4-8"}"#),
        (
            "messages wrong type",
            r#"{"model":"claude-opus-4-8","messages":"hello"}"#,
        ),
        (
            "content wrong type",
            r#"{"model":"claude-opus-4-8","messages":[{"role":"user","content":42}]}"#,
        ),
        (
            "unknown tool choice",
            r#"{"model":"claude-opus-4-8","messages":[{"role":"user","content":"hello"}],"tool_choice":{"type":"sometimes"}}"#,
        ),
    ];

    for (case, body) in cases {
        for path in ["/v1/messages", "/v1/messages/count_tokens"] {
            let harness = if path.ends_with("count_tokens") {
                Harness::tokenizer(test_config(), 123).await
            } else {
                Harness::generation(test_config()).await
            };
            let response = send_raw(
                &harness.app,
                Method::POST,
                path,
                Some("application/json"),
                body.to_string(),
            )
            .await;
            assert_anthropic_error(
                &response,
                StatusCode::BAD_REQUEST,
                &format!("{case} on {path}"),
            );
            assert_eq!(
                harness.upstream.requests().await.len(),
                0,
                "case {case} on {path} reached the upstream"
            );
        }
    }
}

#[tokio::test]
async fn anthropic_messages_routes_reject_wrong_media_types_with_anthropic_errors() {
    let body = basic_request().to_string();
    for path in ["/v1/messages", "/v1/messages/count_tokens"] {
        for content_type in [None, Some("text/plain")] {
            let harness = if path.ends_with("count_tokens") {
                Harness::tokenizer(test_config(), 123).await
            } else {
                Harness::generation(test_config()).await
            };
            let response =
                send_raw(&harness.app, Method::POST, path, content_type, body.clone()).await;
            assert_anthropic_error(
                &response,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                &format!("{content_type:?} on {path}"),
            );
            assert!(harness.upstream.requests().await.is_empty());
        }
    }
}

#[tokio::test]
async fn anthropic_messages_probe_routes_are_side_effect_free() {
    let harness = Harness::generation(test_config()).await;
    for method in [Method::HEAD, Method::OPTIONS] {
        let response = send_raw(&harness.app, method, "/v1/messages", None, Body::empty()).await;
        assert_eq!(response.status, StatusCode::NO_CONTENT);
        assert!(response.body.is_empty());
    }
    assert!(harness.upstream.requests().await.is_empty());
}
