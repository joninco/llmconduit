mod common;

use axum::body::Bytes;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use axum::response::IntoResponse;
use futures::StreamExt;
use http::HeaderMap;
use http::HeaderValue;
use llmconduit::config::FallbackUpstreamConfig;
use llmconduit::models::chat::ChatCompletionRequest;
use llmconduit::upstream::BackendChatRequest;
use llmconduit::upstream::FailoverUpstreamClient;
use llmconduit::upstream::FailoverUpstreamProvider;
use llmconduit::upstream::ProviderStatus;
use llmconduit::upstream::ReqwestUpstreamClient;
use llmconduit::upstream::RoutingUpstreamClient;
use llmconduit::upstream::RoutingUpstreamProvider;
use llmconduit::upstream::UpstreamClient;
use serde_json::Map as JsonMap;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tower::ServiceExt;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

fn request() -> BackendChatRequest {
    BackendChatRequest::new(
        ChatCompletionRequest {
            model: "test-model".to_string(),
            messages: Vec::new(),
            stream: true,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: Some(false),
            reasoning_effort: None,
            response_format: None,
            stream_options: None,
            temperature: None,
            top_p: None,
            max_output_tokens: Some(32),
            frequency_penalty: None,
            presence_penalty: None,
            stop: None,
            extra_body: BTreeMap::new(),
        },
        None,
        None,
        None,
    )
}

fn leaf(server: &MockServer, api_key: Option<&str>) -> ReqwestUpstreamClient {
    ReqwestUpstreamClient::new(
        reqwest::Client::new(),
        format!("{}/v1/", server.uri()).parse().expect("URL"),
        api_key.map(ToString::to_string),
        None,
        true,
        1,
    )
}

async fn never_sends_headers_leaf(
    timeout: Duration,
) -> (ReqwestUpstreamClient, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stalled upstream");
    let address = listener.local_addr().expect("stalled upstream address");
    let server = tokio::spawn(async move {
        let mut sockets = Vec::new();
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            // Keep the connection open without writing a single header byte.
            // Aborting this one owner at test teardown closes every socket, so
            // the fixture itself cannot leave detached pending tasks behind.
            sockets.push(socket);
        }
    });
    let client = ReqwestUpstreamClient::new(
        reqwest::Client::new(),
        format!("http://{address}/v1/").parse().expect("URL"),
        None,
        None,
        true,
        1,
    )
    .with_request_timeout(timeout);
    (client, server)
}

/// Serve one HTTP/1.1 response as explicit chunked DATA frames. `chunks` carry
/// the delay before each frame; when `finish` is false the connection remains
/// open after the last frame so callers can assert an idle-body timeout.
async fn chunked_body_leaf(
    timeout: Duration,
    chunks: Vec<(Duration, Vec<u8>)>,
    finish: bool,
) -> (ReqwestUpstreamClient, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind chunked upstream");
    let address = listener.local_addr().expect("chunked upstream address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept request");
        read_one_http_request(&mut socket).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .await
            .expect("write response headers");
        for (delay, chunk) in chunks {
            tokio::time::sleep(delay).await;
            socket
                .write_all(format!("{:X}\r\n", chunk.len()).as_bytes())
                .await
                .expect("write chunk length");
            socket.write_all(&chunk).await.expect("write chunk");
            socket.write_all(b"\r\n").await.expect("write chunk end");
            socket.flush().await.expect("flush chunk");
        }
        if finish {
            socket
                .write_all(b"0\r\n\r\n")
                .await
                .expect("finish chunked response");
        } else {
            std::future::pending::<()>().await;
        }
    });
    let client = ReqwestUpstreamClient::new(
        reqwest::Client::new(),
        format!("http://{address}/v1/").parse().expect("URL"),
        None,
        None,
        true,
        1,
    )
    .with_request_timeout(timeout);
    (client, server)
}

async fn read_one_http_request(socket: &mut tokio::net::TcpStream) {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let read = socket.read(&mut buffer).await.expect("read request");
        assert!(read > 0, "request ended before its headers");
        request.extend_from_slice(&buffer[..read]);
        if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8_lossy(&request[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while request.len().saturating_sub(header_end) < content_length {
        let read = socket.read(&mut buffer).await.expect("read request body");
        assert!(read > 0, "request ended before its body");
        request.extend_from_slice(&buffer[..read]);
    }
}

fn provider(name: &str, server: &MockServer) -> FailoverUpstreamProvider {
    FailoverUpstreamProvider::new(name, leaf(server, None), None, None, JsonMap::new())
}

fn success_sse() -> &'static str {
    "data: {\"id\":\"chunk-1\",\"object\":\"chat.completion.chunk\",\"created\":0,\
     \"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\
     \"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
}

async fn mount_success(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(success_sse()),
        )
        .mount(server)
        .await;
}

async fn consume_one_chunk(mut stream: llmconduit::upstream::UpstreamStream) {
    let first = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("stream should not hang")
        .expect("stream should yield a chunk")
        .expect("chunk should parse");
    assert_eq!(
        first.choices[0].delta.content.as_deref(),
        Some("ok"),
        "backup response should be the one served"
    );
}

async fn post_count(server: &MockServer, endpoint: &str) -> usize {
    server
        .received_requests()
        .await
        .expect("recorded requests")
        .iter()
        .filter(|request| request.method.as_str() == "POST" && request.url.path() == endpoint)
        .count()
}

#[tokio::test]
async fn malformed_sse_before_first_chunk_fails_over_without_serving_primary_output() {
    let malformed = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("data: not-json\n\ndata: [DONE]\n\n"),
        )
        .mount(&malformed)
        .await;

    let backup = MockServer::start().await;
    mount_success(&backup).await;
    let failover = FailoverUpstreamClient::new(
        vec![
            provider("malformed", &malformed),
            provider("backup", &backup),
        ],
        Duration::from_secs(30),
    );

    let chunks = failover
        .stream_chat_completion_with_timeout(&request(), Duration::from_millis(250))
        .await
        .expect("a malformed first event is safe to fail over")
        .collect::<Vec<_>>()
        .await;

    assert_eq!(chunks.len(), 1, "only the backup chunk may be served");
    let chunk = chunks[0].as_ref().expect("backup chunk parses");
    assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("ok"));
    assert_eq!(post_count(&malformed, "/v1/chat/completions").await, 1);
    assert_eq!(post_count(&backup, "/v1/chat/completions").await, 1);

    let health = failover.provider_health();
    assert_eq!(health[0].status, ProviderStatus::Cooling);
    assert_eq!(health[0].failover_count, 1);
    assert_eq!(health[1].served_count, 1);
}

#[tokio::test]
async fn malformed_sse_after_first_chunk_never_retries_or_duplicates_output() {
    let malformed = MockServer::start().await;
    let body = format!(
        "{}data: not-json\n\ndata: [DONE]\n\n",
        success_sse().trim_end_matches("data: [DONE]\n\n")
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&malformed)
        .await;

    let backup = MockServer::start().await;
    mount_success(&backup).await;
    let failover = FailoverUpstreamClient::new(
        vec![
            provider("malformed", &malformed),
            provider("backup", &backup),
        ],
        Duration::from_secs(30),
    );

    let results = failover
        .stream_chat_completion_with_timeout(&request(), Duration::from_millis(250))
        .await
        .expect("the first valid chunk commits the provider")
        .collect::<Vec<_>>()
        .await;

    assert_eq!(results.len(), 2, "one chunk followed by one stream error");
    assert_eq!(
        results[0].as_ref().expect("first chunk").choices[0]
            .delta
            .content
            .as_deref(),
        Some("ok")
    );
    let error = results[1].as_ref().expect_err("malformed second event");
    assert_eq!(error.code.as_deref(), Some("malformed_upstream_response"));
    assert_eq!(post_count(&malformed, "/v1/chat/completions").await, 1);
    assert_eq!(
        post_count(&backup, "/v1/chat/completions").await,
        0,
        "a stream error after served output must never retry"
    );
}

#[tokio::test]
async fn malformed_sse_failover_does_not_duplicate_responses_items_or_function_calls() {
    let primary = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "list",
            "data": [{"id": "test-model"}]
        })))
        .mount(&primary)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("data: primary-malformed-sentinel\n\ndata: [DONE]\n\n"),
        )
        .mount(&primary)
        .await;

    let backup = MockServer::start().await;
    let backup_body = common::chat_completion_sse_body(&[
        serde_json::json!({
            "id": "chat-backup",
            "choices": [{
                "index": 0,
                "delta": {"content": "backup-only"},
                "finish_reason": null
            }]
        }),
        serde_json::json!({
            "id": "chat-backup",
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "id": "call_backup",
                        "index": 0,
                        "type": "function",
                        "function": {
                            "name": "echo",
                            "arguments": "{\"value\":\"ok\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(backup_body),
        )
        .mount(&backup)
        .await;

    let mut config = common::test_config();
    config.upstream_base_url = format!("{}/v1/", primary.uri()).parse().expect("URL");
    config.fallback_upstreams = vec![FallbackUpstreamConfig {
        resilience: Default::default(),
        name: "backup".to_string(),
        upstream_base_url: format!("{}/v1/", backup.uri()).parse().expect("URL"),
        upstream_api_key: None,
        upstream_model: None,
        exposed_model: None,
        wire_api: Default::default(),
        upstream_chat_kwargs: JsonMap::new(),
        upstream_request_log_path: None,
        responses_capabilities: None,
    }];
    let app = llmconduit::build_app(config);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "model": "test-model",
                        "stream": true,
                        "store": false,
                        "input": "use echo",
                        "parallel_tool_calls": false,
                        "tools": [{
                            "type": "function",
                            "name": "echo",
                            "description": "echo a value",
                            "parameters": {
                                "type": "object",
                                "properties": {"value": {"type": "string"}},
                                "required": ["value"],
                                "additionalProperties": false
                            },
                            "strict": true
                        }]
                    })
                    .to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), http::StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("read Responses SSE");
    let body = String::from_utf8(bytes.to_vec()).expect("UTF-8 SSE");
    assert!(
        !body.contains("primary-malformed-sentinel"),
        "malformed primary bytes must not become public output"
    );
    let events = common::parse_responses_sse_events(&body);

    let text_deltas = events
        .iter()
        .filter(|event| event["type"] == "response.output_text.delta")
        .filter_map(|event| event["delta"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(text_deltas, vec!["backup-only"]);

    let done_items = events
        .iter()
        .filter(|event| event["type"] == "response.output_item.done")
        .map(|event| &event["item"])
        .collect::<Vec<_>>();
    assert_eq!(
        done_items
            .iter()
            .filter(|item| item["type"] == "message")
            .count(),
        1,
        "the served text message must be finalized exactly once"
    );
    let function_calls = done_items
        .iter()
        .filter(|item| item["type"] == "function_call")
        .collect::<Vec<_>>();
    assert_eq!(function_calls.len(), 1);
    assert_eq!(function_calls[0]["call_id"], "call_backup");
    assert_eq!(function_calls[0]["name"], "echo");
    assert_eq!(function_calls[0]["arguments"], r#"{"value":"ok"}"#);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "response.completed")
            .count(),
        1,
        "the retried turn must have exactly one terminal event"
    );

    assert_eq!(post_count(&primary, "/v1/chat/completions").await, 1);
    assert_eq!(post_count(&backup, "/v1/chat/completions").await, 1);
}

#[tokio::test]
async fn response_header_timeout_fails_over_before_first_chunk() {
    let slow = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(500))
                .insert_header("content-type", "text/event-stream")
                .set_body_string(success_sse()),
        )
        .mount(&slow)
        .await;

    let backup = MockServer::start().await;
    mount_success(&backup).await;
    let failover = FailoverUpstreamClient::new(
        vec![provider("slow", &slow), provider("backup", &backup)],
        Duration::from_secs(30),
    );

    let stream = failover
        .stream_chat_completion_with_timeout(&request(), Duration::from_millis(50))
        .await
        .expect("header timeout should fail over to the backup");
    consume_one_chunk(stream).await;

    let health = failover.provider_health();
    assert_eq!(health[0].status, ProviderStatus::Cooling);
    assert_eq!(health[0].failover_count, 1);
    assert_eq!(health[0].consecutive_failures, 1);
    assert_eq!(health[1].served_count, 1);
}

#[tokio::test]
async fn bare_leaf_response_header_wait_is_timed_out() {
    let slow = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(500))
                .insert_header("content-type", "text/event-stream")
                .set_body_string(success_sse()),
        )
        .mount(&slow)
        .await;

    let error = match leaf(&slow, None)
        .stream_chat_completion_with_timeout(&request(), Duration::from_millis(50))
        .await
    {
        Ok(_) => panic!("the bare leaf must time out while waiting for response headers"),
        Err(error) => error,
    };
    assert_eq!(error.status, http::StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(error.message, "upstream response headers timed out");
}

#[tokio::test]
async fn request_intrinsic_4xx_is_terminal_without_fallback_or_cooldown() {
    for (status, expected_code) in [
        (400_u16, "invalid_request_error"),
        (413, "request_too_large"),
        (415, "unsupported_media_type"),
        (422, "unprocessable_entity"),
    ] {
        let rejected = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(status).set_body_string("intrinsic rejection"))
            .mount(&rejected)
            .await;
        let backup = MockServer::start().await;
        mount_success(&backup).await;

        let failover = FailoverUpstreamClient::new(
            vec![provider("primary", &rejected), provider("backup", &backup)],
            Duration::from_secs(30),
        );
        let error = match failover
            .stream_chat_completion_with_timeout(&request(), Duration::from_millis(250))
            .await
        {
            Ok(_) => panic!("status {status} must be terminal, not served by the backup"),
            Err(error) => error,
        };
        assert!(error.to_string().contains(&status.to_string()));
        assert_eq!(error.status.as_u16(), status);
        assert_eq!(error.code.as_deref(), Some(expected_code));
        assert!(
            backup
                .received_requests()
                .await
                .expect("recorded requests")
                .is_empty(),
            "status {status} must make zero fallback requests"
        );

        let health = failover.provider_health();
        assert_eq!(health[0].status, ProviderStatus::Healthy);
        assert_eq!(health[0].failover_count, 0);
        assert_eq!(health[0].consecutive_failures, 0);
        assert_eq!(health[1].served_count, 0);
    }
}

#[tokio::test]
async fn upstream_statuses_receive_sanitized_public_status_codes() {
    for (upstream_status, public_status, code) in [
        (408_u16, 504_u16, "upstream_timeout"),
        (504, 504, "upstream_timeout"),
        (429, 429, "rate_limit_exceeded"),
        (401, 502, "upstream_authentication_error"),
        (403, 502, "upstream_authentication_error"),
        (500, 502, "upstream_error"),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(upstream_status)
                    .set_body_string("secret backend diagnostic must not be public"),
            )
            .mount(&server)
            .await;

        let error = match leaf(&server, None)
            .stream_chat_completion_with_timeout(&request(), Duration::from_millis(250))
            .await
        {
            Ok(_) => panic!("upstream {upstream_status} must fail"),
            Err(error) => error,
        };
        assert_eq!(
            error.status.as_u16(),
            public_status,
            "upstream {upstream_status}"
        );
        assert_eq!(
            error.code.as_deref(),
            Some(code),
            "upstream {upstream_status}"
        );
        assert!(
            !error.client_message.contains("secret backend diagnostic"),
            "upstream {upstream_status} body leaked into the public message"
        );
    }
}

#[tokio::test]
async fn timeout_rate_limit_and_server_errors_still_fail_over_and_cool() {
    for status in [408_u16, 429, 500, 503] {
        let failed = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(status).set_body_string("provider failure"))
            .mount(&failed)
            .await;
        let backup = MockServer::start().await;
        mount_success(&backup).await;

        let failover = FailoverUpstreamClient::new(
            vec![provider("primary", &failed), provider("backup", &backup)],
            Duration::from_secs(30),
        );
        let stream = failover
            .stream_chat_completion_with_timeout(&request(), Duration::from_millis(250))
            .await
            .unwrap_or_else(|error| panic!("status {status} should fail over: {error}"));
        consume_one_chunk(stream).await;

        let health = failover.provider_health();
        assert_eq!(health[0].status, ProviderStatus::Cooling);
        assert_eq!(health[0].failover_count, 1);
        assert_eq!(health[0].consecutive_failures, 1);
        assert_eq!(health[1].served_count, 1);
    }
}

#[tokio::test]
async fn raw_completions_proxy_forwards_only_allowlisted_request_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;

    let mut headers = HeaderMap::new();
    headers.insert("accept", HeaderValue::from_static("application/json"));
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert("x-request-id", HeaderValue::from_static("request-safe"));
    headers.insert("x-trace-id", HeaderValue::from_static("trace-safe"));
    headers.insert(
        "traceparent",
        HeaderValue::from_static("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
    );
    headers.insert(
        "authorization",
        HeaderValue::from_static("Bearer client-credential-must-not-forward"),
    );
    headers.insert(
        "x-api-key",
        HeaderValue::from_static("client-key-must-not-forward"),
    );
    headers.insert(
        "cookie",
        HeaderValue::from_static("session=must-not-forward"),
    );
    headers.insert(
        "x-client-secret",
        HeaderValue::from_static("must-not-forward"),
    );
    headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.9"));

    let response = leaf(&server, Some("configured-upstream-key"))
        .proxy_completions(headers, Bytes::from_static(br#"{"model":"test-model"}"#))
        .await
        .expect("proxy request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(requests.len(), 1);
    let sent = &requests[0].headers;
    assert_eq!(sent["accept"], "application/json");
    assert_eq!(sent["content-type"], "application/json");
    assert_eq!(sent["x-request-id"], "request-safe");
    assert_eq!(sent["x-trace-id"], "trace-safe");
    assert_eq!(
        sent["authorization"], "Bearer configured-upstream-key",
        "the server-owned credential must replace, never preserve, client auth"
    );
    for denied in ["x-api-key", "cookie", "x-client-secret", "x-forwarded-for"] {
        assert!(
            !sent.contains_key(denied),
            "non-allowlisted header {denied} leaked to the upstream"
        );
    }
}

#[tokio::test]
async fn raw_completions_errors_are_openai_shaped_and_never_proxy_provider_bodies() {
    const SENTINEL: &str = "raw-completions-provider-secret-7f4d1a";

    for (upstream_status, public_status, expected_code, expected_type) in [
        (
            400_u16,
            400_u16,
            "invalid_request_error",
            "invalid_request_error",
        ),
        (413, 413, "request_too_large", "invalid_request_error"),
        (415, 415, "unsupported_media_type", "invalid_request_error"),
        (422, 422, "unprocessable_entity", "invalid_request_error"),
        (408, 504, "upstream_timeout", "server_error"),
        (504, 504, "upstream_timeout", "server_error"),
        (429, 429, "rate_limit_exceeded", "invalid_request_error"),
        (401, 502, "upstream_authentication_error", "server_error"),
        (403, 502, "upstream_authentication_error", "server_error"),
        (404, 502, "upstream_error", "server_error"),
        (500, 502, "upstream_error", "server_error"),
        (503, 502, "upstream_error", "server_error"),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/completions"))
            .respond_with(
                ResponseTemplate::new(upstream_status).set_body_json(serde_json::json!({
                    "error": {
                        "message": format!("provider diagnostic containing {SENTINEL}"),
                        "api_key": SENTINEL
                    }
                })),
            )
            .mount(&server)
            .await;

        let error = leaf(&server, None)
            .proxy_completions(
                HeaderMap::new(),
                Bytes::from_static(br#"{"model":"test-model","prompt":"hi"}"#),
            )
            .await
            .expect_err("a non-success raw Completions response must be normalized");

        assert_eq!(
            error.status.as_u16(),
            public_status,
            "upstream {upstream_status}"
        );
        assert_eq!(
            error.code.as_deref(),
            Some(expected_code),
            "upstream {upstream_status}"
        );
        assert!(!error.message.contains(SENTINEL));
        assert!(!error.client_message.contains(SENTINEL));

        let response = error.into_response();
        assert_eq!(response.status().as_u16(), public_status);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read normalized error body");
        let wire: serde_json::Value =
            serde_json::from_slice(&body).expect("OpenAI-shaped JSON error body");
        assert_eq!(wire["error"]["type"], expected_type);
        assert_eq!(wire["error"]["code"], expected_code);
        assert!(wire["error"]["message"].is_string());
        assert!(wire["error"]["param"].is_null());
        assert!(
            !String::from_utf8_lossy(&body).contains(SENTINEL),
            "upstream {upstream_status} body leaked to the public wire"
        );
    }
}

#[tokio::test]
async fn raw_completions_intrinsic_4xx_never_fails_over_or_cools() {
    for status in [400_u16, 413, 415, 422] {
        let rejected = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/completions"))
            .respond_with(
                ResponseTemplate::new(status).set_body_string("provider detail must stay private"),
            )
            .mount(&rejected)
            .await;
        let backup = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string("backup"))
            .mount(&backup)
            .await;

        let failover = FailoverUpstreamClient::new(
            vec![provider("primary", &rejected), provider("backup", &backup)],
            Duration::from_secs(30),
        );
        let error = failover
            .proxy_completions(
                HeaderMap::new(),
                Bytes::from_static(br#"{"model":"test-model","prompt":"hi"}"#),
            )
            .await
            .expect_err("request-intrinsic status must terminate the chain");
        assert_eq!(error.status.as_u16(), status);
        assert!(
            backup
                .received_requests()
                .await
                .expect("backup requests")
                .is_empty(),
            "status {status} must make zero fallback requests"
        );
        let health = failover.provider_health();
        assert_eq!(health[0].status, ProviderStatus::Healthy);
        assert_eq!(health[0].failover_count, 0);
        assert_eq!(health[0].consecutive_failures, 0);
    }
}

#[tokio::test]
async fn raw_completions_provider_failures_fail_over_before_serving_output() {
    for status in [401_u16, 403, 404, 408, 429, 500, 503] {
        let failed = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/completions"))
            .respond_with(
                ResponseTemplate::new(status).set_body_string("provider detail must stay private"),
            )
            .mount(&failed)
            .await;
        let backup = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string("backup"))
            .mount(&backup)
            .await;

        let failover = FailoverUpstreamClient::new(
            vec![provider("primary", &failed), provider("backup", &backup)],
            Duration::from_secs(30),
        );
        let response = failover
            .proxy_completions(
                HeaderMap::new(),
                Bytes::from_static(br#"{"model":"test-model","prompt":"hi"}"#),
            )
            .await
            .unwrap_or_else(|error| panic!("status {status} should fail over: {error}"));
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            backup
                .received_requests()
                .await
                .expect("backup requests")
                .len(),
            1
        );
        let health = failover.provider_health();
        assert_eq!(health[0].status, ProviderStatus::Cooling);
        assert_eq!(health[0].failover_count, 1);
        assert_eq!(health[0].consecutive_failures, 1);
    }
}

#[tokio::test]
async fn exhausted_raw_completions_preserves_timeout_and_rate_limit_statuses() {
    for (upstream_status, public_status, expected_code) in [
        (408_u16, 504_u16, "upstream_timeout"),
        (429_u16, 429_u16, "rate_limit_exceeded"),
    ] {
        let primary = MockServer::start().await;
        let fallback = MockServer::start().await;
        for server in [&primary, &fallback] {
            Mock::given(method("POST"))
                .and(path("/v1/completions"))
                .respond_with(
                    ResponseTemplate::new(upstream_status)
                        .set_body_string("neutral-field-secret-must-not-be-served"),
                )
                .mount(server)
                .await;
        }

        let failover = FailoverUpstreamClient::new(
            vec![
                provider("primary", &primary),
                provider("fallback", &fallback),
            ],
            Duration::from_secs(30),
        );
        let error = failover
            .proxy_completions(
                HeaderMap::new(),
                Bytes::from_static(br#"{"model":"test-model"}"#),
            )
            .await
            .expect_err("all raw-completions providers must be exhausted");

        assert_eq!(
            error.status.as_u16(),
            public_status,
            "upstream {upstream_status}"
        );
        assert_eq!(error.code.as_deref(), Some(expected_code));
        assert!(!error.client_message.contains("neutral-field-secret"));
        assert_eq!(
            primary
                .received_requests()
                .await
                .expect("primary requests")
                .len(),
            1
        );
        assert_eq!(
            fallback
                .received_requests()
                .await
                .expect("fallback requests")
                .len(),
            1
        );
    }
}

#[tokio::test]
async fn non_generation_surfaces_bound_response_header_waits() {
    let (leaf, server) = never_sends_headers_leaf(Duration::from_millis(30)).await;

    let model_error = leaf.list_models().await.expect_err("/models must time out");
    assert_eq!(model_error.status, http::StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(model_error.code.as_deref(), Some("upstream_timeout"));

    let metrics_error = leaf
        .proxy_metrics()
        .await
        .expect_err("/metrics must time out");
    assert_eq!(metrics_error.status, http::StatusCode::GATEWAY_TIMEOUT);

    let completions_error = leaf
        .proxy_completions(
            HeaderMap::new(),
            Bytes::from_static(br#"{"model":"test-model"}"#),
        )
        .await
        .expect_err("/completions must time out");
    assert_eq!(completions_error.status, http::StatusCode::GATEWAY_TIMEOUT);

    assert_eq!(
        leaf.count_tokens(&request()).await.expect("optional count"),
        None,
        "/tokenize timeout is a bounded unsupported/fallback result"
    );
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn models_and_tokenize_success_bodies_have_hard_byte_limits() {
    let models = MockServer::start().await;
    let oversized_catalog = format!(
        r#"{{"object":"list","data":[],"padding":"{}"}}"#,
        "x".repeat(5 * 1024 * 1024)
    );
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_string(oversized_catalog))
        .mount(&models)
        .await;
    let catalog_error = leaf(&models, None)
        .supported_model_catalog()
        .await
        .expect_err("oversized model catalog must be rejected");
    assert_eq!(catalog_error.status, http::StatusCode::BAD_GATEWAY);
    assert_eq!(
        catalog_error.code.as_deref(),
        Some("upstream_response_too_large")
    );
    assert_eq!(
        catalog_error.client_message,
        "the upstream model catalog response was too large"
    );
    assert!(
        !catalog_error.message.contains("xxxxx"),
        "provider body bytes must not enter the error"
    );

    let tokenize = MockServer::start().await;
    let oversized_count = format!(r#"{{"count":42,"padding":"{}"}}"#, "y".repeat(128 * 1024));
    Mock::given(method("POST"))
        .and(path("/tokenize"))
        .respond_with(ResponseTemplate::new(200).set_body_string(oversized_count))
        .mount(&tokenize)
        .await;
    assert_eq!(
        leaf(&tokenize, None)
            .count_tokens(&request())
            .await
            .expect("token counting remains an optional capability"),
        None,
        "an oversized tokenizer body must fall back instead of being retained"
    );
}

#[tokio::test]
async fn malformed_model_catalog_errors_do_not_echo_provider_bytes() {
    const SENTINEL: &str = "catalog-provider-secret-must-not-survive";
    let models = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(format!("not-json containing {SENTINEL}")),
        )
        .mount(&models)
        .await;

    let error = leaf(&models, None)
        .supported_model_catalog()
        .await
        .expect_err("malformed model catalog must fail");
    assert_eq!(error.status, http::StatusCode::BAD_GATEWAY);
    assert_eq!(error.code.as_deref(), Some("invalid_upstream_response"));
    assert_eq!(
        error.message,
        "upstream model catalog returned invalid JSON"
    );
    assert_eq!(
        error.client_message,
        "the upstream model catalog returned invalid JSON"
    );
    assert!(!error.to_string().contains(SENTINEL));
}

#[tokio::test]
async fn models_and_tokenize_stalled_bodies_hit_the_idle_deadline() {
    let timeout = Duration::from_millis(40);
    let (models, models_server) = chunked_body_leaf(
        timeout,
        vec![(Duration::ZERO, br#"{"object":"list","data":["#.to_vec())],
        false,
    )
    .await;
    let started = std::time::Instant::now();
    let catalog_error = models
        .supported_model_catalog()
        .await
        .expect_err("stalled model catalog body must time out");
    assert_eq!(catalog_error.status, http::StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(catalog_error.code.as_deref(), Some("upstream_timeout"));
    assert!(started.elapsed() < Duration::from_millis(500));
    models_server.abort();
    let _ = models_server.await;

    let (tokenize, tokenize_server) = chunked_body_leaf(
        timeout,
        vec![(Duration::ZERO, br#"{"count":"#.to_vec())],
        false,
    )
    .await;
    let started = std::time::Instant::now();
    assert_eq!(
        tokenize
            .count_tokens(&request())
            .await
            .expect("token counting remains optional"),
        None
    );
    assert!(started.elapsed() < Duration::from_millis(500));
    tokenize_server.abort();
    let _ = tokenize_server.await;
}

#[tokio::test]
async fn models_body_idle_deadline_restarts_for_slow_progress() {
    let body = br#"{"object":"list","data":[{"id":"slow-model"}]}"#;
    let chunks = body
        .chunks(4)
        .map(|chunk| (Duration::from_millis(25), chunk.to_vec()))
        .collect();
    let idle_timeout = Duration::from_millis(100);
    let (models, server) = chunked_body_leaf(idle_timeout, chunks, true).await;

    let started = std::time::Instant::now();
    let catalog = models
        .supported_model_catalog()
        .await
        .expect("continuous slow progress must not hit a total deadline");
    assert!(
        started.elapsed() > idle_timeout,
        "fixture must take longer than one idle interval"
    );
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].id, "slow-model");
    server.await.expect("chunked server");
}

#[tokio::test]
async fn routing_catalog_refresh_inherits_leaf_header_deadline() {
    let (primary, server) = never_sends_headers_leaf(Duration::from_millis(30)).await;
    let routing = RoutingUpstreamClient::new(vec![RoutingUpstreamProvider::new(
        "slow-catalog",
        primary,
        None,
        JsonMap::new(),
        Vec::new(),
        Duration::from_secs(30),
    )]);

    let started = std::time::Instant::now();
    let error = routing
        .list_models()
        .await
        .expect_err("routing refresh must fail when the sole catalog stalls");
    assert_eq!(error.status, http::StatusCode::GATEWAY_TIMEOUT);
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "catalog refresh exceeded the configured header deadline"
    );
    server.abort();
    let _ = server.await;
}
