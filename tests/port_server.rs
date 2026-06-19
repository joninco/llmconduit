//! Ported surface: pre-flight context budgeting (gap G3, claude-relay
//! `_completion_token_margin` / `_cap_max_completion_tokens` / `ContextWindowError`).
//!
//! claude-relay capped an EXPLICITLY-requested completion budget down to
//! `(context_limit - input_tokens - 128)` before calling upstream, and rejected
//! requests whose input already exhausts the context window. llmconduit mirrors
//! this in `Gateway::stream_responses` as a single pre-spawn seam: when the
//! upstream catalog reports a context length for the resolved model, an explicit
//! `max_output_tokens` is capped down (never raised, never synthesized), and a
//! clear input overflow returns a 400 before any upstream chat POST is made.
//!
//! G3 is deliberately the proactive complement to G1 (reactive shrink-and-retry,
//! covered in `port_errors.rs`). Option B: the pre-flight estimate is computed
//! over the LOWERED upstream payload (`LoweredTurn`), not the canonical request,
//! so no canonical field can inflate it. These tests therefore compute the
//! expected budget from the `ChatCompletionRequest` the mock upstream ACTUALLY
//! RECEIVED — `available = context − ceil(bytes(recorded messages+tools+lowered
//! scalars)/4) − 128` — so they track the real payload and survive future
//! lowering changes rather than hard-mirroring the estimator.
//!
//! Tests 1-3 drive the in-process gateway with `MockUpstream` and inspect the
//! recorded upstream request. The reject test goes through the full HTTP layer
//! (wiremock upstream) to prove the overflow 400 is surfaced and ZERO chat POSTs
//! are issued.

mod common;

use common::MockSearch;
use common::MockUpstream;
use common::base_request;
use common::collect_stream;
use common::content_chunk;
use common::test_gateway_with_config;
use common::usage_chunk;
use common::user_message;

use serde_json::Value;
use serde_json::json;
use tower::ServiceExt;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

use axum::body::Body;
use axum::http::Request;
use llmconduit::config::Config;
use llmconduit::models::chat::ChatCompletionRequest;
use llmconduit::models::responses::ContentItem;
use llmconduit::models::responses::ReasoningRequest;
use llmconduit::models::responses::ResponseItem;
use llmconduit::models::responses::ResponsesRequest;
use llmconduit::models::responses::ToolSpec;

const MARGIN: i64 = 128;

/// Oracle for `engine::estimate_input_tokens`, computed from the upstream chat
/// request the mock ACTUALLY RECEIVED. It reuses the production
/// `estimate_request_from_lowered` (one source of truth) — which applies the
/// SAME `sanitize_chat_request` the leaf applies (content flattening etc.) — and
/// counts `ceil(bytes/4)` of the serialized result. Because it reads the
/// recorded payload AND shares the estimator's construction + sanitize, it
/// tracks the exact wire bytes across future lowering/sanitize changes.
fn estimate_from_recorded(recorded: &ChatCompletionRequest, flatten_content: bool) -> i64 {
    let tools = recorded.tools.clone().unwrap_or_default();
    let request = llmconduit::engine::estimate_request_from_lowered(
        &recorded.messages,
        &tools,
        &recorded.reasoning_effort,
        &recorded.response_format,
        flatten_content,
    );
    let bytes = serde_json::to_vec(&request)
        .expect("serialize estimate request")
        .len();
    bytes.div_ceil(4) as i64
}

/// Run a single non-tool turn through the gateway with a known per-model context
/// limit, and return the `ChatCompletionRequest` the upstream actually received.
async fn recorded_upstream_request(
    request: ResponsesRequest,
    context_limit: i64,
) -> ChatCompletionRequest {
    let upstream = MockUpstream::default();
    // Register the model in the catalog so it resolves to itself and carries the
    // context limit (ids + limits come from one snapshot in production).
    upstream.set_supported_models([request.model.clone()]).await;
    upstream
        .set_context_limits([(request.model.clone(), context_limit)])
        .await;
    upstream
        .push_response(vec![
            Ok(content_chunk("chat-1", "hi")),
            Ok(usage_chunk("chat-1", 5, 1, 6)),
        ])
        .await;
    let gateway = test_gateway_with_config(
        upstream.clone(),
        MockSearch::default(),
        common::test_config(),
    );
    let _ = collect_stream(
        gateway
            .stream_responses(request)
            .await
            .expect("stream should start"),
    )
    .await;
    let mut requests = upstream.requests().await;
    assert_eq!(requests.len(), 1, "expected exactly one upstream chat POST");
    requests.remove(0)
}

/// Convenience: the capped `max_output_tokens` the upstream received.
async fn recorded_max_output_tokens(request: ResponsesRequest, context_limit: i64) -> Option<i64> {
    recorded_upstream_request(request, context_limit)
        .await
        .max_output_tokens
}

#[tokio::test]
async fn preflight_margin_is_fixed_128() {
    // Known context, explicit budget far above what fits: the sent budget must
    // equal context - estimated_input - 128 (estimate taken from the recorded
    // lowered payload), proving the reserve is exactly 128.
    let mut request = base_request(vec![user_message("hello world")]);
    request.max_output_tokens = Some(1_000_000);
    let context_limit = 65_536;

    let recorded = recorded_upstream_request(request, context_limit).await;
    // test_config has flatten_content = true.
    let expected = context_limit - estimate_from_recorded(&recorded, true) - MARGIN;
    assert_eq!(recorded.max_output_tokens, Some(expected));
}

#[tokio::test]
async fn preflight_keeps_lower_explicit_max_tokens() {
    // Requested budget is BELOW the available budget => left unchanged (G3 only
    // caps down, never raises).
    let mut request = base_request(vec![user_message("hello world")]);
    request.max_output_tokens = Some(256);
    let context_limit = 65_536;

    let recorded = recorded_upstream_request(request, context_limit).await;
    let available = context_limit - estimate_from_recorded(&recorded, true) - MARGIN;
    assert!(
        available > 256,
        "test precondition: available must exceed request"
    );
    assert_eq!(recorded.max_output_tokens, Some(256));
}

#[tokio::test]
async fn preflight_caps_explicit_max_tokens_to_available() {
    // Requested budget is ABOVE the available budget => reduced to exactly the
    // available budget. We size the request first, run it, then re-derive the
    // expected available from the recorded payload (the request's own
    // max_output_tokens does not enter the estimate).
    let mut request = base_request(vec![user_message("hello world")]);
    request.max_output_tokens = Some(1_000_000);
    let context_limit = 2_048;

    let recorded = recorded_upstream_request(request, context_limit).await;
    let available = context_limit - estimate_from_recorded(&recorded, true) - MARGIN;
    assert!(available > 0, "test precondition: some budget remains");
    // 1_000_000 is far above `available`, so the cap must reduce it to exactly
    // `available`.
    assert_eq!(recorded.max_output_tokens, Some(available));
}

#[tokio::test]
async fn preflight_image_generation_tool_does_not_change_budget() {
    // Class guard for the recurring "phantom stripped-tool bytes" finding: the
    // estimator counts the LOWERED upstream tool set, and `lower_tools` strips
    // `image_generation` to nothing. So an image_generation tool must charge
    // ZERO budget bytes — the cap must be identical with and without it, and it
    // must never trip a false pre-flight 400. Counting the raw `ToolSpec` would
    // inflate the estimate and break this.
    let context_limit = 4_096;

    let without_tool = {
        let mut request = base_request(vec![user_message("draw a cat")]);
        request.max_output_tokens = Some(1_000_000);
        recorded_max_output_tokens(request, context_limit).await
    };

    let with_image_tool = {
        let mut request = base_request(vec![user_message("draw a cat")]);
        request.max_output_tokens = Some(1_000_000);
        request.tools = vec![ToolSpec::ImageGeneration {
            output_format: Some("png".to_string()),
        }];
        recorded_max_output_tokens(request, context_limit).await
    };

    assert_eq!(
        with_image_tool, without_tool,
        "a stripped image_generation tool must not change the budget"
    );
}

#[tokio::test]
async fn preflight_image_generation_call_in_input_does_not_change_budget() {
    // Class guard for the INPUT side: `lower_request` drops
    // `ResponseItem::ImageGenerationCall` (the only `=> {}` arm), so the
    // conservative estimator excludes it. Its presence in input must charge ZERO
    // budget bytes — identical cap with and without it, and never a false 400.
    // This would fail under an estimator that serialized the raw `request.input`.
    let context_limit = 4_096;
    let big_result = "x".repeat(50_000); // large field that, if counted, would shrink the cap

    let without_call = {
        let mut request = base_request(vec![user_message("hello")]);
        request.max_output_tokens = Some(1_000_000);
        recorded_max_output_tokens(request, context_limit).await
    };

    let with_call = {
        let mut request = base_request(vec![
            user_message("hello"),
            ResponseItem::ImageGenerationCall {
                id: "ig_1".to_string(),
                status: "completed".to_string(),
                revised_prompt: Some("a very detailed revised prompt".to_string()),
                result: big_result,
            },
        ]);
        request.max_output_tokens = Some(1_000_000);
        recorded_max_output_tokens(request, context_limit).await
    };

    assert_eq!(
        with_call, without_call,
        "a dropped image_generation_call input item must not change the budget"
    );
}

#[tokio::test]
async fn preflight_reasoning_summary_does_not_change_budget() {
    // Field-level class guard: lowering keeps only `reasoning_effort`, dropping
    // `reasoning.summary`. Because the estimate is computed over the LOWERED
    // payload, a large `reasoning.summary` must charge ZERO budget bytes —
    // identical cap with and without it, never a false 400. A canonical-request
    // estimator (serializing `request.reasoning`) would over-count here.
    let context_limit = 4_096;

    let without_summary = {
        let mut request = base_request(vec![user_message("hello")]);
        request.max_output_tokens = Some(1_000_000);
        recorded_max_output_tokens(request, context_limit).await
    };

    let with_big_summary = {
        let mut request = base_request(vec![user_message("hello")]);
        request.max_output_tokens = Some(1_000_000);
        request.reasoning = Some(ReasoningRequest {
            effort: Some("medium".to_string()),
            summary: Some("s".repeat(50_000)),
        });
        recorded_max_output_tokens(request, context_limit).await
    };

    assert_eq!(
        with_big_summary, without_summary,
        "a dropped reasoning.summary must not change the budget"
    );
}

#[tokio::test]
async fn preflight_multipart_text_budgets_like_flattened_string() {
    // Terminal-layer guard: the leaf's `sanitize_chat_request` flattens a
    // text-only multi-part `Message.content` array to a bare "a\nb" string
    // before POSTing. The estimate counts the POST-sanitize body, so a multi-part
    // message must budget IDENTICALLY to its already-flattened-string equivalent
    // and must not trip a false 400. A pre-sanitize estimate would count the
    // `{"type":"text",...}` array-wrapper bytes and diverge near the boundary.
    let context_limit = 8_192;
    let part_a = "A".repeat(2_000);
    let part_b = "B".repeat(2_000);

    // Multi-part: two InputText parts -> lowered to a content array -> flattened.
    let multipart_budget = {
        let mut request = base_request(vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: part_a.clone(),
                },
                ContentItem::InputText {
                    text: part_b.clone(),
                },
            ],
            phase: None,
        }]);
        request.max_output_tokens = Some(1_000_000);
        recorded_max_output_tokens(request, context_limit).await
    };

    // Flattened-string equivalent: one InputText whose text is the join.
    let flattened_budget = {
        let mut request = base_request(vec![user_message(&format!("{part_a}\n{part_b}"))]);
        request.max_output_tokens = Some(1_000_000);
        recorded_max_output_tokens(request, context_limit).await
    };

    assert!(
        multipart_budget.is_some(),
        "multi-part text must not cause a false 400"
    );
    assert_eq!(
        multipart_budget, flattened_budget,
        "multi-part text must budget identically to its flattened-string form"
    );
}

/// Non-streaming `/v1/responses` config pointed at a wiremock upstream.
fn config_for(server_uri: &str) -> Config {
    Config {
        bind_addr: "127.0.0.1:0".parse().expect("socket addr"),
        upstream_base_url: format!("{server_uri}/v1/").parse().expect("url"),
        upstream_api_key: None,
        upstream_model: None,
        system_prompt_prefix: None,
        upstream_request_log_path: None,
        upstream_chat_kwargs: serde_json::Map::new(),
        upstreams: Vec::new(),
        fallback_upstreams: Vec::new(),
        upstream_failure_cooldown_secs: 30,
        model_profiles: std::collections::BTreeMap::new(),
        model_routes: Vec::new(),
        brave_base_url: "https://example.com/".parse().expect("url"),
        brave_api_key: None,
        brave_max_results: 5,
        request_timeout: std::time::Duration::from_secs(30),
        connect_timeout_secs: 10,
        max_web_search_rounds: 5,
        flatten_content: true,
        max_replay_entries: 1000,
        debug_log_max_age_hours: None,
        min_completion_tokens: 4096,
        max_sse_frame_bytes: 8 * 1024 * 1024,
        image_agent_enabled: false,
        vision_url: None,
        vision_model: None,
        image_cache_max_size: 100,
        image_cache_ttl_secs: 300,
        template_family: None,
    }
}

#[tokio::test]
async fn preflight_rejects_context_exhausted() {
    // The model reports a small context window, and the input is sized so the
    // LOWERED payload + the fixed 128 margin exceeds it. The non-streaming
    // request must get a 400 and the upstream must see ZERO chat POSTs. Using a
    // context (256) larger than the margin alone exercises the INPUT-size path,
    // not just margin > context.
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "tiny-ctx", "max_model_len": 256}]
        })))
        .mount(&server)
        .await;

    // Mounted so that ANY chat POST would succeed -- its absence is what proves
    // the request was rejected pre-spawn.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("data: [DONE]\n\n"),
        )
        .mount(&server)
        .await;

    // ~900-char input => lowered user message ~225+ est tokens; + 128 margin
    // comfortably exceeds the 256 context window.
    let big_input = "overflow ".repeat(100);
    let app = llmconduit::build_app(config_for(&server.uri()));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "tiny-ctx",
                        "stream": false,
                        "input": big_input,
                    })
                    .to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(
        response.status().as_u16(),
        400,
        "input overflow must be a clean 400"
    );
    let body_bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("read body");
    let body: Value = serde_json::from_slice(&body_bytes).expect("json body");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("context window"),
        "error message should mention the context window: {body}"
    );

    let chat_posts = server
        .received_requests()
        .await
        .expect("requests")
        .into_iter()
        .filter(|request| {
            request.method.as_str() == "POST" && request.url.path() == "/v1/chat/completions"
        })
        .count();
    assert_eq!(chat_posts, 0, "rejected request must not POST to upstream");
}
