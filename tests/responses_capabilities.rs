mod common;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use llmconduit::config::{FallbackUpstreamConfig, ModelProfile, UpstreamConfig};
use llmconduit::responses_capabilities::{
    AgentMessageEncryptedContentCapability, EncryptedReasoningCapability, InputImageCapability,
    PromptCacheKeyCapability, ResponsesCapabilitiesConfig, StructuredOutputCapability,
    TruncationAutoCapability,
};
use serde_json::json;
use std::collections::BTreeMap;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn mount_models(server: &MockServer, model: &str) {
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{"id": model, "object": "model"}]
        })))
        .mount(server)
        .await;
}

async fn mount_models_with_context(server: &MockServer, model: &str, context_window: i64) {
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{
                "id": model,
                "object": "model",
                "context_window": context_window
            }]
        })))
        .mount(server)
        .await;
}

async fn mount_success(server: &MockServer, model: &str) {
    let body = common::chat_completion_sse_body(&[
        json!({
            "id": "chat-capabilities",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": model,
            "choices": [{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]
        }),
        json!({
            "id": "chat-capabilities",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": model,
            "choices": [],
            "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
        }),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(server)
        .await;
}

async fn mount_success_with_content(server: &MockServer, model: &str, content: &str) {
    let body = common::chat_completion_sse_body(&[
        json!({
            "id": "chat-structured-output",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": model,
            "choices": [{"index":0,"delta":{"content":content},"finish_reason":"stop"}]
        }),
        json!({
            "id": "chat-structured-output",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": model,
            "choices": [],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
        }),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(server)
        .await;
}

async fn mount_success_with_service_tier(server: &MockServer, model: &str, tier: &str) {
    let body = common::chat_completion_sse_body(&[
        json!({
            "id": "chat-capabilities",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": model,
            "choices": [{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]
        }),
        json!({
            "id": "chat-capabilities",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": model,
            "service_tier": tier,
            "choices": [],
            "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
        }),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(server)
        .await;
}

async fn post_responses(app: axum::Router, body: serde_json::Value) -> axum::response::Response {
    app.oneshot(
        Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request"),
    )
    .await
    .expect("response")
}

#[tokio::test]
async fn responses_primary_capability_failure_is_pre_dispatch_400() {
    let upstream = MockServer::start().await;
    mount_models(&upstream, "model-a").await;
    let mut config = common::test_config();
    config.upstream_base_url = format!("{}/v1", upstream.uri()).parse().unwrap();

    let response = post_responses(
        llmconduit::build_app(config),
        json!({
            "model": "model-a",
            "input": "hello",
            "parallel_tool_calls": true
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["error"]["code"], "unsupported_parameter");
    assert_eq!(body["error"]["param"], "parallel_tool_calls");
    assert!(
        upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.url.path() != "/v1/chat/completions")
    );
}

#[tokio::test]
async fn responses_agent_message_encrypted_content_requires_plaintext_compat() {
    let request = || {
        json!({
            "model": "model-a",
            "input": [{
                "type": "agent_message",
                "id": "amsg_worker",
                "author": "/root/worker",
                "recipient": "/root",
                "content": [
                    {"type": "input_text", "text": "Payload:\n"},
                    {"type": "encrypted_content", "encrypted_content": "worker result"}
                ]
            }]
        })
    };

    let unsupported_upstream = MockServer::start().await;
    mount_models(&unsupported_upstream, "model-a").await;
    let mut unsupported = common::test_config();
    unsupported.upstream_base_url = format!("{}/v1", unsupported_upstream.uri())
        .parse()
        .unwrap();
    let rejected = post_responses(llmconduit::build_app(unsupported), request()).await;
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(rejected.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["error"]["code"], "unsupported_parameter");
    assert_eq!(body["error"]["param"], "input[0].content[1]");
    assert!(
        unsupported_upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.url.path() != "/v1/chat/completions")
    );

    let supported_upstream = MockServer::start().await;
    mount_models(&supported_upstream, "model-a").await;
    mount_success(&supported_upstream, "model-a").await;
    let mut supported = common::test_config();
    supported.upstream_base_url = format!("{}/v1", supported_upstream.uri()).parse().unwrap();
    supported
        .responses_capabilities
        .agent_message_encrypted_content =
        Some(AgentMessageEncryptedContentCapability::PlaintextCompat);
    let accepted = post_responses(llmconduit::build_app(supported), request()).await;
    assert_eq!(accepted.status(), StatusCode::OK);

    let requests = supported_upstream.received_requests().await.unwrap();
    let chat = requests
        .iter()
        .find(|request| request.url.path() == "/v1/chat/completions")
        .expect("chat request");
    let upstream_body: serde_json::Value = serde_json::from_slice(&chat.body).unwrap();
    assert_eq!(upstream_body["messages"][0]["role"], "user");
    assert_eq!(
        upstream_body["messages"][0]["content"],
        format!(
            "Inter-agent delivery: {}\nPayload:\n\nworker result",
            json!({"author": "/root/worker", "recipient": "/root"})
        )
    );
    assert!(
        upstream_body
            .get("llmconduit_agent_message_plaintext_compat")
            .is_none()
    );
}

#[tokio::test]
async fn responses_truncation_auto_is_capability_gated() {
    let unsupported_upstream = MockServer::start().await;
    mount_models(&unsupported_upstream, "model-a").await;
    let mut unsupported = common::test_config();
    unsupported.upstream_base_url = format!("{}/v1", unsupported_upstream.uri())
        .parse()
        .unwrap();

    let rejected = post_responses(
        llmconduit::build_app(unsupported),
        json!({
            "model": "model-a",
            "input": "hello",
            "truncation": "auto"
        }),
    )
    .await;
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(rejected.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["param"], "truncation");
    assert_eq!(body["error"]["code"], "unsupported_parameter");
    assert!(
        unsupported_upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.url.path() != "/v1/chat/completions")
    );

    let supported_upstream = MockServer::start().await;
    mount_models(&supported_upstream, "model-a").await;
    mount_success(&supported_upstream, "model-a").await;
    let mut supported = common::test_config();
    supported.upstream_base_url = format!("{}/v1", supported_upstream.uri()).parse().unwrap();
    supported.responses_capabilities = ResponsesCapabilitiesConfig {
        truncation_auto: Some(TruncationAutoCapability::Upstream),
        ..Default::default()
    };

    let accepted = post_responses(
        llmconduit::build_app(supported),
        json!({
            "model": "model-a",
            "input": "hello",
            "truncation": "auto"
        }),
    )
    .await;
    assert_eq!(accepted.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(accepted.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["status"], "completed");
    assert_eq!(body["truncation"], "auto");

    let requests = supported_upstream.received_requests().await.unwrap();
    let chat = requests
        .iter()
        .find(|request| request.url.path() == "/v1/chat/completions")
        .expect("chat request");
    let upstream_body: serde_json::Value = serde_json::from_slice(&chat.body).unwrap();
    assert_eq!(upstream_body["truncation"], "auto");
}

#[tokio::test]
async fn responses_structured_output_supports_all_three_formats() {
    let cases = [
        (json!({"type":"text"}), "plain output"),
        (json!({"type":"json_object"}), r#"{"ok":true}"#),
        (
            json!({
                "type":"json_schema",
                "name":"answer",
                "schema":{
                    "type":"object",
                    "properties":{"answer":{"type":"string"}},
                    "required":["answer"],
                    "additionalProperties":false
                },
                "strict":true
            }),
            r#"{"answer":"yes"}"#,
        ),
    ];

    for (format, generated) in cases {
        let upstream = MockServer::start().await;
        mount_models(&upstream, "model-a").await;
        mount_success_with_content(&upstream, "model-a", generated).await;
        let mut config = common::test_config();
        config.upstream_base_url = format!("{}/v1", upstream.uri()).parse().unwrap();
        config.responses_capabilities.structured_outputs = Some(vec![
            StructuredOutputCapability::Text,
            StructuredOutputCapability::JsonObject,
            StructuredOutputCapability::JsonSchema,
        ]);

        let response = post_responses(
            llmconduit::build_app(config),
            json!({
                "model":"model-a",
                "input":"answer",
                "text":{"format":format.clone()}
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "format={format}");
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["object"], "response", "format={format}");
        assert_eq!(body["status"], "completed", "format={format}");
        assert!(body["completed_at"].is_number(), "format={format}");
        assert!(body["error"].is_null(), "format={format}");
        assert!(body["incomplete_details"].is_null(), "format={format}");
        assert_eq!(body["text"]["format"], format, "format={format}");
        assert_eq!(body["output"][0]["type"], "message", "format={format}");
        assert_eq!(
            body["output"][0]["content"][0],
            json!({
                "type":"output_text",
                "text":generated,
                "annotations":[],
                "logprobs":[]
            }),
            "format={format}"
        );
        assert_eq!(body["usage"]["input_tokens"], 3, "format={format}");
        assert_eq!(body["usage"]["output_tokens"], 2, "format={format}");
        assert_eq!(body["usage"]["total_tokens"], 5, "format={format}");

        let requests = upstream.received_requests().await.unwrap();
        let chat = requests
            .iter()
            .find(|request| request.url.path() == "/v1/chat/completions")
            .expect("chat request");
        let upstream_body: serde_json::Value = serde_json::from_slice(&chat.body).unwrap();
        assert_eq!(
            upstream_body["response_format"]["type"], format["type"],
            "format={format}"
        );
        if format["type"] == "json_schema" {
            assert_eq!(
                upstream_body["response_format"]["json_schema"]["schema"],
                format["schema"]
            );
        }
    }
}

fn function_output_image_request() -> serde_json::Value {
    json!({
        "model": "model-a",
        "input": [
            {
                "type": "function_call",
                "name": "screenshot",
                "arguments": "{}",
                "call_id": "call_image"
            },
            {
                "type": "function_call_output",
                "call_id": "call_image",
                "output": [{
                    "type": "input_image",
                    "image_url": "data:image/png;base64,RECURSIVE_IMAGE_SENTINEL"
                }]
            },
            {"role": "user", "content": "describe the result"}
        ]
    })
}

fn custom_output_image_request() -> serde_json::Value {
    json!({
        "model": "model-a",
        "input": [
            {
                "type": "custom_tool_call",
                "name": "screenshot",
                "input": "capture",
                "call_id": "call_custom_image"
            },
            {
                "type": "custom_tool_call_output",
                "call_id": "call_custom_image",
                "output": [{
                    "type": "input_image",
                    "image_url": "data:image/png;base64,CUSTOM_IMAGE_SENTINEL"
                }]
            },
            {"role": "user", "content": "describe the result"}
        ]
    })
}

fn custom_output_file_request() -> serde_json::Value {
    json!({
        "model": "model-a",
        "input": [{
            "type": "custom_tool_call_output",
            "call_id": "call_custom_file",
            "output": [{
                "type": "input_file",
                "file_data": "CUSTOM_FILE_SENTINEL",
                "filename": "result.txt"
            }]
        }]
    })
}

fn instruction_image_request() -> serde_json::Value {
    json!({
        "model": "model-a",
        "instructions": [{
            "role": "developer",
            "content": [{
                "type": "input_image",
                "image_url": "data:image/png;base64,INSTRUCTION_IMAGE_SENTINEL"
            }]
        }],
        "input": "answer the request"
    })
}

#[tokio::test]
async fn responses_instruction_images_are_capability_gated_and_never_leak_non_native() {
    let rejecting_upstream = MockServer::start().await;
    mount_models(&rejecting_upstream, "model-a").await;
    let mut reject = common::test_config();
    reject.upstream_base_url = format!("{}/v1", rejecting_upstream.uri()).parse().unwrap();
    reject.responses_capabilities.input_image = Some(InputImageCapability::Reject);
    let rejected = post_responses(llmconduit::build_app(reject), instruction_image_request()).await;
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    let error: serde_json::Value =
        serde_json::from_slice(&to_bytes(rejected.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(error["error"]["param"], "instructions[0].content[0]");
    assert!(
        rejecting_upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.url.path() != "/v1/chat/completions")
    );

    let placeholder_upstream = MockServer::start().await;
    mount_models(&placeholder_upstream, "model-a").await;
    mount_success(&placeholder_upstream, "model-a").await;
    let mut placeholder = common::test_config();
    placeholder.upstream_base_url = format!("{}/v1", placeholder_upstream.uri())
        .parse()
        .unwrap();
    placeholder.responses_capabilities.input_image = Some(InputImageCapability::Placeholder);
    let accepted = post_responses(
        llmconduit::build_app(placeholder),
        instruction_image_request(),
    )
    .await;
    assert_eq!(accepted.status(), StatusCode::OK);
    let requests = placeholder_upstream.received_requests().await.unwrap();
    let chat = requests
        .iter()
        .find(|request| request.url.path() == "/v1/chat/completions")
        .expect("chat request");
    let body = String::from_utf8_lossy(&chat.body);
    assert!(!body.contains("INSTRUCTION_IMAGE_SENTINEL"));
    assert!(body.contains("image omitted"));
}

#[tokio::test]
async fn responses_reject_capability_catches_function_output_images() {
    let upstream = MockServer::start().await;
    mount_models(&upstream, "model-a").await;
    let mut config = common::test_config();
    config.upstream_base_url = format!("{}/v1", upstream.uri()).parse().unwrap();
    config.responses_capabilities.input_image = Some(InputImageCapability::Reject);

    let response = post_responses(
        llmconduit::build_app(config),
        function_output_image_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["error"]["code"], "unsupported_parameter");
    assert_eq!(body["error"]["param"], "input[1].output[0]");
    assert!(
        upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.url.path() != "/v1/chat/completions")
    );
}

#[tokio::test]
async fn responses_placeholder_capability_degrades_function_output_images() {
    let upstream = MockServer::start().await;
    mount_models(&upstream, "model-a").await;
    mount_success(&upstream, "model-a").await;
    let mut config = common::test_config();
    config.upstream_base_url = format!("{}/v1", upstream.uri()).parse().unwrap();
    config.responses_capabilities.input_image = Some(InputImageCapability::Placeholder);

    let response = post_responses(
        llmconduit::build_app(config),
        function_output_image_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let requests = upstream.received_requests().await.unwrap();
    let chat = requests
        .iter()
        .find(|request| request.url.path() == "/v1/chat/completions")
        .expect("chat request");
    let body = String::from_utf8_lossy(&chat.body);
    assert!(!body.contains("RECURSIVE_IMAGE_SENTINEL"));
    assert!(body.contains("the tool returned an image"));
}

#[tokio::test]
async fn responses_custom_outputs_share_image_and_file_capability_gates() {
    let rejecting_upstream = MockServer::start().await;
    mount_models(&rejecting_upstream, "model-a").await;
    let mut reject = common::test_config();
    reject.upstream_base_url = format!("{}/v1", rejecting_upstream.uri()).parse().unwrap();
    reject.responses_capabilities.input_image = Some(InputImageCapability::Reject);

    let rejected =
        post_responses(llmconduit::build_app(reject), custom_output_image_request()).await;
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(rejected.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["error"]["param"], "input[1].output[0]");
    assert!(
        rejecting_upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.url.path() != "/v1/chat/completions")
    );

    let placeholder_upstream = MockServer::start().await;
    mount_models(&placeholder_upstream, "model-a").await;
    mount_success(&placeholder_upstream, "model-a").await;
    let mut placeholder = common::test_config();
    placeholder.upstream_base_url = format!("{}/v1", placeholder_upstream.uri())
        .parse()
        .unwrap();
    placeholder.responses_capabilities.input_image = Some(InputImageCapability::Placeholder);
    let accepted = post_responses(
        llmconduit::build_app(placeholder),
        custom_output_image_request(),
    )
    .await;
    assert_eq!(accepted.status(), StatusCode::OK);
    let requests = placeholder_upstream.received_requests().await.unwrap();
    let chat = requests
        .iter()
        .find(|request| request.url.path() == "/v1/chat/completions")
        .expect("chat request");
    let upstream_body = String::from_utf8_lossy(&chat.body);
    assert!(!upstream_body.contains("CUSTOM_IMAGE_SENTINEL"));
    assert!(upstream_body.contains("the tool returned an image"));

    let file_upstream = MockServer::start().await;
    mount_models(&file_upstream, "model-a").await;
    let mut file_config = common::test_config();
    file_config.upstream_base_url = format!("{}/v1", file_upstream.uri()).parse().unwrap();
    let rejected_file = post_responses(
        llmconduit::build_app(file_config),
        custom_output_file_request(),
    )
    .await;
    assert_eq!(rejected_file.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = serde_json::from_slice(
        &to_bytes(rejected_file.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["error"]["param"], "input[0].output[0]");
    assert!(
        file_upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.url.path() != "/v1/chat/completions")
    );
}

#[tokio::test]
async fn responses_agent_capability_strips_function_output_images() {
    let upstream = MockServer::start().await;
    mount_models(&upstream, "model-a").await;
    mount_success(&upstream, "model-a").await;
    let mut config = common::test_config();
    config.upstream_base_url = format!("{}/v1", upstream.uri()).parse().unwrap();
    config.responses_capabilities.input_image = Some(InputImageCapability::Agent);
    config.image_agent_enabled = true;
    config.vision_url = Some("http://127.0.0.1:9/v1".parse().unwrap());

    let response = post_responses(
        llmconduit::build_app(config),
        function_output_image_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let requests = upstream.received_requests().await.unwrap();
    let chat = requests
        .iter()
        .find(|request| request.url.path() == "/v1/chat/completions")
        .expect("chat request");
    let body = String::from_utf8_lossy(&chat.body);
    assert!(!body.contains("RECURSIVE_IMAGE_SENTINEL"));
    assert!(body.contains("[Image #1]"));
    assert!(body.contains("analyzeImage"));
}

#[tokio::test]
async fn responses_native_capability_requires_and_honors_native_backend_policy() {
    let upstream = MockServer::start().await;
    mount_models(&upstream, "model-a").await;
    mount_success(&upstream, "model-a").await;

    let mut non_native = common::test_config();
    non_native.upstream_base_url = format!("{}/v1", upstream.uri()).parse().unwrap();
    non_native.responses_capabilities.input_image = Some(InputImageCapability::Native);
    let rejected = post_responses(
        llmconduit::build_app(non_native),
        function_output_image_request(),
    )
    .await;
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);

    let mut native = common::test_config();
    native.upstream_base_url = format!("{}/v1", upstream.uri()).parse().unwrap();
    native.responses_capabilities.input_image = Some(InputImageCapability::Native);
    native.model_profiles = BTreeMap::from([(
        "model-a".to_string(),
        ModelProfile {
            native_vision: Some(true),
            ..Default::default()
        },
    )]);
    let accepted = post_responses(
        llmconduit::build_app(native),
        function_output_image_request(),
    )
    .await;
    assert_eq!(accepted.status(), StatusCode::OK);
    let requests = upstream.received_requests().await.unwrap();
    let chat = requests
        .iter()
        .rev()
        .find(|request| request.url.path() == "/v1/chat/completions")
        .expect("native chat request");
    assert!(String::from_utf8_lossy(&chat.body).contains("RECURSIVE_IMAGE_SENTINEL"));
}

#[tokio::test]
async fn responses_multimodal_optional_fields_report_exact_type_paths() {
    let cases = [
        (
            json!({
                "model":"model-a",
                "input":[{"role":"user","content":[{
                    "type":"input_image", "image_url":"https://example.test/a.png", "detail":7
                }]}]
            }),
            "input[0].content[0].detail",
        ),
        (
            json!({
                "model":"model-a",
                "input":[{"type":"function_call_output","call_id":"call_1","output":[{
                    "type":"input_file", "file_data":false
                }]}]
            }),
            "input[0].output[0].file_data",
        ),
        (
            json!({
                "model":"model-a",
                "input":[{"type":"custom_tool_call_output","call_id":"call_1","output":[{
                    "type":"input_file", "file_data":false
                }]}]
            }),
            "input[0].output[0].file_data",
        ),
        (
            json!({
                "model":"model-a",
                "input":[{"role":"user","content":[{
                    "type":"input_image", "image_url":{"url":42}
                }]}]
            }),
            "input[0].content[0].image_url.url",
        ),
        (
            json!({
                "model":"model-a",
                "instructions":[{"role":"developer","content":[{
                    "type":"input_file", "file_data":false
                }]}],
                "input":"hello"
            }),
            "instructions[0].content[0].file_data",
        ),
    ];

    for (request, expected_param) in cases {
        let response = post_responses(llmconduit::build_app(common::test_config()), request).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["error"]["code"], "invalid_type");
        assert_eq!(body["error"]["param"], expected_param);
    }
}

#[tokio::test]
async fn responses_web_search_tool_choice_lowers_to_upstream_function_selector() {
    let upstream = MockServer::start().await;
    mount_models(&upstream, "model-a").await;
    mount_success(&upstream, "model-a").await;
    let mut config = common::test_config();
    config.upstream_base_url = format!("{}/v1", upstream.uri()).parse().unwrap();
    config.brave_api_key = Some("test-only-key".to_string());

    let response = post_responses(
        llmconduit::build_app(config),
        json!({
            "model":"model-a",
            "input":"find current information",
            "tools":[{"type":"web_search"}],
            "tool_choice":{"type":"web_search"}
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let requests = upstream.received_requests().await.unwrap();
    let chat = requests
        .iter()
        .find(|request| request.url.path() == "/v1/chat/completions")
        .expect("chat request");
    let body: serde_json::Value = serde_json::from_slice(&chat.body).unwrap();
    assert_eq!(
        body["tool_choice"],
        json!({"type":"function","function":{"name":"web_search"}})
    );
}

#[tokio::test]
async fn responses_custom_tool_choice_lowers_to_upstream_function_selector() {
    let upstream = MockServer::start().await;
    mount_models(&upstream, "model-a").await;
    mount_success(&upstream, "model-a").await;
    let mut config = common::test_config();
    config.upstream_base_url = format!("{}/v1", upstream.uri()).parse().unwrap();

    let response = post_responses(
        llmconduit::build_app(config),
        json!({
            "model":"model-a",
            "input":"apply the patch",
            "tools":[{
                "type":"custom",
                "name":"apply_patch",
                "format":{"type":"text"}
            }],
            "tool_choice":{"type":"custom","name":"apply_patch"}
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let requests = upstream.received_requests().await.unwrap();
    let chat = requests
        .iter()
        .find(|request| request.url.path() == "/v1/chat/completions")
        .expect("chat request");
    let body: serde_json::Value = serde_json::from_slice(&chat.body).unwrap();
    assert_eq!(
        body["tool_choice"],
        json!({"type":"function","function":{"name":"apply_patch"}})
    );
}

#[tokio::test]
async fn responses_unsupported_tool_choice_selector_is_explicit() {
    let upstream = MockServer::start().await;
    mount_models(&upstream, "model-a").await;
    let mut config = common::test_config();
    config.upstream_base_url = format!("{}/v1", upstream.uri()).parse().unwrap();

    let response = post_responses(
        llmconduit::build_app(config),
        json!({
            "model":"model-a",
            "input":"hello",
            "tools":[{
                "type":"function","name":"echo","strict":false,
                "parameters":{"type":"object"}
            }],
            "tool_choice":{"type":"file_search"}
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["error"]["code"], "unsupported_parameter");
    assert_eq!(body["error"]["param"], "tool_choice.type");
}

#[tokio::test]
async fn responses_capability_pruning_removes_only_incapable_fallbacks() {
    let primary = MockServer::start().await;
    let fallback = MockServer::start().await;
    mount_models(&primary, "model-a").await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("primary failed"))
        .expect(1)
        .mount(&primary)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&fallback)
        .await;

    let mut config = common::test_config();
    config.upstreams = vec![UpstreamConfig {
        name: "route-a".to_string(),
        upstream_base_url: format!("{}/v1", primary.uri()).parse().unwrap(),
        upstream_api_key: None,
        upstream_model: None,
        wire_api: Default::default(),
        upstream_chat_kwargs: Default::default(),
        upstream_request_log_path: None,
        responses_capabilities: Some(ResponsesCapabilitiesConfig {
            parallel_tool_calls: Some(true),
            ..Default::default()
        }),
        fallback_upstreams: vec![FallbackUpstreamConfig {
            name: "fallback-a".to_string(),
            upstream_base_url: format!("{}/v1", fallback.uri()).parse().unwrap(),
            upstream_api_key: None,
            upstream_model: None,
            exposed_model: None,
            wire_api: Default::default(),
            upstream_chat_kwargs: Default::default(),
            upstream_request_log_path: None,
            responses_capabilities: Some(ResponsesCapabilitiesConfig {
                parallel_tool_calls: Some(false),
                ..Default::default()
            }),
        }],
    }];

    let response = post_responses(
        llmconduit::build_app(config),
        json!({
            "model": "model-a",
            "input": "hello",
            "parallel_tool_calls": true
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(fallback.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn responses_gateway_hash_cache_key_is_echoed_but_not_forwarded() {
    let upstream = MockServer::start().await;
    mount_models(&upstream, "model-a").await;
    mount_success(&upstream, "model-a").await;
    let mut config = common::test_config();
    config.upstream_base_url = format!("{}/v1", upstream.uri()).parse().unwrap();
    config.responses_capabilities = ResponsesCapabilitiesConfig {
        prompt_cache_key: Some(PromptCacheKeyCapability::GatewayHash),
        prompt_cache_retention: Some(vec!["24h".to_string()]),
        ..Default::default()
    };

    let response = post_responses(
        llmconduit::build_app(config),
        json!({
            "model": "model-a",
            "input": "hello",
            "prompt_cache_key": "opaque-sentinel-cache-key",
            "prompt_cache_retention": "24h"
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(
        response_body["prompt_cache_key"],
        "opaque-sentinel-cache-key"
    );
    assert_eq!(response_body["prompt_cache_retention"], "24h");

    let requests = upstream.received_requests().await.unwrap();
    let chat = requests
        .iter()
        .find(|request| request.url.path() == "/v1/chat/completions")
        .expect("chat request");
    let body: serde_json::Value = serde_json::from_slice(&chat.body).unwrap();
    assert!(body.get("prompt_cache_key").is_none());
    assert_eq!(body["prompt_cache_retention"], "24h");
    assert!(!String::from_utf8_lossy(&chat.body).contains("opaque-sentinel-cache-key"));
}

#[tokio::test]
async fn responses_rejects_impossible_output_limit_without_dispatch() {
    let upstream = MockServer::start().await;
    mount_models_with_context(&upstream, "model-a", 128).await;
    let mut config = common::test_config();
    config.upstream_base_url = format!("{}/v1", upstream.uri()).parse().unwrap();

    let response = post_responses(
        llmconduit::build_app(config),
        json!({
            "model": "model-a",
            "input": "hello",
            "max_output_tokens": 129
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["error"]["param"], "max_output_tokens");
    assert_eq!(body["error"]["code"], "invalid_value");
    assert!(
        upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.url.path() != "/v1/chat/completions")
    );
}

#[tokio::test]
async fn responses_context_overflow_never_shrinks_or_retries() {
    let upstream = MockServer::start().await;
    mount_models_with_context(&upstream, "model-a", 202752).await;
    let overflow = "This model's maximum context length is 202752 tokens. \
        However, you requested 64000 output tokens and your prompt contains 139000 input tokens.";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_string(overflow))
        .mount(&upstream)
        .await;
    let mut config = common::test_config();
    config.upstream_base_url = format!("{}/v1", upstream.uri()).parse().unwrap();

    let response = post_responses(
        llmconduit::build_app(config),
        json!({
            "model": "model-a",
            "input": "hello",
            "max_output_tokens": 64000
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let chat_requests: Vec<_> = upstream
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/v1/chat/completions")
        .collect();
    assert_eq!(chat_requests.len(), 1);
    let sent: serde_json::Value = serde_json::from_slice(&chat_requests[0].body).unwrap();
    assert_eq!(sent["max_tokens"], 64000);
}

#[tokio::test]
async fn responses_service_tier_is_gated_forwarded_and_reports_actual_tier() {
    let upstream = MockServer::start().await;
    mount_models(&upstream, "model-a").await;
    mount_success_with_service_tier(&upstream, "model-a", "priority").await;
    let mut config = common::test_config();
    config.upstream_base_url = format!("{}/v1", upstream.uri()).parse().unwrap();
    config.responses_capabilities = ResponsesCapabilitiesConfig {
        service_tiers: Some(vec!["default".to_string()]),
        ..Default::default()
    };

    let response = post_responses(
        llmconduit::build_app(config),
        json!({
            "model": "model-a",
            "input": "hello",
            "service_tier": "default"
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(response_body["service_tier"], "priority");

    let requests = upstream.received_requests().await.unwrap();
    let chat = requests
        .iter()
        .find(|request| request.url.path() == "/v1/chat/completions")
        .expect("chat request");
    let body: serde_json::Value = serde_json::from_slice(&chat.body).unwrap();
    assert_eq!(body["service_tier"], "default");
}

#[tokio::test]
async fn responses_service_tier_is_null_when_upstream_does_not_report_it() {
    let upstream = MockServer::start().await;
    mount_models(&upstream, "model-a").await;
    mount_success(&upstream, "model-a").await;
    let mut config = common::test_config();
    config.upstream_base_url = format!("{}/v1", upstream.uri()).parse().unwrap();
    config.responses_capabilities = ResponsesCapabilitiesConfig {
        service_tiers: Some(vec!["default".to_string()]),
        ..Default::default()
    };

    let response = post_responses(
        llmconduit::build_app(config),
        json!({
            "model": "model-a",
            "input": "hello",
            "service_tier": "default"
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(response_body["service_tier"], serde_json::Value::Null);

    let requests = upstream.received_requests().await.unwrap();
    let chat = requests
        .iter()
        .find(|request| request.url.path() == "/v1/chat/completions")
        .expect("chat request");
    let body: serde_json::Value = serde_json::from_slice(&chat.body).unwrap();
    assert_eq!(body["service_tier"], "default");
}

#[tokio::test]
async fn responses_encrypted_reasoning_requires_include_and_passthrough_capability() {
    let upstream = MockServer::start().await;
    mount_models(&upstream, "model-a").await;
    let body = common::chat_completion_sse_body(&[
        json!({
            "id": "chat-reasoning",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": "model-a",
            "choices": [{
                "index": 0,
                "delta": {"thinking": {"content": "safe summary", "signature": "opaque-state"}},
                "finish_reason": "stop"
            }]
        }),
        json!({
            "id": "chat-reasoning",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": "model-a",
            "choices": [],
            "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
        }),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&upstream)
        .await;

    let mut config = common::test_config();
    config.upstream_base_url = format!("{}/v1", upstream.uri()).parse().unwrap();
    config.responses_capabilities = ResponsesCapabilitiesConfig {
        encrypted_reasoning: Some(EncryptedReasoningCapability::Passthrough),
        ..Default::default()
    };
    let app = llmconduit::build_app(config);

    let hidden = post_responses(app.clone(), json!({"model":"model-a","input":"hello"})).await;
    assert_eq!(hidden.status(), StatusCode::OK);
    let hidden: serde_json::Value =
        serde_json::from_slice(&to_bytes(hidden.into_body(), usize::MAX).await.unwrap()).unwrap();
    let hidden_reasoning = hidden["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "reasoning")
        .unwrap();
    assert!(hidden_reasoning.get("encrypted_content").is_none());

    let exposed = post_responses(
        app,
        json!({
            "model":"model-a",
            "input":"hello",
            "include":["reasoning.encrypted_content"]
        }),
    )
    .await;
    assert_eq!(exposed.status(), StatusCode::OK);
    let exposed: serde_json::Value =
        serde_json::from_slice(&to_bytes(exposed.into_body(), usize::MAX).await.unwrap()).unwrap();
    let exposed_reasoning = exposed["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "reasoning")
        .unwrap();
    assert_eq!(exposed_reasoning["encrypted_content"], "opaque-state");
}
