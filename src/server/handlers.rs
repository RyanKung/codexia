//! Route handlers and streaming response helpers for the HTTP server.

use crate::{
    anthropic::{
        CountTokensResponse, MessagesRequest, content_block_stop, error_body,
        estimate_input_tokens, from_openai_response, message_delta_event, message_start_event,
        message_stop_event, text_block_start, text_delta, to_openai_request, tool_block_start,
        tool_json_delta,
    },
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

/// Anthropic-compatible Messages API handler used by Claude SDKs and Claude Code.
pub async fn messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<MessagesRequest>,
) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return anthropic_error_response(error);
    }

    let credentials = match state.token_manager.credentials().await {
        Ok(credentials) => credentials,
        Err(error) => return anthropic_error_response(error),
    };
    let openai_request = match to_openai_request(&request) {
        Ok(request) => request,
        Err(error) => return anthropic_error_response(error),
    };

    let input_tokens = estimate_input_tokens(&request);
    let model = request.model.clone();

    if request.wants_stream() {
        match state.codex.stream_chat(openai_request, &credentials).await {
            Ok(stream) => anthropic_sse_response(stream, model, input_tokens).into_response(),
            Err(error) => anthropic_error_response(error),
        }
    } else {
        match state
            .codex
            .complete_chat(openai_request, &credentials)
            .await
        {
            Ok(response) => Json(from_openai_response(response)).into_response(),
            Err(error) => anthropic_error_response(error),
        }
    }
}

/// Anthropic-compatible token counting endpoint.
pub async fn count_message_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<MessagesRequest>,
) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return anthropic_error_response(error);
    }

    Json(CountTokensResponse {
        input_tokens: estimate_input_tokens(&request),
    })
    .into_response()
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

fn anthropic_sse_response(
    stream: Pin<
        Box<dyn Stream<Item = Result<crate::openai::response::ChatCompletionChunk>> + Send>,
    >,
    model: String,
    input_tokens: u32,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>>> {
    let mapped = async_stream::stream! {
        let id = format!("msg_{}", rand::random::<u64>());
        let mut stream = stream;
        let mut current_index = 0_u32;
        let mut text_open = false;

        match message_start_event(&id, &model, input_tokens) {
            Ok(event) => yield Ok(event),
            Err(error) => {
                yield Ok(anthropic_error_event(error));
                return;
            }
        }

        while let Some(item) = stream.next().await {
            match item {
                Ok(chunk) => {
                    let Some(choice) = chunk.choices.into_iter().next() else {
                        continue;
                    };

                    if let Some(text) = choice.delta.content
                        && !text.is_empty()
                    {
                        if !text_open {
                            match text_block_start(current_index) {
                                Ok(event) => yield Ok(event),
                                Err(error) => {
                                    yield Ok(anthropic_error_event(error));
                                    return;
                                }
                            }
                            text_open = true;
                        }
                        match text_delta(current_index, &text) {
                            Ok(event) => yield Ok(event),
                            Err(error) => {
                                yield Ok(anthropic_error_event(error));
                                return;
                            }
                        }
                    }

                    for tool_call in choice.delta.tool_calls.into_iter().flatten() {
                        if text_open {
                            match content_block_stop(current_index) {
                                Ok(event) => yield Ok(event),
                                Err(error) => {
                                    yield Ok(anthropic_error_event(error));
                                    return;
                                }
                            }
                            current_index += 1;
                            text_open = false;
                        }

                        let tool_call = crate::openai::types::ToolCall {
                            id: tool_call.id,
                            kind: tool_call.kind.to_owned(),
                            function: tool_call.function,
                        };

                        for event in [
                            tool_block_start(current_index, &tool_call),
                            tool_json_delta(current_index, &tool_call.function.arguments),
                            content_block_stop(current_index),
                        ] {
                            match event {
                                Ok(event) => yield Ok(event),
                                Err(error) => {
                                    yield Ok(anthropic_error_event(error));
                                    return;
                                }
                            }
                        }
                        current_index += 1;
                    }

                    if let Some(reason) = choice.finish_reason {
                        if text_open {
                            match content_block_stop(current_index) {
                                Ok(event) => yield Ok(event),
                                Err(error) => {
                                    yield Ok(anthropic_error_event(error));
                                    return;
                                }
                            }
                        }
                        for event in [message_delta_event(&reason), message_stop_event()] {
                            match event {
                                Ok(event) => yield Ok(event),
                                Err(error) => {
                                    yield Ok(anthropic_error_event(error));
                                    return;
                                }
                            }
                        }
                        return;
                    }
                }
                Err(error) => {
                    yield Ok(anthropic_error_event(error));
                    return;
                }
            }
        }

        if text_open {
            match content_block_stop(current_index) {
                Ok(event) => yield Ok(event),
                Err(error) => {
                    yield Ok(anthropic_error_event(error));
                    return;
                }
            }
        }
        for event in [message_delta_event("stop"), message_stop_event()] {
            match event {
                Ok(event) => yield Ok(event),
                Err(error) => {
                    yield Ok(anthropic_error_event(error));
                    return;
                }
            }
        }
    };

    Sse::new(mapped).keep_alive(KeepAlive::default())
}

fn anthropic_error_response(error: crate::Error) -> Response {
    (error.status_code(), Json(error_body(&error))).into_response()
}

fn anthropic_error_event(error: crate::Error) -> Event {
    Event::default()
        .event("error")
        .data(error_body(&error).to_string())
}
