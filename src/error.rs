use axum::Json;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use serde::Serialize;
use std::fmt;

pub type AppResult<T> = Result<T, AppError>;

/// Structured error code carried by a terminal context-overflow the CLIENT must
/// fix by shrinking its input (OpenAI wire name). The `response.failed` event
/// keeps only `message` + `code`, so every place that rebuilds an HTTP response
/// from that event keys on THIS code to restore the 400 "prompt is too long"
/// shape instead of the generic 502 (see [`AppError::from_terminal_event`]).
pub(crate) const CONTEXT_LENGTH_EXCEEDED_CODE: &str = "context_length_exceeded";

/// How the multi-provider `FailoverUpstreamClient`/routing layer should treat a
/// failed upstream attempt. This is a property of the upstream-attempt OUTCOME,
/// not a generic error policy: only the leaf upstream client decides it, and
/// only the failover loop reads it.
///
/// `Failover` (the default) retries and cools a provider. `FailoverNoCooldown`
/// retries elsewhere without penalizing a healthy provider for a request it
/// rejected. `Terminal` surfaces the error without retrying or cooling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum FailoverDisposition {
    /// Provider-failure-shaped: failover may retry on the next provider.
    #[default]
    Failover,
    /// Request-shaped failure: another provider may accept it, but this
    /// provider remains healthy and must not enter cooldown.
    FailoverNoCooldown,
    /// Same-provider terminal: surface as-is, do not fail over.
    Terminal,
}

#[derive(Debug)]
pub struct AppError {
    pub status: StatusCode,
    pub message: String,
    pub client_message: String,
    /// Optional STRUCTURED error code carried on the canonical Responses
    /// `response.failed` event (its `error.code`). `None` keeps the historical
    /// `"gateway_error"` default; a constructor like
    /// [`AppError::unknown_tool_repair_exhausted`] sets a specific machine code
    /// (e.g. `"invalid_tool_call"`) so a terminal failure is a structured event,
    /// not a raw message. The Responses converter renders it as `error.code`, and
    /// the Chat converter renders it as the OpenAI error object's `code` field;
    /// the Anthropic error shape has no `code` slot (it carries an error `type`),
    /// so there the `client_message` stays informative on its own.
    pub code: Option<String>,
    /// JSON request path associated with a client-correctable error.
    pub param: Option<String>,
    /// The failover disposition of the upstream attempt that produced this
    /// error. Generic errors carry the default (`Failover`); only the leaf
    /// upstream client promotes an error to `Terminal`.
    failover: FailoverDisposition,
}

impl AppError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        let msg = message.into();
        Self {
            status: StatusCode::BAD_REQUEST,
            client_message: msg.clone(),
            message: msg,
            code: None,
            param: None,
            failover: FailoverDisposition::default(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        let msg = message.into();
        Self {
            status: StatusCode::NOT_FOUND,
            client_message: msg.clone(),
            message: msg,
            code: None,
            param: None,
            failover: FailoverDisposition::default(),
        }
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        let msg = message.into();
        Self {
            status: StatusCode::UNAUTHORIZED,
            client_message: msg.clone(),
            message: msg,
            code: Some("invalid_api_key".to_string()),
            param: None,
            failover: FailoverDisposition::Terminal,
        }
    }

    pub fn unsupported_media_type(message: impl Into<String>) -> Self {
        let msg = message.into();
        Self {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            client_message: msg.clone(),
            message: msg,
            code: Some("unsupported_media_type".to_string()),
            param: None,
            failover: FailoverDisposition::Terminal,
        }
    }

    pub fn payload_too_large(limit_bytes: usize) -> Self {
        let msg = format!("request body exceeds the {limit_bytes}-byte limit");
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            client_message: msg.clone(),
            message: msg,
            code: Some("request_too_large".to_string()),
            param: None,
            failover: FailoverDisposition::Terminal,
        }
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        let msg = message.into();
        Self {
            status: StatusCode::CONFLICT,
            client_message: msg.clone(),
            message: msg,
            code: None,
            param: None,
            failover: FailoverDisposition::default(),
        }
    }

    pub fn upstream(message: impl Into<String>) -> Self {
        let msg = message.into();
        Self {
            status: StatusCode::BAD_GATEWAY,
            client_message: "the upstream request failed".to_string(),
            message: msg,
            code: None,
            param: None,
            failover: FailoverDisposition::default(),
        }
    }

    pub fn gateway_timeout(message: impl Into<String>) -> Self {
        let msg = message.into();
        Self {
            status: StatusCode::GATEWAY_TIMEOUT,
            client_message: "the upstream response timed out".to_string(),
            message: msg,
            code: Some("upstream_timeout".to_string()),
            param: None,
            failover: FailoverDisposition::default(),
        }
    }

    /// E1: the upstream model returned a tool call whose name was NOT in the
    /// offered tool set and could not self-correct within the bounded in-gateway
    /// repair ceiling. Surfaces as a STRUCTURED terminal `response.failed`
    /// (code `invalid_tool_call`) — NOT a raw mid-stream `?` abort — so all three
    /// inbound converters render a clean terminal frame. The client message is
    /// deliberately generic (it does not echo the hallucinated tool name; the
    /// operator gets that via the `tracing::warn!` + monitor phase).
    pub fn unknown_tool_repair_exhausted() -> Self {
        Self::upstream(
            "the model requested a tool that is not available and could not recover; \
             the request was ended without completing the tool call",
        )
        .with_code("invalid_tool_call")
    }

    /// Attach a structured [`code`](Self::code) for the canonical `response.failed`
    /// event. Builder form so a constructor can tag a specific machine code
    /// without widening every call site.
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    pub fn with_param(mut self, param: impl Into<String>) -> Self {
        self.param = Some(param.into());
        self
    }

    pub fn unsupported_parameter(param: impl Into<String>) -> Self {
        let param = param.into();
        Self::bad_request(format!("unsupported parameter: {param}"))
            .with_code("unsupported_parameter")
            .with_param(param)
    }

    /// An upstream error tagged with an explicit failover disposition. The leaf
    /// upstream client uses this to mark a context-window overflow that survived
    /// its shrink-and-retry as `Terminal` so failover/routing surfaces it
    /// instead of retrying the same oversized prompt on another provider.
    pub(crate) fn upstream_with_disposition(
        message: impl Into<String>,
        disposition: FailoverDisposition,
    ) -> Self {
        Self {
            failover: disposition,
            ..Self::upstream(message)
        }
    }

    /// A terminal context-window overflow the CLIENT must resolve by shrinking
    /// its input: the prompt (plus any completion budget worth sending) cannot
    /// fit the upstream window, so no gateway-side retry, failover, or "try
    /// again in a moment" can help. Served as HTTP 400 with a message that
    /// leads with the Anthropic-style `prompt is too long` phrase — clients
    /// like Claude Code key on that shape to engage their own trim/compaction
    /// fallbacks instead of hammering a "temporary" 502 — and carries the
    /// OpenAI-style structured code for the Chat/Responses surfaces.
    pub(crate) fn prompt_too_long(message: impl Into<String>) -> Self {
        let msg = message.into();
        Self {
            status: StatusCode::BAD_REQUEST,
            client_message:
                "prompt is too long for the selected model; reduce the input or output limit"
                    .to_string(),
            message: msg,
            code: Some(CONTEXT_LENGTH_EXCEEDED_CODE.to_string()),
            param: None,
            failover: FailoverDisposition::Terminal,
        }
    }

    /// Rebuild an [`AppError`] from a canonical `response.failed` event's
    /// `message` + structured `code`. The event does not carry the original
    /// HTTP status, so the collectors historically collapsed every terminal to
    /// a 502 — turning a permanent "your prompt cannot fit" into a
    /// "temporary, try again" that clients hammer. The context-overflow code
    /// restores the 400 `prompt is too long` shape; everything else keeps the
    /// 502 upstream default.
    pub(crate) fn from_terminal_event(
        message: &str,
        code: Option<&str>,
        status: Option<u16>,
        param: Option<&str>,
    ) -> Self {
        let mut error = match code {
            Some(CONTEXT_LENGTH_EXCEEDED_CODE) => Self::prompt_too_long(message),
            Some("internal_error") => Self::internal(message),
            Some("upstream_timeout") => Self::gateway_timeout(message),
            Some("rate_limit_exceeded") => Self {
                status: StatusCode::TOO_MANY_REQUESTS,
                client_message: message.to_string(),
                message: message.to_string(),
                code: Some("rate_limit_exceeded".to_string()),
                param: None,
                failover: FailoverDisposition::Terminal,
            },
            Some("request_too_large") => Self {
                status: StatusCode::PAYLOAD_TOO_LARGE,
                client_message: message.to_string(),
                message: message.to_string(),
                code: Some("request_too_large".to_string()),
                param: None,
                failover: FailoverDisposition::Terminal,
            },
            Some("unsupported_media_type") => Self::unsupported_media_type(message.to_string()),
            Some("unprocessable_entity") => Self {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                client_message: message.to_string(),
                message: message.to_string(),
                code: Some("unprocessable_entity".to_string()),
                param: None,
                failover: FailoverDisposition::Terminal,
            },
            Some("invalid_request_error") => {
                Self::bad_request(message.to_string()).with_code("invalid_request_error")
            }
            Some(code) => Self::upstream(message).with_code(code),
            None => Self::upstream(message),
        };
        if let Some(status) = status.and_then(|status| StatusCode::from_u16(status).ok()) {
            error.status = status;
            error.failover = FailoverDisposition::Terminal;
        }
        error.param = param.map(str::to_string);
        error
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
            client_message: "internal server error".to_string(),
            code: Some("internal_error".to_string()),
            param: None,
            failover: FailoverDisposition::default(),
        }
    }

    pub fn cancelled() -> Self {
        Self {
            status: StatusCode::from_u16(499).expect("valid status code"),
            message: "client disconnected".to_string(),
            client_message: "client disconnected".to_string(),
            code: None,
            param: None,
            failover: FailoverDisposition::default(),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.status == StatusCode::from_u16(499).expect("valid status code")
    }

    pub fn status_code(&self) -> StatusCode {
        self.status
    }

    /// The failover disposition of the upstream attempt that produced this error.
    /// The failover loop matches on this to decide whether to retry the next
    /// provider (`Failover`) or surface the error terminally (`Terminal`).
    pub(crate) fn failover_disposition(&self) -> FailoverDisposition {
        self.failover
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for AppError {}

#[derive(Debug, Serialize)]
struct ErrorBody<'a> {
    error: ErrorPayload<'a>,
}

#[derive(Debug, Serialize)]
struct ErrorPayload<'a> {
    message: &'a str,
    #[serde(rename = "type")]
    kind: &'a str,
    param: Option<&'a str>,
    code: Option<&'a str>,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let detail = bounded_operator_detail(&self.message, 2048);
        tracing::error!(status = %self.status, detail = %detail, "request error");
        let status = self.status_code();
        let body = ErrorBody {
            error: ErrorPayload {
                message: &self.client_message,
                kind: if self.status.is_client_error() {
                    "invalid_request_error"
                } else {
                    "server_error"
                },
                param: self.param.as_deref(),
                code: self.code.as_deref(),
            },
        };
        (status, Json(body)).into_response()
    }
}

fn bounded_operator_detail(message: &str, max_chars: usize) -> String {
    let redacted = crate::redaction::redact_image_uris(message);
    if redacted.chars().count() <= max_chars {
        return redacted;
    }
    let end = redacted
        .char_indices()
        .nth(max_chars)
        .map(|(index, _)| index)
        .unwrap_or(redacted.len());
    format!("{}…[truncated]", &redacted[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    async fn response_body_string(error: AppError) -> String {
        let response = error.into_response();
        let body = response.into_body();
        let bytes = body.collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn test_internal_error_hides_detail() {
        let body = response_body_string(AppError::internal("secret detail")).await;
        assert!(body.contains("internal server error"));
        assert!(!body.contains("secret detail"));
    }

    #[tokio::test]
    async fn test_bad_request_shows_detail() {
        let body = response_body_string(AppError::bad_request("invalid field X")).await;
        assert!(body.contains("invalid field X"));
    }

    #[tokio::test]
    async fn test_upstream_error_hides_operator_detail() {
        let body = response_body_string(AppError::upstream("provider returned 500: oops")).await;
        assert!(body.contains("the upstream request failed"));
        assert!(!body.contains("provider returned 500: oops"));
    }

    #[test]
    fn terminal_event_codes_restore_nonstream_http_statuses() {
        for (code, expected, expected_message) in [
            (
                "invalid_request_error",
                StatusCode::BAD_REQUEST,
                "sanitized",
            ),
            (
                "request_too_large",
                StatusCode::PAYLOAD_TOO_LARGE,
                "sanitized",
            ),
            (
                "unsupported_media_type",
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "sanitized",
            ),
            (
                "unprocessable_entity",
                StatusCode::UNPROCESSABLE_ENTITY,
                "sanitized",
            ),
            (
                "upstream_timeout",
                StatusCode::GATEWAY_TIMEOUT,
                "the upstream response timed out",
            ),
            (
                "internal_error",
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal server error",
            ),
            (
                "rate_limit_exceeded",
                StatusCode::TOO_MANY_REQUESTS,
                "sanitized",
            ),
            (
                "upstream_authentication_error",
                StatusCode::BAD_GATEWAY,
                "the upstream request failed",
            ),
            (
                "upstream_error",
                StatusCode::BAD_GATEWAY,
                "the upstream request failed",
            ),
        ] {
            let error = AppError::from_terminal_event("sanitized", Some(code), None, None);
            assert_eq!(error.status, expected, "code={code}");
            assert_eq!(error.client_message, expected_message);
            assert_eq!(error.code.as_deref(), Some(code));
        }
    }

    // Disposition equivalence vs the old `failover_eligible: bool`. The previous
    // representation had EVERY constructor failover-eligible (`true`) except the
    // terminal one (`false`). The typed disposition must reproduce that exact
    // truth table: every generic constructor defaults to `Failover`, and only
    // the explicit-disposition constructor with `Terminal` is non-failover.
    #[test]
    fn generic_constructors_default_to_failover_disposition() {
        let cases = [
            AppError::bad_request("x"),
            AppError::conflict("x"),
            AppError::upstream("x"),
            AppError::internal("x"),
            AppError::cancelled(),
            // An upstream error explicitly tagged `Failover` stays eligible.
            AppError::upstream_with_disposition("x", FailoverDisposition::Failover),
        ];
        for error in cases {
            assert_eq!(
                error.failover_disposition(),
                FailoverDisposition::Failover,
                "generic/explicit-failover errors must remain failover-eligible \
                 (status {})",
                error.status
            );
        }
    }

    #[test]
    fn unknown_tool_repair_exhausted_carries_structured_code() {
        // E1: the bounded-repair terminal is a 502 upstream error tagged with a
        // structured `invalid_tool_call` code (rendered on the canonical
        // `response.failed`), and its client message must NOT echo any tool name.
        let error = AppError::unknown_tool_repair_exhausted();
        assert_eq!(error.status, StatusCode::BAD_GATEWAY);
        assert_eq!(error.code.as_deref(), Some("invalid_tool_call"));
        assert!(error.failover_disposition() == FailoverDisposition::Failover);
        assert!(!error.client_message.is_empty());
    }

    #[test]
    fn with_code_sets_structured_code_and_generic_constructors_have_none() {
        assert_eq!(AppError::upstream("x").code, None);
        assert_eq!(
            AppError::upstream("x").with_code("my_code").code.as_deref(),
            Some("my_code")
        );
    }

    #[test]
    fn upstream_terminal_disposition_is_not_failover() {
        let error = AppError::upstream_with_disposition("overflow", FailoverDisposition::Terminal);
        assert_eq!(error.failover_disposition(), FailoverDisposition::Terminal);
        // It is still a 502 upstream error in every other respect; only the
        // disposition differs from a plain `upstream(...)`.
        assert_eq!(error.status, StatusCode::BAD_GATEWAY);
    }
}
