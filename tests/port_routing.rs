//! Ported surface: backend-routing (claude-relay test_backend.py).
//!
//! claude-relay injected per-model `model_sampling` defaults and let client
//! params override them. llmconduit's equivalent contract (AGENTS.md): explicit
//! request fields WIN over configured `upstream_chat_kwargs`, and typed fields
//! remove the conflicting default key rather than double-sending it.
//!
//! The model-family reshaping behaviors (Kimi/DeepSeek) are a whole gap (G2 in
//! GAPS.md) and are recorded as `#[ignore]` near-miss stubs below.
//!
//! These tests drive the full gateway with `MockUpstream` and inspect the
//! serialized upstream request body, so the effective wire value is asserted
//! regardless of whether it lands in a typed field or flattened `extra_body`.

mod common;

use common::MockSearch;
use common::MockUpstream;
use common::base_request;
use common::collect_stream;
use common::content_chunk;
use common::test_config;
use common::test_gateway_with_config;
use common::usage_chunk;
use common::user_message;
use serde_json::json;

/// Build a config whose configured upstream default sets temperature = 0.1.
fn config_with_default_temperature() -> llmconduit::config::Config {
    let mut config = test_config();
    let mut kwargs = serde_json::Map::new();
    kwargs.insert("temperature".to_string(), json!(0.1));
    config.upstream_chat_kwargs = kwargs;
    config
}

async fn run_and_capture_upstream_body(
    config: llmconduit::config::Config,
    request: llmconduit::models::responses::ResponsesRequest,
) -> serde_json::Value {
    let upstream = MockUpstream::default();
    upstream
        .push_response(vec![
            Ok(content_chunk("chat-1", "hi")),
            Ok(usage_chunk("chat-1", 5, 1, 6)),
        ])
        .await;
    let gateway = test_gateway_with_config(upstream.clone(), MockSearch::default(), config);
    let _ = collect_stream(gateway.stream_responses(request).await.expect("stream")).await;
    let requests = upstream.requests().await;
    assert_eq!(requests.len(), 1, "exactly one upstream request");
    serde_json::to_value(&requests[0]).expect("serialize upstream request")
}

/// test_send_to_backend_client_params_beat_sampling_defaults:
/// an explicit request temperature overrides the configured default.
#[tokio::test]
async fn explicit_request_temperature_beats_configured_default() {
    let mut request = base_request(vec![user_message("hello")]);
    request.temperature = Some(0.9);

    let body = run_and_capture_upstream_body(config_with_default_temperature(), request).await;

    assert_eq!(
        body["temperature"],
        json!(0.9),
        "explicit request temperature must win over the configured 0.1 default"
    );
}

/// test_send_to_backend_injects_sampling_from_detected_model:
/// when the request omits temperature, the configured default is applied.
#[tokio::test]
async fn configured_default_temperature_applies_when_request_omits_it() {
    let mut request = base_request(vec![user_message("hello")]);
    request.temperature = None;

    let body = run_and_capture_upstream_body(config_with_default_temperature(), request).await;

    assert_eq!(
        body["temperature"],
        json!(0.1),
        "configured default temperature should flow upstream when request omits it"
    );
}

// ---------------------------------------------------------------------------
// GAP G2 — backend model-family reshaping (Kimi / DeepSeek). llmconduit has
// partial Kimi handling (sentinel cleanup, nested-thinking parsing) but no
// automatic family detection + family-specific chat_template_kwargs injection.
// ---------------------------------------------------------------------------

/// test_kimi_thinking_always_enabled_even_when_client_inactive:
/// a Kimi-family backend should always send thinking=true to stop reasoning
/// leakage. llmconduit injects no family-specific kwargs.
#[tokio::test]
#[ignore = "GAP: backend-routing/kimi_family_forces_thinking_kwargs"]
async fn kimi_backend_forces_thinking_kwargs() {
    let request = base_request(vec![user_message("hello")]);
    let body = run_and_capture_upstream_body(test_config(), request).await;
    assert_eq!(body["chat_template_kwargs"]["thinking"], json!(true));
}

/// test_deepseek_thinking_active_regression:
/// a DeepSeek-family backend should inject enable_thinking + reasoning_effort.
#[tokio::test]
#[ignore = "GAP: backend-routing/deepseek_family_injects_enable_thinking"]
async fn deepseek_backend_injects_enable_thinking() {
    let request = base_request(vec![user_message("hello")]);
    let body = run_and_capture_upstream_body(test_config(), request).await;
    assert_eq!(body["chat_template_kwargs"]["enable_thinking"], json!(true));
}
