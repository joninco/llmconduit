use crate::engine::Gateway;
use crate::error::AppError;
use crate::error::AppResult;
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

pub fn build_router(gateway: Arc<Gateway>) -> Router {
    Router::new()
        .route("/v1/responses", post(post_responses))
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
