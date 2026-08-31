//! Route handlers and streaming response helpers for the HTTP server.

mod response_resources;
mod responses;
mod sse;

use crate::{
    Error,
    anthropic::{
        CountTokensResponse, MessageBatch, MessageBatchCreateRequest, MessageBatchDeleted,
        MessageBatchListResponse, MessageBatchRequestCounts, MessagesRequest, error_body,
        estimate_input_tokens, from_openai_response_value, message_batch_list_response,
    },
    codex::{
        client::{
            ResponseCreationStrategy, ResponseResourceCapability, resolve_grok_tts_websocket_url,
        },
        convert::responses_to_upstream_request,
    },
    config::{Credentials, Provider},
    openai::{
        response::{
            ImageGenerationResponse, ResponseCompaction, ResponseInputTokens, ResponseObject,
            generated_images_from_output, image_generation_response,
        },
        types::{ChatCompletionRequest, ImageGenerationRequest, ResponsesRequest},
    },
    server::{
        AppState, UpstreamState,
        auth::authorize,
        status_response::{build_status_response, build_unsupported_provider_status_response},
    },
};
use axum::{
    Json,
    body::Body,
    extract::{
        Path, RawQuery, State, WebSocketUpgrade,
        ws::{CloseFrame as DownstreamCloseFrame, Message as DownstreamMessage, WebSocket},
    },
    http::{
        HeaderMap, HeaderName, HeaderValue, StatusCode,
        header::{AUTHORIZATION, USER_AGENT},
    },
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use responses::{
    anthropic_responses_request, batch_results_url, build_batch_id, collect_response_input_items,
    compact_response_items, estimate_response_input_tokens, image_generation_responses_request,
    load_previous_response, maybe_store_response, response_object_from_chat,
    response_object_from_upstream, response_request_requires_raw_mode, responses_to_chat_request,
    run_message_batch_worker,
};
use serde::Serialize;
use serde_json::{Value, json};
use sse::{
    anthropic_error_response, anthropic_raw_messages_sse_response, openai_raw_responses_sse,
    openai_responses_sse, sse_response,
};
use std::time::Duration;
use tokio::{net::TcpStream, time::timeout};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{
        self, Message as UpstreamMessage,
        client::IntoClientRequest,
        protocol::{CloseFrame as UpstreamCloseFrame, frame::coding::CloseCode},
    },
};

pub use response_resources::{
    cancel_response, delete_response, get_response, list_response_input_items,
};

/// Lightweight healthcheck for the local service.
pub async fn health() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

fn trace_request<T: Serialize>(endpoint: &str, request: &T) {
    crate::logging::trace_json(&format!("request.{endpoint}"), request);
}

fn trace_response<T: Serialize>(endpoint: &str, response: &T) {
    crate::logging::trace_json(&format!("response.{endpoint}"), response);
}

fn trace_named_tools(event: &str, tools: &[String]) {
    if tracing::enabled!(tracing::Level::TRACE) {
        tracing::trace!(event = event, tools_count = tools.len(), tool_names = ?tools);
    }
}

fn anthropic_tool_names(request: &MessagesRequest) -> Vec<String> {
    request.tools.as_ref().map_or_else(Vec::new, |tools| {
        tools.iter().map(|tool| tool.name.clone()).collect()
    })
}

fn responses_tool_names(request: &ResponsesRequest) -> Vec<String> {
    request.tools.as_ref().map_or_else(Vec::new, |tools| {
        tools
            .iter()
            .map(|tool| {
                tool.function
                    .as_ref()
                    .map_or_else(|| tool.kind.clone(), |function| function.name.clone())
            })
            .collect()
    })
}

fn upstream_tool_names(body: &Value) -> Vec<String> {
    body.get("tools")
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |tools| {
            tools
                .iter()
                .map(|tool| {
                    tool.pointer("/function/name")
                        .and_then(Value::as_str)
                        .or_else(|| tool.get("name").and_then(Value::as_str))
                        .or_else(|| tool.get("type").and_then(Value::as_str))
                        .unwrap_or("unknown")
                        .to_owned()
                })
                .collect()
        })
}

fn upstream_response_resource_supported(upstream: &UpstreamState) -> bool {
    upstream.client.response_resource_capabilities().retrieve
        == ResponseResourceCapability::UpstreamSupported
}

fn should_use_native_responses_path(
    upstream: &UpstreamState,
    request: &ResponsesRequest,
    previous: Option<&crate::server::store::StoredResponse>,
) -> bool {
    match upstream.client.response_creation_strategy() {
        ResponseCreationStrategy::NativeResponses => true,
        ResponseCreationStrategy::ChatCompatibility => {
            response_request_requires_raw_mode(request, previous)
        }
    }
}

fn can_forward_previous_response_id(upstream: &UpstreamState, request: &ResponsesRequest) -> bool {
    request.previous_response_id.is_some()
        && upstream.client.response_creation_strategy() == ResponseCreationStrategy::NativeResponses
        && upstream_response_resource_supported(upstream)
}

fn should_use_upstream_previous_response(
    upstream: &UpstreamState,
    request: &ResponsesRequest,
    previous: Option<&crate::server::store::StoredResponse>,
) -> bool {
    can_forward_previous_response_id(upstream, request)
        && previous
            .is_none_or(|stored| stored.provider == upstream.provider && stored.upstream_resource)
}

async fn store_response_if_requested(
    state: &AppState,
    request: &ResponsesRequest,
    response: ResponseObject,
    input_items: Vec<Value>,
    provider: Provider,
    upstream_resource: bool,
) {
    maybe_store_response(
        state,
        request,
        response,
        input_items,
        provider,
        upstream_resource,
    )
    .await;
}

async fn run_raw_response(
    state: &AppState,
    upstream: &UpstreamState,
    credentials: &Credentials,
    request: ResponsesRequest,
    previous: Option<&crate::server::store::StoredResponse>,
) -> Response {
    let use_upstream_previous = should_use_upstream_previous_response(upstream, &request, previous);
    let upstream_previous = if use_upstream_previous {
        None
    } else {
        previous
    };
    let upstream_input_items = match collect_response_input_items(&request, upstream_previous) {
        Ok(items) => items,
        Err(error) => return error.into_response(),
    };
    let storage_input_items = if use_upstream_previous && previous.is_some() {
        match collect_response_input_items(&request, previous) {
            Ok(items) => items,
            Err(error) => return error.into_response(),
        }
    } else {
        upstream_input_items.clone()
    };
    let mut upstream_request = request.clone();
    if !use_upstream_previous {
        upstream_request.previous_response_id = None;
    }
    let body = match responses_to_upstream_request(
        upstream.provider,
        &upstream_request,
        &upstream_input_items,
    ) {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    trace_named_tools("responses_tools_upstream", &upstream_tool_names(&body));
    if request.wants_stream() {
        return match upstream.client.stream_response(body, credentials).await {
            Ok(stream) => openai_raw_responses_sse(
                stream,
                request,
                storage_input_items,
                state.responses.clone(),
                upstream.provider,
                upstream_response_resource_supported(upstream),
            )
            .into_response(),
            Err(error) => error.into_response(),
        };
    }

    match upstream.client.complete_response(body, credentials).await {
        Ok(value) => {
            let response_object = response_object_from_upstream(&request, &value);
            store_response_if_requested(
                state,
                &request,
                response_object.clone(),
                storage_input_items,
                upstream.provider,
                upstream_response_resource_supported(upstream),
            )
            .await;
            trace_response("responses", &response_object);
            Json(response_object).into_response()
        }
        Err(error) => error.into_response(),
    }
}

/// Returns the configured model list after optional local API key validation.
pub async fn models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match authorize(&headers, state.api_key.as_deref()) {
        Ok(()) => {
            if headers.contains_key("anthropic-version") {
                let ids = state
                    .models
                    .data
                    .iter()
                    .map(|model| model.id.clone())
                    .collect::<Vec<_>>();
                Json(crate::anthropic::models_response(&ids)).into_response()
            } else {
                Json(state.models).into_response()
            }
        }
        Err(error) => error.into_response(),
    }
}

/// Creates an OpenAI-compatible Responses API object.
///
/// `previous_response_id` is supported only through local in-memory
/// continuation state while this rotom process remains alive.
pub async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut request): Json<ResponsesRequest>,
) -> Response {
    state.rewrite_model(&mut request.model);
    trace_request("responses", &request);
    trace_named_tools("responses_tools_inbound", &responses_tool_names(&request));
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return error.into_response();
    }

    let Some(upstream) = state.upstream_for_model(&request.model) else {
        return Error::config(format!("not logged in for model {}", request.model)).into_response();
    };
    let credentials = match upstream.token_manager.credentials().await {
        Ok(credentials) => credentials,
        Err(error) => return error.into_response(),
    };
    let previous =
        match load_previous_response(&state, request.previous_response_id.as_deref()).await {
            Ok(previous) => previous,
            Err(_) if can_forward_previous_response_id(upstream, &request) => None,
            Err(error) => return error.into_response(),
        };

    if should_use_native_responses_path(upstream, &request, previous.as_ref()) {
        return run_raw_response(&state, upstream, &credentials, request, previous.as_ref()).await;
    }

    let (chat_request, input_items) = match responses_to_chat_request(&request, previous.as_ref()) {
        Ok(converted) => converted,
        Err(error) => return error.into_response(),
    };

    if request.wants_stream() {
        match upstream
            .client
            .stream_chat(chat_request, &credentials)
            .await
        {
            Ok(stream) => openai_responses_sse(
                stream,
                responses::build_response_id(),
                request,
                input_items,
                state.responses.clone(),
                upstream.provider,
            )
            .into_response(),
            Err(error) => error.into_response(),
        }
    } else {
        match upstream
            .client
            .complete_chat(chat_request, &credentials)
            .await
        {
            Ok(response) => {
                let response_object = response_object_from_chat(&request, response);
                store_response_if_requested(
                    &state,
                    &request,
                    response_object.clone(),
                    input_items,
                    upstream.provider,
                    false,
                )
                .await;
                trace_response("responses", &response_object);
                Json(response_object).into_response()
            }
            Err(error) => error.into_response(),
        }
    }
}

/// Returns an estimated input token count for a Responses API request.
///
/// When `previous_response_id` is present, the estimate uses only local
/// in-memory continuation state and does not consult any upstream retrievable
/// response resource.
pub async fn count_response_input_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut request): Json<ResponsesRequest>,
) -> Response {
    state.rewrite_model(&mut request.model);
    trace_request("responses.input_tokens", &request);
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return error.into_response();
    }

    let previous =
        match load_previous_response(&state, request.previous_response_id.as_deref()).await {
            Ok(previous) => previous,
            Err(error) => return error.into_response(),
        };
    let (_, input_items) = match responses_to_chat_request(&request, previous.as_ref()) {
        Ok(converted) => converted,
        Err(error) => return error.into_response(),
    };

    let response = ResponseInputTokens {
        object: "response.input_tokens",
        input_tokens: estimate_response_input_tokens(&request, &input_items),
    };
    trace_response("responses.input_tokens", &response);
    Json(response).into_response()
}

/// Runs a local best-effort Responses API compaction pass.
pub async fn compact_response(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut request): Json<ResponsesRequest>,
) -> Response {
    state.rewrite_model(&mut request.model);
    trace_request("responses.compact", &request);
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return error.into_response();
    }

    let previous =
        match load_previous_response(&state, request.previous_response_id.as_deref()).await {
            Ok(previous) => previous,
            Err(error) => return error.into_response(),
        };
    let input_items = match collect_response_input_items(&request, previous.as_ref()) {
        Ok(items) => items,
        Err(error) => return error.into_response(),
    };
    let response = ResponseCompaction {
        output: compact_response_items(&input_items),
    };
    trace_response("responses.compact", &response);
    Json(response).into_response()
}

/// Refreshes the saved OAuth credentials without exposing the raw tokens.
pub async fn manual_refresh(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return error.into_response();
    }

    let mut refreshed = Vec::new();
    for upstream in state.upstreams.iter() {
        match upstream.token_manager.refresh().await {
            Ok(credentials) => refreshed.push(json!({
                "provider": credentials.provider,
                "account_id": credentials.account_id,
                "expires_at": credentials.expires_at,
            })),
            Err(error) => return error.into_response(),
        }
    }
    Json(json!({ "providers": refreshed })).into_response()
}

/// Returns account, token, and rate-limit status in a structured JSON format.
pub async fn status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return error.into_response();
    }

    let Some(upstream) = state.default_upstream() else {
        return Error::config("no upstream providers configured").into_response();
    };
    let credentials = match upstream.token_manager.credentials().await {
        Ok(credentials) => credentials,
        Err(error) => return error.into_response(),
    };

    if upstream.provider != crate::config::Provider::Codex {
        return Json(build_unsupported_provider_status_response(&credentials)).into_response();
    }

    let snapshot = state.status.fetch_status(&credentials).await;
    Json(build_status_response(&credentials, &snapshot)).into_response()
}

/// Proxies OpenAI-compatible chat completion requests to the Codex backend.
pub async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut request): Json<ChatCompletionRequest>,
) -> Response {
    state.rewrite_model(&mut request.model);
    trace_request("chat_completions", &request);
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return error.into_response();
    }

    let Some(upstream) = state.upstream_for_model(&request.model) else {
        return Error::config(format!("not logged in for model {}", request.model)).into_response();
    };
    let credentials = match upstream.token_manager.credentials().await {
        Ok(credentials) => credentials,
        Err(error) => return error.into_response(),
    };

    if request.wants_stream() {
        match upstream.client.stream_chat(request, &credentials).await {
            Ok(stream) => sse_response(stream).into_response(),
            Err(error) => error.into_response(),
        }
    } else {
        match upstream.client.complete_chat(request, &credentials).await {
            Ok(response) => {
                trace_response("chat_completions", &response);
                Json(response).into_response()
            }
            Err(error) => error.into_response(),
        }
    }
}

/// Anthropic-compatible Messages API handler used by Claude SDKs and Claude Code.
pub async fn messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut request): Json<MessagesRequest>,
) -> Response {
    state.rewrite_model(&mut request.model);
    trace_request("messages", &request);
    trace_named_tools("messages_tools_inbound", &anthropic_tool_names(&request));
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return anthropic_error_response(&error);
    }

    apply_anthropic_headers(&headers, &mut request);

    let Some(upstream) = state.upstream_for_model(&request.model) else {
        return anthropic_error_response(&Error::config(format!(
            "not logged in for model {}",
            request.model
        )));
    };
    let credentials = match upstream.token_manager.credentials().await {
        Ok(credentials) => credentials,
        Err(error) => return anthropic_error_response(&error),
    };

    let response_request = match anthropic_responses_request(&request) {
        Ok(request) => request,
        Err(error) => return anthropic_error_response(&error),
    };
    let input_items = match collect_response_input_items(&response_request, None) {
        Ok(items) => items,
        Err(error) => return anthropic_error_response(&error),
    };
    let body =
        match responses_to_upstream_request(upstream.provider, &response_request, &input_items) {
            Ok(body) => body,
            Err(error) => return anthropic_error_response(&error),
        };
    trace_named_tools("messages_tools_upstream", &upstream_tool_names(&body));

    let input_tokens = estimate_response_input_tokens(&response_request, &input_items);
    let model = request.model.clone();

    if request.wants_stream() {
        match upstream.client.stream_response(body, &credentials).await {
            Ok(stream) => {
                anthropic_raw_messages_sse_response(stream, model, input_tokens).into_response()
            }
            Err(error) => anthropic_error_response(&error),
        }
    } else {
        match upstream.client.complete_response(body, &credentials).await {
            Ok(value) => {
                let response = from_openai_response_value(&value, &request.model);
                trace_response("messages", &response);
                Json(response).into_response()
            }
            Err(error) => anthropic_error_response(&error),
        }
    }
}

/// OpenAI-compatible Images API handler backed by the Codex Responses image tool.
pub async fn image_generations(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut request): Json<ImageGenerationRequest>,
) -> Response {
    state.rewrite_model(&mut request.model);
    trace_request("image_generations", &request);
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return error.into_response();
    }

    let Some(upstream) = state.upstream_for_model(&request.model) else {
        return Error::config(format!("not logged in for model {}", request.model)).into_response();
    };
    let credentials = match upstream.token_manager.credentials().await {
        Ok(credentials) => credentials,
        Err(error) => return error.into_response(),
    };

    let response_request = image_generation_responses_request(&request);
    let input_items = match collect_response_input_items(&response_request, None) {
        Ok(items) => items,
        Err(error) => return error.into_response(),
    };
    let body =
        match responses_to_upstream_request(upstream.provider, &response_request, &input_items) {
            Ok(body) => body,
            Err(error) => return error.into_response(),
        };

    match upstream.client.complete_response(body, &credentials).await {
        Ok(value) => {
            let images = generated_images_from_output(
                value
                    .get("output")
                    .and_then(Value::as_array)
                    .map_or(&[], Vec::as_slice),
            );
            let response = image_generation_response(crate::config::now_unix(), images);
            trace_response("image_generations", &response);
            Json::<ImageGenerationResponse>(response).into_response()
        }
        Err(error) => error.into_response(),
    }
}

/// Proxies the native xAI text-to-speech request shape to the logged-in Grok upstream.
pub async fn tts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    trace_request("tts", &request);
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return error.into_response();
    }

    let Some(upstream) = state.upstream_for_provider(Provider::Grok) else {
        return Error::config(
            "not logged in for provider grok; run `rotom login --provider grok` first",
        )
        .into_response();
    };
    let credentials = match upstream.token_manager.credentials().await {
        Ok(credentials) => credentials,
        Err(error) => return error.into_response(),
    };

    match upstream
        .client
        .synthesize_grok_speech(&request, &credentials)
        .await
    {
        Ok(response) => proxy_upstream_response(response),
        Err(error) => error.into_response(),
    }
}

const GROK_TTS_WEBSOCKET_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Proxies the native xAI bidirectional text-to-speech WebSocket protocol.
pub async fn tts_websocket(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    websocket: WebSocketUpgrade,
) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return error.into_response();
    }

    let Some(upstream) = state.upstream_for_provider(Provider::Grok) else {
        return Error::config(
            "not logged in for provider grok; run `rotom login --provider grok` first",
        )
        .into_response();
    };
    let credentials = match upstream.token_manager.credentials().await {
        Ok(credentials) => credentials,
        Err(error) => return error.into_response(),
    };
    let websocket_url =
        match resolve_grok_tts_websocket_url(upstream.client.base_url(), raw_query.as_deref()) {
            Ok(url) => url,
            Err(error) => return error.into_response(),
        };
    let mut upstream_request = match websocket_url.as_str().into_client_request() {
        Ok(request) => request,
        Err(error) => {
            return Error::config(format!("invalid Grok TTS WebSocket request: {error}"))
                .into_response();
        }
    };
    let Ok(authorization) = HeaderValue::from_str(&format!("Bearer {}", credentials.access_token))
    else {
        return Error::config("invalid Grok access token header").into_response();
    };
    upstream_request
        .headers_mut()
        .insert(AUTHORIZATION, authorization);
    upstream_request
        .headers_mut()
        .insert(USER_AGENT, HeaderValue::from_static("rotom"));

    let upstream_socket = match timeout(
        GROK_TTS_WEBSOCKET_CONNECT_TIMEOUT,
        connect_async(upstream_request),
    )
    .await
    {
        Ok(Ok((socket, _response))) => socket,
        Ok(Err(error)) => return grok_tts_websocket_connect_error(error),
        Err(_elapsed) => {
            return Error::upstream_with_status(
                StatusCode::GATEWAY_TIMEOUT,
                "Grok TTS WebSocket handshake timed out",
            )
            .into_response();
        }
    };

    websocket
        .on_upgrade(move |socket| proxy_grok_tts_websocket(socket, upstream_socket))
        .into_response()
}

fn grok_tts_websocket_connect_error(error: tungstenite::Error) -> Response {
    match error {
        tungstenite::Error::Http(response) => {
            let status = response.status();
            let headers = response.headers().clone();
            let body = response.body().clone().unwrap_or_default();
            let mut downstream = Response::new(Body::from(body));
            *downstream.status_mut() = status;
            for (name, value) in &headers {
                if !is_hop_by_hop_header(name) {
                    downstream.headers_mut().append(name.clone(), value.clone());
                }
            }
            downstream
        }
        error => {
            Error::upstream(format!("Grok TTS WebSocket handshake failed: {error}")).into_response()
        }
    }
}

async fn proxy_grok_tts_websocket(
    downstream: WebSocket,
    upstream: WebSocketStream<MaybeTlsStream<TcpStream>>,
) {
    let (mut downstream_sender, mut downstream_receiver) = downstream.split();
    let (mut upstream_sender, mut upstream_receiver) = upstream.split();

    loop {
        tokio::select! {
            downstream_message = downstream_receiver.next() => {
                let Some(downstream_message) = downstream_message else {
                    let _ = upstream_sender.send(UpstreamMessage::Close(None)).await;
                    break;
                };
                let downstream_message = match downstream_message {
                    Ok(message) => message,
                    Err(error) => {
                        tracing::debug!(error = %error, "Grok TTS downstream WebSocket closed with an error");
                        let _ = upstream_sender.send(UpstreamMessage::Close(None)).await;
                        break;
                    }
                };
                let should_flush_downstream = matches!(
                    &downstream_message,
                    DownstreamMessage::Ping(_) | DownstreamMessage::Close(_)
                );
                let is_close = matches!(&downstream_message, DownstreamMessage::Close(_));
                if let Some(message) = downstream_to_upstream_message(downstream_message) {
                    if upstream_sender.send(message).await.is_err() {
                        break;
                    }
                }
                if should_flush_downstream {
                    let _ = downstream_sender.flush().await;
                }
                if is_close {
                    break;
                }
            }
            upstream_message = upstream_receiver.next() => {
                let Some(upstream_message) = upstream_message else {
                    let _ = downstream_sender.send(DownstreamMessage::Close(None)).await;
                    break;
                };
                let upstream_message = match upstream_message {
                    Ok(message) => message,
                    Err(error) => {
                        tracing::debug!(error = %error, "Grok TTS upstream WebSocket closed with an error");
                        let _ = downstream_sender.send(DownstreamMessage::Close(None)).await;
                        break;
                    }
                };
                let should_flush_upstream = matches!(
                    &upstream_message,
                    UpstreamMessage::Ping(_) | UpstreamMessage::Close(_)
                );
                let is_close = matches!(&upstream_message, UpstreamMessage::Close(_));
                if let Some(message) = upstream_to_downstream_message(upstream_message) {
                    if downstream_sender.send(message).await.is_err() {
                        break;
                    }
                }
                if should_flush_upstream {
                    let _ = upstream_sender.flush().await;
                }
                if is_close {
                    break;
                }
            }
        }
    }
}

fn downstream_to_upstream_message(message: DownstreamMessage) -> Option<UpstreamMessage> {
    match message {
        DownstreamMessage::Text(text) => Some(UpstreamMessage::Text(text.to_string().into())),
        DownstreamMessage::Binary(bytes) => Some(UpstreamMessage::Binary(bytes)),
        DownstreamMessage::Close(frame) => Some(UpstreamMessage::Close(frame.map(|frame| {
            UpstreamCloseFrame {
                code: CloseCode::from(frame.code),
                reason: frame.reason.to_string().into(),
            }
        }))),
        DownstreamMessage::Ping(_) | DownstreamMessage::Pong(_) => None,
    }
}

fn upstream_to_downstream_message(message: UpstreamMessage) -> Option<DownstreamMessage> {
    match message {
        UpstreamMessage::Text(text) => Some(DownstreamMessage::Text(text.to_string().into())),
        UpstreamMessage::Binary(bytes) => Some(DownstreamMessage::Binary(bytes)),
        UpstreamMessage::Close(frame) => Some(DownstreamMessage::Close(frame.map(|frame| {
            DownstreamCloseFrame {
                code: u16::from(frame.code),
                reason: frame.reason.to_string().into(),
            }
        }))),
        UpstreamMessage::Ping(_) | UpstreamMessage::Pong(_) | UpstreamMessage::Frame(_) => None,
    }
}

/// Proxies the native xAI built-in text-to-speech voice list.
pub async fn tts_voices(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return error.into_response();
    }

    let Some(upstream) = state.upstream_for_provider(Provider::Grok) else {
        return Error::config(
            "not logged in for provider grok; run `rotom login --provider grok` first",
        )
        .into_response();
    };
    let credentials = match upstream.token_manager.credentials().await {
        Ok(credentials) => credentials,
        Err(error) => return error.into_response(),
    };

    match upstream.client.list_grok_tts_voices(&credentials).await {
        Ok(response) => proxy_upstream_response(response),
        Err(error) => error.into_response(),
    }
}

fn proxy_upstream_response(upstream: reqwest::Response) -> Response {
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
    *response.status_mut() = status;
    for (name, value) in &headers {
        if !is_hop_by_hop_header(name) {
            response.headers_mut().append(name.clone(), value.clone());
        }
    }
    response
}

fn is_hop_by_hop_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

/// Anthropic-compatible token counting endpoint.
pub async fn count_message_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut request): Json<MessagesRequest>,
) -> Response {
    state.rewrite_model(&mut request.model);
    trace_request("messages.count_tokens", &request);
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return anthropic_error_response(&error);
    }

    apply_anthropic_headers(&headers, &mut request);
    let input_tokens = anthropic_responses_request(&request)
        .and_then(|response_request| {
            collect_response_input_items(&response_request, None)
                .map(|input_items| estimate_response_input_tokens(&response_request, &input_items))
        })
        .unwrap_or_else(|_| estimate_input_tokens(&request));

    let response = CountTokensResponse { input_tokens };
    trace_response("messages.count_tokens", &response);
    Json(response).into_response()
}

/// Creates an Anthropic-compatible message batch and schedules background execution.
pub async fn create_message_batch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut request): Json<MessageBatchCreateRequest>,
) -> Response {
    trace_request("messages.batches.create", &request);
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return anthropic_error_response(&error);
    }

    for item in &mut request.requests {
        state.rewrite_model(&mut item.params.model);
        apply_anthropic_headers(&headers, &mut item.params);
    }

    let batch_id = build_batch_id();
    let created_at = chrono::Utc::now();
    let results_url = Some(batch_results_url(&headers, &batch_id));
    let total_requests = u32::try_from(request.requests.len()).unwrap_or(u32::MAX);

    let batch = MessageBatch {
        archived_at: None,
        cancel_initiated_at: None,
        created_at: created_at.to_rfc3339(),
        ended_at: None,
        expires_at: (created_at + chrono::TimeDelta::hours(24)).to_rfc3339(),
        id: batch_id.clone(),
        processing_status: "in_progress",
        request_counts: MessageBatchRequestCounts {
            canceled: 0,
            errored: 0,
            expired: 0,
            processing: total_requests,
            succeeded: 0,
        },
        results_url,
        kind: "message_batch",
    };

    state
        .batches
        .insert(crate::server::store::StoredBatch {
            batch: batch.clone(),
            results: Vec::new(),
            cancel_requested: false,
        })
        .await;

    let batches = state.batches.clone();
    let upstreams = state.upstreams.clone();
    tokio::spawn(async move {
        run_message_batch_worker(batches, upstreams, batch_id, request.requests).await;
    });
    trace_response("messages.batches.create", &batch);
    Json(batch).into_response()
}

fn apply_anthropic_headers(headers: &HeaderMap, request: &mut MessagesRequest) {
    if let Some(version) = headers
        .get("anthropic-version")
        .and_then(|value| value.to_str().ok())
    {
        request.extra.insert(
            "rotom_anthropic_version".to_owned(),
            Value::String(version.to_owned()),
        );
    }

    let betas = headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| Value::String(value.to_owned()))
        .collect::<Vec<_>>();
    if !betas.is_empty() {
        request
            .extra
            .insert("rotom_anthropic_beta".to_owned(), Value::Array(betas));
    }
}

/// Lists previously created Anthropic message batches in newest-first order.
pub async fn list_message_batches(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return anthropic_error_response(&error);
    }

    let batches = state
        .batches
        .list()
        .await
        .into_iter()
        .map(|stored| stored.batch)
        .collect::<Vec<_>>();
    Json::<MessageBatchListResponse>(message_batch_list_response(batches)).into_response()
}

/// Retrieves a previously created Anthropic message batch.
pub async fn get_message_batch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(batch_id): Path<String>,
) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return anthropic_error_response(&error);
    }

    match state.batches.get(&batch_id).await {
        Some(stored) => Json(stored.batch).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(error_body(&crate::Error::config(format!(
                "message batch `{batch_id}` was not found"
            )))),
        )
            .into_response(),
    }
}

/// Returns JSONL results for a completed Anthropic message batch.
pub async fn message_batch_results(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(batch_id): Path<String>,
) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return anthropic_error_response(&error);
    }

    match state.batches.get(&batch_id).await {
        Some(stored) => (
            [(axum::http::header::CONTENT_TYPE, "application/x-ndjson")],
            stored
                .results
                .iter()
                .map(|result| serde_json::to_string(result).unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\n"),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(error_body(&crate::Error::config(format!(
                "message batch `{batch_id}` was not found"
            )))),
        )
            .into_response(),
    }
}

/// Initiates cancellation for a previously created Anthropic message batch.
pub async fn cancel_message_batch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(batch_id): Path<String>,
) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return anthropic_error_response(&error);
    }

    match state
        .batches
        .update(&batch_id, |stored| {
            if stored.batch.cancel_initiated_at.is_none() && stored.batch.ended_at.is_none() {
                stored.batch.cancel_initiated_at = Some(chrono::Utc::now().to_rfc3339());
                stored.batch.processing_status = "canceling";
                stored.cancel_requested = true;
            }
        })
        .await
    {
        Some(stored) => Json(stored.batch).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(error_body(&crate::Error::config(format!(
                "message batch `{batch_id}` was not found"
            )))),
        )
            .into_response(),
    }
}

/// Deletes a previously completed Anthropic message batch.
pub async fn delete_message_batch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(batch_id): Path<String>,
) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return anthropic_error_response(&error);
    }

    match state.batches.remove(&batch_id).await {
        Some(_) => Json(MessageBatchDeleted {
            id: batch_id,
            kind: "message_batch_deleted",
        })
        .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(error_body(&crate::Error::config(format!(
                "message batch `{batch_id}` was not found"
            )))),
        )
            .into_response(),
    }
}
