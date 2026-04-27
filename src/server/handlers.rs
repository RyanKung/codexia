//! Route handlers and streaming response helpers for the HTTP server.

use crate::{
    error::Result,
    openai::types::ChatCompletionRequest,
    server::{AppState, auth::authorize, status_response::build_status_response},
};
use axum::{
    Json,
    extract::State,
    http::HeaderMap,
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use futures_util::{Stream, StreamExt, stream};
use serde_json::json;
use std::{convert::Infallible, pin::Pin};

/// Lightweight healthcheck for the local service.
pub async fn health() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

/// Returns the configured model list after optional local API key validation.
pub async fn models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match authorize(&headers, state.api_key.as_deref()) {
        Ok(()) => Json(state.models).into_response(),
        Err(error) => error.into_response(),
    }
}

/// Refreshes the saved OAuth credentials without exposing the raw tokens.
pub async fn manual_refresh(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return error.into_response();
    }

    match state.token_manager.refresh().await {
        Ok(credentials) => Json(json!({
            "account_id": credentials.account_id,
            "expires_at": credentials.expires_at,
        }))
        .into_response(),
        Err(error) => error.into_response(),
    }
}

/// Returns account, token, and rate-limit status in a structured JSON format.
pub async fn status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return error.into_response();
    }

    let credentials = match state.token_manager.credentials().await {
        Ok(credentials) => credentials,
        Err(error) => return error.into_response(),
    };

    let snapshot = state.status.fetch_status(&credentials).await;
    Json(build_status_response(&credentials, &snapshot)).into_response()
}

/// Proxies OpenAI-compatible chat completion requests to the Codex backend.
pub async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ChatCompletionRequest>,
) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return error.into_response();
    }

    let credentials = match state.token_manager.credentials().await {
        Ok(credentials) => credentials,
        Err(error) => return error.into_response(),
    };

    if request.wants_stream() {
        match state.codex.stream_chat(request, &credentials).await {
            Ok(stream) => sse_response(stream).into_response(),
            Err(error) => error.into_response(),
        }
    } else {
        match state.codex.complete_chat(request, &credentials).await {
            Ok(response) => Json(response).into_response(),
            Err(error) => error.into_response(),
        }
    }
}

fn sse_response(
    stream: Pin<
        Box<dyn Stream<Item = Result<crate::openai::response::ChatCompletionChunk>> + Send>,
    >,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>>> {
    let mapped = stream.map(|item| {
        let event = match item {
            Ok(chunk) => Event::default().data(serde_json::to_string(&chunk).unwrap_or_default()),
            Err(error) => Event::default().data(
                json!({"error": {"message": error.to_string(), "type": "upstream_error"}})
                    .to_string(),
            ),
        };
        Ok(event)
    });

    let done = stream::once(async { Ok(Event::default().data("[DONE]")) });
    Sse::new(mapped.chain(done)).keep_alive(KeepAlive::default())
}
