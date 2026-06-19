//! Gap G3, second half — reasoning-aware keep-alive peek.
//!
//! claude-relay's `_peek_with_keepalive` did two things for a streaming turn:
//! it sent SSE keep-alive comment frames while the upstream was silent, and it
//! buffered a reasoning-only stream until the first client-visible output (or
//! `[DONE]`), promoting/holding the reasoning accordingly. After studying the
//! llmconduit architecture, BOTH halves already exist here — built differently —
//! so the contracted, testable behavior is to LOCK what exists, not to port a
//! redundant peek (a re-implemented peek would duplicate the egress converter
//! and add cancellation complexity to the engine for no observable gain).
//!
//! SCOPE OF THIS FILE (deliberately narrow): the two surfaces that genuinely
//! provide the G3 behavior in llmconduit —
//!   1. SSE TRANSPORT KEEP-ALIVE — every streaming HTTP response is built with
//!      axum's `KeepAlive::new()` (`http.rs` `stream_anthropic_response` /
//!      `stream_chat_completions_response` / `stream_responses_response`),
//!      whose `KeepAliveStream` emits a `:`-comment ping after each idle
//!      interval (15s default). The test drives a real, IDLE Anthropic SSE
//!      response (a `PendingUpstream` that stalls after the engine's prologue)
//!      through the http.rs path, advances the paused clock past the interval,
//!      and asserts a keep-alive comment frame appears in the response body — so
//!      deleting `.keep_alive(...)` makes it fail (no ping is ever emitted).
//!   2. ANTHROPIC-EGRESS reasoning-only deferral/promotion — the
//!      `responses_to_anthropic::AnthropicStreamConverter` (gap G8) DEFERS
//!      reasoning: it emits NO content block while only reasoning has arrived,
//!      and resolves the buffer at the terminal (promote reasoning-only@`stop`
//!      to a single `text` block). The test asserts event-by-event that no
//!      content block is emitted before `response.completed`, then exactly one
//!      promoted `text` block — so an impl that streamed reasoning as text
//!      mid-stream (no deferral) fails.
//!
//! NOT claimed: this is Anthropic-EGRESS + SSE-transport only. The canonical
//! Responses and Chat egress paths are NOT covered by the G8 reasoning-deferral
//! behavior (they stream reasoning progressively), so nothing here asserts a
//! reasoning-only buffering contract for them. The full reasoning-deferral
//! matrix (length/signed/late/then-text) lives in
//! `tests/port_response_translation.rs` and the `responses_to_anthropic.rs`
//! unit tests; this file adds only the G3-specific ORDERING (deferral) lock and
//! the transport keep-alive lock. Assertions are on the event sequence, never
//! on wall-clock timing.

mod common;

use common::MockSearch;
use common::base_request;
use common::test_gateway;
use common::user_message;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::Request;
use futures::StreamExt;
use futures::poll;
use llmconduit::adapters::responses_to_anthropic::AnthropicStreamConverter;
use llmconduit::engine::Gateway;
use llmconduit::engine::SseEvent;
use llmconduit::error::AppError;
use llmconduit::models::anthropic::AnthropicContentBlockStart;
use llmconduit::models::anthropic::AnthropicDelta;
use llmconduit::models::anthropic::AnthropicStreamEvent;
use llmconduit::models::chat::ChatCompletionRequest;
use llmconduit::monitor::MonitorHub;
use llmconduit::replay::ReplayStore;
use llmconduit::upstream::UpstreamClient;
use llmconduit::upstream::UpstreamModelEntry;
use llmconduit::upstream::UpstreamStream;
use serde_json::json;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;
use tower::ServiceExt;

/// Build a gateway over an arbitrary `UpstreamClient` (the shared
/// `common::test_gateway` is fixed to `MockUpstream`), reusing the standard
/// `common::test_config`.
fn gateway_with_upstream<U: UpstreamClient + 'static>(upstream: U) -> Arc<Gateway> {
    let config = common::test_config();
    let vision: Arc<dyn llmconduit::vision::VisionClient> = Arc::new(
        llmconduit::vision::ReqwestVisionClient::new(reqwest::Client::new(), &config),
    );
    let image_cache = Arc::new(llmconduit::vision::ImageCache::from_config(&config));
    Arc::new(Gateway::new(
        config,
        ReplayStore::new(1000),
        Arc::new(upstream),
        Arc::new(MockSearch::default()),
        vision,
        image_cache,
        MonitorHub::disabled(),
        None,
    ))
}

// --------------------------------------------------------------------------
// (1) SSE transport keep-alive — observe the ACTUAL axum KeepAlive output
// --------------------------------------------------------------------------

/// An upstream whose chat stream never yields a chunk: it stays pending forever.
/// This forces the engine's SSE output to go IDLE after its prologue
/// (`response.created` / `response.in_progress` → Anthropic `ping` /
/// `message_start`), which is exactly the state axum's keep-alive timer guards.
#[derive(Clone, Default)]
struct PendingUpstream;

#[async_trait]
impl UpstreamClient for PendingUpstream {
    async fn stream_chat_completion(
        &self,
        _request: &ChatCompletionRequest,
    ) -> Result<UpstreamStream, AppError> {
        // A stream that is immediately and permanently pending.
        let stream = async_stream::stream! {
            std::future::pending::<()>().await;
            // Unreachable; present only to fix the stream item type.
            yield Err(AppError::internal("unreachable"));
        };
        Ok(Box::pin(stream))
    }

    async fn list_models(&self) -> Result<reqwest::Response, AppError> {
        Err(AppError::internal("unused in this test"))
    }

    async fn supported_model_catalog(&self) -> Result<Vec<UpstreamModelEntry>, AppError> {
        Ok(vec![UpstreamModelEntry {
            id: "glm-5.1".to_string(),
            context_limit: None,
        }])
    }
}

/// Drive a real, IDLE Anthropic `/v1/messages` SSE response through the http.rs
/// streaming path and assert axum's keep-alive timer emits a `:`-comment ping
/// once the idle interval elapses. Uses paused time (`start_paused`) so the test
/// is deterministic and instant.
///
/// FAILS FAST IF `.keep_alive(...)` IS REMOVED from
/// `http.rs::stream_anthropic_response`: without the `KeepAliveStream` wrapper
/// the idle body never produces a ping. The test only ever polls the body
/// (never `.await`s it), so after advancing the paused clock past the interval a
/// still-`Pending` body triggers an immediate `panic!` — a deterministic
/// failure, not a hang on the permanently-pending upstream.
#[tokio::test(start_paused = true)]
async fn anthropic_idle_stream_emits_keepalive_ping() {
    let app = llmconduit::build_app_from_gateway(gateway_with_upstream(PendingUpstream));

    let body = json!({
        "model": "glm-5.1",
        "max_tokens": 1024,
        "stream": true,
        "messages": [{ "role": "user", "content": "hi" }]
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

    let mut body = response.into_body().into_data_stream();

    // STEP 1 — drain the engine prologue until the body is genuinely IDLE.
    // The spawned engine/converter tasks emit a few frames (Anthropic `ping` /
    // `message_start`) before stalling on the permanently-pending upstream. Poll
    // (never `.await`) the body, yielding to the runtime between polls so those
    // spawned tasks make progress. A single `Pending` is NOT proof of idle — a
    // prologue frame may still be propagating through the multi-stage pipeline
    // (engine task -> mpsc -> converter task -> mpsc -> ReceiverStream ->
    // KeepAliveStream), which can take several scheduler passes. We declare idle
    // only after several CONSECUTIVE `Pending` polls (each separated by a
    // `yield_now`) with the paused clock UNCHANGED: once the prologue is fully
    // drained the permanently-pending upstream guarantees the body stays pending
    // forever (until the clock advances), so no number of further yields can
    // produce a frame. A keep-alive frame must not appear here (the interval has
    // not elapsed); if one somehow did, that already proves keep-alive is wired.
    const IDLE_CONFIRM_POLLS: usize = 16;
    let mut saw_keepalive = false;
    let mut idle = false;
    let mut consecutive_pending = 0;
    for _ in 0..256 {
        let mut next = std::pin::pin!(body.next());
        match poll!(next.as_mut()) {
            Poll::Ready(Some(Ok(bytes))) => {
                consecutive_pending = 0;
                if bytes.starts_with(b":") {
                    saw_keepalive = true;
                    break;
                }
                // Prologue frame; keep draining.
            }
            Poll::Ready(Some(Err(err))) => panic!("stream error draining prologue: {err}"),
            Poll::Ready(None) => panic!("stream ended before going idle (no keep-alive possible)"),
            Poll::Pending => {
                consecutive_pending += 1;
                if consecutive_pending >= IDLE_CONFIRM_POLLS {
                    idle = true;
                    break;
                }
            }
        }
        // Let the spawned engine/converter tasks advance (no clock movement).
        tokio::task::yield_now().await;
    }

    // STEP 2 — the decisive lock. Advance the paused clock past the 15s
    // keep-alive interval, then POLL THE IDLE BODY EXACTLY ONCE. With
    // `.keep_alive(...)` configured, the elapsed `Sleep` makes axum's
    // `KeepAliveStream` yield the `:` comment immediately, so this poll is
    // `Ready`. Without it, the body has nothing to wake it and the poll is
    // `Pending` — we `panic!` at once instead of `.await`ing (which would hang
    // forever on the permanently-pending upstream → a CI timeout, not a clean
    // failure). This is the assertion that breaks if keep-alive is removed.
    if !saw_keepalive {
        assert!(idle, "body never reached the idle state to test keep-alive");
        tokio::time::advance(Duration::from_secs(16)).await;
        let mut next = std::pin::pin!(body.next());
        match poll!(next.as_mut()) {
            Poll::Ready(Some(Ok(bytes))) => {
                assert!(
                    bytes.starts_with(b":"),
                    "idle Anthropic SSE body produced a non-keepalive frame after the \
                     interval elapsed: {:?}",
                    String::from_utf8_lossy(&bytes)
                );
                saw_keepalive = true;
            }
            Poll::Ready(Some(Err(err))) => panic!("stream error after advance: {err}"),
            Poll::Ready(None) => panic!("idle stream ended without a keep-alive ping"),
            Poll::Pending => panic!(
                "idle Anthropic SSE body produced NO keep-alive ping after advancing past the \
                 interval: `.keep_alive(...)` appears removed from \
                 http.rs::stream_anthropic_response (fast deterministic failure, not a hang)"
            ),
        }
    }

    assert!(
        saw_keepalive,
        "an idle Anthropic SSE response must emit a `:` keep-alive comment \
         (axum KeepAlive in http.rs::stream_anthropic_response)"
    );
}

/// Companion sanity lock (the idle-ping test above is the STRONG behavioral
/// lock): a streaming response also advertises the keep-alive transport headers
/// set in the same http.rs builder block as `.keep_alive(...)`
/// (`Connection: keep-alive`, `X-Accel-Buffering: no`). Cheap coverage of the
/// canonical Responses path; not a substitute for observing the ping.
#[tokio::test]
async fn responses_stream_advertises_keep_alive_headers() {
    let upstream = common::MockUpstream::default();
    upstream
        .push_response(vec![Ok(common::content_chunk("chat-1", "hi"))])
        .await;
    let app = llmconduit::build_app_from_gateway(test_gateway(upstream, MockSearch::default()));

    let mut request = base_request(vec![user_message("hi")]);
    request.stream = true;
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&request).expect("serialize")))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status().as_u16(), 200);
    let headers = response.headers();
    assert_eq!(
        headers.get("connection").and_then(|v| v.to_str().ok()),
        Some("keep-alive"),
        "streaming response must advertise Connection: keep-alive"
    );
    assert_eq!(
        headers
            .get("x-accel-buffering")
            .and_then(|v| v.to_str().ok()),
        Some("no"),
        "streaming response must disable proxy buffering"
    );
}

// --------------------------------------------------------------------------
// (2) Anthropic-egress reasoning-only deferral — event-by-event ORDERING lock
// --------------------------------------------------------------------------

fn created_event() -> SseEvent {
    SseEvent {
        event: "response.created".to_string(),
        data: json!({ "type": "response.created", "response": { "id": "resp_g3" } }),
    }
}

fn reasoning_item_added_event() -> SseEvent {
    SseEvent {
        event: "response.output_item.added".to_string(),
        data: json!({
            "type": "response.output_item.added",
            "item": { "type": "reasoning", "role": "" }
        }),
    }
}

fn reasoning_delta_event(text: &str) -> SseEvent {
    SseEvent {
        event: "response.reasoning_text.delta".to_string(),
        data: json!({
            "type": "response.reasoning_text.delta",
            "delta": text,
            "content_index": 0
        }),
    }
}

fn reasoning_item_done_event() -> SseEvent {
    SseEvent {
        event: "response.output_item.done".to_string(),
        data: json!({
            "type": "response.output_item.done",
            "item": { "type": "reasoning" }
        }),
    }
}

fn completed_event() -> SseEvent {
    SseEvent {
        event: "response.completed".to_string(),
        data: json!({ "type": "response.completed", "response": { "id": "resp_g3" } }),
    }
}

/// G3 contract (Anthropic egress only) — reasoning-only@`stop` is DEFERRED then
/// PROMOTED. Feed the canonical Responses SSE for a reasoning-only turn ending
/// at a clean completion straight into `AnthropicStreamConverter` and assert,
/// event by event:
///   - NO `content_block_start` (text, thinking, or otherwise) is emitted while
///     only reasoning has arrived — i.e. before the terminal — proving the
///     reasoning is BUFFERED, not streamed; and
///   - the reasoning then appears as exactly one PROMOTED `text` block whose
///     `content_block_start` arrives at/after the terminal `response.completed`,
///     with NO `thinking` block.
///
/// FAILS IF the converter loses deferral: an impl that emitted reasoning as a
/// text/thinking block mid-stream would produce a `content_block_start` before
/// the completion event, tripping the "no content block before terminal" assert.
#[test]
fn anthropic_reasoning_only_is_buffered_until_terminal_then_promoted_to_text() {
    let mut converter = AnthropicStreamConverter::new("claude-3-7-sonnet".to_string());

    // Per-input emissions, kept separate so we can assert WHAT was emitted at
    // each step (the ordering is the contract, not just the final set).
    let after_created = converter.convert(&created_event());
    let after_item_added = converter.convert(&reasoning_item_added_event());
    let after_reasoning_1 = converter.convert(&reasoning_delta_event("The answer "));
    let after_reasoning_2 = converter.convert(&reasoning_delta_event("is 42"));
    let after_item_done = converter.convert(&reasoning_item_done_event());
    let after_completed = converter.convert(&completed_event());

    // While only reasoning has been seen, the converter must NOT open any content
    // block — the reasoning is buffered/deferred. (message_start/ping and the
    // progressive usage `message_delta`s are allowed; a `content_block_start` is
    // NOT.)
    let pre_terminal = [
        ("created", &after_created),
        ("reasoning_item_added", &after_item_added),
        ("reasoning_delta_1", &after_reasoning_1),
        ("reasoning_delta_2", &after_reasoning_2),
        ("reasoning_item_done", &after_item_done),
    ];
    for (label, batch) in pre_terminal {
        assert!(
            !batch
                .iter()
                .any(|event| matches!(event, AnthropicStreamEvent::ContentBlockStart { .. })),
            "no content block may be opened before the terminal (deferral); \
             one was opened at step `{label}`: {batch:?}"
        );
        assert!(
            !batch
                .iter()
                .any(|event| matches!(event, AnthropicStreamEvent::ContentBlockDelta { .. })),
            "no content_block_delta may be emitted before the terminal; \
             one was emitted at step `{label}`: {batch:?}"
        );
    }

    // The promotion happens AT the terminal: the completed event is where the
    // buffer is resolved into a single `text` block.
    let block_starts: Vec<&AnthropicContentBlockStart> = after_completed
        .iter()
        .filter_map(|event| match event {
            AnthropicStreamEvent::ContentBlockStart { content_block, .. } => Some(content_block),
            _ => None,
        })
        .collect();
    assert_eq!(
        block_starts.len(),
        1,
        "the terminal must open exactly one content block (the promoted text): {after_completed:?}"
    );
    assert!(
        matches!(block_starts[0], AnthropicContentBlockStart::Text { .. }),
        "reasoning-only@stop must promote to a TEXT block, not thinking/other: {after_completed:?}"
    );

    // The promoted text carries the buffered reasoning, and NO thinking block or
    // thinking delta is ever emitted across the whole stream.
    let all: Vec<AnthropicStreamEvent> = [
        after_created,
        after_item_added,
        after_reasoning_1,
        after_reasoning_2,
        after_item_done,
        after_completed,
    ]
    .into_iter()
    .flatten()
    .collect();

    let promoted_text: String = all
        .iter()
        .filter_map(|event| match event {
            AnthropicStreamEvent::ContentBlockDelta {
                delta: AnthropicDelta::TextDelta { text },
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(promoted_text, "The answer is 42");

    assert!(
        !all.iter().any(|event| matches!(
            event,
            AnthropicStreamEvent::ContentBlockStart {
                content_block: AnthropicContentBlockStart::Thinking { .. },
                ..
            }
        )),
        "promoted reasoning must NOT also emit a thinking block: {all:?}"
    );
    assert!(
        !all.iter().any(|event| matches!(
            event,
            AnthropicStreamEvent::ContentBlockDelta {
                delta: AnthropicDelta::ThinkingDelta { .. },
                ..
            }
        )),
        "no thinking_delta may be emitted for a promoted reasoning-only turn: {all:?}"
    );

    // The stream still terminates cleanly.
    assert!(
        all.iter()
            .any(|event| matches!(event, AnthropicStreamEvent::MessageStop)),
        "stream must end with message_stop: {all:?}"
    );
}
