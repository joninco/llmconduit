use crate::adapters::anthropic_to_responses;
use crate::adapters::responses_to_anthropic::AnthropicStreamCollector;
use crate::adapters::responses_to_anthropic::AnthropicStreamConverter;
use crate::engine::Gateway;
use crate::error::AppError;
use crate::error::AppResult;
use crate::models::anthropic::AnthropicRequest;
use crate::models::responses::ResponsesRequest;
use crate::upstream::collect_models_response;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderName;
use axum::http::HeaderValue;
use axum::http::header;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::response::Sse;
use axum::routing::get;
use axum::routing::post;
use futures::StreamExt;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

pub fn build_router(gateway: Arc<Gateway>) -> Router {
    Router::new()
        .route("/v1/responses", post(post_responses))
        .route("/v1/messages", post(post_messages))
        .route("/v1/models", get(get_models))
        .with_state(gateway)
}

async fn post_responses(
    State(gateway): State<Arc<Gateway>>,
    Json(request): Json<ResponsesRequest>,
) -> AppResult<Response> {
    let stream = gateway.stream_responses(request).await?;
    let mapped = stream.map(|event| {
        Ok::<_, Infallible>(
            axum::response::sse::Event::default()
                .event(event.event)
                .data(event.data.to_string()),
        )
    });
    let mut response = Sse::new(mapped)
        .keep_alive(axum::response::sse::KeepAlive::new())
        .into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    response.headers_mut().insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    Ok(response)
}

async fn post_messages(
    State(gateway): State<Arc<Gateway>>,
    Json(request): Json<AnthropicRequest>,
) -> Response {
    match handle_post_messages(gateway, request).await {
        Ok(response) => response,
        Err(err) => anthropic_error_response(err),
    }
}

async fn handle_post_messages(
    gateway: Arc<Gateway>,
    request: AnthropicRequest,
) -> AppResult<Response> {
    let model = request.model.clone();
    let wants_stream = request.stream;
    let responses_request = anthropic_to_responses::convert_request(request)?;
    let stream = gateway.stream_responses(responses_request).await?;

    if wants_stream {
        stream_anthropic_response(model, stream)
    } else {
        collect_anthropic_response(model, stream).await
    }
}

fn stream_anthropic_response(
    model: String,
    stream: ReceiverStream<crate::engine::SseEvent>,
) -> AppResult<Response> {
    let (tx, rx) = mpsc::channel(128);
    tokio::spawn(async move {
        let mut converter = AnthropicStreamConverter::new(model);
        let mut stream = std::pin::pin!(stream);
        while let Some(event) = stream.next().await {
            let anthropic_events = converter.convert(&event);
            for anthropic_event in anthropic_events {
                if tx.send(anthropic_event).await.is_err() {
                    break;
                }
            }
        }
    });

    let mapped = ReceiverStream::new(rx).map(|event| {
        Ok::<_, Infallible>(
            axum::response::sse::Event::default()
                .event(event.sse_event_type())
                .data(event.to_json()),
        )
    });

    let mut response = Sse::new(mapped)
        .keep_alive(axum::response::sse::KeepAlive::new())
        .into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    response.headers_mut().insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    Ok(response)
}

async fn collect_anthropic_response(
    model: String,
    stream: ReceiverStream<crate::engine::SseEvent>,
) -> AppResult<Response> {
    let mut collector = AnthropicStreamCollector::new(model);
    let mut stream = std::pin::pin!(stream);
    while let Some(event) = stream.next().await {
        collector.process(&event);
    }
    match collector.into_response() {
        Ok(msg) => Ok(Json(msg).into_response()),
        Err(err) => Ok(anthropic_error_response(AppError::upstream(err.message))),
    }
}

fn anthropic_error_response(err: AppError) -> Response {
    let status = err.status_code();
    let error_type = match err.status_code() {
        axum::http::StatusCode::BAD_REQUEST => "invalid_request_error",
        axum::http::StatusCode::CONFLICT => "invalid_request_error",
        _ => "api_error",
    };
    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": error_type,
            "message": err.to_string(),
        }
    });
    (status, Json(body)).into_response()
}

async fn get_models(State(gateway): State<Arc<Gateway>>) -> AppResult<Response> {
    let response = gateway.upstream_client().list_models().await?;
    let (status, body, etag) = collect_models_response(response).await?;
    let mut headers = HeaderMap::new();
    if let Some(etag) = etag {
        headers.insert(
            http::header::ETAG,
            HeaderValue::from_str(&etag)
                .map_err(|err| AppError::internal(format!("invalid ETag header: {err}")))?,
        );
    }
    Ok((status, headers, Json(body)).into_response())
}
