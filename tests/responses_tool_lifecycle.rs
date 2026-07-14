//! Public-wire regression coverage for Responses tool and reasoning lifecycles.
//!
//! These tests use the real Axum Responses route because the internal canonical
//! stream intentionally contains fields and events that the public projector
//! removes.

mod common;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use common::{
    MockSearch, MockUpstream, content_chunk, finish_chunk, parse_responses_sse_events,
    reasoning_chunk, test_config, test_gateway, test_gateway_with_config, usage_chunk,
};
use llmconduit::models::chat::{
    ChatChunkChoice, ChatCompletionChunk, ChatDelta, ChatFunctionCall, ChatToolCall,
};
use llmconduit::responses_capabilities::{ReasoningSummaryCapability, ResponsesCapabilitiesConfig};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tower::ServiceExt;

const BODY_LIMIT: usize = 1024 * 1024;

async fn post_json(app: Router, body: Value) -> axum::response::Response {
    app.oneshot(
        Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&body).expect("serialize Responses request"),
            ))
            .expect("build Responses request"),
    )
    .await
    .expect("route Responses request")
}

async fn response_json(response: axum::response::Response) -> Value {
    let body = to_bytes(response.into_body(), BODY_LIMIT)
        .await
        .expect("read response body");
    serde_json::from_slice(&body).expect("valid JSON response")
}

async fn response_events(response: axum::response::Response) -> Vec<Value> {
    let body = to_bytes(response.into_body(), BODY_LIMIT)
        .await
        .expect("read SSE body");
    parse_responses_sse_events(std::str::from_utf8(&body).expect("UTF-8 SSE body"))
}

fn function_tool(name: &str, strict: bool) -> Value {
    json!({
        "type": "function",
        "name": name,
        "strict": strict,
        "parameters": {
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"],
            "additionalProperties": false
        }
    })
}

fn tool_call_chunk(
    response_id: &str,
    calls: Vec<ChatToolCall>,
    finish_reason: Option<&str>,
) -> ChatCompletionChunk {
    ChatCompletionChunk {
        service_tier: None,
        id: response_id.to_string(),
        choices: vec![ChatChunkChoice {
            index: 0,
            delta: ChatDelta {
                tool_calls: Some(calls),
                ..Default::default()
            },
            finish_reason: finish_reason.map(ToString::to_string),
            stop_reason: None,
        }],
        usage: None,
    }
}

fn tool_fragment(
    index: usize,
    call_id: Option<&str>,
    name: Option<&str>,
    arguments: &str,
) -> ChatToolCall {
    ChatToolCall {
        id: call_id.map(ToString::to_string),
        index: Some(index),
        kind: "function".to_string(),
        function: ChatFunctionCall {
            name: name.map(ToString::to_string),
            arguments: Some(Value::String(arguments.to_string())),
        },
    }
}

fn explicit_summary_chunk(
    response_id: &str,
    summary: &str,
    hidden_reasoning: Option<&str>,
) -> ChatCompletionChunk {
    ChatCompletionChunk {
        service_tier: None,
        id: response_id.to_string(),
        choices: vec![ChatChunkChoice {
            index: 0,
            delta: ChatDelta {
                reasoning_content: hidden_reasoning.map(ToString::to_string),
                extra: BTreeMap::from([("reasoning_summary".to_string(), json!(summary))]),
                ..Default::default()
            },
            finish_reason: None,
            stop_reason: None,
        }],
        usage: None,
    }
}

fn terminal_event(events: &[Value]) -> &Value {
    let terminal = events
        .iter()
        .filter(|event| {
            matches!(
                event["type"].as_str(),
                Some("response.completed" | "response.incomplete" | "response.failed")
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(terminal.len(), 1, "expected exactly one terminal event");
    terminal[0]
}

fn event_position(events: &[Value], predicate: impl Fn(&Value) -> bool) -> usize {
    events
        .iter()
        .position(predicate)
        .expect("expected lifecycle event")
}

#[tokio::test]
async fn responses_function_call_sse_lifecycle_parallel() {
    let upstream = MockUpstream::default();
    upstream.set_supported_models(["glm-5.1"]).await;
    upstream
        .push_response(vec![
            Ok(tool_call_chunk(
                "chat-parallel-tools",
                vec![
                    tool_fragment(0, Some("call_alpha"), Some("alpha"), "{\"value\":\""),
                    tool_fragment(1, Some("call_beta"), Some("beta"), "{\"value\":\""),
                ],
                None,
            )),
            Ok(tool_call_chunk(
                "chat-parallel-tools",
                vec![
                    tool_fragment(0, None, None, r#"one"}"#),
                    tool_fragment(1, None, None, r#"two"}"#),
                ],
                Some("tool_calls"),
            )),
        ])
        .await;

    let mut config = test_config();
    config.responses_capabilities = ResponsesCapabilitiesConfig {
        parallel_tool_calls: Some(true),
        ..Default::default()
    };
    let app = llmconduit::build_app_from_gateway(test_gateway_with_config(
        upstream,
        MockSearch::default(),
        config,
    ));
    let response = post_json(
        app,
        json!({
            "model": "glm-5.1",
            "stream": true,
            "store": false,
            "input": "call both functions",
            "parallel_tool_calls": true,
            "tools": [function_tool("alpha", false), function_tool("beta", false)]
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let events = response_events(response).await;

    let added = events
        .iter()
        .filter(|event| {
            event["type"] == "response.output_item.added"
                && event["item"]["type"] == "function_call"
        })
        .collect::<Vec<_>>();
    assert_eq!(added.len(), 2, "both function items must be introduced");
    assert_eq!(added[0]["output_index"], 0);
    assert_eq!(added[1]["output_index"], 1);

    for (call_id, name, arguments, expected_index) in [
        ("call_alpha", "alpha", r#"{"value":"one"}"#, 0),
        ("call_beta", "beta", r#"{"value":"two"}"#, 1),
    ] {
        let item_added = added
            .iter()
            .copied()
            .find(|event| event["item"]["call_id"] == call_id)
            .expect("matching function item added");
        let item_id = item_added["item"]["id"]
            .as_str()
            .expect("public function item id");
        assert!(item_id.starts_with("fc_"));
        assert_eq!(item_added["item"]["name"], name);
        assert_eq!(item_added["item"]["arguments"], "");
        assert_eq!(item_added["output_index"], expected_index);

        let deltas = events
            .iter()
            .filter(|event| {
                event["type"] == "response.function_call_arguments.delta"
                    && event["item_id"] == item_id
            })
            .collect::<Vec<_>>();
        assert!(
            !deltas.is_empty(),
            "validated arguments must have a delta lifecycle"
        );
        assert_eq!(
            deltas
                .iter()
                .map(|event| event["delta"].as_str().expect("argument delta"))
                .collect::<String>(),
            arguments
        );
        assert!(deltas.iter().all(|event| {
            event["output_index"] == expected_index && event.get("call_id").is_none()
        }));

        let arguments_done = events
            .iter()
            .find(|event| {
                event["type"] == "response.function_call_arguments.done"
                    && event["item_id"] == item_id
            })
            .expect("function arguments done");
        assert_eq!(arguments_done["output_index"], expected_index);
        assert_eq!(arguments_done["name"], name);
        assert_eq!(arguments_done["arguments"], arguments);
        assert!(arguments_done.get("call_id").is_none());

        let item_done = events
            .iter()
            .find(|event| {
                event["type"] == "response.output_item.done" && event["item"]["id"] == item_id
            })
            .expect("function item done");
        assert_eq!(item_done["output_index"], expected_index);
        assert_eq!(item_done["item"]["call_id"], call_id);
        assert_eq!(item_done["item"]["name"], name);
        assert_eq!(item_done["item"]["arguments"], arguments);

        let added_position = event_position(&events, |event| {
            event["type"] == "response.output_item.added" && event["item"]["id"] == item_id
        });
        let last_delta_position = events
            .iter()
            .rposition(|event| {
                event["type"] == "response.function_call_arguments.delta"
                    && event["item_id"] == item_id
            })
            .expect("last function argument delta");
        let arguments_done_position = event_position(&events, |event| {
            event["type"] == "response.function_call_arguments.done" && event["item_id"] == item_id
        });
        let item_done_position = event_position(&events, |event| {
            event["type"] == "response.output_item.done" && event["item"]["id"] == item_id
        });
        assert!(added_position < last_delta_position);
        assert!(last_delta_position < arguments_done_position);
        assert!(arguments_done_position < item_done_position);
    }

    let terminal = terminal_event(&events);
    assert_eq!(terminal["type"], "response.completed");
    assert_eq!(terminal["response"]["status"], "completed");
    assert_eq!(terminal["response"]["output"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn responses_validated_function_arguments_use_bounded_utf8_deltas() {
    let upstream = MockUpstream::default();
    upstream.set_supported_models(["glm-5.1"]).await;
    let arguments = serde_json::to_string(&json!({ "value": "é".repeat(70_000) }))
        .expect("serialize large arguments");
    upstream
        .push_response(vec![Ok(tool_call_chunk(
            "chat-large-tool-arguments",
            vec![tool_fragment(
                0,
                Some("call_large"),
                Some("echo"),
                &arguments,
            )],
            Some("tool_calls"),
        ))])
        .await;
    let app = llmconduit::build_app_from_gateway(test_gateway(upstream, MockSearch::default()));

    let response = post_json(
        app,
        json!({
            "model": "glm-5.1",
            "stream": true,
            "store": false,
            "input": "echo a large value",
            "tools": [function_tool("echo", false)]
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let events = response_events(response).await;
    let deltas = events
        .iter()
        .filter(|event| event["type"] == "response.function_call_arguments.delta")
        .map(|event| event["delta"].as_str().expect("argument delta"))
        .collect::<Vec<_>>();
    assert!(
        deltas.len() >= 3,
        "large arguments must be split: {deltas:?}"
    );
    assert!(
        deltas.iter().all(|delta| delta.len() <= 64 * 1024),
        "every synthesized delta stays bounded"
    );
    let accumulated = deltas.concat();
    let done = events
        .iter()
        .find(|event| event["type"] == "response.function_call_arguments.done")
        .expect("arguments done");
    assert_eq!(accumulated, arguments);
    assert_eq!(done["arguments"], arguments);
    assert_eq!(terminal_event(&events)["type"], "response.completed");
}

#[tokio::test]
async fn responses_local_shell_and_tool_search_stream_lifecycle() {
    let cases = [
        (
            json!({ "type": "local_shell" }),
            "local_shell",
            r#"{"command":["ls"]}"#,
            "local_shell_call",
            "lsc_",
        ),
        (
            json!({
                "type": "tool_search",
                "execution": "client",
                "description": "Find an available tool.",
                "parameters": {
                    "type": "object",
                    "properties": { "query": { "type": "string" } },
                    "required": ["query"]
                }
            }),
            "tool_search",
            r#"{"query":"filesystem"}"#,
            "tool_search_call",
            "tsc_",
        ),
    ];

    for (tool, upstream_name, arguments, item_type, item_id_prefix) in cases {
        let upstream = MockUpstream::default();
        upstream.set_supported_models(["glm-5.1"]).await;
        let call_id = format!("call_{upstream_name}");
        upstream
            .push_response(vec![Ok(tool_call_chunk(
                &format!("chat-{upstream_name}"),
                vec![tool_fragment(
                    0,
                    Some(&call_id),
                    Some(upstream_name),
                    arguments,
                )],
                Some("tool_calls"),
            ))])
            .await;
        let app = llmconduit::build_app_from_gateway(test_gateway(upstream, MockSearch::default()));

        let response = post_json(
            app,
            json!({
                "model": "glm-5.1",
                "stream": true,
                "store": false,
                "input": "use the tool",
                "tools": [tool]
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{upstream_name}");
        let events = response_events(response).await;
        let terminal = terminal_event(&events);
        assert_eq!(terminal["type"], "response.completed", "{upstream_name}");
        assert!(events.iter().all(|event| {
            !matches!(
                event["type"].as_str(),
                Some(
                    "response.function_call_arguments.delta"
                        | "response.function_call_arguments.done"
                )
            )
        }));
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    event["type"] == "response.output_item.added"
                        && event["item"]["type"] == item_type
                })
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    event["type"] == "response.output_item.done"
                        && event["item"]["type"] == item_type
                })
                .count(),
            1
        );

        let added_position = event_position(&events, |event| {
            event["type"] == "response.output_item.added"
                && event["item"]["type"] == item_type
                && event["item"]["call_id"] == call_id
        });
        let done_position = event_position(&events, |event| {
            event["type"] == "response.output_item.done"
                && event["item"]["type"] == item_type
                && event["item"]["call_id"] == call_id
        });
        assert!(added_position < done_position, "{upstream_name}");
        assert_eq!(events[added_position]["output_index"], 0);
        assert_eq!(events[done_position]["output_index"], 0);
        assert_eq!(events[added_position]["item"]["status"], "in_progress");
        assert_eq!(events[done_position]["item"]["status"], "completed");
        let item_id = events[added_position]["item"]["id"]
            .as_str()
            .expect("dedicated tool item id");
        assert!(item_id.starts_with(item_id_prefix));
        assert_ne!(item_id, call_id);
        assert_eq!(events[done_position]["item"]["id"], item_id);
        assert_eq!(terminal["response"]["output"].as_array().unwrap().len(), 1);
        assert_eq!(terminal["response"]["output"][0]["id"], item_id);
        assert_eq!(
            terminal["response"]["output"][0],
            events[done_position]["item"]
        );
        if item_type == "local_shell_call" {
            assert_eq!(events[done_position]["item"]["action"]["env"], json!({}));
        }
    }
}

#[tokio::test]
async fn responses_tainted_batch_never_leaves_open_items() {
    let upstream = MockUpstream::default();
    upstream.set_supported_models(["glm-5.1"]).await;
    upstream
        .push_response(vec![
            Ok(tool_call_chunk(
                "chat-tainted-tools",
                vec![tool_fragment(
                    0,
                    Some("call_valid"),
                    Some("echo"),
                    r#"{"value":"one"}"#,
                )],
                None,
            )),
            Ok(tool_call_chunk(
                "chat-tainted-tools",
                vec![tool_fragment(
                    1,
                    Some("call_unknown"),
                    Some("Grep"),
                    r#"{"pattern":"needle"}"#,
                )],
                Some("tool_calls"),
            )),
        ])
        .await;
    upstream
        .push_response(vec![
            Ok(content_chunk("chat-repaired-tools", "recovered")),
            Ok(finish_chunk("chat-repaired-tools", "stop")),
        ])
        .await;
    let app =
        llmconduit::build_app_from_gateway(test_gateway(upstream.clone(), MockSearch::default()));

    let response = post_json(
        app,
        json!({
            "model": "glm-5.1",
            "stream": true,
            "store": false,
            "input": "use echo",
            "tools": [function_tool("echo", false)]
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let events = response_events(response).await;
    let terminal = terminal_event(&events);
    assert_eq!(terminal["type"], "response.completed");
    assert_eq!(upstream.requests().await.len(), 2, "one repair round");
    assert!(events.iter().all(|event| {
        !(event["type"] == "response.output_item.added" && event["item"]["type"] == "function_call")
    }));
    assert!(events.iter().all(|event| {
        !matches!(
            event["type"].as_str(),
            Some(
                "response.function_call_arguments.delta" | "response.function_call_arguments.done"
            )
        )
    }));

    let added_ids = events
        .iter()
        .filter(|event| event["type"] == "response.output_item.added")
        .map(|event| {
            event["item"]["id"]
                .as_str()
                .expect("added item id")
                .to_string()
        })
        .collect::<Vec<_>>();
    let done_items = events
        .iter()
        .filter(|event| event["type"] == "response.output_item.done")
        .map(|event| event["item"].clone())
        .collect::<Vec<_>>();
    for item_id in &added_ids {
        assert!(
            done_items
                .iter()
                .any(|item| item["id"].as_str() == Some(item_id)),
            "added item {item_id} was never completed"
        );
    }
    assert_eq!(terminal["response"]["output"], Value::Array(done_items));
    assert_eq!(
        terminal["response"]["output"][0]["content"][0]["text"],
        "recovered"
    );
}

async fn assert_function_call_fails_before_done(
    app: Router,
    call_id: &str,
    arguments: &str,
    strict: bool,
) {
    let response = post_json(
        app,
        json!({
            "model": "glm-5.1",
            "stream": true,
            "store": false,
            "input": "call the function",
            "tools": [function_tool("echo", strict)]
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let events = response_events(response).await;

    assert!(events.iter().all(|event| {
        !(event["type"] == "response.output_item.added" && event["item"]["call_id"] == call_id)
    }));
    assert!(events.iter().all(|event| {
        !matches!(
            event["type"].as_str(),
            Some(
                "response.function_call_arguments.delta" | "response.function_call_arguments.done"
            )
        )
    }));
    assert!(events.iter().all(|event| {
        !(event["type"] == "response.output_item.done" && event["item"]["call_id"] == call_id)
    }));

    let terminal = terminal_event(&events);
    assert_eq!(terminal["type"], "response.failed");
    assert_eq!(
        events.last(),
        Some(terminal),
        "failure must terminate the stream"
    );
    assert!(
        terminal["response"]["output"]
            .as_array()
            .expect("failed output array")
            .iter()
            .all(|item| item["call_id"] != call_id)
    );
    assert!(!terminal.to_string().contains(arguments));
}

#[tokio::test]
async fn responses_malformed_arguments_fail_before_item_done() {
    let upstream = MockUpstream::default();
    upstream.set_supported_models(["glm-5.1"]).await;
    let malformed = r#"{"value":"unterminated"#;
    upstream
        .push_response(vec![Ok(tool_call_chunk(
            "chat-malformed-arguments",
            vec![tool_fragment(
                0,
                Some("call_malformed"),
                Some("echo"),
                malformed,
            )],
            Some("tool_calls"),
        ))])
        .await;
    let app = llmconduit::build_app_from_gateway(test_gateway(upstream, MockSearch::default()));

    assert_function_call_fails_before_done(app, "call_malformed", malformed, false).await;
}

#[tokio::test]
async fn responses_strict_tool_arguments_validate_against_schema() {
    let upstream = MockUpstream::default();
    upstream.set_supported_models(["glm-5.1"]).await;
    let schema_mismatch = r#"{"value":1}"#;
    upstream
        .push_response(vec![Ok(tool_call_chunk(
            "chat-strict-schema-mismatch",
            vec![tool_fragment(
                0,
                Some("call_schema_mismatch"),
                Some("echo"),
                schema_mismatch,
            )],
            Some("tool_calls"),
        ))])
        .await;
    let app = llmconduit::build_app_from_gateway(test_gateway(upstream, MockSearch::default()));

    assert_function_call_fails_before_done(app, "call_schema_mismatch", schema_mismatch, true)
        .await;
}

#[tokio::test]
async fn responses_function_output_continuation_multiturn() {
    let upstream = MockUpstream::default();
    upstream.set_supported_models(["glm-5.1"]).await;
    upstream
        .push_response(vec![Ok(tool_call_chunk(
            "chat-parent-tool",
            vec![tool_fragment(
                0,
                Some("call_parent_echo"),
                Some("echo"),
                r#"{"value":"question"}"#,
            )],
            Some("tool_calls"),
        ))])
        .await;
    upstream
        .push_response(vec![
            Ok(content_chunk(
                "chat-child-answer",
                "The tool returned forty-two.",
            )),
            Ok(finish_chunk("chat-child-answer", "stop")),
        ])
        .await;
    let app =
        llmconduit::build_app_from_gateway(test_gateway(upstream.clone(), MockSearch::default()));

    let parent = post_json(
        app.clone(),
        json!({
            "model": "glm-5.1",
            "stream": false,
            "store": true,
            "input": "Use echo.",
            "tools": [function_tool("echo", false)]
        }),
    )
    .await;
    assert_eq!(parent.status(), StatusCode::OK);
    let parent = response_json(parent).await;
    assert_eq!(parent["status"], "completed");
    let parent_id = parent["id"].as_str().expect("stored parent id");
    let call_id = parent["output"][0]["call_id"]
        .as_str()
        .expect("parent function call id");
    assert_eq!(call_id, "call_parent_echo");

    let child = post_json(
        app,
        json!({
            "model": "glm-5.1",
            "stream": false,
            "store": false,
            "previous_response_id": parent_id,
            "input": [{
                "type": "function_call_output",
                "call_id": call_id,
                "output": "forty-two"
            }]
        }),
    )
    .await;
    assert_eq!(child.status(), StatusCode::OK);
    let child = response_json(child).await;
    assert_eq!(child["status"], "completed");
    assert_eq!(child["previous_response_id"], parent_id);
    assert_eq!(
        child["output"][0]["content"][0]["text"],
        "The tool returned forty-two."
    );

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 2);
    let messages = &requests[1].messages;
    assert_eq!(
        messages
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        vec!["user", "assistant", "tool"]
    );
    let prior_call = messages[1]
        .tool_calls
        .as_ref()
        .and_then(|calls| calls.first())
        .expect("stored assistant function call");
    assert_eq!(prior_call.id.as_deref(), Some(call_id));
    assert_eq!(messages[2].tool_call_id.as_deref(), Some(call_id));
    assert_eq!(messages[2].content, Some(json!("forty-two")));
}

#[tokio::test]
async fn responses_tool_error_output_continues_normally() {
    let upstream = MockUpstream::default();
    upstream.set_supported_models(["glm-5.1"]).await;
    upstream
        .push_response(vec![
            Ok(content_chunk(
                "chat-tool-error-continuation",
                "I handled the failed tool result.",
            )),
            Ok(finish_chunk("chat-tool-error-continuation", "stop")),
        ])
        .await;
    let app =
        llmconduit::build_app_from_gateway(test_gateway(upstream.clone(), MockSearch::default()));
    let error_output = r#"{"error":"permission denied","exit_code":1}"#;

    let response = post_json(
        app,
        json!({
            "model": "glm-5.1",
            "stream": false,
            "store": false,
            "input": [
                { "role": "user", "content": "Run the command." },
                {
                    "type": "function_call",
                    "id": "fc_history_failure",
                    "call_id": "call_history_failure",
                    "name": "run_command",
                    "arguments": "{\"value\":\"false\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_history_failure",
                    "output": error_output
                }
            ]
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = response_json(response).await;
    assert_eq!(response["status"], "completed");
    assert_eq!(
        response["output"][0]["content"][0]["text"],
        "I handled the failed tool result."
    );

    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]
            .messages
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        vec!["user", "assistant", "tool"]
    );
    assert_eq!(requests[0].messages[2].content, Some(json!(error_output)));
    assert_eq!(
        requests[0].messages[2].tool_call_id.as_deref(),
        Some("call_history_failure")
    );
}

#[tokio::test]
async fn responses_reasoning_summary_lifecycle_is_complete() {
    let upstream = MockUpstream::default();
    upstream.set_supported_models(["glm-5.1"]).await;
    upstream
        .push_response(vec![
            Ok(explicit_summary_chunk(
                "chat-safe-summary-lifecycle",
                "safe ",
                Some("private reasoning must not be served"),
            )),
            Ok(explicit_summary_chunk(
                "chat-safe-summary-lifecycle",
                "summary",
                None,
            )),
            Ok(finish_chunk("chat-safe-summary-lifecycle", "stop")),
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
        json!({
            "model": "glm-5.1",
            "stream": true,
            "store": false,
            "input": "summarize the reasoning",
            "reasoning": { "summary": "auto" }
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let events = response_events(response).await;
    assert_eq!(
        events
            .iter()
            .map(|event| event["type"].as_str().expect("event type"))
            .collect::<Vec<_>>(),
        vec![
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.reasoning_summary_part.added",
            "response.reasoning_summary_text.delta",
            "response.reasoning_summary_text.delta",
            "response.reasoning_summary_text.done",
            "response.reasoning_summary_part.done",
            "response.output_item.done",
            "response.completed",
        ]
    );

    let added = &events[2];
    let item_id = added["item"]["id"].as_str().expect("reasoning item id");
    let output_index = added["output_index"].clone();
    assert_eq!(added["item"]["type"], "reasoning");
    assert_eq!(added["item"]["summary"], json!([]));

    for event in &events[3..8] {
        assert_eq!(event["item_id"], item_id);
        assert_eq!(event["output_index"], output_index);
        assert_eq!(event["summary_index"], 0);
    }
    assert_eq!(
        events[3]["part"],
        json!({ "type": "summary_text", "text": "" })
    );
    assert_eq!(events[4]["delta"], "safe ");
    assert_eq!(events[5]["delta"], "summary");
    assert_eq!(events[6]["text"], "safe summary");
    assert_eq!(
        events[7]["part"],
        json!({ "type": "summary_text", "text": "safe summary" })
    );
    assert_eq!(events[8]["item"]["id"], item_id);
    assert_eq!(events[8]["output_index"], output_index);
    assert_eq!(
        events[8]["item"]["summary"],
        json!([{ "type": "summary_text", "text": "safe summary" }])
    );
    assert!(
        events
            .iter()
            .all(|event| event["type"] != "response.reasoning_text.delta")
    );

    let terminal = terminal_event(&events);
    assert_eq!(terminal["response"]["output"][0]["id"], item_id);
    assert_eq!(
        terminal["response"]["output"][0]["summary"],
        json!([{ "type": "summary_text", "text": "safe summary" }])
    );
    assert!(terminal["response"]["output"][0].get("content").is_none());
}

#[tokio::test]
async fn responses_reasoning_usage_is_not_inferred() {
    let upstream = MockUpstream::default();
    upstream.set_supported_models(["glm-5.1"]).await;
    upstream
        .push_response(vec![
            Ok(reasoning_chunk(
                "chat-unreported-reasoning-usage",
                &"hidden reasoning text ".repeat(256),
            )),
            Ok(content_chunk(
                "chat-unreported-reasoning-usage",
                "short answer",
            )),
            Ok(finish_chunk("chat-unreported-reasoning-usage", "stop")),
            // The provider reports exact totals but no cached/reasoning detail
            // blocks. Public Responses requires zero-valued detail fields; the
            // hidden reasoning text must not be used to invent a token count.
            Ok(usage_chunk("chat-unreported-reasoning-usage", 11, 7, 18)),
        ])
        .await;
    let app = llmconduit::build_app_from_gateway(test_gateway(upstream, MockSearch::default()));

    let response = post_json(
        app,
        json!({
            "model": "glm-5.1",
            "stream": true,
            "store": false,
            "input": "answer briefly"
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let events = response_events(response).await;
    let terminal = terminal_event(&events);
    let usage = &terminal["response"]["usage"];
    assert_eq!(usage["input_tokens"], 11);
    assert_eq!(usage["output_tokens"], 7);
    assert_eq!(usage["total_tokens"], 18);
    assert_eq!(usage["input_tokens_details"]["cached_tokens"], 0);
    assert_eq!(usage["output_tokens_details"]["reasoning_tokens"], 0);
    assert!(
        events
            .iter()
            .all(|event| event["type"] != "response.reasoning_text.delta")
    );
    let reasoning = terminal["response"]["output"]
        .as_array()
        .expect("terminal output")
        .iter()
        .find(|item| item["type"] == "reasoning")
        .expect("reasoning item retained without exposing hidden content");
    assert_eq!(reasoning["summary"], json!([]));
    assert!(reasoning.get("content").is_none());
}
