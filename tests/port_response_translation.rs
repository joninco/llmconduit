//! Ported surface: reasoning promotion / suppression (claude-relay
//! test_convert_stream.py::test_reasoning_* / ::test_*_promoted* /
//! ::test_signature_* / ::test_late_reasoning_*), GAPS.md G8.
//!
//! claude-relay buffered upstream reasoning in its Chat->Anthropic converter and
//! decided its final shape only once the stream's shape was known. llmconduit's
//! egress converter (`responses_to_anthropic.rs`) carries the same heuristics on
//! the canonical-Responses -> Anthropic path:
//!   - reasoning-only ending cleanly (`finish_reason:stop`) is PROMOTED to a
//!     `text` block (the backend put its answer in the reasoning channel),
//!   - reasoning-only truncated (`finish_reason:length` -> `max_tokens`) stays a
//!     `thinking` block (genuine, truncated chain-of-thought),
//!   - reasoning carrying a signature stays a `thinking` block even at stop
//!     (never promoted),
//!   - reasoning arriving AFTER text has started is "late" and dropped,
//!   - normal reasoning-then-text flushes the reasoning as a `thinking` block
//!     before the text.
//!
//! These drive the full gateway through the Anthropic `/v1/messages` surface
//! (mirroring `gateway.rs`) so the heuristics are proven end-to-end across the
//! chat -> responses -> anthropic pipeline, then assert on the Anthropic block
//! sequence via the shared `parse_anthropic_sse_events` collector. Assertions
//! are on the event/block sequence, never on timing.

mod common;

use axum::body::Body;
use axum::http::Request;
use common::MockSearch;
use common::MockUpstream;
use common::content_chunk;
use common::finish_chunk;
use common::nested_thinking_chunk;
use common::parse_anthropic_sse_events;
use common::reasoning_chunk;
use common::test_gateway;
use common::tool_call_chunk;
use llmconduit::error::AppError;
use llmconduit::models::chat::ChatCompletionChunk;
use serde_json::Value;
use serde_json::json;
use tower::ServiceExt;

/// Run an Anthropic streaming `/v1/messages` turn with `thinking` enabled over a
/// single canned upstream turn, returning the parsed Anthropic SSE events.
async fn run_anthropic_stream(
    upstream_chunks: Vec<Result<ChatCompletionChunk, AppError>>,
) -> Vec<Value> {
    let upstream = MockUpstream::default();
    upstream.push_response(upstream_chunks).await;
    let gateway = test_gateway(upstream, MockSearch::default());
    let app = llmconduit::build_app_from_gateway(gateway);

    let body = json!({
        "model": "claude-3-7-sonnet-20250219",
        "max_tokens": 1024,
        "stream": true,
        // Enabled thinking so the request asks the backend for reasoning; the
        // egress converter still decides promotion vs. suppression per turn.
        "thinking": { "type": "enabled", "budget_tokens": 1024 },
        "messages": [{ "role": "user", "content": "Think then answer." }]
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).expect("serialize")))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status().as_u16(), 200);

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    let body_text = String::from_utf8(body_bytes.to_vec()).expect("utf8");
    parse_anthropic_sse_events(&body_text)
}

/// The ordered `type` of every `content_block_start` in the stream.
fn block_kinds(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter(|event| event["type"] == "content_block_start")
        .map(|event| {
            event["content_block"]["type"]
                .as_str()
                .expect("content_block type")
                .to_string()
        })
        .collect()
}

/// Concatenated text from every `text_delta`.
fn text_payload(events: &[Value]) -> String {
    events
        .iter()
        .filter(|event| {
            event["type"] == "content_block_delta" && event["delta"]["type"] == "text_delta"
        })
        .filter_map(|event| event["delta"]["text"].as_str())
        .collect()
}

/// Concatenated text from every `thinking_delta`.
fn thinking_payload(events: &[Value]) -> String {
    events
        .iter()
        .filter(|event| {
            event["type"] == "content_block_delta" && event["delta"]["type"] == "thinking_delta"
        })
        .filter_map(|event| event["delta"]["thinking"].as_str())
        .collect()
}

fn terminal_stop_reason(events: &[Value]) -> String {
    events
        .iter()
        .find(|event| event["type"] == "message_delta" && event["delta"]["stop_reason"].is_string())
        .and_then(|event| event["delta"]["stop_reason"].as_str())
        .expect("terminal stop_reason")
        .to_string()
}

/// content_block_start/stop must be balanced and the stream must end with
/// `message_stop` (no client left hanging).
fn assert_well_formed(events: &[Value]) {
    let starts = events
        .iter()
        .filter(|event| event["type"] == "content_block_start")
        .count();
    let stops = events
        .iter()
        .filter(|event| event["type"] == "content_block_stop")
        .count();
    assert_eq!(starts, stops, "content blocks must be balanced: {events:?}");
    assert_eq!(
        events.last().map(|event| event["type"].as_str()),
        Some(Some("message_stop")),
        "stream must end with message_stop: {events:?}"
    );
}

/// reasoning-only + `finish_reason:stop` => the backend put the answer in the
/// reasoning channel, so it is promoted to a single `text` block (no thinking).
#[tokio::test]
async fn reasoning_only_at_stop_is_promoted_to_text() {
    let events = run_anthropic_stream(vec![
        Ok(reasoning_chunk("chat-1", "The answer is 42")),
        Ok(finish_chunk("chat-1", "stop")),
    ])
    .await;

    assert_eq!(
        block_kinds(&events),
        vec!["text"],
        "reasoning-only@stop must be a single promoted text block"
    );
    assert_eq!(text_payload(&events), "The answer is 42");
    assert!(
        thinking_payload(&events).is_empty(),
        "promoted reasoning must not also emit a thinking block"
    );
    assert_eq!(terminal_stop_reason(&events), "end_turn");
    assert_well_formed(&events);
}

/// reasoning-only + `finish_reason:length` => truncated genuine CoT. It is NOT
/// promoted; it stays a `thinking` block and the turn ends with `max_tokens`.
#[tokio::test]
async fn reasoning_only_at_length_stays_thinking() {
    let events = run_anthropic_stream(vec![
        Ok(reasoning_chunk("chat-1", "partial think")),
        Ok(finish_chunk("chat-1", "length")),
    ])
    .await;

    assert_eq!(
        block_kinds(&events),
        vec!["thinking"],
        "truncated reasoning must stay a thinking block, not promote"
    );
    assert_eq!(thinking_payload(&events), "partial think");
    assert!(
        text_payload(&events).is_empty(),
        "length-truncated reasoning must not be promoted to text"
    );
    assert_eq!(terminal_stop_reason(&events), "max_tokens");
    assert_well_formed(&events);
}

/// reasoning carrying a signature is genuine CoT: it stays a `thinking` block
/// (with its `signature_delta`) and is never promoted, even at `stop`.
#[tokio::test]
async fn signed_reasoning_only_stays_thinking_at_stop() {
    let events = run_anthropic_stream(vec![
        Ok(nested_thinking_chunk("chat-1", "Hidden chain", "sig_abc")),
        Ok(finish_chunk("chat-1", "stop")),
    ])
    .await;

    assert_eq!(
        block_kinds(&events),
        vec!["thinking"],
        "signed reasoning must stay thinking even at stop"
    );
    assert_eq!(thinking_payload(&events), "Hidden chain");
    assert!(
        text_payload(&events).is_empty(),
        "signed reasoning must never be promoted to text"
    );
    assert!(
        events.iter().any(|event| {
            event["type"] == "content_block_delta"
                && event["delta"]["type"] == "signature_delta"
                && event["delta"]["signature"] == "sig_abc"
        }),
        "the buffered signature must be surfaced: {events:?}"
    );
    assert_eq!(terminal_stop_reason(&events), "end_turn");
    assert_well_formed(&events);
}

/// Concatenated signature from every `signature_delta`.
fn signature_payload(events: &[Value]) -> String {
    events
        .iter()
        .filter(|event| {
            event["type"] == "content_block_delta" && event["delta"]["type"] == "signature_delta"
        })
        .filter_map(|event| event["delta"]["signature"].as_str())
        .collect()
}

/// A thinking signature delivered across MULTIPLE upstream chunks (and thus
/// multiple `signature_delta` events) must be accumulated in order: the emitted
/// thinking block's signature is the full concatenation, not just the last
/// fragment.
#[tokio::test]
async fn multi_part_signature_is_accumulated() {
    let events = run_anthropic_stream(vec![
        Ok(nested_thinking_chunk("chat-1", "Hidden chain", "sig_")),
        Ok(nested_thinking_chunk("chat-1", "", "part2_")),
        Ok(nested_thinking_chunk("chat-1", "", "end")),
        Ok(finish_chunk("chat-1", "stop")),
    ])
    .await;

    assert_eq!(
        block_kinds(&events),
        vec!["thinking"],
        "signed reasoning must stay a single thinking block"
    );
    assert_eq!(
        signature_payload(&events),
        "sig_part2_end",
        "multi-part signature must be concatenated, not truncated to the last piece"
    );
    assert!(
        text_payload(&events).is_empty(),
        "signed reasoning must never be promoted to text"
    );
    assert_eq!(terminal_stop_reason(&events), "end_turn");
    assert_well_formed(&events);
}

/// Reasoning arriving AFTER text has started is "late" / abnormal and dropped:
/// only the text block survives, with no thinking block and no promotion.
#[tokio::test]
async fn late_reasoning_after_text_is_dropped() {
    let events = run_anthropic_stream(vec![
        Ok(content_chunk("chat-1", "Hey")),
        Ok(reasoning_chunk("chat-1", "should not appear")),
        Ok(finish_chunk("chat-1", "stop")),
    ])
    .await;

    assert_eq!(
        block_kinds(&events),
        vec!["text"],
        "late reasoning must be dropped, leaving only the text block"
    );
    assert_eq!(text_payload(&events), "Hey");
    assert!(
        thinking_payload(&events).is_empty(),
        "late reasoning must not be flushed as a thinking block"
    );
    assert!(
        !text_payload(&events).contains("should not appear"),
        "late reasoning text must not leak into the answer"
    );
    assert_eq!(terminal_stop_reason(&events), "end_turn");
    assert_well_formed(&events);
}

/// Normal reasoning-then-text: the reasoning is a genuine preface, flushed as a
/// `thinking` block (in order) before the `text` block. (Promotion only applies
/// when reasoning is the *only* output.)
#[tokio::test]
async fn reasoning_then_text_flushes_thinking_before_text() {
    let events = run_anthropic_stream(vec![
        Ok(reasoning_chunk("chat-1", "Let me think")),
        Ok(content_chunk("chat-1", "Answer")),
        Ok(finish_chunk("chat-1", "stop")),
    ])
    .await;

    assert_eq!(
        block_kinds(&events),
        vec!["thinking", "text"],
        "reasoning must flush as thinking before the text block"
    );
    assert_eq!(thinking_payload(&events), "Let me think");
    assert_eq!(text_payload(&events), "Answer");
    assert_eq!(terminal_stop_reason(&events), "end_turn");
    assert_well_formed(&events);
}

/// Run an Anthropic streaming `/v1/messages` turn with the server-side
/// `web_search` tool enabled across TWO upstream rounds (the search round and
/// the post-results continuation round), returning the parsed Anthropic SSE
/// events. Brave runs server-side, so the gateway injects the results between
/// the two rounds and emits a `response.web_search_results` event.
async fn run_anthropic_web_search_stream(
    first_round: Vec<Result<ChatCompletionChunk, AppError>>,
    second_round: Vec<Result<ChatCompletionChunk, AppError>>,
) -> Vec<Value> {
    let upstream = MockUpstream::default();
    upstream.push_response(first_round).await;
    upstream.push_response(second_round).await;
    let gateway = test_gateway(upstream, MockSearch::default());
    let app = llmconduit::build_app_from_gateway(gateway);

    let body = json!({
        "model": "claude-3-7-sonnet-20250219",
        "max_tokens": 1024,
        "stream": true,
        "thinking": { "type": "enabled", "budget_tokens": 1024 },
        "tools": [{ "type": "web_search_20250305", "name": "web_search" }],
        "messages": [{ "role": "user", "content": "What's the weather?" }]
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).expect("serialize")))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status().as_u16(), 200);

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    let body_text = String::from_utf8(body_bytes.to_vec()).expect("utf8");
    parse_anthropic_sse_events(&body_text)
}

/// A web-search turn is: reasoning -> web_search_call -> web_search_results ->
/// CONTINUATION reasoning -> text answer. The additive `web_search_results`
/// block must NOT trip the late-reasoning drop gate, so the post-search
/// continuation reasoning (a genuine thinking block) AND the text answer must
/// both survive -- nothing is dropped.
#[tokio::test]
async fn post_web_search_reasoning_and_answer_are_preserved() {
    let events = run_anthropic_web_search_stream(
        // Round 1: reasoning, then the model calls web_search.
        vec![
            Ok(reasoning_chunk("chat-1", "Pre-search thought.")),
            Ok(tool_call_chunk(
                "chat-1",
                "call_ws_1",
                "web_search",
                "{\"query\":\"weather\"}",
            )),
        ],
        // Round 2 (after Brave results are injected): more reasoning, then the
        // text answer.
        vec![
            Ok(reasoning_chunk("chat-2", "Post-search thought.")),
            Ok(content_chunk("chat-2", "It is sunny.")),
            Ok(finish_chunk("chat-2", "stop")),
        ],
    )
    .await;

    // The server-side web search must be surfaced (proves we took the
    // web_search_results path that previously tripped the gate).
    assert!(
        block_kinds(&events)
            .iter()
            .any(|kind| kind == "web_search_tool_result"),
        "web_search_tool_result block must be emitted: {:?}",
        block_kinds(&events)
    );

    // Both reasoning segments must surface as thinking: the pre-search reasoning
    // flushed at the search boundary, the post-search continuation reasoning
    // flushed when the answer begins. Pre-fix, the post-search reasoning was
    // dropped because web_search_results set `content_started`.
    let thinking = thinking_payload(&events);
    assert!(
        thinking.contains("Pre-search thought."),
        "pre-search reasoning must be preserved, got {thinking:?}"
    );
    assert!(
        thinking.contains("Post-search thought."),
        "post-search continuation reasoning must NOT be dropped, got {thinking:?}"
    );

    // The text answer must survive intact.
    assert_eq!(
        text_payload(&events),
        "It is sunny.",
        "the text answer must be preserved after a web search"
    );
    assert_eq!(terminal_stop_reason(&events), "end_turn");
    assert_well_formed(&events);
}
