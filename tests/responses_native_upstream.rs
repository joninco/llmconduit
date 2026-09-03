mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use llmconduit::config::{FallbackUpstreamConfig, UpstreamConfig, UpstreamWireApi};
use llmconduit::responses_capabilities::{
    EncryptedReasoningCapability, ReasoningSummaryCapability, ResponsesCapabilitiesConfig,
    StructuredOutputCapability,
};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request as WireRequest, Respond, ResponseTemplate};

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| {
            let event_type = event["type"].as_str().expect("event type");
            format!("event: {event_type}\ndata: {event}\n\n")
        })
        .collect()
}

fn text_events(text: &str) -> Vec<Value> {
    // Match the pinned Codex private fixtures: lifecycle deltas and item.done
    // omit public Responses indexes, and the terminal resource need not repeat
    // output. llmconduit's private projector owns those public identities.
    vec![
        json!({"type":"response.created","response":{"id":"resp_private"}}),
        json!({"type":"response.output_text.delta","delta":text}),
        json!({"type":"response.output_item.done","item":{"type":"message","id":"msg_native_1","role":"assistant","content":[{"type":"output_text","text":text}]}}),
        json!({
            "type":"response.completed",
            "response":{
                "id":"resp_private",
                "usage":{
                    "input_tokens":11,
                    "output_tokens":3,
                    "total_tokens":14,
                    "input_tokens_details":{"cached_tokens":2},
                    "output_tokens_details":{"reasoning_tokens":1}
                }
            }
        }),
    ]
}

fn function_events() -> Vec<Value> {
    vec![
        json!({"type":"response.created","response":{"id":"resp_private_tool"}}),
        json!({"type":"response.function_call_arguments.delta","item_id":"fc_native_1","delta":"{\"key\":\"answer\"}"}),
        json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_native_1","name":"get_value","arguments":"{\"key\":\"answer\"}"}}),
        json!({"type":"response.completed","response":{"id":"resp_private_tool","usage":{"input_tokens":9,"output_tokens":4,"total_tokens":13}}}),
    ]
}

#[derive(Clone)]
struct NativeResponder {
    calls: Arc<AtomicUsize>,
}

impl Respond for NativeResponder {
    fn respond(&self, request: &WireRequest) -> ResponseTemplate {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let body: Value = serde_json::from_slice(&request.body).expect("native request JSON");
        let has_output = body["input"].as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("function_call_output")
            })
        });
        let events = if has_output {
            text_events("tool complete")
        } else if body["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
        {
            function_events()
        } else {
            text_events("native ok")
        };
        ResponseTemplate::new(200).set_body_raw(sse(&events), "text/event-stream")
    }
}

async fn app(server: &MockServer) -> axum::Router {
    app_with_retry(server, true).await
}

async fn app_with_retry(server: &MockServer, retry_enabled: bool) -> axum::Router {
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object":"list",
            "data":[{"id":"gpt-5.6-sol","object":"model","context_window":372000}]
        })))
        .mount(server)
        .await;
    let mut config = common::test_config();
    let mut resilience = llmconduit::config::UpstreamResilienceConfig::default();
    resilience.retry.enabled = retry_enabled;
    config.upstreams = vec![UpstreamConfig {
        resilience,
        name: "codex-subscription".to_string(),
        upstream_base_url: format!("{}/v1/", server.uri()).parse().expect("url"),
        upstream_api_key: Some("local-sidecar-token".to_string()),
        upstream_model: None,
        wire_api: UpstreamWireApi::CodexResponses,
        upstream_chat_kwargs: Default::default(),
        upstream_request_log_path: None,
        responses_capabilities: Some(ResponsesCapabilitiesConfig {
            parallel_tool_calls: Some(false),
            structured_outputs: Some(vec![
                StructuredOutputCapability::Text,
                StructuredOutputCapability::JsonSchema,
            ]),
            reasoning_summary: Some(ReasoningSummaryCapability::Upstream),
            encrypted_reasoning: Some(EncryptedReasoningCapability::Passthrough),
            ..Default::default()
        }),
        fallback_upstreams: Vec::new(),
    }];
    llmconduit::build_app(config)
}

async fn mixed_app(native: &MockServer, chat: &MockServer, native_first: bool) -> axum::Router {
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object":"list",
            "data":[{"id":"gpt-5.6-sol","object":"model","context_window":372000}]
        })))
        .mount(native)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object":"list",
            "data":[{"id":"local-chat","object":"model","context_window":131072}]
        })))
        .mount(chat)
        .await;

    let native_provider = UpstreamConfig {
        resilience: Default::default(),
        name: "codex-subscription".to_string(),
        upstream_base_url: format!("{}/v1/", native.uri()).parse().expect("url"),
        upstream_api_key: Some("local-sidecar-token".to_string()),
        upstream_model: None,
        wire_api: UpstreamWireApi::CodexResponses,
        upstream_chat_kwargs: Default::default(),
        upstream_request_log_path: None,
        responses_capabilities: None,
        fallback_upstreams: Vec::new(),
    };
    let chat_provider = UpstreamConfig {
        resilience: Default::default(),
        name: "local-chat".to_string(),
        upstream_base_url: format!("{}/v1/", chat.uri()).parse().expect("url"),
        upstream_api_key: None,
        upstream_model: None,
        wire_api: UpstreamWireApi::ChatCompletions,
        upstream_chat_kwargs: Default::default(),
        upstream_request_log_path: None,
        responses_capabilities: None,
        fallback_upstreams: Vec::new(),
    };
    let mut config = common::test_config();
    config.upstreams = if native_first {
        vec![native_provider, chat_provider]
    } else {
        vec![chat_provider, native_provider]
    };
    llmconduit::build_app(config)
}

async fn count_tokens(
    app: axum::Router,
    model: &str,
    message: &str,
    tools: Value,
) -> (StatusCode, Option<String>, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages/count_tokens")
                .header("content-type", "application/json")
                .header("anthropic-version", "2023-06-01")
                .body(Body::from(
                    json!({
                        "model": model,
                        "max_tokens": 128,
                        "messages": [{"role":"user","content":message}],
                        "tools": tools
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let quality = response
        .headers()
        .get("x-llmconduit-token-count-quality")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    (status, quality, body)
}

#[tokio::test]
async fn native_responses_nonstream_preserves_output_usage_and_private_boundary() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(NativeResponder {
            calls: Arc::clone(&calls),
        })
        .mount(&server)
        .await;
    let app = app(&server).await;
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model":"gpt-5.6-sol",
                        "input":"say exactly native ok",
                        "max_output_tokens":123,
                        "reasoning":{"effort":"low"},
                        "store":false
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let response_bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&response_bytes)
    );
    let body: Value = serde_json::from_slice(&response_bytes).unwrap();
    assert_eq!(body["output"][0]["content"][0]["text"], "native ok");
    assert_eq!(body["usage"]["input_tokens"], 11);
    assert_eq!(body["usage"]["input_tokens_details"]["cached_tokens"], 2);
    assert_ne!(body["id"], "resp_private");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let requests = server.received_requests().await.unwrap();
    let generation = requests
        .iter()
        .find(|request| request.url.path() == "/v1/responses")
        .expect("generation request");
    assert_eq!(
        generation
            .headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap(),
        "Bearer local-sidecar-token"
    );
    let upstream: Value = serde_json::from_slice(&generation.body).unwrap();
    assert_eq!(upstream["stream"], true);
    assert_eq!(upstream["store"], false);
    assert_eq!(upstream["max_output_tokens"], 123);
    assert_eq!(upstream["reasoning"]["effort"], "low");
    assert!(upstream["reasoning"].get("summary").is_none());
    assert_eq!(upstream["include"], json!(["reasoning.encrypted_content"]));
}

#[tokio::test]
async fn native_raw_responses_supplies_zero_usage_details_on_both_wire_modes() {
    let server = MockServer::start().await;
    let mut events = text_events("native ok");
    events.last_mut().unwrap()["response"]["usage"] =
        json!({"input_tokens":11,"output_tokens":3,"total_tokens":14});
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse(&events), "text/event-stream"))
        .expect(2)
        .mount(&server)
        .await;
    let app = app(&server).await;

    let nonstream = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model":"gpt-5.6-sol",
                        "input":"say exactly native ok",
                        "stream":false,
                        "store":false
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(nonstream.status(), StatusCode::OK);
    let nonstream: Value =
        serde_json::from_slice(&to_bytes(nonstream.into_body(), usize::MAX).await.unwrap())
            .unwrap();
    assert_eq!(nonstream["usage"]["input_tokens"], 11);
    assert_eq!(nonstream["usage"]["output_tokens"], 3);
    assert_eq!(nonstream["usage"]["total_tokens"], 14);
    assert_eq!(
        nonstream["usage"]["input_tokens_details"]["cached_tokens"],
        0
    );
    assert_eq!(
        nonstream["usage"]["output_tokens_details"]["reasoning_tokens"],
        0
    );

    let stream = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model":"gpt-5.6-sol",
                        "input":"say exactly native ok",
                        "stream":true,
                        "store":false
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stream.status(), StatusCode::OK);
    let stream = String::from_utf8(
        to_bytes(stream.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    let completed = stream
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .find(|event| event["type"] == "response.completed")
        .expect("completed Responses event");
    let usage = &completed["response"]["usage"];
    assert_eq!(usage["input_tokens"], 11);
    assert_eq!(usage["output_tokens"], 3);
    assert_eq!(usage["total_tokens"], 14);
    assert_eq!(usage["input_tokens_details"]["cached_tokens"], 0);
    assert_eq!(usage["output_tokens_details"]["reasoning_tokens"], 0);
}

#[tokio::test]
async fn anthropic_tool_output_continues_the_same_native_turn() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(NativeResponder {
            calls: Arc::clone(&calls),
        })
        .mount(&server)
        .await;
    let app = app(&server).await;
    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("anthropic-version", "2023-06-01")
                .body(Body::from(
                    json!({
                        "model":"gpt-5.6-sol",
                        "max_tokens":128,
                        "messages":[{"role":"user","content":"use the tool"}],
                        "tools":[{
                            "name":"get_value",
                            "description":"Return a value",
                            "input_schema":{"type":"object","properties":{"key":{"type":"string"}},"required":["key"]}
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let first_status = first.status();
    let first_bytes = to_bytes(first.into_body(), usize::MAX).await.unwrap();
    assert_eq!(
        first_status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&first_bytes)
    );
    let first_body: Value = serde_json::from_slice(&first_bytes).unwrap();
    assert_eq!(first_body["stop_reason"], "tool_use");
    assert_eq!(first_body["content"][0]["id"], "call_native_1");

    let second = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("anthropic-version", "2023-06-01")
                .body(Body::from(
                    json!({
                        "model":"gpt-5.6-sol",
                        "max_tokens":128,
                        "messages":[
                            {"role":"user","content":"use the tool"},
                            {"role":"assistant","content":[{"type":"tool_use","id":"call_native_1","name":"get_value","input":{"key":"answer"}}]},
                            {"role":"user","content":[
                                {"type":"tool_result","tool_use_id":"call_native_1","content":"42"},
                                {"type":"text","text":"Use that result in the same turn."}
                            ]}
                        ],
                        "tools":[{
                            "name":"get_value",
                            "description":"Return a value",
                            "input_schema":{"type":"object","properties":{"key":{"type":"string"}},"required":["key"]}
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    let second_body: Value =
        serde_json::from_slice(&to_bytes(second.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(second_body["content"][0]["text"], "tool complete");

    let third = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("anthropic-version", "2023-06-01")
                .body(Body::from(
                    json!({
                        "model":"gpt-5.6-sol",
                        "max_tokens":128,
                        "messages":[
                            {"role":"user","content":"use the tool"},
                            {"role":"assistant","content":[{"type":"tool_use","id":"call_native_1","name":"get_value","input":{"key":"answer"}}]},
                            {"role":"user","content":[
                                {"type":"tool_result","tool_use_id":"call_native_1","content":"42"},
                                {"type":"text","text":"Use that result in the same turn."}
                            ]},
                            {"role":"assistant","content":"tool complete"},
                            {"role":"user","content":"now answer without a tool"}
                        ],
                        "tools":[{
                            "name":"get_value",
                            "description":"Return a value",
                            "input_schema":{"type":"object","properties":{"key":{"type":"string"}},"required":["key"]}
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(third.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::SeqCst), 3);

    let requests = server.received_requests().await.unwrap();
    let generations = requests
        .iter()
        .filter(|request| request.url.path() == "/v1/responses")
        .collect::<Vec<_>>();
    assert_eq!(generations.len(), 3);
    let first_key = generations[0]
        .headers
        .get("x-proxy-conversation-key")
        .unwrap();
    let second_key = generations[1]
        .headers
        .get("x-proxy-conversation-key")
        .unwrap();
    assert_eq!(first_key, second_key);
    assert_eq!(
        first_key,
        generations[2]
            .headers
            .get("x-proxy-conversation-key")
            .unwrap()
    );
    assert_eq!(
        generations[0]
            .headers
            .get("x-codex-proxy-continuation")
            .unwrap(),
        "false"
    );
    assert_eq!(
        generations[1]
            .headers
            .get("x-codex-proxy-continuation")
            .unwrap(),
        "true"
    );
    let continuation_body: Value = serde_json::from_slice(&generations[1].body).unwrap();
    let continuation_input = continuation_body["input"].as_array().unwrap();
    let output_index = continuation_input
        .iter()
        .position(|item| {
            item.get("type").and_then(Value::as_str) == Some("function_call_output")
                && item.get("call_id").and_then(Value::as_str) == Some("call_native_1")
        })
        .expect("tool result is preserved in native input");
    assert!(
        continuation_input[output_index + 1]
            .get("role")
            .and_then(Value::as_str)
            == Some("user"),
        "adjacent user text must remain after the function output"
    );
    assert_eq!(
        generations[2]
            .headers
            .get("x-codex-proxy-continuation")
            .unwrap(),
        "false"
    );
    assert_eq!(
        generations[0].headers.get("x-codex-proxy-turn-id"),
        generations[1].headers.get("x-codex-proxy-turn-id")
    );
    assert_ne!(
        generations[1].headers.get("x-codex-proxy-turn-id"),
        generations[2].headers.get("x-codex-proxy-turn-id")
    );
}

#[tokio::test]
async fn ordinary_anthropic_turns_keep_conversation_but_rotate_private_turn() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(NativeResponder {
            calls: Arc::new(AtomicUsize::new(0)),
        })
        .mount(&server)
        .await;
    let app = app(&server).await;

    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("anthropic-version", "2023-06-01")
                .body(Body::from(
                    json!({
                        "model":"gpt-5.6-sol",
                        "max_tokens":64,
                        "messages":[{"role":"user","content":"first turn"}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);

    let second = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("anthropic-version", "2023-06-01")
                .body(Body::from(
                    json!({
                        "model":"gpt-5.6-sol",
                        "max_tokens":64,
                        "messages":[
                            {"role":"user","content":"first turn"},
                            {"role":"assistant","content":"native ok"},
                            {"role":"user","content":"second turn"}
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::OK);

    let requests = server.received_requests().await.unwrap();
    let generations = requests
        .iter()
        .filter(|request| request.url.path() == "/v1/responses")
        .collect::<Vec<_>>();
    assert_eq!(generations.len(), 2);
    assert_eq!(
        generations[0].headers.get("x-proxy-conversation-key"),
        generations[1].headers.get("x-proxy-conversation-key"),
        "visible-history continuation must retain prompt-cache/session affinity"
    );
    assert_ne!(
        generations[0].headers.get("x-codex-proxy-turn-id"),
        generations[1].headers.get("x-codex-proxy-turn-id"),
        "a new human turn must not reuse private turn state"
    );
    assert_eq!(
        generations[1]
            .headers
            .get("x-codex-proxy-continuation")
            .unwrap(),
        "false"
    );
}

#[tokio::test]
async fn terminal_only_unoffered_native_function_is_never_exposed() {
    let server = MockServer::start().await;
    let events = vec![
        json!({"type":"response.created","response":{"id":"resp_private"}}),
        json!({
            "type":"response.completed",
            "response":{
                "id":"resp_private",
                "output":[{
                    "type":"function_call",
                    "call_id":"call_unoffered",
                    "name":"not_offered",
                    "arguments":"{}"
                }],
                "usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}
            }
        }),
    ];
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse(&events), "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let app = app(&server).await;
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model":"gpt-5.6-sol",
                        "input":"call only the offered function",
                        "store":false,
                        "tools":[{
                            "type":"function",
                            "name":"get_value",
                            "parameters":{"type":"object","properties":{},"required":[],"additionalProperties":false},
                            "strict":true
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(
        status,
        StatusCode::BAD_GATEWAY,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let text = String::from_utf8_lossy(&body);
    assert!(!text.contains("not_offered"));
    assert!(!text.contains("call_unoffered"));
    assert!(text.contains("invalid_tool_call"));
}

#[tokio::test]
async fn native_turn_conflict_is_terminal_and_never_calls_fallback() {
    let primary = MockServer::start().await;
    let fallback = MockServer::start().await;
    for server in [&primary, &fallback] {
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object":"list",
                "data":[{"id":"gpt-5.6-sol","object":"model","context_window":372000}]
            })))
            .mount(server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error":{
                "message":"private turn is no longer active",
                "type":"invalid_request_error",
                "param":"input",
                "code":"turn_state_lost"
            }
        })))
        .expect(1)
        .mount(&primary)
        .await;

    let mut config = common::test_config();
    config.upstreams = vec![UpstreamConfig {
        resilience: Default::default(),
        name: "codex-primary".into(),
        upstream_base_url: format!("{}/v1/", primary.uri()).parse().unwrap(),
        upstream_api_key: Some("primary-token".into()),
        upstream_model: None,
        wire_api: UpstreamWireApi::CodexResponses,
        upstream_chat_kwargs: Default::default(),
        upstream_request_log_path: None,
        responses_capabilities: None,
        fallback_upstreams: vec![FallbackUpstreamConfig {
            resilience: Default::default(),
            name: "codex-fallback".into(),
            upstream_base_url: format!("{}/v1/", fallback.uri()).parse().unwrap(),
            upstream_api_key: Some("fallback-token".into()),
            upstream_model: None,
            exposed_model: None,
            wire_api: UpstreamWireApi::CodexResponses,
            upstream_chat_kwargs: Default::default(),
            upstream_request_log_path: None,
            responses_capabilities: None,
        }],
    }];
    let response = llmconduit::build_app(config)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model":"gpt-5.6-sol","input":"hello","store":false}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert!(!String::from_utf8_lossy(&body).contains("llmconduit_error_"));
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"]["code"], "turn_state_lost");
    assert_eq!(body["error"]["param"], "input");
    assert!(
        fallback
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.url.path() != "/v1/responses"),
        "a private turn conflict is request-terminal and must not fail over"
    );
}

#[tokio::test]
async fn native_sidecar_statuses_preserve_anthropic_nonstream_and_stream_error_types() {
    for (status, code, param, expected_type) in [
        (409, "turn_state_lost", Some("input"), "conflict_error"),
        (
            429,
            "rate_limit_exceeded",
            Some("model"),
            "rate_limit_error",
        ),
        (504, "upstream_timeout", None, "timeout_error"),
    ] {
        // Retryable 429/504 responses cool their provider. Use a fresh app for
        // each wire mode so this regression isolates error projection.
        for stream in [false, true] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/responses"))
                .respond_with(ResponseTemplate::new(status).set_body_json(json!({
                    "error": {
                        "message": "private sidecar diagnostic must not be reflected",
                        "type": "invalid_request_error",
                        "param": param,
                        "code": code
                    }
                })))
                .expect(1)
                .mount(&server)
                .await;
            let response = app_with_retry(&server, false)
                .await
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/messages")
                        .header("content-type", "application/json")
                        .header("anthropic-version", "2023-06-01")
                        .body(Body::from(
                            json!({
                                "model":"gpt-5.6-sol",
                                "max_tokens":128,
                                "stream":stream,
                                "messages":[{"role":"user","content":"hello"}]
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            if !stream {
                assert_eq!(response.status().as_u16(), status);
                let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
                let body_json: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body_json["type"], "error");
                assert_eq!(body_json["error"]["type"], expected_type);
                assert!(!String::from_utf8_lossy(&body).contains("llmconduit_error_"));
                assert!(!String::from_utf8_lossy(&body).contains("private sidecar diagnostic"));
                continue;
            }

            assert_eq!(response.status(), StatusCode::OK);
            let body = String::from_utf8(
                to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap()
                    .to_vec(),
            )
            .unwrap();
            let error = body
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .filter_map(|data| serde_json::from_str::<Value>(data).ok())
                .find(|event| event["type"] == "error")
                .unwrap_or_else(|| panic!("missing Anthropic error event: {body}"));
            assert_eq!(error["error"]["type"], expected_type);
            assert!(!body.contains("llmconduit_error_"));
            assert!(!body.contains("private sidecar diagnostic"));
        }
    }
}

#[tokio::test]
async fn native_structured_output_is_validated_before_completion() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse(&text_events("not json")), "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let response = app(&server)
        .await
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model":"gpt-5.6-sol",
                        "input":"return an object",
                        "store":false,
                        "text":{"format":{
                            "type":"json_schema",
                            "name":"answer",
                            "schema":{
                                "type":"object",
                                "properties":{"answer":{"type":"integer"}},
                                "required":["answer"],
                                "additionalProperties":false
                            },
                            "strict":true
                        }}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["error"]["code"], "invalid_structured_output");
    assert!(!String::from_utf8_lossy(&body).contains("not json"));
}

#[tokio::test]
async fn anthropic_optional_strict_fields_are_required_on_native_responses_wire() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&text_events(
                r#"{"ok":true,"reason":"complete","impossible":false}"#,
            )),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let response = app(&server)
        .await
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("anthropic-version", "2023-06-01")
                .body(Body::from(
                    json!({
                        "model":"gpt-5.6-sol",
                        "max_tokens":128,
                        "stream":false,
                        "messages":[{"role":"user","content":"Evaluate the goal."}],
                        "output_config":{"format":{
                            "type":"json_schema",
                            "name":"goal_result",
                            "strict":true,
                            "schema":{
                                "type":"object",
                                "properties":{
                                    "impossible":{"type":"boolean"},
                                    "ok":{"type":"boolean"},
                                    "reason":{"type":"string"}
                                },
                                "required":["ok","reason"],
                                "additionalProperties":false
                            }
                        }}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let response_body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&response_body)
    );

    let requests = server.received_requests().await.unwrap();
    let generation = requests
        .iter()
        .find(|request| request.url.path() == "/v1/responses")
        .expect("generation request");
    let upstream: Value = serde_json::from_slice(&generation.body).unwrap();
    assert_eq!(
        upstream["text"]["format"]["schema"]["required"],
        json!(["ok", "reason", "impossible"])
    );
}

#[tokio::test]
async fn native_usage_without_provider_total_is_rejected_not_inferred() {
    let server = MockServer::start().await;
    let mut events = text_events("ok");
    events.last_mut().unwrap()["response"]["usage"] = json!({"input_tokens":5,"output_tokens":1});
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse(&events), "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let response = app(&server)
        .await
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model":"gpt-5.6-sol","input":"hello","store":false}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["error"]["code"], "malformed_upstream_response");
}

#[tokio::test]
async fn native_estimate_does_not_poison_later_chat_tokenizer() {
    let native = MockServer::start().await;
    let chat = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/tokenize"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"count":313})))
        .expect(1)
        .mount(&chat)
        .await;
    let app = mixed_app(&native, &chat, true).await;
    let tools = json!([{
        "name":"configure_environment",
        "description":"Configure Unicode environment values",
        "input_schema":{
            "type":"object",
            "properties":{
                "variables":{
                    "type":"object",
                    "additionalProperties":{"type":"string"}
                }
            },
            "required":["variables"]
        }
    }]);

    let (native_status, native_quality, native_body) =
        count_tokens(app.clone(), "gpt-5.6-sol", "Count 🦀 日本語", tools).await;
    assert_eq!(native_status, StatusCode::OK, "{native_body}");
    assert_eq!(native_quality.as_deref(), Some("estimated"));
    assert!(native_body["input_tokens"].as_u64().is_some_and(|n| n > 64));

    let (chat_status, chat_quality, chat_body) =
        count_tokens(app, "local-chat", "count exactly", json!([])).await;
    assert_eq!(chat_status, StatusCode::OK, "{chat_body}");
    assert_eq!(chat_quality.as_deref(), Some("exact"));
    assert_eq!(chat_body["input_tokens"], 313);

    let native_requests = native.received_requests().await.unwrap();
    assert!(
        native_requests
            .iter()
            .all(|request| request.url.path() == "/v1/models"),
        "native count must not trigger a billed generation or fake count request"
    );
}

#[tokio::test]
async fn unsupported_chat_tokenizer_does_not_poison_later_native_estimate() {
    let native = MockServer::start().await;
    let chat = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/tokenize"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&chat)
        .await;
    let app = mixed_app(&native, &chat, false).await;

    let (chat_status, chat_quality, chat_body) =
        count_tokens(app.clone(), "local-chat", "not supported", json!([])).await;
    assert_eq!(chat_status, StatusCode::NOT_FOUND, "{chat_body}");
    assert_eq!(chat_quality, None);

    let (native_status, native_quality, native_body) = count_tokens(
        app,
        "gpt-5.6-sol",
        "still count this native prompt",
        json!([]),
    )
    .await;
    assert_eq!(native_status, StatusCode::OK, "{native_body}");
    assert_eq!(native_quality.as_deref(), Some("estimated"));
    assert!(native_body["input_tokens"].as_u64().is_some_and(|n| n > 0));
}
