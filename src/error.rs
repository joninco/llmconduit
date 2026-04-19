use axum::Json;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use serde::Serialize;
use std::fmt;

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug)]
pub struct AppError {
    pub status: StatusCode,
    pub message: String,
    pub client_message: String,
}

impl AppError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        let msg = message.into();
        Self {
            status: StatusCode::BAD_REQUEST,
            client_message: msg.clone(),
            message: msg,
        }
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        let msg = message.into();
        Self {
            status: StatusCode::CONFLICT,
            client_message: msg.clone(),
            message: msg,
        }
    }

    pub fn upstream(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: message.into(),
            client_message: "upstream error".to_string(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
            client_message: "internal server error".to_string(),
        }
    }

    pub fn status_code(&self) -> StatusCode {
        self.status
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
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::error!(status = %self.status, detail = %self.message, "request error");
        let status = self.status_code();
        let body = ErrorBody {
            error: ErrorPayload {
                message: &self.client_message,
            },
        };
        (status, Json(body)).into_response()
    }
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
    async fn test_upstream_error_hides_detail() {
        let body =
            response_body_string(AppError::upstream("provider returned 500: {body}")).await;
        assert!(body.contains("upstream error"));
        assert!(!body.contains("provider returned 500"));
    }
}
