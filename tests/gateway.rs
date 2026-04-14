use async_trait::async_trait;
use axum::body::Body;
use axum::http::Request;
use futures::StreamExt;
use futures::stream;
use pretty_assertions::assert_eq;
use resp2chat::config::Config;
use resp2chat::engine::Gateway;
use resp2chat::models::chat::ChatChunkChoice;
use resp2chat::models::chat::ChatCompletionChunk;
use resp2chat::models::chat::ChatCompletionRequest;
use resp2chat::models::chat::ChatDelta;
use resp2chat::models::chat::ChatFunctionCall;
use resp2chat::models::chat::ChatToolCall;
use resp2chat::models::responses::ContentItem;
use resp2chat::models::responses::ResponseItem;
use resp2chat::models::responses::ResponsesRequest;
use resp2chat::models::responses::ToolSpec;
use resp2chat::monitor::MonitorHub;
use resp2chat::replay::ReplayStore;
use resp2chat::search::SearchClient;
use resp2chat::upstream::UpstreamClient;
use resp2chat::upstream::UpstreamStream;
use serde_json::Map as JsonMap;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;
use tower::ServiceExt;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[derive(Clone, Default)]
struct MockUpstream {
    requests: Arc<Mutex<Vec<ChatCompletionRequest>>>,
    responses: Arc<Mutex<VecDeque<Vec<Result<ChatCompletionChunk, resp2chat::error::AppError>>>>>,
}

impl MockUpstream {
    async fn push_response(
        &self,
        chunks: Vec<Result<ChatCompletionChunk, resp2chat::error::AppError>>,
    ) {
        self.responses.lock().await.push_back(chunks);
    }

    async fn requests(&self) -> Vec<ChatCompletionRequest> {
        self.requests.lock().await.clone()
    }
}

#[async_trait]
impl UpstreamClient for MockUpstream {
    async fn stream_chat_completion(
        &self,
        request: &ChatCompletionRequest,
    ) -> Result<UpstreamStream, resp2chat::error::AppError> {
        self.requests.lock().await.push(request.clone());
        let chunks = self
            .responses
            .lock()
            .await
            .pop_front()
            .expect("queued upstream response");
        Ok(Box::pin(stream::iter(chunks)))
    }

    async fn list_models(&self) -> Result<reqwest::Response, resp2chat::error::AppError> {
        Err(resp2chat::error::AppError::internal("unused in this test"))
    }
}

#[derive(Clone, Default)]
struct MockSearch {
    queries: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl SearchClient for MockSearch {
    async fn search(&self, query: &str) -> Result<String, resp2chat::error::AppError> {
        self.queries.lock().await.push(query.to_string());
        Ok(format!("Search result for {query}"))
    }
}

#[tokio::test]
async fn streams_function_call_turn() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(tool_call_chunk(
            "chat-1",
            "call_fn_1",
            "echo",
            "{\"value\":\"hi\"}",
        ))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());

    let request = ResponsesRequest {
        model: "glm-5.1".to_string(),
        instructions: String::new(),
        input: vec![user_message("hello")],
        tools: vec![ToolSpec::Function {
            name: "echo".to_string(),
            description: "Echo back a value".to_string(),
            strict: false,
            parameters: json!({
                "type": "object",
                "properties": { "value": { "type": "string" } },
                "required": ["value"]
            }),
        }],
        tool_choice: "auto".to_string(),
        parallel_tool_calls: true,
        reasoning: None,
        store: false,
        stream: true,
        include: Vec::new(),
        service_tier: None,
        prompt_cache_key: None,
        text: None,
        client_metadata: None,
        previous_response_id: None,
    };

    let events = collect_stream(gateway.stream_responses(request).await.expect("stream"));
    let events = events.await;

    assert_eq!(
        event_names(&events),
        vec![
            "response.created",
            "response.output_item.done",
            "response.completed",
        ]
    );
    assert_eq!(events[1]["item"]["type"].as_str(), Some("function_call"));
    assert_eq!(events[1]["item"]["name"].as_str(), Some("echo"));

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].parallel_tool_calls, false);
    assert_eq!(requests[0].tools.as_ref().map(Vec::len), Some(1));
}

#[tokio::test]
async fn uses_configured_upstream_model_override() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(content_chunk("chat-1", "hello"))])
        .await;
    let gateway = test_gateway_with_config(
        upstream.clone(),
        MockSearch::default(),
        Config {
            bind_addr: "127.0.0.1:0".parse().expect("socket addr"),
            upstream_base_url: "http://127.0.0.1:8000/v1".parse().expect("url"),
            upstream_api_key: None,
            upstream_model: Some("grok-4".to_string()),
            upstream_chat_kwargs: JsonMap::new(),
            brave_base_url: "https://example.com/".parse().expect("url"),
            brave_api_key: None,
            brave_max_results: 5,
            request_timeout: std::time::Duration::from_secs(30),
        },
    );

    let _ = collect_stream(
        gateway
            .stream_responses(base_request(vec![user_message("hello")]))
            .await
            .expect("stream"),
    )
    .await;

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "grok-4");
}

#[tokio::test]
async fn replays_reasoning_into_follow_up_request() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![
            Ok(reasoning_chunk("chat-1", "think step")),
            Ok(content_chunk("chat-1", "hello")),
        ])
        .await;
    upstream
        .push_response(vec![Ok(content_chunk("chat-2", "follow up done"))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());

    let first_request = base_request(vec![user_message("hello")]);
    let first_events = collect_stream(
        gateway
            .clone()
            .stream_responses(first_request.clone())
            .await
            .expect("first stream"),
    )
    .await;
    let public_items = done_items(&first_events);

    let mut second_input = first_request.input;
    second_input.extend(public_items);
    second_input.push(user_message("again"));
    let second_request = base_request(second_input);

    let _ = collect_stream(
        gateway
            .stream_responses(second_request)
            .await
            .expect("second stream"),
    )
    .await;

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 2);
    let second_messages = &requests[1].messages;
    assert_eq!(second_messages.len(), 3);
    assert_eq!(second_messages[1].role, "assistant");
    assert_eq!(
        second_messages[1].reasoning_content.as_deref(),
        Some("think step")
    );
    assert_eq!(
        second_messages[1]
            .content
            .as_ref()
            .and_then(|value| value.as_str()),
        Some("hello")
    );
}

#[tokio::test]
async fn forwards_configured_upstream_chat_kwargs() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(content_chunk("chat-1", "hello"))])
        .await;
    let gateway = test_gateway_with_config(
        upstream.clone(),
        MockSearch::default(),
        Config {
            bind_addr: "127.0.0.1:0".parse().expect("socket addr"),
            upstream_base_url: "http://127.0.0.1:8000/v1".parse().expect("url"),
            upstream_api_key: None,
            upstream_model: Some("GLM-5.1".to_string()),
            upstream_chat_kwargs: JsonMap::from_iter([(
                "clear_thinking".to_string(),
                json!(false),
            )]),
            brave_base_url: "https://example.com/".parse().expect("url"),
            brave_api_key: None,
            brave_max_results: 5,
            request_timeout: std::time::Duration::from_secs(30),
        },
    );

    let _ = collect_stream(
        gateway
            .stream_responses(base_request(vec![user_message("hello")]))
            .await
            .expect("stream"),
    )
    .await;

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].extra_body.get("clear_thinking"),
        Some(&json!(false))
    );
}

#[tokio::test]
async fn hides_web_search_loop_but_replays_internal_tool_result() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(tool_call_chunk(
            "chat-1",
            "call_ws_1",
            "web_search",
            "{\"query\":\"weather seattle\"}",
        ))])
        .await;
    upstream
        .push_response(vec![Ok(content_chunk("chat-2", "It is rainy."))])
        .await;
    upstream
        .push_response(vec![Ok(content_chunk("chat-3", "Follow up done."))])
        .await;
    let search = MockSearch::default();
    let gateway = test_gateway(upstream.clone(), search.clone());

    let mut first_request = base_request(vec![user_message("weather?")]);
    first_request.tools = vec![ToolSpec::WebSearch {
        external_web_access: Some(true),
        filters: None,
        user_location: None,
        search_context_size: None,
        search_content_types: None,
    }];
    let first_events = collect_stream(
        gateway
            .clone()
            .stream_responses(first_request.clone())
            .await
            .expect("first stream"),
    )
    .await;

    assert_eq!(
        event_names(&first_events),
        vec![
            "response.created",
            "response.output_item.added",
            "response.output_item.done",
            "response.output_item.added",
            "response.output_text.delta",
            "response.output_item.done",
            "response.completed",
        ]
    );
    assert_eq!(
        first_events[2]["item"]["type"].as_str(),
        Some("web_search_call")
    );
    assert_eq!(
        search.queries.lock().await.as_slice(),
        &["weather seattle".to_string()]
    );

    let mut second_input = first_request.input;
    second_input.extend(done_items(&first_events));
    second_input.push(user_message("why?"));
    let second_request = base_request(second_input);
    let _ = collect_stream(
        gateway
            .stream_responses(second_request)
            .await
            .expect("second stream"),
    )
    .await;

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].messages.len(), 3);
    assert_eq!(requests[1].messages[2].role, "tool");
    assert_eq!(
        requests[1].messages[2]
            .content
            .as_ref()
            .and_then(|value| value.as_str()),
        Some("Search result for weather seattle")
    );
    assert_eq!(requests[2].messages.len(), 5);
    assert_eq!(requests[2].messages[2].role, "tool");
    assert_eq!(requests[2].messages[3].role, "assistant");
}

#[tokio::test]
async fn degrades_gracefully_when_web_search_replay_baseline_is_missing() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(content_chunk("chat-1", "Recovered follow up."))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());

    let request = base_request(vec![
        user_message("weather?"),
        ResponseItem::WebSearchCall {
            id: Some("ws_old_1".to_string()),
            status: Some("completed".to_string()),
            action: Some(resp2chat::models::responses::WebSearchAction::Search {
                query: Some("weather seattle".to_string()),
                queries: None,
            }),
        },
        ResponseItem::message_text("assistant", "It is rainy."),
        user_message("why?"),
    ]);

    let events = collect_stream(
        gateway
            .stream_responses(request)
            .await
            .expect("stream should not fail"),
    )
    .await;

    assert_eq!(
        event_names(&events),
        vec![
            "response.created",
            "response.output_item.added",
            "response.output_text.delta",
            "response.output_item.done",
            "response.completed",
        ]
    );

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].messages.len(), 5);
    assert_eq!(requests[0].messages[1].role, "assistant");
    assert_eq!(requests[0].messages[2].role, "tool");
    assert_eq!(
        requests[0].messages[2].tool_call_id.as_deref(),
        Some("ws_old_1")
    );
    assert_eq!(
        requests[0].messages[2]
            .content
            .as_ref()
            .and_then(|value| value.as_str()),
        Some(
            "Previous web_search completed in an earlier turn, but the original tool result is unavailable because replay state was missing. Query: weather seattle"
        )
    );
}

#[tokio::test]
async fn proxies_models_endpoint_with_etag() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"etag-1\"")
                .set_body_json(json!({
                    "data": [{"id": "glm-5.1"}]
                })),
        )
        .mount(&server)
        .await;

    let config = Config {
        bind_addr: "127.0.0.1:0".parse().expect("socket addr"),
        upstream_base_url: format!("{}/v1/", server.uri()).parse().expect("url"),
        upstream_api_key: None,
        upstream_model: None,
        upstream_chat_kwargs: JsonMap::new(),
        brave_base_url: "https://example.com/".parse().expect("url"),
        brave_api_key: None,
        brave_max_results: 5,
        request_timeout: std::time::Duration::from_secs(30),
    };
    let app = resp2chat::build_app(config);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        response
            .headers()
            .get("etag")
            .and_then(|value| value.to_str().ok()),
        Some("\"etag-1\"")
    );
}

#[tokio::test]
async fn proxies_models_endpoint_with_upstream_api_key() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer upstream-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "glm-5.1"}]
        })))
        .mount(&server)
        .await;

    let config = Config {
        bind_addr: "127.0.0.1:0".parse().expect("socket addr"),
        upstream_base_url: format!("{}/v1/", server.uri()).parse().expect("url"),
        upstream_api_key: Some("upstream-secret".to_string()),
        upstream_model: None,
        upstream_chat_kwargs: JsonMap::new(),
        brave_base_url: "https://example.com/".parse().expect("url"),
        brave_api_key: None,
        brave_max_results: 5,
        request_timeout: std::time::Duration::from_secs(30),
    };
    let app = resp2chat::build_app(config);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status().as_u16(), 200);
}

fn test_gateway(upstream: MockUpstream, search: MockSearch) -> Arc<Gateway> {
    test_gateway_with_config(
        upstream,
        search,
        Config {
            bind_addr: "127.0.0.1:0".parse().expect("socket addr"),
            upstream_base_url: "http://127.0.0.1:8000/v1".parse().expect("url"),
            upstream_api_key: None,
            upstream_model: None,
            upstream_chat_kwargs: JsonMap::new(),
            brave_base_url: "https://example.com/".parse().expect("url"),
            brave_api_key: None,
            brave_max_results: 5,
            request_timeout: std::time::Duration::from_secs(30),
        },
    )
}

fn test_gateway_with_config(
    upstream: MockUpstream,
    search: MockSearch,
    config: Config,
) -> Arc<Gateway> {
    Arc::new(Gateway::new(
        config,
        ReplayStore::new(),
        Arc::new(upstream),
        Arc::new(search),
        MonitorHub::new(128),
    ))
}

fn base_request(input: Vec<ResponseItem>) -> ResponsesRequest {
    ResponsesRequest {
        model: "glm-5.1".to_string(),
        instructions: String::new(),
        input,
        tools: Vec::new(),
        tool_choice: "auto".to_string(),
        parallel_tool_calls: true,
        reasoning: Some(resp2chat::models::responses::ReasoningRequest {
            effort: Some("medium".to_string()),
            summary: None,
        }),
        store: false,
        stream: true,
        include: Vec::new(),
        service_tier: None,
        prompt_cache_key: None,
        text: None,
        client_metadata: None,
        previous_response_id: None,
    }
}

fn user_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
    }
}

fn content_chunk(id: &str, content: &str) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_string(),
        choices: vec![ChatChunkChoice {
            index: 0,
            delta: ChatDelta {
                content: Some(content.to_string()),
                reasoning_content: None,
                tool_calls: None,
            },
            finish_reason: None,
        }],
    }
}

fn reasoning_chunk(id: &str, reasoning: &str) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_string(),
        choices: vec![ChatChunkChoice {
            index: 0,
            delta: ChatDelta {
                content: None,
                reasoning_content: Some(reasoning.to_string()),
                tool_calls: None,
            },
            finish_reason: None,
        }],
    }
}

fn tool_call_chunk(id: &str, call_id: &str, name: &str, arguments: &str) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_string(),
        choices: vec![ChatChunkChoice {
            index: 0,
            delta: ChatDelta {
                content: None,
                reasoning_content: None,
                tool_calls: Some(vec![ChatToolCall {
                    id: Some(call_id.to_string()),
                    index: Some(0),
                    kind: "function".to_string(),
                    function: ChatFunctionCall {
                        name: Some(name.to_string()),
                        arguments: Some(serde_json::Value::String(arguments.to_string())),
                    },
                }]),
            },
            finish_reason: Some("tool_calls".to_string()),
        }],
    }
}

async fn collect_stream(
    stream: tokio_stream::wrappers::ReceiverStream<resp2chat::engine::SseEvent>,
) -> Vec<serde_json::Value> {
    stream
        .map(|event| {
            let mut value = event.data;
            if let serde_json::Value::Object(map) = &mut value {
                map.insert("_event".to_string(), serde_json::Value::String(event.event));
            }
            value
        })
        .collect()
        .await
}

fn event_names(events: &[serde_json::Value]) -> Vec<&str> {
    events
        .iter()
        .map(|event| {
            event["_event"]
                .as_str()
                .expect("event name should be present")
        })
        .collect()
}

fn done_items(events: &[serde_json::Value]) -> Vec<ResponseItem> {
    events
        .iter()
        .filter(|event| event["_event"] == "response.output_item.done")
        .map(|event| serde_json::from_value(event["item"].clone()).expect("response item"))
        .collect()
}
