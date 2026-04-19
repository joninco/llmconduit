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
use resp2chat::models::responses::NamespaceToolSpec;
use resp2chat::models::responses::ReasoningSummaryItem;
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
        tool_choice: json!("auto"),
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
        temperature: None,
        top_p: None,
        max_output_tokens: None,
    };

    let events = collect_stream(gateway.stream_responses(request).await.expect("stream"));
    let events = events.await;

    assert_eq!(
        event_names(&events),
        vec![
            "response.created",
            "response.in_progress",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
    let done_event = events.iter().find(|e| e["_event"] == "response.output_item.done").unwrap();
    assert_eq!(done_event["item"]["type"].as_str(), Some("function_call"));
    assert_eq!(done_event["item"]["name"].as_str(), Some("echo"));

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].parallel_tool_calls, false);
    assert_eq!(requests[0].tools.as_ref().map(Vec::len), Some(1));
}

#[tokio::test]
async fn flattens_namespace_tools_for_upstream_and_preserves_namespace_in_output() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(tool_call_chunk(
            "chat-1",
            "call_fn_1",
            "calendar_search",
            "{\"query\":\"today\"}",
        ))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());

    let request = ResponsesRequest {
        model: "glm-5.1".to_string(),
        instructions: String::new(),
        input: vec![user_message("what's on my calendar?")],
        tools: vec![ToolSpec::Namespace {
            name: "mcp__calendar".to_string(),
            description: "Calendar tools".to_string(),
            tools: vec![NamespaceToolSpec::Function {
                name: "calendar_search".to_string(),
                description: "Search calendar events".to_string(),
                strict: false,
                parameters: json!({
                    "type": "object",
                    "properties": { "query": { "type": "string" } },
                    "required": ["query"]
                }),
            }],
        }],
        tool_choice: json!("auto"),
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
        temperature: None,
        top_p: None,
        max_output_tokens: None,
    };

    let events = collect_stream(gateway.stream_responses(request).await.expect("stream")).await;

    let done_event = events.iter().find(|e| e["_event"] == "response.output_item.done").unwrap();
    assert_eq!(done_event["item"]["type"].as_str(), Some("function_call"));
    assert_eq!(done_event["item"]["name"].as_str(), Some("calendar_search"));
    assert_eq!(
        done_event["item"]["namespace"].as_str(),
        Some("mcp__calendar")
    );

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].tools.as_ref().map(Vec::len), Some(1));
    assert_eq!(
        requests[0]
            .tools
            .as_ref()
            .and_then(|tools| tools.first())
            .map(|tool| tool.function.name.as_str()),
        Some("calendar_search")
    );
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
            upstream_request_log_path: None,
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
async fn normalizes_developer_messages_to_system_for_upstream() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(content_chunk("chat-1", "ok"))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());

    let request = base_request(vec![
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "developer instruction".to_string(),
            }],
            phase: None,
        },
        user_message("hello"),
    ]);

    let _ = collect_stream(gateway.stream_responses(request).await.expect("stream")).await;

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].messages.len(), 2);
    assert_eq!(requests[0].messages[0].role, "system");
    assert_eq!(
        requests[0].messages[0]
            .content
            .as_ref()
            .and_then(|value| value.as_str()),
        Some("developer instruction")
    );
    assert_eq!(requests[0].messages[1].role, "user");
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
            upstream_request_log_path: None,
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
            "response.in_progress",
            "response.function_call_arguments.delta",
            "response.output_item.added",
            "response.output_item.done",
            "response.output_item.added",
            "response.output_text.delta",
            "response.output_text.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
    let ws_done = first_events.iter().find(|e| {
        e["_event"] == "response.output_item.done"
            && e["item"]["type"].as_str() == Some("web_search_call")
    });
    assert!(ws_done.is_some(), "expected a web_search_call done event");
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
            "response.in_progress",
            "response.output_item.added",
            "response.output_text.delta",
            "response.output_text.done",
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
        upstream_request_log_path: None,
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
        upstream_request_log_path: None,
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

#[tokio::test]
async fn merges_instructions_and_developer_into_single_system_message() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(content_chunk("chat-1", "ok"))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());

    let mut request = base_request(vec![
        user_message("hello"),
        ResponseItem::message_text("assistant", "hi"),
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "be concise".to_string(),
            }],
            phase: None,
        },
        user_message("how are you?"),
    ]);
    request.instructions = "You are a helpful assistant.".to_string();

    let _ = collect_stream(gateway.stream_responses(request).await.expect("stream")).await;

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].messages[0].role, "system");
    assert_eq!(
        requests[0].messages[0]
            .content
            .as_ref()
            .and_then(|v| v.as_str()),
        Some("You are a helpful assistant.")
    );
    // Mid-conversation developer message stays in place (not hoisted)
    assert_eq!(requests[0].messages[3].role, "system");
    assert_eq!(
        requests[0].messages[3]
            .content
            .as_ref()
            .and_then(|v| v.as_str()),
        Some("be concise")
    );
}

#[tokio::test]
async fn merges_multiple_developer_messages_scattered_in_history() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(content_chunk("chat-1", "ok"))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());

    let mut request = base_request(vec![
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "first instruction".to_string(),
            }],
            phase: None,
        },
        user_message("hello"),
        ResponseItem::message_text("assistant", "hi"),
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "second instruction".to_string(),
            }],
            phase: None,
        },
        user_message("how are you?"),
        ResponseItem::message_text("assistant", "fine"),
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "third instruction".to_string(),
            }],
            phase: None,
        },
        user_message("bye"),
    ]);
    request.instructions = "base".to_string();

    let _ = collect_stream(gateway.stream_responses(request).await.expect("stream")).await;

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 1);
    let messages = &requests[0].messages;
    assert_eq!(messages[0].role, "system");
    assert_eq!(
        messages[0].content.as_ref().and_then(|v| v.as_str()),
        Some("base\n\nfirst instruction")
    );
    let system_count = messages.iter().filter(|m| m.role == "system").count();
    assert_eq!(system_count, 3, "initial block coalesced, mid-conversation stay in place");
}

#[tokio::test]
async fn no_system_message_when_no_instructions_and_no_developer() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(content_chunk("chat-1", "ok"))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());

    let request = base_request(vec![
        user_message("hello"),
        ResponseItem::message_text("assistant", "hi"),
        user_message("bye"),
    ]);

    let _ = collect_stream(gateway.stream_responses(request).await.expect("stream")).await;

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 1);
    let system_count = requests[0]
        .messages
        .iter()
        .filter(|m| m.role == "system")
        .count();
    assert_eq!(system_count, 0, "no system message when nothing to merge");
    assert_eq!(requests[0].messages[0].role, "user");
}

#[tokio::test]
async fn developer_only_no_instructions_produces_single_system_message() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(content_chunk("chat-1", "ok"))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());

    let request = base_request(vec![
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "instruction A".to_string(),
            }],
            phase: None,
        },
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "instruction B".to_string(),
            }],
            phase: None,
        },
        user_message("hello"),
    ]);

    let _ = collect_stream(gateway.stream_responses(request).await.expect("stream")).await;

    let requests = upstream.requests().await;
    let messages = &requests[0].messages;
    assert_eq!(messages[0].role, "system");
    assert_eq!(
        messages[0].content.as_ref().and_then(|v| v.as_str()),
        Some("instruction A\n\ninstruction B")
    );
    assert_eq!(messages[1].role, "user");
    let system_count = messages.iter().filter(|m| m.role == "system").count();
    assert_eq!(system_count, 1);
}

#[tokio::test]
async fn function_call_history_with_developer_message_produces_correct_ordering() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(content_chunk("chat-1", "result is 555"))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());

    let mut request = base_request(vec![
        user_message("what is 15 * 37?"),
        ResponseItem::FunctionCall {
            id: None,
            name: "calculator".to_string(),
            namespace: None,
            arguments: r#"{"expression":"15*37"}"#.to_string(),
            call_id: "call_001".to_string(),
        },
        ResponseItem::FunctionCallOutput {
            call_id: "call_001".to_string(),
            output: serde_json::Value::String("555".to_string()),
        },
        ResponseItem::message_text("assistant", "15 * 37 = 555"),
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "show work step by step".to_string(),
            }],
            phase: None,
        },
        user_message("now add 45"),
    ]);
    request.instructions = "You are a calculator.".to_string();
    request.tools = vec![ToolSpec::Function {
        name: "calculator".to_string(),
        description: "Evaluate math".to_string(),
        strict: false,
        parameters: json!({
            "type": "object",
            "properties": { "expression": { "type": "string" } },
            "required": ["expression"]
        }),
    }];

    let events = collect_stream(gateway.stream_responses(request).await.expect("stream")).await;
    assert!(
        event_names(&events).contains(&"response.completed"),
        "stream should complete successfully"
    );

    let requests = upstream.requests().await;
    let messages = &requests[0].messages;
    assert_eq!(messages[0].role, "system");
    assert_eq!(
        messages[0].content.as_ref().and_then(|v| v.as_str()),
        Some("You are a calculator.")
    );
    assert_eq!(messages[1].role, "user");
    assert_eq!(messages[2].role, "assistant");
    assert!(
        messages[2].tool_calls.is_some(),
        "assistant message should have tool_calls"
    );
    assert_eq!(messages[3].role, "tool");
    assert_eq!(messages[3].tool_call_id.as_deref(), Some("call_001"));
    assert_eq!(messages[4].role, "assistant");
    assert_eq!(messages[5].role, "system");
    assert_eq!(
        messages[5].content.as_ref().and_then(|v| v.as_str()),
        Some("show work step by step")
    );
    assert_eq!(messages[6].role, "user");
    let system_count = messages.iter().filter(|m| m.role == "system").count();
    assert_eq!(system_count, 2);
}

#[tokio::test]
async fn multiple_function_calls_interleaved_with_developer_messages() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(content_chunk("chat-1", "done"))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());

    let mut request = base_request(vec![
        user_message("weather and time"),
        ResponseItem::FunctionCall {
            id: None,
            name: "get_weather".to_string(),
            namespace: None,
            arguments: r#"{"city":"NYC"}"#.to_string(),
            call_id: "call_w1".to_string(),
        },
        ResponseItem::FunctionCallOutput {
            call_id: "call_w1".to_string(),
            output: serde_json::Value::String("72F".to_string()),
        },
        ResponseItem::FunctionCall {
            id: None,
            name: "get_time".to_string(),
            namespace: None,
            arguments: r#"{"tz":"EST"}"#.to_string(),
            call_id: "call_t1".to_string(),
        },
        ResponseItem::FunctionCallOutput {
            call_id: "call_t1".to_string(),
            output: serde_json::Value::String("2:30 PM".to_string()),
        },
        ResponseItem::message_text("assistant", "NYC: 72F, 2:30 PM"),
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "include celsius".to_string(),
            }],
            phase: None,
        },
        user_message("what about London?"),
    ]);
    request.instructions = "You have weather and time tools.".to_string();
    request.tools = vec![
        ToolSpec::Function {
            name: "get_weather".to_string(),
            description: "Get weather".to_string(),
            strict: false,
            parameters: json!({"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}),
        },
        ToolSpec::Function {
            name: "get_time".to_string(),
            description: "Get time".to_string(),
            strict: false,
            parameters: json!({"type": "object", "properties": {"tz": {"type": "string"}}, "required": ["tz"]}),
        },
    ];

    let events = collect_stream(gateway.stream_responses(request).await.expect("stream")).await;
    assert!(event_names(&events).contains(&"response.completed"));

    let requests = upstream.requests().await;
    let messages = &requests[0].messages;
    assert_eq!(messages[0].role, "system");
    assert_eq!(
        messages[0].content.as_ref().and_then(|v| v.as_str()),
        Some("You have weather and time tools.")
    );
    let roles: Vec<&str> = messages.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(
        roles,
        vec!["system", "user", "assistant", "tool", "assistant", "tool", "assistant", "system", "user"]
    );
}

#[tokio::test]
async fn reasoning_with_developer_message_preserves_reasoning_content() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(content_chunk("chat-1", "5x^4"))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());

    let mut request = base_request(vec![
        user_message("derivative of x^3?"),
        ResponseItem::Reasoning {
            id: "rsn_1".to_string(),
            summary: vec![ReasoningSummaryItem::SummaryText {
                text: "power rule".to_string(),
            }],
            content: None,
            encrypted_content: None,
        },
        ResponseItem::message_text("assistant", "3x^2"),
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "show derivation steps".to_string(),
            }],
            phase: None,
        },
        user_message("what about x^5?"),
    ]);
    request.instructions = "You are a math tutor.".to_string();

    let events = collect_stream(gateway.stream_responses(request).await.expect("stream")).await;
    assert!(event_names(&events).contains(&"response.completed"));

    let requests = upstream.requests().await;
    let messages = &requests[0].messages;
    assert_eq!(messages[0].role, "system");
    assert_eq!(
        messages[0].content.as_ref().and_then(|v| v.as_str()),
        Some("You are a math tutor.")
    );
    assert_eq!(messages[1].role, "user");
    assert_eq!(messages[2].role, "assistant");
    assert_eq!(
        messages[2].reasoning_content.as_deref(),
        Some("power rule"),
        "reasoning should be attached to the following assistant message"
    );
    assert_eq!(messages[3].role, "system");
    assert_eq!(
        messages[3].content.as_ref().and_then(|v| v.as_str()),
        Some("show derivation steps")
    );
    assert_eq!(messages[4].role, "user");
}

#[tokio::test]
async fn custom_tool_call_with_developer_message() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(content_chunk("chat-1", "result"))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());

    let mut request = base_request(vec![
        user_message("run my script"),
        ResponseItem::CustomToolCall {
            status: Some("completed".to_string()),
            call_id: "call_ct1".to_string(),
            name: "run_script".to_string(),
            input: r#"print('hello')"#.to_string(),
        },
        ResponseItem::CustomToolCallOutput {
            call_id: "call_ct1".to_string(),
            name: Some("run_script".to_string()),
            output: serde_json::Value::String("hello".to_string()),
        },
        ResponseItem::message_text("assistant", "script printed hello"),
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "always explain what the script does".to_string(),
            }],
            phase: None,
        },
        user_message("run it again with different input"),
    ]);
    request.instructions = "You can run scripts.".to_string();
    request.tools = vec![ToolSpec::Custom {
        name: "run_script".to_string(),
        description: "Run a Python script".to_string(),
        format: resp2chat::models::responses::CustomToolFormat {
            kind: "text".to_string(),
            syntax: "python".to_string(),
            definition: "Python code to execute".to_string(),
        },
    }];

    let events = collect_stream(gateway.stream_responses(request).await.expect("stream")).await;
    assert!(event_names(&events).contains(&"response.completed"));

    let requests = upstream.requests().await;
    let messages = &requests[0].messages;
    assert_eq!(messages[0].role, "system");
    assert_eq!(
        messages[0].content.as_ref().and_then(|v| v.as_str()),
        Some("You can run scripts.")
    );
    let system_count = messages.iter().filter(|m| m.role == "system").count();
    assert_eq!(system_count, 2);
    assert_eq!(messages[1].role, "user");
    assert_eq!(messages[2].role, "assistant");
    assert!(messages[2].tool_calls.is_some());
    assert_eq!(messages[3].role, "tool");
    assert_eq!(messages[4].role, "assistant");
    assert_eq!(messages[5].role, "system");
    assert_eq!(
        messages[5].content.as_ref().and_then(|v| v.as_str()),
        Some("always explain what the script does")
    );
    assert_eq!(messages[6].role, "user");
}

#[tokio::test]
async fn local_shell_call_in_history_with_developer_message() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(content_chunk("chat-1", "ok"))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());

    let mut request = base_request(vec![
        user_message("list files"),
        ResponseItem::LocalShellCall {
            id: Some("ls_1".to_string()),
            call_id: Some("call_ls1".to_string()),
            status: "completed".to_string(),
            action: resp2chat::models::responses::LocalShellAction::Exec(
                resp2chat::models::responses::LocalShellExecAction {
                    command: vec!["ls".to_string(), "-la".to_string()],
                    timeout_ms: None,
                    working_directory: None,
                    env: None,
                    user: None,
                },
            ),
        },
        ResponseItem::FunctionCallOutput {
            call_id: "call_ls1".to_string(),
            output: serde_json::Value::String("file1.txt\nfile2.txt".to_string()),
        },
        ResponseItem::message_text("assistant", "found 2 files"),
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "include file sizes".to_string(),
            }],
            phase: None,
        },
        user_message("show details"),
    ]);
    request.instructions = "You can run shell commands.".to_string();
    request.tools = vec![ToolSpec::LocalShell {}];

    let events = collect_stream(gateway.stream_responses(request).await.expect("stream")).await;
    assert!(event_names(&events).contains(&"response.completed"));

    let requests = upstream.requests().await;
    let messages = &requests[0].messages;
    assert_eq!(messages[0].role, "system");
    let system_count = messages.iter().filter(|m| m.role == "system").count();
    assert_eq!(system_count, 2);
    let roles: Vec<&str> = messages.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(
        roles,
        vec!["system", "user", "assistant", "tool", "assistant", "system", "user"]
    );
}

#[tokio::test]
async fn long_multi_turn_conversation_no_system_drift() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(content_chunk("chat-1", "5"))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());

    let mut request = base_request(vec![
        user_message("count from 1"),
        ResponseItem::message_text("assistant", "1"),
        user_message("next"),
        ResponseItem::message_text("assistant", "2"),
        user_message("next"),
        ResponseItem::message_text("assistant", "3"),
        user_message("next"),
        ResponseItem::message_text("assistant", "4"),
        user_message("next"),
    ]);
    request.instructions = "You count numbers.".to_string();

    let events = collect_stream(gateway.stream_responses(request).await.expect("stream")).await;
    assert!(event_names(&events).contains(&"response.completed"));

    let requests = upstream.requests().await;
    let messages = &requests[0].messages;
    assert_eq!(messages[0].role, "system");
    assert_eq!(messages.len(), 10); // system + 5 user + 4 assistant
    let system_count = messages.iter().filter(|m| m.role == "system").count();
    assert_eq!(system_count, 1);
}

#[tokio::test]
async fn system_role_in_input_merged_with_instructions() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(content_chunk("chat-1", "ok"))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());

    let mut request = base_request(vec![
        ResponseItem::Message {
            id: None,
            role: "system".to_string(),
            content: vec![ContentItem::InputText {
                text: "extra system context".to_string(),
            }],
            phase: None,
        },
        user_message("hello"),
    ]);
    request.instructions = "base instructions".to_string();

    let _ = collect_stream(gateway.stream_responses(request).await.expect("stream")).await;

    let requests = upstream.requests().await;
    let messages = &requests[0].messages;
    assert_eq!(messages[0].role, "system");
    assert_eq!(
        messages[0].content.as_ref().and_then(|v| v.as_str()),
        Some("base instructions\n\nextra system context")
    );
    let system_count = messages.iter().filter(|m| m.role == "system").count();
    assert_eq!(system_count, 1);
}

#[tokio::test]
async fn tool_call_triggers_new_function_call_response_item() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(tool_call_chunk(
            "chat-1",
            "call_calc_1",
            "calculator",
            "{\"expression\":\"45+555\"}",
        ))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());

    let mut request = base_request(vec![
        user_message("what is 15*37?"),
        ResponseItem::FunctionCall {
            id: None,
            name: "calculator".to_string(),
            namespace: None,
            arguments: r#"{"expression":"15*37"}"#.to_string(),
            call_id: "call_001".to_string(),
        },
        ResponseItem::FunctionCallOutput {
            call_id: "call_001".to_string(),
            output: serde_json::Value::String("555".to_string()),
        },
        ResponseItem::message_text("assistant", "555"),
        user_message("add 45"),
    ]);
    request.tools = vec![ToolSpec::Function {
        name: "calculator".to_string(),
        description: "Evaluate math".to_string(),
        strict: false,
        parameters: json!({
            "type": "object",
            "properties": { "expression": { "type": "string" } },
            "required": ["expression"]
        }),
    }];

    let events = collect_stream(gateway.stream_responses(request).await.expect("stream")).await;

    let done_events: Vec<_> = events
        .iter()
        .filter(|e| e["_event"] == "response.output_item.done")
        .collect();
    assert!(
        done_events
            .iter()
            .any(|e| e["item"]["type"].as_str() == Some("function_call")),
        "should emit a function_call output item"
    );
    let fc = done_events
        .iter()
        .find(|e| e["item"]["type"].as_str() == Some("function_call"))
        .unwrap();
    assert_eq!(fc["item"]["name"].as_str(), Some("calculator"));
    assert_eq!(fc["item"]["call_id"].as_str(), Some("call_calc_1"));
}

#[tokio::test]
async fn response_completed_includes_usage_from_upstream() {
    use resp2chat::models::chat::ChunkUsage;

    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![
            Ok(content_chunk("chat-1", "hello")),
            Ok(ChatCompletionChunk {
                id: "chat-1".to_string(),
                choices: vec![],
                usage: Some(ChunkUsage {
                    prompt_tokens: 100,
                    completion_tokens: 25,
                    total_tokens: 125,
                    prompt_tokens_details: None,
                    completion_tokens_details: None,
                }),
            }),
        ])
        .await;
    let gateway = test_gateway(upstream, MockSearch::default());
    let request = base_request(vec![user_message("hi")]);
    let events = collect_stream(gateway.stream_responses(request).await.expect("stream")).await;

    let completed = events
        .iter()
        .find(|e| e["type"].as_str() == Some("response.completed"))
        .expect("response.completed event");
    let usage = &completed["response"]["usage"];
    assert_eq!(usage["input_tokens"], 100);
    assert_eq!(usage["output_tokens"], 25);
    assert_eq!(usage["total_tokens"], 125);
}

#[tokio::test]
async fn response_completed_accumulates_usage_across_web_search_rounds() {
    use resp2chat::models::chat::ChunkUsage;

    let upstream = MockUpstream::default();
    // Round 1: model calls web_search
    upstream
        .push_response(vec![
            Ok(tool_call_chunk(
                "chat-1",
                "call_ws_1",
                "web_search",
                r#"{"query":"rust async"}"#,
            )),
            Ok(ChatCompletionChunk {
                id: "chat-1".to_string(),
                choices: vec![],
                usage: Some(ChunkUsage {
                    prompt_tokens: 200,
                    completion_tokens: 30,
                    total_tokens: 230,
                    prompt_tokens_details: None,
                    completion_tokens_details: None,
                }),
            }),
        ])
        .await;
    // Round 2: model produces final text after search
    upstream
        .push_response(vec![
            Ok(content_chunk("chat-2", "found it")),
            Ok(ChatCompletionChunk {
                id: "chat-2".to_string(),
                choices: vec![],
                usage: Some(ChunkUsage {
                    prompt_tokens: 350,
                    completion_tokens: 20,
                    total_tokens: 370,
                    prompt_tokens_details: None,
                    completion_tokens_details: None,
                }),
            }),
        ])
        .await;

    let gateway = test_gateway(upstream, MockSearch::default());
    let mut request = base_request(vec![user_message("search for rust async")]);
    request.tools = vec![ToolSpec::WebSearch {
        external_web_access: Some(true),
        filters: None,
        user_location: None,
        search_context_size: None,
        search_content_types: None,
    }];

    let events = collect_stream(gateway.stream_responses(request).await.expect("stream")).await;

    let completed = events
        .iter()
        .find(|e| e["type"].as_str() == Some("response.completed"))
        .expect("response.completed event");
    let usage = &completed["response"]["usage"];
    assert_eq!(usage["input_tokens"], 200 + 350);
    assert_eq!(usage["output_tokens"], 30 + 20);
    assert_eq!(usage["total_tokens"], 230 + 370);
}

#[tokio::test]
async fn merges_assistant_message_and_tool_call_into_single_upstream_message() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(content_chunk("chat-2", "done"))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());

    let request = ResponsesRequest {
        model: "glm-5.1".to_string(),
        instructions: "You are helpful.".to_string(),
        input: vec![
            user_message("explore the codebase"),
            // Assistant reasoning (from a previous turn)
            ResponseItem::Reasoning {
                id: "reasoning_1".to_string(),
                summary: vec![],
                content: Some(vec![
                    resp2chat::models::responses::ReasoningContentItem::ReasoningText {
                        text: "Let me look at the files.".to_string(),
                    },
                ]),
                encrypted_content: None,
            },
            // Assistant text message (from same turn)
            ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: "I'll search the codebase.".to_string(),
                }],
                phase: None,
            },
            // Tool call (from same turn — should merge with the assistant message above)
            ResponseItem::FunctionCall {
                id: None,
                call_id: "call_abc".to_string(),
                name: "exec_command".to_string(),
                arguments: r#"{"cmd":"ls"}"#.to_string(),
                namespace: None,
            },
            // Tool result
            ResponseItem::FunctionCallOutput {
                call_id: "call_abc".to_string(),
                output: json!("file1.rs\nfile2.rs"),
            },
        ],
        tools: vec![ToolSpec::Function {
            name: "exec_command".to_string(),
            description: "Run a command".to_string(),
            strict: false,
            parameters: json!({
                "type": "object",
                "properties": { "cmd": { "type": "string" } },
                "required": ["cmd"]
            }),
        }],
        tool_choice: json!("auto"),
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
        temperature: None,
        top_p: None,
        max_output_tokens: None,
    };

    let _ = collect_stream(gateway.stream_responses(request).await.expect("stream")).await;

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 1);
    let messages = &requests[0].messages;

    // M4: assistant with content does NOT merge with tool call — separate messages
    let content_msg = messages
        .iter()
        .find(|m| m.role == "assistant" && m.content.is_some())
        .expect("assistant message with content");
    assert_eq!(
        content_msg.content,
        Some(serde_json::Value::String("I'll search the codebase.".to_string()))
    );
    assert!(content_msg.reasoning_content.is_some());
    assert!(content_msg.tool_calls.is_none());

    let tool_msg = messages
        .iter()
        .find(|m| m.role == "assistant" && m.tool_calls.is_some())
        .expect("assistant message with tool_calls");
    assert!(tool_msg.content.is_none());
    assert_eq!(tool_msg.tool_calls.as_ref().unwrap().len(), 1);
}

#[tokio::test]
async fn merges_multiple_tool_calls_into_single_upstream_assistant_message() {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![Ok(content_chunk("chat-2", "done"))])
        .await;
    let gateway = test_gateway(upstream.clone(), MockSearch::default());

    let request = ResponsesRequest {
        model: "glm-5.1".to_string(),
        instructions: "You are helpful.".to_string(),
        input: vec![
            user_message("read two files"),
            // Three tool calls from the same assistant turn
            ResponseItem::FunctionCall {
                id: None,
                call_id: "call_1".to_string(),
                name: "read_file".to_string(),
                arguments: r#"{"path":"a.rs"}"#.to_string(),
                namespace: None,
            },
            ResponseItem::FunctionCall {
                id: None,
                call_id: "call_2".to_string(),
                name: "read_file".to_string(),
                arguments: r#"{"path":"b.rs"}"#.to_string(),
                namespace: None,
            },
            ResponseItem::FunctionCall {
                id: None,
                call_id: "call_3".to_string(),
                name: "grep".to_string(),
                arguments: r#"{"pattern":"TODO"}"#.to_string(),
                namespace: None,
            },
            ResponseItem::FunctionCallOutput {
                call_id: "call_1".to_string(),
                output: json!("contents of a.rs"),
            },
            ResponseItem::FunctionCallOutput {
                call_id: "call_2".to_string(),
                output: json!("contents of b.rs"),
            },
            ResponseItem::FunctionCallOutput {
                call_id: "call_3".to_string(),
                output: json!("no matches"),
            },
        ],
        tools: vec![
            ToolSpec::Function {
                name: "read_file".to_string(),
                description: "Read a file".to_string(),
                strict: false,
                parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
            },
            ToolSpec::Function {
                name: "grep".to_string(),
                description: "Search".to_string(),
                strict: false,
                parameters: json!({"type":"object","properties":{"pattern":{"type":"string"}},"required":["pattern"]}),
            },
        ],
        tool_choice: json!("auto"),
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
        temperature: None,
        top_p: None,
        max_output_tokens: None,
    };

    let _ = collect_stream(gateway.stream_responses(request).await.expect("stream")).await;

    let requests = upstream.requests().await;
    let messages = &requests[0].messages;

    // All three tool calls should be in a single assistant message
    let assistant_msgs: Vec<_> = messages.iter().filter(|m| m.role == "assistant").collect();
    assert_eq!(assistant_msgs.len(), 1, "expected exactly one assistant message");
    assert_eq!(
        assistant_msgs[0].tool_calls.as_ref().unwrap().len(),
        3,
        "expected 3 tool calls in the single assistant message"
    );

    // Three tool results should follow
    let tool_msgs: Vec<_> = messages.iter().filter(|m| m.role == "tool").collect();
    assert_eq!(tool_msgs.len(), 3);
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
            upstream_request_log_path: None,
            upstream_chat_kwargs: JsonMap::new(),
            brave_base_url: "https://example.com/".parse().expect("url"),
            brave_api_key: Some("test-key".to_string()),
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
        tool_choice: json!("auto"),
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
        temperature: None,
        top_p: None,
        max_output_tokens: None,
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
        usage: None,
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
        usage: None,
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
        usage: None,
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
