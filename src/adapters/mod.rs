pub mod anthropic_to_responses;
pub mod chat_completions;
pub mod chat_to_responses;
pub mod codex_private_responses;
pub mod responses_to_anthropic;
pub mod responses_to_chat;

use serde_json::Value;

/// Gateway-only terminal metadata carried alongside a canonical
/// `response.failed`. It is intentionally outside the public Response resource
/// and is removed by the raw Responses projection. Adapters retain only a
/// bounded, valid HTTP status and JSON-parameter path so an internal error
/// cannot create unbounded collector state or leak arbitrary provider text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CanonicalErrorMetadata {
    pub status: Option<u16>,
    pub param: Option<String>,
    pub code: Option<String>,
    pub retry_after_secs: Option<u64>,
}

pub(crate) fn canonical_error_metadata(data: &Value) -> CanonicalErrorMetadata {
    const MAX_PARAM_BYTES: usize = 256;
    const MAX_CODE_BYTES: usize = 128;

    let status = data
        .get("llmconduit_error_status")
        .and_then(Value::as_u64)
        .and_then(|status| u16::try_from(status).ok())
        .filter(|status| {
            http::StatusCode::from_u16(*status)
                .is_ok_and(|status| status.is_client_error() || status.is_server_error())
        });
    let param = data
        .get("llmconduit_error_param")
        .and_then(Value::as_str)
        .filter(|param| !param.is_empty() && param.len() <= MAX_PARAM_BYTES)
        .filter(|param| {
            param
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '[' | ']'))
        })
        .map(str::to_string);
    let code = data
        .pointer("/response/error/code")
        .and_then(Value::as_str)
        .filter(|code| !code.is_empty() && code.len() <= MAX_CODE_BYTES)
        .filter(|code| {
            code.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
        })
        .map(str::to_string);
    let retry_after_secs = data
        .get("llmconduit_retry_after_secs")
        .and_then(Value::as_u64)
        .filter(|seconds| (1..=300).contains(seconds));
    CanonicalErrorMetadata {
        status,
        param,
        code,
        retry_after_secs,
    }
}

#[cfg(test)]
mod error_metadata_tests {
    use super::CanonicalErrorMetadata;
    use super::canonical_error_metadata;
    use serde_json::json;

    #[test]
    fn canonical_error_metadata_accepts_only_bounded_status_and_parameter() {
        assert_eq!(
            canonical_error_metadata(&json!({
                "response": {"error": {"code": "turn_state_conflict"}},
                "llmconduit_error_status": 409,
                "llmconduit_error_param": "input[2].content"
            })),
            CanonicalErrorMetadata {
                status: Some(409),
                param: Some("input[2].content".to_string()),
                code: Some("turn_state_conflict".to_string()),
                retry_after_secs: None,
            }
        );
        assert_eq!(
            canonical_error_metadata(&json!({
                "llmconduit_error_status": 99,
                "llmconduit_error_param": "x".repeat(257)
            })),
            CanonicalErrorMetadata::default()
        );
        assert_eq!(
            canonical_error_metadata(&json!({
                "llmconduit_error_status": 200,
                "llmconduit_error_param": "input;authorization"
            })),
            CanonicalErrorMetadata::default()
        );
    }
}
