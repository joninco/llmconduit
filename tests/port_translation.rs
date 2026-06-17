//! Ported surface: request-translation (claude-relay test_convert_request.py).
//!
//! claude-relay translated inbound requests into OpenAI **Chat** shape; llmconduit
//! translates into the canonical **Responses** shape. So each behavior is ported
//! at the contract level, not the literal output shape:
//!   - "system merged into a system string"  ->  Anthropic: system lands in
//!     `instructions`; Chat: system stays a system-role item in `input`.
//!   - "tool_choice any -> required"          ->  asserted against llmconduit's
//!     own tool_choice mapping (see port_tools coverage).
//!
//! Pure-function tests: they call the adapter `convert_request` directly -- no
//! gateway/mock needed, so this file does not pull in the shared `common` lever.

use llmconduit::adapters::anthropic_to_responses;
use llmconduit::adapters::chat_completions;
use llmconduit::models::anthropic::AnthropicRequest;
use llmconduit::models::chat::ChatCompletionRequest;
use serde_json::json;

fn anthropic(value: serde_json::Value) -> AnthropicRequest {
    serde_json::from_value(value).expect("valid AnthropicRequest")
}

fn chat(value: serde_json::Value) -> ChatCompletionRequest {
    serde_json::from_value(value).expect("valid ChatCompletionRequest")
}

// ===========================================================================
// Anthropic Messages -> Responses
// ===========================================================================

/// claude-relay rejected unsupported sampling knobs; llmconduit 400s on top_k.
#[test]
fn anthropic_top_k_is_rejected() {
    let err = anthropic_to_responses::convert_request(anthropic(json!({
        "model": "claude-3",
        "max_tokens": 64,
        "top_k": 40,
        "messages": [{"role": "user", "content": "hi"}],
    })))
    .expect_err("top_k must be rejected");
    assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
}

/// test_system_role / test_multiple_system_messages: system content is surfaced.
/// llmconduit lifts the system string into canonical `instructions`.
#[test]
fn anthropic_system_string_becomes_instructions() {
    let result = anthropic_to_responses::convert_request(anthropic(json!({
        "model": "claude-3",
        "max_tokens": 64,
        "system": "You are a helpful assistant.",
        "messages": [{"role": "user", "content": "hi"}],
    })))
    .expect("convert");
    assert!(
        result.instructions.contains("You are a helpful assistant."),
        "system text should land in instructions, got {:?}",
        result.instructions
    );
}

/// test_system_message ... list content: system text blocks are joined into one
/// instruction string (claude-relay collapsed list content to a string).
#[test]
fn anthropic_system_blocks_join_into_instructions() {
    let result = anthropic_to_responses::convert_request(anthropic(json!({
        "model": "claude-3",
        "max_tokens": 64,
        "system": [
            {"type": "text", "text": "First rule."},
            {"type": "text", "text": "Second rule."}
        ],
        "messages": [{"role": "user", "content": "hi"}],
    })))
    .expect("convert");
    assert!(result.instructions.contains("First rule."));
    assert!(result.instructions.contains("Second rule."));
}

/// stop_sequences are vendor-specific in Responses -> routed through extra_body.
#[test]
fn anthropic_stop_sequences_move_to_extra_body() {
    let result = anthropic_to_responses::convert_request(anthropic(json!({
        "model": "claude-3",
        "max_tokens": 64,
        "stop_sequences": ["STOP", "END"],
        "messages": [{"role": "user", "content": "hi"}],
    })))
    .expect("convert");
    assert_eq!(result.extra_body.get("stop"), Some(&json!(["STOP", "END"])));
}

#[test]
fn anthropic_max_tokens_maps_to_max_output_tokens() {
    let result = anthropic_to_responses::convert_request(anthropic(json!({
        "model": "claude-3",
        "max_tokens": 256,
        "messages": [{"role": "user", "content": "hi"}],
    })))
    .expect("convert");
    assert_eq!(result.max_output_tokens, Some(256));
}

/// Hard rule: parallel_tool_calls is forced false regardless of caller input
/// (AGENTS.md engine.rs:707-726). Anthropic conversion sets it false up front.
#[test]
fn anthropic_forces_parallel_tool_calls_false() {
    let result = anthropic_to_responses::convert_request(anthropic(json!({
        "model": "claude-3",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hi"}],
    })))
    .expect("convert");
    assert!(!result.parallel_tool_calls);
}

/// llmconduit's output_config carries structured-output `format`, not effort.
/// A json_schema format must survive into canonical `text` controls.
#[test]
fn anthropic_output_config_json_schema_maps_to_text() {
    let result = anthropic_to_responses::convert_request(anthropic(json!({
        "model": "claude-3",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hi"}],
        "output_config": {
            "format": {
                "type": "json_schema",
                "name": "answer",
                "schema": {"type": "object", "properties": {}}
            }
        }
    })))
    .expect("convert");
    let text =
        serde_json::to_value(result.text.expect("text controls present")).expect("serialize");
    assert_eq!(text["format"]["type"], "json_schema");
    assert_eq!(text["format"]["name"], "answer");
}

/// Non-json_schema structured-output formats are not supported -> 400.
#[test]
fn anthropic_output_config_non_json_schema_rejected() {
    let err = anthropic_to_responses::convert_request(anthropic(json!({
        "model": "claude-3",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hi"}],
        "output_config": {"format": {"type": "text"}}
    })))
    .expect_err("non json_schema format must be rejected");
    assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
}

/// output_config without a `format` key is a no-op (no text controls emitted).
#[test]
fn anthropic_output_config_without_format_is_noop() {
    let result = anthropic_to_responses::convert_request(anthropic(json!({
        "model": "claude-3",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hi"}],
        "output_config": {"effort": "high"}
    })))
    .expect("convert");
    assert!(result.text.is_none());
}

/// GAP (partial): claude-relay mapped output_config.effort + adaptive thinking
/// onto chat_template_kwargs.reasoning_effort. llmconduit derives effort from
/// `thinking`, and treats output_config purely as structured-output `format`.
/// There is no effort path through output_config.
#[test]
#[ignore = "GAP: request-translation/output_config_effort_to_reasoning_effort"]
fn anthropic_output_config_effort_maps_to_reasoning_effort() {
    let result = anthropic_to_responses::convert_request(anthropic(json!({
        "model": "claude-3",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hi"}],
        "thinking": {"type": "adaptive"},
        "output_config": {"effort": "high"}
    })))
    .expect("convert");
    let effort = result.reasoning.and_then(|r| r.effort);
    assert_eq!(effort.as_deref(), Some("high"));
}

// ===========================================================================
// OpenAI Chat Completions -> Responses
// ===========================================================================

/// Chat conversion never populates `instructions`; system context is carried as
/// a system-role item in `input` and hoisted later during lowering.
#[test]
fn chat_instructions_always_empty_system_stays_in_input() {
    let result = chat_completions::convert_request(chat(json!({
        "model": "glm-5.1",
        "messages": [
            {"role": "system", "content": "Be concise."},
            {"role": "user", "content": "hi"}
        ],
    })))
    .expect("convert");
    assert_eq!(result.instructions, "");
    let input = serde_json::to_value(&result.input).expect("serialize input");
    assert_eq!(input[0]["role"], "system");
}

#[test]
fn chat_reasoning_effort_maps_to_reasoning() {
    let result = chat_completions::convert_request(chat(json!({
        "model": "glm-5.1",
        "messages": [{"role": "user", "content": "hi"}],
        "reasoning_effort": "high",
    })))
    .expect("convert");
    assert_eq!(
        result.reasoning.and_then(|r| r.effort).as_deref(),
        Some("high")
    );
}

#[test]
fn chat_tool_choice_defaults_to_auto() {
    let result = chat_completions::convert_request(chat(json!({
        "model": "glm-5.1",
        "messages": [{"role": "user", "content": "hi"}],
    })))
    .expect("convert");
    assert_eq!(result.tool_choice, json!("auto"));
}

/// extra_body is the catch-all: an unknown provider knob round-trips untyped.
#[test]
fn chat_unknown_knob_round_trips_through_extra_body() {
    let result = chat_completions::convert_request(chat(json!({
        "model": "glm-5.1",
        "messages": [{"role": "user", "content": "hi"}],
        "provider_specific_knob": {"nested": 42},
    })))
    .expect("convert");
    assert_eq!(
        result.extra_body.get("provider_specific_knob"),
        Some(&json!({"nested": 42}))
    );
}
