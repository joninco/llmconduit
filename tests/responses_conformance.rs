//! Focused, deterministic conformance checks for the public OpenAI Responses
//! surface. These tests intentionally drive the real Axum router: the canonical
//! engine stream is an internal protocol and does not include the final public
//! projection (event filtering, sequence numbers, and internal-field removal).

mod common;

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use common::{
    MockSearch, MockUpstream, base_request, content_chunk, finish_chunk, nested_thinking_chunk,
    parse_responses_sse_events, test_config, test_gateway, test_gateway_with_config,
    tool_call_chunk, usage_chunk, user_message,
};
use llmconduit::api_auth::ApiAuth;
use llmconduit::models::chat::{
    ChatChunkChoice, ChatCompletionChunk, ChatDelta, ChatFunctionCall, ChatToolCall,
};
use llmconduit::models::responses::ResponseItem;
use llmconduit::response_store::{
    ResponseStore, ResponseStoreError, ResponseStoreHandle, StoredResponse,
};
use llmconduit::responses_capabilities::{ReasoningSummaryCapability, ResponsesCapabilitiesConfig};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Notify;
use tower::ServiceExt;

const BODY_LIMIT: usize = 1024 * 1024;

struct TempResponseStoreDir {
    root: PathBuf,
}

impl TempResponseStoreDir {
    fn new() -> Self {
        Self {
            root: std::env::temp_dir().join(format!(
                "llmconduit-responses-conformance-{}",
                uuid::Uuid::new_v4().simple()
            )),
        }
    }

    fn sqlite_path(&self) -> PathBuf {
        self.root.join("responses.sqlite3")
    }
}

impl Drop for TempResponseStoreDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[derive(Default)]
struct BlockingPrepareStore {
    prepare_started: Notify,
    release_prepare: Notify,
    publish_started: Notify,
    release_publish: Notify,
    delete_finished: Notify,
    prepared: AtomicBool,
    block_publish: AtomicBool,
    publish_calls: AtomicUsize,
    delete_calls: AtomicUsize,
    post_prepare_delete_calls: AtomicUsize,
}

#[async_trait]
impl ResponseStore for BlockingPrepareStore {
    async fn prepare(
        &self,
        _id: String,
        _requested_model: String,
        _served_model: String,
        _history: Vec<ResponseItem>,
        _created_at: i64,
    ) -> Result<(), ResponseStoreError> {
        self.prepare_started.notify_one();
        self.release_prepare.notified().await;
        self.prepared.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn publish(&self, _id: &str) -> Result<(), ResponseStoreError> {
        self.publish_started.notify_one();
        if self.block_publish.load(Ordering::SeqCst) {
            self.release_publish.notified().await;
        }
        self.publish_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn get(&self, _id: &str) -> Result<Option<StoredResponse>, ResponseStoreError> {
        Ok(None)
    }

    async fn delete(&self, _id: &str) -> Result<(), ResponseStoreError> {
        self.delete_calls.fetch_add(1, Ordering::SeqCst);
        if self.prepared.load(Ordering::SeqCst) {
            self.post_prepare_delete_calls
                .fetch_add(1, Ordering::SeqCst);
            self.delete_finished.notify_one();
        }
        Ok(())
    }

    async fn find_item(&self, _item_id: &str) -> Result<Option<ResponseItem>, ResponseStoreError> {
        Ok(None)
    }
}

fn responses_request(stream: bool, store: bool, input: &str) -> Value {
    json!({
        "model": "glm-5.1",
        "stream": stream,
        "store": store,
        "input": input,
        "metadata": { "suite": "responses_conformance" }
    })
}

async fn post_json(app: Router, body: &Value) -> axum::response::Response {
    app.oneshot(
        Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(body).expect("serialize request"),
            ))
            .expect("request"),
    )
    .await
    .expect("router response")
}

async fn response_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), BODY_LIMIT)
        .await
        .expect("read response body");
    serde_json::from_slice(&bytes).expect("JSON response")
}

async fn response_text(response: axum::response::Response) -> String {
    let bytes = to_bytes(response.into_body(), BODY_LIMIT)
        .await
        .expect("read response body");
    String::from_utf8(bytes.to_vec()).expect("UTF-8 response")
}

async fn assert_previous_response_not_found(app: Router, previous_response_id: &str) {
    let response = post_json(
        app,
        &json!({
            "model": "glm-5.1",
            "stream": false,
            "store": false,
            "previous_response_id": previous_response_id,
            "input": "continuation"
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    let error = response_json(response).await;
    assert_eq!(error["error"]["type"], "invalid_request_error");
    assert_eq!(error["error"]["param"], "previous_response_id");
    assert_eq!(error["error"]["code"], "response_not_found");
    assert_eq!(
        error["error"]["message"],
        "previous response was not found or is no longer stored"
    );
}

async fn queue_text_turn(upstream: &MockUpstream, id: &str, text: &str) {
    upstream.set_supported_models(["glm-5.1"]).await;
    upstream
        .push_response(vec![
            Ok(content_chunk(id, text)),
            Ok(finish_chunk(id, "stop")),
            Ok(usage_chunk(id, 7, 2, 9)),
        ])
        .await;
}

fn terminal_events(events: &[Value]) -> Vec<&Value> {
    events
        .iter()
        .filter(|event| {
            matches!(
                event["type"].as_str(),
                Some("response.completed" | "response.incomplete" | "response.failed")
            )
        })
        .collect()
}

fn custom_tool_call_fragment(
    response_id: &str,
    call_id: Option<&str>,
    name: Option<&str>,
    arguments: &str,
    finish_reason: Option<&str>,
) -> ChatCompletionChunk {
    ChatCompletionChunk {
        service_tier: None,
        id: response_id.to_string(),
        choices: vec![ChatChunkChoice {
            index: 0,
            delta: ChatDelta {
                tool_calls: Some(vec![ChatToolCall {
                    id: call_id.map(str::to_string),
                    index: Some(0),
                    kind: "function".to_string(),
                    function: ChatFunctionCall {
                        name: name.map(str::to_string),
                        arguments: Some(Value::String(arguments.to_string())),
                    },
                }]),
                ..Default::default()
            },
            finish_reason: finish_reason.map(str::to_string),
            stop_reason: None,
        }],
        usage: None,
    }
}

fn custom_tool_request(stream: bool) -> Value {
    json!({
        "model": "glm-5.1",
        "stream": stream,
        "store": false,
        "input": "apply the patch",
        "tools": [{
            "type": "custom",
            "name": "apply_patch",
            "description": "Apply a patch to the workspace",
            "format": {
                "type": "grammar",
                "syntax": "lark",
                "definition": "start: /(?s).+/"
            }
        }]
    })
}

#[tokio::test]
async fn responses_text_sse_lifecycle_is_ordered() {
    let upstream = MockUpstream::default();
    queue_text_turn(&upstream, "chat-stream", "Hello").await;
    let app = llmconduit::build_app_from_gateway(test_gateway(upstream, MockSearch::default()));

    let response = post_json(app, &responses_request(true, false, "Hi")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    let events = parse_responses_sse_events(&response_text(response).await);
    assert!(!events.is_empty());

    let types: Vec<&str> = events
        .iter()
        .map(|event| event["type"].as_str().expect("event type"))
        .collect();
    assert_eq!(types.first(), Some(&"response.created"));
    assert_eq!(types.get(1), Some(&"response.in_progress"));
    assert_eq!(types.last(), Some(&"response.completed"));
    assert_eq!(
        terminal_events(&events).len(),
        1,
        "exactly one terminal event"
    );

    for (expected, event) in events.iter().enumerate() {
        assert_eq!(
            event["sequence_number"].as_u64(),
            Some(expected as u64),
            "sequence number at event {expected}: {event}"
        );
    }

    let created = &events[0]["response"];
    let in_progress = &events[1]["response"];
    let terminal = &events.last().expect("terminal")["response"];
    let response_id = created["id"].as_str().expect("response id");
    assert!(response_id.starts_with("resp_"));
    assert_eq!(in_progress["id"], response_id);
    assert_eq!(terminal["id"], response_id);
    assert_eq!(in_progress["created_at"], created["created_at"]);
    assert_eq!(terminal["created_at"], created["created_at"]);

    let item_added = events
        .iter()
        .find(|event| event["type"] == "response.output_item.added")
        .expect("message item added");
    let item_done = events
        .iter()
        .find(|event| event["type"] == "response.output_item.done")
        .expect("message item done");
    let delta = events
        .iter()
        .find(|event| event["type"] == "response.output_text.delta")
        .expect("output text delta");
    let item_id = item_added["item"]["id"].as_str().expect("message id");
    assert!(item_id.starts_with("msg_"));
    assert_eq!(item_added["output_index"], 0);
    assert_eq!(item_done["output_index"], 0);
    assert_eq!(item_done["item"]["id"], item_id);
    assert_eq!(delta["item_id"], item_id);
    assert_eq!(delta["output_index"], 0);
    assert_eq!(terminal["output"][0]["id"], item_id);
    assert_eq!(terminal["output"][0]["content"][0]["text"], "Hello");
    for required in [
        "completed_at",
        "conversation",
        "error",
        "incomplete_details",
        "instructions",
        "max_output_tokens",
        "metadata",
        "parallel_tool_calls",
        "previous_response_id",
        "reasoning",
        "temperature",
        "text",
        "top_p",
        "truncation",
        "usage",
    ] {
        assert!(
            created.get(required).is_some(),
            "missing {required}: {created}"
        );
    }
    assert_eq!(created["parallel_tool_calls"], false);
    assert_eq!(created["temperature"], 1.0);
    assert_eq!(created["top_p"], 1.0);
    assert_eq!(created["truncation"], "disabled");
    assert_eq!(created["text"]["format"]["type"], "text");
    assert_eq!(item_added["item"]["content"], json!([]));

    // Gateway-only transport fields and events must never escape the public
    // Responses projection.
    for event in &events {
        assert_ne!(
            event["type"],
            "response.reasoning_summary_text.signature_delta"
        );
        assert_ne!(event["type"], "response.web_search_results");
        if let Some(resource) = event.get("response") {
            assert!(resource.get("estimated_input_tokens").is_none(), "{event}");
            assert!(resource.get("terminal_reason").is_none(), "{event}");
            assert!(resource.get("stop_sequence").is_none(), "{event}");
        }
        if let Some(item) = event.get("item") {
            assert!(item.get("namespace").is_none(), "{event}");
        }
    }
}

#[tokio::test]
async fn responses_internal_fields_never_reach_public_wire() {
    let upstream = MockUpstream::default();
    upstream.set_supported_models(["glm-5.1"]).await;
    upstream
        .push_response(vec![
            Ok(nested_thinking_chunk(
                "chat-reasoning",
                "safe summary",
                "opaque-internal-signature",
            )),
            Ok(content_chunk("chat-reasoning", "answer")),
            Ok(finish_chunk("chat-reasoning", "stop")),
        ])
        .await;
    let app = llmconduit::build_app_from_gateway(test_gateway(upstream, MockSearch::default()));

    let response = post_json(app, &responses_request(true, false, "reason")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_text(response).await;
    assert!(!body.contains("opaque-internal-signature"), "{body}");
    let events = parse_responses_sse_events(&body);
    assert!(events.iter().all(|event| {
        event["type"] != "response.reasoning_text.delta"
            && event["type"] != "response.reasoning_summary_text.delta"
    }));
    assert!(events.iter().all(|event| {
        event["type"] != "response.reasoning_summary_text.signature_delta"
            && event["type"] != "response.web_search_results"
    }));
    for (sequence_number, event) in events.iter().enumerate() {
        assert_eq!(
            event["sequence_number"].as_u64(),
            Some(sequence_number as u64),
            "filtering an internal event must not leave a sequence gap: {event}"
        );
        if let Some(resource) = event.get("response") {
            for key in ["estimated_input_tokens", "terminal_reason", "stop_sequence"] {
                assert!(
                    resource.get(key).is_none(),
                    "public response leaked {key}: {event}"
                );
            }
            for item in resource["output"].as_array().into_iter().flatten() {
                if item["type"] == "reasoning" {
                    assert_eq!(item["summary"], json!([]));
                    assert!(
                        item.get("content").is_none(),
                        "hidden reasoning leaked: {item}"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn responses_reasoning_summary_uses_only_explicit_safe_channel() {
    let upstream = MockUpstream::default();
    upstream.set_supported_models(["glm-5.1"]).await;
    upstream
        .push_response(vec![
            Ok(ChatCompletionChunk {
                service_tier: None,
                id: "chat-safe-summary".to_string(),
                choices: vec![ChatChunkChoice {
                    index: 0,
                    delta: ChatDelta {
                        reasoning_content: Some("private chain of thought".to_string()),
                        extra: std::collections::BTreeMap::from([(
                            "reasoning_summary".to_string(),
                            json!("brief safe summary"),
                        )]),
                        ..Default::default()
                    },
                    finish_reason: None,
                    stop_reason: None,
                }],
                usage: None,
            }),
            Ok(finish_chunk("chat-safe-summary", "stop")),
        ])
        .await;
    let mut config = test_config();
    config.responses_capabilities = ResponsesCapabilitiesConfig {
        reasoning_summary: Some(ReasoningSummaryCapability::Upstream),
        ..Default::default()
    };
    let app = llmconduit::build_app_from_gateway(test_gateway_with_config(
        upstream,
        MockSearch::default(),
        config,
    ));
    let response = post_json(
        app,
        &json!({
            "model": "glm-5.1",
            "stream": true,
            "store": false,
            "input": "reason",
            "reasoning": { "summary": "auto" }
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_text(response).await;
    assert!(!body.contains("private chain of thought"), "{body}");
    assert!(body.contains("brief safe summary"), "{body}");
    let events = parse_responses_sse_events(&body);
    assert!(events.iter().any(|event| {
        event["type"] == "response.reasoning_summary_text.delta"
            && event["delta"] == "brief safe summary"
    }));
}

#[tokio::test]
async fn responses_failed_event_contains_live_partial_resource() {
    let upstream = MockUpstream::default();
    upstream.set_supported_models(["glm-5.1"]).await;
    upstream
        .push_response(vec![
            Ok(content_chunk("chat-partial-failure", "partial text")),
            Err(llmconduit::error::AppError::upstream(
                "upstream stream broke",
            )),
        ])
        .await;
    let app = llmconduit::build_app_from_gateway(test_gateway(upstream, MockSearch::default()));
    let response = post_json(app, &responses_request(true, false, "hello")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let events = parse_responses_sse_events(&response_text(response).await);
    let failed = events
        .iter()
        .find(|event| event["type"] == "response.failed")
        .expect("failed terminal event");
    assert_eq!(terminal_events(&events).len(), 1);
    assert_eq!(failed["response"]["status"], "failed");
    assert!(failed["response"]["completed_at"].is_null());
    assert_eq!(failed["response"]["output"][0]["status"], "incomplete");
    assert_eq!(
        failed["response"]["output"][0]["content"][0]["text"],
        "partial text"
    );
}

#[tokio::test]
async fn unterminated_stream_fails_without_exposing_unvalidated_function_call() {
    let upstream = MockUpstream::default();
    upstream.set_supported_models(["glm-5.1"]).await;
    let mut function_chunk = tool_call_chunk(
        "chat-unterminated",
        "call_unterminated",
        "echo",
        r#"{"value":"partial""#,
    );
    function_chunk.choices[0].finish_reason = None;
    upstream
        .push_unterminated_response(vec![
            Ok(content_chunk("chat-unterminated", "partial text")),
            Ok(function_chunk),
        ])
        .await;

    let app = llmconduit::build_app_from_gateway(test_gateway(upstream, MockSearch::default()));
    let mut request = responses_request(true, false, "hello");
    request["tools"] = json!([{
        "type": "function",
        "name": "echo",
        "strict": false,
        "parameters": {
            "type": "object",
            "properties": { "value": { "type": "string" } }
        }
    }]);
    let response = post_json(app, &request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let events = parse_responses_sse_events(&response_text(response).await);
    let event_types = events
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(terminal_events(&events).len(), 1, "{event_types:?}");
    assert_eq!(event_types.last(), Some(&"response.failed"));
    for forbidden in [
        "response.output_text.done",
        "response.content_part.done",
        "response.function_call_arguments.done",
        "response.output_item.done",
        "response.completed",
        "response.incomplete",
    ] {
        assert!(
            !event_types.contains(&forbidden),
            "unterminated streams must not emit {forbidden}: {event_types:?}"
        );
    }

    assert!(events.iter().all(|event| {
        !(event["type"] == "response.output_item.added" && event["item"]["type"] == "function_call")
    }));
    assert!(
        !event_types.contains(&"response.function_call_arguments.delta"),
        "unvalidated arguments must remain quarantined: {event_types:?}"
    );

    let failed = events.last().expect("failed terminal event");
    assert!(
        failed["response"]["output"]
            .as_array()
            .expect("failed output")
            .iter()
            .all(|item| item["type"] != "function_call")
    );
    let failed_message = failed["response"]["output"]
        .as_array()
        .expect("failed output")
        .iter()
        .find(|item| item["type"] == "message")
        .expect("already-served partial text remains visible");
    assert_eq!(failed_message["status"], "incomplete");
}

#[tokio::test]
async fn responses_refusal_lifecycle_is_conformant() {
    let upstream = MockUpstream::default();
    upstream.set_supported_models(["glm-5.1"]).await;
    upstream
        .push_response(vec![
            Ok(ChatCompletionChunk {
                service_tier: None,
                id: "chat-refusal".to_string(),
                choices: vec![ChatChunkChoice {
                    index: 0,
                    delta: ChatDelta {
                        refusal: Some("I cannot help with that.".to_string()),
                        ..Default::default()
                    },
                    finish_reason: None,
                    stop_reason: None,
                }],
                usage: None,
            }),
            Ok(finish_chunk("chat-refusal", "content_filter")),
        ])
        .await;
    let app = llmconduit::build_app_from_gateway(test_gateway(upstream, MockSearch::default()));
    let response = post_json(app, &responses_request(true, false, "unsafe request")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let events = parse_responses_sse_events(&response_text(response).await);
    let types = events
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect::<Vec<_>>();
    for required in [
        "response.output_item.added",
        "response.content_part.added",
        "response.refusal.delta",
        "response.refusal.done",
        "response.content_part.done",
        "response.output_item.done",
        "response.incomplete",
    ] {
        assert!(types.contains(&required), "missing {required}: {types:?}");
    }
    let terminal = &terminal_events(&events)[0]["response"];
    let done = events
        .iter()
        .find(|event| event["type"] == "response.output_item.done")
        .expect("refusal item done");
    assert_eq!(done["item"]["status"], "incomplete");
    assert!(terminal["completed_at"].is_null());
    assert_eq!(terminal["output"][0]["status"], "incomplete");
    assert_eq!(terminal["incomplete_details"]["reason"], "content_filter");
    assert_eq!(terminal["output"][0]["content"][0]["type"], "refusal");
}

#[tokio::test]
async fn responses_length_and_content_filter_are_incomplete() {
    for (finish_reason, incomplete_reason) in [
        ("length", "max_output_tokens"),
        ("content_filter", "content_filter"),
    ] {
        let upstream = MockUpstream::default();
        upstream.set_supported_models(["glm-5.1"]).await;
        upstream
            .push_response(vec![
                Ok(content_chunk(
                    &format!("chat-{finish_reason}"),
                    "partial answer",
                )),
                Ok(finish_chunk(
                    &format!("chat-{finish_reason}"),
                    finish_reason,
                )),
            ])
            .await;
        let app = llmconduit::build_app_from_gateway(test_gateway(upstream, MockSearch::default()));
        let response = post_json(app, &responses_request(true, false, "hello")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let events = parse_responses_sse_events(&response_text(response).await);
        let done = events
            .iter()
            .find(|event| event["type"] == "response.output_item.done")
            .expect("output item done");
        assert_eq!(done["item"]["status"], "incomplete");
        let terminal = terminal_events(&events);
        assert_eq!(terminal.len(), 1);
        assert_eq!(terminal[0]["type"], "response.incomplete");
        let resource = &terminal[0]["response"];
        assert_eq!(resource["status"], "incomplete");
        assert!(resource["completed_at"].is_null());
        assert_eq!(resource["incomplete_details"]["reason"], incomplete_reason);
        assert_eq!(resource["output"][0]["status"], "incomplete");
    }
}

#[tokio::test]
async fn responses_web_search_sources_are_included_only_when_requested() {
    let upstream = MockUpstream::default();
    upstream.set_supported_models(["glm-5.1"]).await;
    upstream
        .push_response(vec![Ok(tool_call_chunk(
            "chat-search-call",
            "call_search",
            "web_search",
            r#"{"query":"rust"}"#,
        ))])
        .await;
    queue_text_turn(&upstream, "chat-search-answer", "search answer").await;
    let search = MockSearch::default();
    let mut config = test_config();
    config.brave_api_key = Some("test-only-key".to_string());
    let app =
        llmconduit::build_app_from_gateway(test_gateway_with_config(upstream, search, config));
    let response = post_json(
        app,
        &json!({
            "model": "glm-5.1",
            "stream": false,
            "store": false,
            "input": "search",
            "include": ["web_search_call.action.sources"],
            "tools": [{ "type": "web_search" }]
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    let call = body["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "web_search_call")
        .expect("web search output item");
    assert_eq!(call["action"]["sources"][0]["type"], "url");
    assert_eq!(
        call["action"]["sources"][0]["url"],
        "https://example.com/result"
    );
}

#[tokio::test]
async fn responses_function_argument_events_use_stable_public_item_identity() {
    let upstream = MockUpstream::default();
    upstream.set_supported_models(["glm-5.1"]).await;
    upstream
        .push_response(vec![Ok(tool_call_chunk(
            "chat-tool",
            "call_upstream_1",
            "echo",
            r#"{"value":"hi"}"#,
        ))])
        .await;
    let app = llmconduit::build_app_from_gateway(test_gateway(upstream, MockSearch::default()));
    let response = post_json(
        app,
        &json!({
            "model": "glm-5.1",
            "stream": true,
            "store": false,
            "input": "call the function",
            "tools": [{
                "type": "function",
                "name": "echo",
                "strict": false,
                "parameters": {
                    "type": "object",
                    "properties": { "value": { "type": "string" } },
                    "required": ["value"]
                }
            }]
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let events = parse_responses_sse_events(&response_text(response).await);
    let added = events
        .iter()
        .find(|event| event["type"] == "response.output_item.added")
        .expect("function item added");
    let delta = events
        .iter()
        .find(|event| event["type"] == "response.function_call_arguments.delta")
        .expect("arguments delta");
    let arguments_done = events
        .iter()
        .find(|event| event["type"] == "response.function_call_arguments.done")
        .expect("arguments done");
    let item_done = events
        .iter()
        .find(|event| event["type"] == "response.output_item.done")
        .expect("function item done");
    let item_id = added["item"]["id"].as_str().expect("function item id");
    assert!(item_id.starts_with("fc_"));
    assert_eq!(delta["item_id"], item_id);
    assert_eq!(arguments_done["item_id"], item_id);
    assert_eq!(item_done["item"]["id"], item_id);
    assert_eq!(delta["output_index"], added["output_index"]);
    assert_eq!(arguments_done["output_index"], added["output_index"]);
    assert_eq!(item_done["output_index"], added["output_index"]);
    for event in [delta, arguments_done] {
        assert!(
            event.get("call_id").is_none(),
            "internal call_id leaked: {event}"
        );
    }
    assert!(delta.get("name").is_none(), "delta name leaked: {delta}");
    assert_eq!(arguments_done["name"], "echo");
    assert_eq!(item_done["item"]["call_id"], "call_upstream_1");
    assert_eq!(terminal_events(&events).len(), 1);
}

#[tokio::test]
async fn responses_generated_custom_tool_lifecycle_stream_and_nonstream() {
    let upstream = MockUpstream::default();
    upstream.set_supported_models(["glm-5.1"]).await;
    let patch = "*** Begin Patch\n*** Add File: generated.txt\n+hello\n*** End Patch";
    let arguments = json!({ "input": patch }).to_string();
    // Split immediately after a JSON escape introducer, then again in the
    // middle of the freeform body. No individual upstream fragment is valid
    // JSON; only their ordered accumulation is.
    let first_end = arguments.find("\\n").expect("escaped newline") + 1;
    let second_end = first_end + (arguments.len() - first_end) / 2;
    let chunks = || {
        vec![
            Ok(custom_tool_call_fragment(
                "chat-custom",
                Some("call_apply_patch"),
                Some("apply_patch"),
                &arguments[..first_end],
                None,
            )),
            Ok(custom_tool_call_fragment(
                "chat-custom",
                None,
                None,
                &arguments[first_end..second_end],
                None,
            )),
            Ok(custom_tool_call_fragment(
                "chat-custom",
                None,
                None,
                &arguments[second_end..],
                Some("tool_calls"),
            )),
        ]
    };
    upstream.push_response(chunks()).await;
    upstream.push_response(chunks()).await;
    let app = llmconduit::build_app_from_gateway(test_gateway(upstream, MockSearch::default()));

    let streaming = post_json(app.clone(), &custom_tool_request(true)).await;
    assert_eq!(streaming.status(), StatusCode::OK);
    let events = parse_responses_sse_events(&response_text(streaming).await);
    let event_types = events
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        event_types,
        vec![
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.custom_tool_call_input.delta",
            "response.custom_tool_call_input.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
    assert!(
        !event_types
            .iter()
            .any(|event| event.starts_with("response.function_call_arguments")),
        "the Chat JSON wrapper must not escape as function events: {event_types:?}"
    );

    let added = &events[2];
    let delta = &events[3];
    let input_done = &events[4];
    let item_done = &events[5];
    let terminal = &events[6]["response"];
    let item_id = added["item"]["id"].as_str().expect("custom output item id");
    assert!(item_id.starts_with("ctc_"));
    assert_eq!(added["item"]["type"], "custom_tool_call");
    assert_eq!(added["item"]["call_id"], "call_apply_patch");
    assert_eq!(added["item"]["name"], "apply_patch");
    assert_eq!(added["item"]["input"], "");
    assert!(added["item"].get("status").is_none());
    assert_eq!(delta["item_id"], item_id);
    assert_eq!(delta["output_index"], added["output_index"]);
    assert_eq!(delta["delta"], patch);
    assert!(delta.get("call_id").is_none());
    assert_eq!(input_done["item_id"], item_id);
    assert_eq!(input_done["output_index"], added["output_index"]);
    assert_eq!(input_done["input"], patch);
    assert!(input_done.get("call_id").is_none());
    assert_eq!(item_done["item"]["id"], item_id);
    assert_eq!(item_done["item"]["input"], patch);
    assert!(item_done["item"].get("status").is_none());
    assert_eq!(item_done["output_index"], added["output_index"]);
    assert_eq!(terminal["output"][0]["id"], item_id);
    assert_eq!(terminal["output"][0]["input"], patch);
    assert!(terminal["output"][0].get("status").is_none());
    assert_eq!(terminal_events(&events).len(), 1);

    let nonstreaming = post_json(app, &custom_tool_request(false)).await;
    assert_eq!(nonstreaming.status(), StatusCode::OK);
    let body = response_json(nonstreaming).await;
    assert_eq!(body["status"], "completed");
    assert_eq!(body["output"][0]["type"], "custom_tool_call");
    assert_eq!(body["output"][0]["call_id"], "call_apply_patch");
    assert_eq!(body["output"][0]["name"], "apply_patch");
    assert_eq!(body["output"][0]["input"], patch);
    assert!(body["output"][0].get("status").is_none());
    assert!(
        body["output"][0]["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("ctc_"))
    );
}

#[tokio::test]
async fn responses_malformed_custom_tool_input_fails_before_item_completion() {
    let upstream = MockUpstream::default();
    upstream.set_supported_models(["glm-5.1"]).await;
    upstream
        .push_response(vec![Ok(custom_tool_call_fragment(
            "chat-custom-malformed",
            Some("call_bad_patch"),
            Some("apply_patch"),
            r#"{"input":"*** Begin Patch"#,
            Some("tool_calls"),
        ))])
        .await;
    let app = llmconduit::build_app_from_gateway(test_gateway(upstream, MockSearch::default()));

    let response = post_json(app, &custom_tool_request(true)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let events = parse_responses_sse_events(&response_text(response).await);
    let event_types = events
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(event_types.last(), Some(&"response.failed"));
    assert_eq!(terminal_events(&events).len(), 1);
    for forbidden in [
        "response.output_item.added",
        "response.custom_tool_call_input.delta",
        "response.custom_tool_call_input.done",
        "response.output_item.done",
        "response.completed",
    ] {
        assert!(
            !event_types.contains(&forbidden),
            "malformed custom input emitted {forbidden}: {event_types:?}"
        );
    }
}

#[tokio::test]
async fn responses_terminal_output_matches_public_item_appearance_order() {
    let upstream = MockUpstream::default();
    upstream.set_supported_models(["glm-5.1"]).await;
    upstream
        .push_response(vec![
            Ok(tool_call_chunk(
                "chat-interleaved",
                "call_first",
                "echo",
                r#"{"value":"hi"}"#,
            )),
            Ok(content_chunk("chat-interleaved", "text after call")),
        ])
        .await;
    let app = llmconduit::build_app_from_gateway(test_gateway(upstream, MockSearch::default()));
    let response = post_json(
        app,
        &json!({
            "model": "glm-5.1",
            "stream": true,
            "store": false,
            "input": "call",
            "tools": [{
                "type": "function",
                "name": "echo",
                "strict": false,
                "parameters": {
                    "type": "object",
                    "properties": { "value": { "type": "string" } },
                    "required": ["value"]
                }
            }]
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let events = parse_responses_sse_events(&response_text(response).await);
    let added = events
        .iter()
        .filter(|event| event["type"] == "response.output_item.added")
        .collect::<Vec<_>>();
    assert_eq!(added.len(), 2);
    assert_eq!(added[0]["item"]["type"], "message");
    assert_eq!(added[0]["output_index"], 0);
    assert_eq!(added[1]["item"]["type"], "function_call");
    assert_eq!(added[1]["output_index"], 1);
    let terminal = &terminal_events(&events)[0]["response"];
    assert_eq!(terminal["output"][0]["type"], "message");
    assert_eq!(terminal["output"][1]["type"], "function_call");
    assert_eq!(terminal["output"][1]["call_id"], "call_first");
}

fn erase_generated_identity(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for key in ["id", "created_at", "completed_at"] {
                object.remove(key);
            }
            for value in object.values_mut() {
                erase_generated_identity(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                erase_generated_identity(value);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn responses_full_resource_stream_nonstream_equivalence() {
    let upstream = MockUpstream::default();
    queue_text_turn(&upstream, "chat-stream", "same answer").await;
    queue_text_turn(&upstream, "chat-json", "same answer").await;
    let app = llmconduit::build_app_from_gateway(test_gateway(upstream, MockSearch::default()));

    let streaming = post_json(app.clone(), &responses_request(true, false, "equivalence")).await;
    let events = parse_responses_sse_events(&response_text(streaming).await);
    let mut stream_resource = terminal_events(&events)[0]["response"].clone();

    let nonstream = post_json(app, &responses_request(false, false, "equivalence")).await;
    assert_eq!(nonstream.status(), StatusCode::OK);
    assert_eq!(
        nonstream
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    let mut json_resource = response_json(nonstream).await;

    erase_generated_identity(&mut stream_resource);
    erase_generated_identity(&mut json_resource);
    assert_eq!(json_resource, stream_resource);
    assert_eq!(json_resource["status"], "completed");
    assert_eq!(json_resource["usage"]["input_tokens"], 7);
    assert_eq!(json_resource["usage"]["output_tokens"], 2);
    assert_eq!(json_resource["usage"]["total_tokens"], 9);
}

#[tokio::test]
async fn responses_previous_response_id_continues_memory_history() {
    let upstream = MockUpstream::default();
    queue_text_turn(&upstream, "chat-parent", "parent answer").await;
    queue_text_turn(&upstream, "chat-child", "child answer").await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());
    let app = llmconduit::build_app_from_gateway(gateway);

    let parent = post_json(app.clone(), &responses_request(false, true, "parent input")).await;
    assert_eq!(parent.status(), StatusCode::OK);
    let parent = response_json(parent).await;
    let parent_id = parent["id"].as_str().expect("stored response id");

    let child = post_json(
        app,
        &json!({
            "model": "glm-5.1",
            "stream": false,
            "store": false,
            "previous_response_id": parent_id,
            "input": "child input"
        }),
    )
    .await;
    assert_eq!(child.status(), StatusCode::OK);
    let child = response_json(child).await;
    assert_eq!(child["previous_response_id"], parent_id);

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 2);
    let child_messages = &requests[1].messages;
    assert_eq!(
        child_messages
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        vec!["user", "assistant", "user"]
    );
    assert_eq!(child_messages[0].content, Some(json!("parent input")));
    assert_eq!(child_messages[1].content, Some(json!("parent answer")));
    assert_eq!(child_messages[2].content, Some(json!("child input")));
}

#[tokio::test]
async fn responses_previous_response_id_survives_sqlite_restart() {
    let temp = TempResponseStoreDir::new();
    let sqlite_path = temp.sqlite_path();

    let parent_upstream = MockUpstream::default();
    queue_text_turn(
        &parent_upstream,
        "chat-sqlite-parent",
        "durable parent answer",
    )
    .await;
    let parent_store = Arc::new(
        ResponseStoreHandle::sqlite(sqlite_path.clone(), 10, 720)
            .expect("create SQLite response store"),
    );
    let parent_gateway =
        Arc::try_unwrap(test_gateway(parent_upstream.clone(), MockSearch::default()))
            .ok()
            .expect("test gateway has one owner")
            .with_response_store(parent_store);
    let parent_app = llmconduit::build_app_from_gateway(Arc::new(parent_gateway));

    let parent = post_json(
        parent_app.clone(),
        &responses_request(false, true, "durable parent input"),
    )
    .await;
    assert_eq!(parent.status(), StatusCode::OK);
    let parent = response_json(parent).await;
    let parent_id = parent["id"]
        .as_str()
        .expect("stored response id")
        .to_string();
    drop(parent_app);

    let child_upstream = MockUpstream::default();
    queue_text_turn(&child_upstream, "chat-sqlite-child", "child answer").await;
    let restarted_store = Arc::new(
        ResponseStoreHandle::sqlite(sqlite_path, 10, 720).expect("reopen SQLite response store"),
    );
    let child_gateway =
        Arc::try_unwrap(test_gateway(child_upstream.clone(), MockSearch::default()))
            .ok()
            .expect("test gateway has one owner")
            .with_response_store(restarted_store);
    let child_app = llmconduit::build_app_from_gateway(Arc::new(child_gateway));

    let child = post_json(
        child_app,
        &json!({
            "model": "glm-5.1",
            "stream": false,
            "store": false,
            "previous_response_id": parent_id,
            "input": "child input after restart"
        }),
    )
    .await;
    assert_eq!(child.status(), StatusCode::OK);
    let child = response_json(child).await;
    assert_eq!(child["previous_response_id"], parent_id);

    let requests = child_upstream.requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]
            .messages
            .iter()
            .map(|message| (message.role.as_str(), message.content.clone()))
            .collect::<Vec<_>>(),
        vec![
            ("user", Some(json!("durable parent input"))),
            ("assistant", Some(json!("durable parent answer"))),
            ("user", Some(json!("child input after restart"))),
        ]
    );
}

#[tokio::test]
async fn responses_previous_instructions_are_not_inherited() {
    let upstream = MockUpstream::default();
    queue_text_turn(&upstream, "chat-instructions-parent", "parent answer").await;
    queue_text_turn(&upstream, "chat-instructions-child", "child answer").await;
    let app =
        llmconduit::build_app_from_gateway(test_gateway(upstream.clone(), MockSearch::default()));

    let parent = post_json(
        app.clone(),
        &json!({
            "model": "glm-5.1",
            "stream": false,
            "store": true,
            "instructions": "parent-only instruction sentinel",
            "input": "parent input"
        }),
    )
    .await;
    assert_eq!(parent.status(), StatusCode::OK);
    let parent_id = response_json(parent).await["id"]
        .as_str()
        .expect("stored response id")
        .to_string();

    let child = post_json(
        app,
        &json!({
            "model": "glm-5.1",
            "stream": false,
            "store": false,
            "previous_response_id": parent_id,
            "instructions": "current child instruction sentinel",
            "input": "child input"
        }),
    )
    .await;
    assert_eq!(child.status(), StatusCode::OK);
    let child = response_json(child).await;
    assert_eq!(child["instructions"], "current child instruction sentinel");

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0]
            .messages
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        vec!["system", "user"]
    );
    assert_eq!(
        requests[0].messages[0].content,
        Some(json!("parent-only instruction sentinel"))
    );
    assert_eq!(
        requests[1]
            .messages
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        vec!["system", "user", "assistant", "user"]
    );
    assert_eq!(
        requests[1].messages[0].content,
        Some(json!("current child instruction sentinel"))
    );
    assert!(requests[1].messages.iter().all(|message| {
        message.content.as_ref().is_none_or(|content| {
            !content
                .to_string()
                .contains("parent-only instruction sentinel")
        })
    }));
}

#[tokio::test]
async fn responses_structured_instructions_lower_and_echo_exact_union() {
    let upstream = MockUpstream::default();
    queue_text_turn(&upstream, "chat-structured-instructions", "answer").await;
    let app =
        llmconduit::build_app_from_gateway(test_gateway(upstream.clone(), MockSearch::default()));
    let instructions = json!([
        { "role": "developer", "content": "Follow the policy." },
        {
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": "Instruction context" }]
        }
    ]);

    let response = post_json(
        app,
        &json!({
            "model": "glm-5.1",
            "stream": false,
            "store": false,
            "instructions": instructions,
            "input": "Actual request"
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let resource = response_json(response).await;
    assert_eq!(
        resource["instructions"],
        json!([
            {
                "type": "message",
                "role": "developer",
                "content": [{ "type": "input_text", "text": "Follow the policy." }]
            },
            {
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "Instruction context" }]
            }
        ])
    );

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]
            .messages
            .iter()
            .map(|message| (message.role.as_str(), message.content.clone()))
            .collect::<Vec<_>>(),
        vec![
            ("developer", Some(json!("Follow the policy."))),
            ("user", Some(json!("Instruction context"))),
            ("user", Some(json!("Actual request"))),
        ]
    );
}

#[tokio::test]
async fn responses_instruction_item_reference_errors_name_exact_path() {
    let upstream = MockUpstream::default();
    upstream.set_supported_models(["glm-5.1"]).await;
    let app = llmconduit::build_app_from_gateway(test_gateway(upstream, MockSearch::default()));
    let response = post_json(
        app,
        &json!({
            "model": "glm-5.1",
            "store": false,
            "instructions": [{ "type": "item_reference", "id": "item_missing" }],
            "input": "hello"
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = response_json(response).await;
    assert_eq!(body["error"]["code"], "item_not_found");
    assert_eq!(body["error"]["param"], "instructions[0].id");
}

#[tokio::test]
async fn responses_expired_evicted_and_unknown_ids_return_404() {
    let evicting_upstream = MockUpstream::default();
    queue_text_turn(&evicting_upstream, "chat-evicted-parent", "first answer").await;
    queue_text_turn(
        &evicting_upstream,
        "chat-eviction-replacement",
        "second answer",
    )
    .await;
    let evicting_store = Arc::new(ResponseStoreHandle::memory(1, 720));
    let evicting_gateway = Arc::try_unwrap(test_gateway(
        evicting_upstream.clone(),
        MockSearch::default(),
    ))
    .ok()
    .expect("test gateway has one owner")
    .with_response_store(evicting_store);
    let evicting_app = llmconduit::build_app_from_gateway(Arc::new(evicting_gateway));

    let parent = post_json(
        evicting_app.clone(),
        &responses_request(false, true, "evict me"),
    )
    .await;
    assert_eq!(parent.status(), StatusCode::OK);
    let evicted_id = response_json(parent).await["id"]
        .as_str()
        .expect("stored response id")
        .to_string();
    let replacement = post_json(
        evicting_app.clone(),
        &responses_request(false, true, "replacement"),
    )
    .await;
    assert_eq!(replacement.status(), StatusCode::OK);
    assert_previous_response_not_found(evicting_app.clone(), &evicted_id).await;
    assert_previous_response_not_found(evicting_app, "resp_never_existed").await;
    assert_eq!(evicting_upstream.requests().await.len(), 2);

    let temp = TempResponseStoreDir::new();
    let sqlite_path = temp.sqlite_path();
    let expiring_upstream = MockUpstream::default();
    queue_text_turn(&expiring_upstream, "chat-expired-parent", "expired answer").await;
    let expiring_store = Arc::new(
        ResponseStoreHandle::sqlite(sqlite_path.clone(), 10, 720)
            .expect("create SQLite response store"),
    );
    let expiring_gateway = Arc::try_unwrap(test_gateway(
        expiring_upstream.clone(),
        MockSearch::default(),
    ))
    .ok()
    .expect("test gateway has one owner")
    .with_response_store(expiring_store);
    let expiring_app = llmconduit::build_app_from_gateway(Arc::new(expiring_gateway));
    let parent = post_json(
        expiring_app.clone(),
        &responses_request(false, true, "expire me"),
    )
    .await;
    assert_eq!(parent.status(), StatusCode::OK);
    let expired_id = response_json(parent).await["id"]
        .as_str()
        .expect("stored response id")
        .to_string();

    let connection = rusqlite::Connection::open(&sqlite_path).expect("open response store");
    assert_eq!(
        connection
            .execute(
                "UPDATE stored_responses SET expires_at = 0 WHERE id = ?1",
                [&expired_id],
            )
            .expect("expire stored response"),
        1
    );
    drop(connection);

    assert_previous_response_not_found(expiring_app, &expired_id).await;
    assert_eq!(expiring_upstream.requests().await.len(), 1);
}

#[tokio::test]
async fn responses_concurrent_children_read_the_same_parent_safely() {
    let upstream = MockUpstream::default();
    queue_text_turn(&upstream, "chat-shared-parent", "shared answer").await;
    queue_text_turn(&upstream, "chat-child-a", "answer a").await;
    queue_text_turn(&upstream, "chat-child-b", "answer b").await;
    let app =
        llmconduit::build_app_from_gateway(test_gateway(upstream.clone(), MockSearch::default()));

    let parent = post_json(
        app.clone(),
        &responses_request(false, true, "shared parent input"),
    )
    .await;
    assert_eq!(parent.status(), StatusCode::OK);
    let parent_id = response_json(parent).await["id"]
        .as_str()
        .expect("stored response id")
        .to_string();
    let child_a = json!({
        "model": "glm-5.1",
        "stream": false,
        "store": false,
        "previous_response_id": parent_id,
        "input": "child a"
    });
    let child_b = json!({
        "model": "glm-5.1",
        "stream": false,
        "store": false,
        "previous_response_id": parent_id,
        "input": "child b"
    });
    let (response_a, response_b) =
        tokio::join!(post_json(app.clone(), &child_a), post_json(app, &child_b));
    assert_eq!(response_a.status(), StatusCode::OK);
    assert_eq!(response_b.status(), StatusCode::OK);

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 3);
    let mut child_inputs = Vec::new();
    for request in &requests[1..] {
        assert_eq!(request.messages.len(), 3);
        assert_eq!(request.messages[0].role, "user");
        assert_eq!(
            request.messages[0].content,
            Some(json!("shared parent input"))
        );
        assert_eq!(request.messages[1].role, "assistant");
        assert_eq!(request.messages[1].content, Some(json!("shared answer")));
        assert_eq!(request.messages[2].role, "user");
        child_inputs.push(request.messages[2].content.clone());
    }
    child_inputs.sort_by_key(|value| value.as_ref().map(Value::to_string).unwrap_or_default());
    assert_eq!(
        child_inputs,
        vec![Some(json!("child a")), Some(json!("child b"))]
    );
}

#[tokio::test]
async fn responses_item_reference_resolves_stored_output_item() {
    let upstream = MockUpstream::default();
    queue_text_turn(&upstream, "chat-item-parent", "stored answer").await;
    queue_text_turn(&upstream, "chat-item-child", "continued answer").await;
    let app =
        llmconduit::build_app_from_gateway(test_gateway(upstream.clone(), MockSearch::default()));

    let parent = post_json(app.clone(), &responses_request(false, true, "parent input")).await;
    assert_eq!(parent.status(), StatusCode::OK);
    let parent = response_json(parent).await;
    let item_id = parent["output"][0]["id"]
        .as_str()
        .expect("stored output item id");

    let child = post_json(
        app,
        &json!({
            "model": "glm-5.1",
            "stream": false,
            "store": false,
            "input": [
                { "type": "item_reference", "id": item_id },
                { "role": "user", "content": "continue" }
            ]
        }),
    )
    .await;
    assert_eq!(child.status(), StatusCode::OK);
    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].messages[0].role, "assistant");
    assert_eq!(
        requests[1].messages[0].content,
        Some(json!("stored answer"))
    );
    assert_eq!(requests[1].messages[1].role, "user");
}

#[tokio::test]
async fn responses_unknown_item_reference_returns_sanitized_404() {
    let upstream = MockUpstream::default();
    upstream.set_supported_models(["glm-5.1"]).await;
    let app =
        llmconduit::build_app_from_gateway(test_gateway(upstream.clone(), MockSearch::default()));
    let response = post_json(
        app,
        &json!({
            "model": "glm-5.1",
            "input": [{ "type": "item_reference", "id": "item_missing" }]
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = response_json(response).await;
    assert_eq!(body["error"]["code"], "item_not_found");
    assert_eq!(body["error"]["param"], "input[0].id");
    assert!(upstream.requests().await.is_empty());
}

#[tokio::test]
async fn responses_store_false_is_not_referenceable() {
    let upstream = MockUpstream::default();
    queue_text_turn(&upstream, "chat-unstored", "not stored").await;
    let app =
        llmconduit::build_app_from_gateway(test_gateway(upstream.clone(), MockSearch::default()));

    let first = post_json(app.clone(), &responses_request(false, false, "unstored")).await;
    assert_eq!(first.status(), StatusCode::OK);
    let first = response_json(first).await;
    let unstored_id = first["id"].as_str().expect("response id");

    for previous_response_id in [unstored_id, "resp_unknown"] {
        let response = post_json(
            app.clone(),
            &json!({
                "model": "glm-5.1",
                "stream": false,
                "store": false,
                "previous_response_id": previous_response_id,
                "input": "continuation"
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let error = response_json(response).await;
        assert_eq!(error["error"]["type"], "invalid_request_error");
        assert_eq!(error["error"]["param"], "previous_response_id");
        assert_eq!(error["error"]["code"], "response_not_found");
        assert!(error["error"]["message"].is_string());
    }
    assert_eq!(upstream.requests().await.len(), 1);
}

#[tokio::test]
async fn responses_cancelled_prepare_is_never_published_and_is_cleaned_up() {
    let upstream = MockUpstream::default();
    queue_text_turn(&upstream, "chat-cancelled-store", "answer").await;
    let store = Arc::new(BlockingPrepareStore::default());
    let gateway = Arc::try_unwrap(test_gateway(upstream, MockSearch::default()))
        .ok()
        .expect("test gateway has one owner")
        .with_response_store(store.clone());
    let mut request = base_request(vec![user_message("persist me")]);
    request.store = true;

    let stream = Arc::new(gateway)
        .stream_responses(request)
        .await
        .expect("start response stream");
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        store.prepare_started.notified(),
    )
    .await
    .expect("storage prepare started");

    // Dropping the sole receiver makes `tx.closed()` ready. The engine's
    // biased cancellation branch must win even if the hidden write completes
    // immediately afterward, then delete that prepared row asynchronously.
    drop(stream);
    store.release_prepare.notify_one();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        store.delete_finished.notified(),
    )
    .await
    .expect("cancelled prepared state was discarded");

    assert_eq!(store.publish_calls.load(Ordering::SeqCst), 0);
    assert!(store.delete_calls.load(Ordering::SeqCst) >= 1);
    assert!(store.post_prepare_delete_calls.load(Ordering::SeqCst) >= 1);
}

#[tokio::test]
async fn responses_cancelled_publish_is_aborted_and_cleaned_up() {
    let upstream = MockUpstream::default();
    queue_text_turn(&upstream, "chat-cancelled-publish", "answer").await;
    let store = Arc::new(BlockingPrepareStore::default());
    store.block_publish.store(true, Ordering::SeqCst);
    let gateway = Arc::try_unwrap(test_gateway(upstream, MockSearch::default()))
        .ok()
        .expect("test gateway has one owner")
        .with_response_store(store.clone());
    let mut request = base_request(vec![user_message("persist me")]);
    request.store = true;

    let stream = Arc::new(gateway)
        .stream_responses(request)
        .await
        .expect("start response stream");
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        store.prepare_started.notified(),
    )
    .await
    .expect("storage prepare started");
    store.release_prepare.notify_one();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        store.publish_started.notified(),
    )
    .await
    .expect("storage publish started");

    drop(stream);
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        store.delete_finished.notified(),
    )
    .await
    .expect("cancelled publish state was discarded");

    assert_eq!(store.publish_calls.load(Ordering::SeqCst), 0);
    assert!(store.post_prepare_delete_calls.load(Ordering::SeqCst) >= 1);
}

#[tokio::test]
async fn replay_is_decoupled_from_store_and_disabled_by_default() {
    let upstream = MockUpstream::default();
    queue_text_turn(&upstream, "chat-replay-first", "first answer").await;
    queue_text_turn(&upstream, "chat-replay-second", "second answer").await;
    let app =
        llmconduit::build_app_from_gateway(test_gateway(upstream.clone(), MockSearch::default()));
    let request = responses_request(false, true, "same prompt");
    let first = response_json(post_json(app.clone(), &request).await).await;
    let second = response_json(post_json(app, &request).await).await;
    assert_eq!(first["output"][0]["content"][0]["text"], "first answer");
    assert_eq!(second["output"][0]["content"][0]["text"], "second answer");
    assert_eq!(upstream.requests().await.len(), 2);
}

#[tokio::test]
async fn responses_invalid_structured_output_fails_and_is_not_stored() {
    let cases = [
        ("json-object", "not json", json!({ "type": "json_object" })),
        (
            "json-schema",
            r#"{"value":"wrong type"}"#,
            json!({
                "type": "json_schema",
                "name": "integer_value",
                "strict": true,
                "schema": {
                    "type": "object",
                    "properties": { "value": { "type": "integer" } },
                    "required": ["value"],
                    "additionalProperties": false
                }
            }),
        ),
    ];

    for (case, output, format) in cases {
        let upstream = MockUpstream::default();
        upstream.set_supported_models(["glm-5.1"]).await;
        upstream
            .push_response(vec![
                Ok(content_chunk(&format!("chat-invalid-{case}"), output)),
                Ok(finish_chunk(&format!("chat-invalid-{case}"), "stop")),
            ])
            .await;
        let mut config = test_config();
        config.responses_capabilities.structured_outputs = Some(vec![
            llmconduit::responses_capabilities::StructuredOutputCapability::Text,
            llmconduit::responses_capabilities::StructuredOutputCapability::JsonObject,
            llmconduit::responses_capabilities::StructuredOutputCapability::JsonSchema,
        ]);
        let app = llmconduit::build_app_from_gateway(test_gateway_with_config(
            upstream.clone(),
            MockSearch::default(),
            config,
        ));

        let response = post_json(
            app.clone(),
            &json!({
                "model": "glm-5.1",
                "stream": true,
                "store": true,
                "input": "return JSON",
                "text": { "format": format }
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{case}");
        let events = parse_responses_sse_events(&response_text(response).await);
        let event_types = events
            .iter()
            .filter_map(|event| event["type"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(terminal_events(&events).len(), 1, "{case}: {event_types:?}");
        assert_eq!(event_types.last(), Some(&"response.failed"), "{case}");
        for forbidden in [
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
        ] {
            assert!(
                !event_types.contains(&forbidden),
                "{case} advertised {forbidden} before validation failed: {event_types:?}"
            );
        }

        let failed = terminal_events(&events)[0];
        assert_eq!(
            failed["response"]["error"]["code"], "invalid_structured_output",
            "{case}"
        );
        assert_eq!(
            failed["response"]["output"][0]["status"], "incomplete",
            "{case}"
        );
        let failed_id = failed["response"]["id"].as_str().expect("response id");

        let continuation = post_json(
            app,
            &json!({
                "model": "glm-5.1",
                "stream": false,
                "store": false,
                "previous_response_id": failed_id,
                "input": "continue"
            }),
        )
        .await;
        assert_eq!(continuation.status(), StatusCode::NOT_FOUND, "{case}");
        assert_eq!(upstream.requests().await.len(), 1, "{case}");
    }
}

#[tokio::test]
async fn responses_malformed_json_and_media_type_use_openai_errors() {
    let app = llmconduit::build_app_from_gateway(test_gateway(
        MockUpstream::default(),
        MockSearch::default(),
    ));
    let cases = [
        (
            "application/json",
            "{broken",
            StatusCode::BAD_REQUEST,
            "invalid_json",
        ),
        (
            "text/plain",
            r#"{"model":"glm-5.1","input":"hi"}"#,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
        ),
    ];

    for (content_type, body, status, code) in cases {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header(header::CONTENT_TYPE, content_type)
                    .body(Body::from(body))
                    .expect("request"),
            )
            .await
            .expect("router response");
        assert_eq!(response.status(), status);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
        let body = response_json(response).await;
        let error = body["error"].as_object().expect("OpenAI error object");
        for field in ["message", "type", "param", "code"] {
            assert!(error.contains_key(field), "missing error.{field}: {body}");
        }
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["code"], code);
    }
}

#[tokio::test]
async fn responses_wrong_types_and_hosted_variants_use_parameter_errors() {
    let app = llmconduit::build_app_from_gateway(test_gateway(
        MockUpstream::default(),
        MockSearch::default(),
    ));
    let cases = [
        (
            json!({
                "model": "glm-5.1",
                "input": "hi",
                "parallel_tool_calls": "yes"
            }),
            "invalid_type",
            "parallel_tool_calls",
        ),
        (
            json!({
                "model": "glm-5.1",
                "input": "hi",
                "tools": [{ "type": "code_interpreter", "container": "auto" }]
            }),
            "unsupported_parameter",
            "tools[0].type",
        ),
        (
            json!({
                "model": "glm-5.1",
                "input": "hi",
                "strict_schema_dialect": "anthropic"
            }),
            "unsupported_parameter",
            "strict_schema_dialect",
        ),
        (
            json!({
                "model": "glm-5.1",
                "instructions": [{ "type": "code_interpreter_call", "id": "ci_1" }],
                "input": "hi"
            }),
            "unsupported_parameter",
            "instructions[0].type",
        ),
        (
            json!({
                "model": "glm-5.1",
                "instructions": [{
                    "role": "developer",
                    "content": [{ "type": "input_audio", "input_audio": {} }]
                }],
                "input": "hi"
            }),
            "unsupported_parameter",
            "instructions[0].content[0].type",
        ),
        (
            json!({
                "model": "glm-5.1",
                "input": [{
                    "type": "custom_tool_call_output",
                    "call_id": "call_1",
                    "output": [{ "type": "input_audio", "input_audio": {} }]
                }]
            }),
            "unsupported_parameter",
            "input[0].output[0].type",
        ),
        (
            json!({
                "model": "glm-5.1",
                "input": [{
                    "role": "user",
                    "content": [{ "type": "input_text", "text": 42 }]
                }]
            }),
            "invalid_type",
            "input[0]",
        ),
    ];
    for (request, code, param_prefix) in cases {
        let response = post_json(app.clone(), &request).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response_json(response).await;
        assert_eq!(body["error"]["code"], code, "{body}");
        assert!(
            body["error"]["param"]
                .as_str()
                .is_some_and(|param| param.starts_with(param_prefix)),
            "{body}"
        );
    }
}

#[tokio::test]
async fn responses_missing_and_unknown_models_return_openai_errors() {
    let upstream = MockUpstream::default();
    upstream.set_supported_models(["glm-5.1"]).await;
    let app =
        llmconduit::build_app_from_gateway(test_gateway(upstream.clone(), MockSearch::default()));
    let cases = [
        (
            json!({ "input": "missing model", "store": false }),
            StatusCode::BAD_REQUEST,
            "invalid_value",
        ),
        (
            json!({ "model": "does-not-exist", "input": "unknown", "store": false }),
            StatusCode::NOT_FOUND,
            "model_not_found",
        ),
    ];

    for (request, status, code) in cases {
        let response = post_json(app.clone(), &request).await;
        assert_eq!(response.status(), status);
        let body = response_json(response).await;
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["param"], "model");
        assert_eq!(body["error"]["code"], code);
        assert!(body["error"]["message"].is_string());
    }
    assert!(
        upstream.requests().await.is_empty(),
        "invalid Responses models must fail before dispatch"
    );

    let empty_catalog = MockUpstream::default();
    let empty_app = llmconduit::build_app_from_gateway(test_gateway(
        empty_catalog.clone(),
        MockSearch::default(),
    ));
    let response = post_json(
        empty_app,
        &json!({ "model": "does-not-exist", "input": "unknown", "store": false }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = response_json(response).await;
    assert_eq!(body["error"]["code"], "model_not_found");
    assert_eq!(body["error"]["param"], "model");
    assert!(empty_catalog.requests().await.is_empty());

    let unavailable_catalog = MockUpstream::default();
    unavailable_catalog.set_catalog_error(true);
    let unavailable_app = llmconduit::build_app_from_gateway(test_gateway(
        unavailable_catalog.clone(),
        MockSearch::default(),
    ));
    let response = post_json(
        unavailable_app,
        &json!({ "model": "glm-5.1", "input": "catalog down", "store": false }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = response_json(response).await;
    assert_eq!(body["error"]["code"], "model_catalog_unavailable");
    assert!(body["error"]["param"].is_null());
    assert!(
        !body
            .to_string()
            .contains("sentinel provider catalog failure details")
    );
    assert!(unavailable_catalog.requests().await.is_empty());
}

#[tokio::test]
async fn responses_api_auth_accepts_dedicated_headers_and_rejects_invalid_tokens() {
    let upstream = MockUpstream::default();
    queue_text_turn(&upstream, "chat-bearer", "bearer ok").await;
    queue_text_turn(&upstream, "chat-key", "key ok").await;
    let gateway = match Arc::try_unwrap(test_gateway(upstream.clone(), MockSearch::default())) {
        Ok(gateway) => gateway,
        Err(_) => panic!("test gateway has one owner"),
    };
    let gateway =
        Arc::new(gateway.with_api_auth(Some(Arc::new(ApiAuth::new("dedicated-test-token")))));
    let app = llmconduit::build_app_from_gateway(gateway);
    let body =
        serde_json::to_vec(&responses_request(false, false, "auth")).expect("serialize request");

    for authorization in [None, Some((header::AUTHORIZATION, "Bearer wrong"))] {
        let mut request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some((name, value)) = authorization {
            request = request.header(name, value);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::from(body.clone())).expect("request"))
            .await
            .expect("router response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let error = response_json(response).await;
        assert_eq!(error["error"]["code"], "invalid_api_key");
    }

    for (name, value) in [
        (header::AUTHORIZATION, "Bearer dedicated-test-token"),
        (
            axum::http::HeaderName::from_static("x-api-key"),
            "dedicated-test-token",
        ),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(name, value)
                    .body(Body::from(body.clone()))
                    .expect("request"),
            )
            .await
            .expect("router response");
        assert_eq!(response.status(), StatusCode::OK);
        let _ = response_json(response).await;
    }
    assert_eq!(upstream.requests().await.len(), 2);
}
