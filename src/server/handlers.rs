//! Route handlers and streaming response helpers for the HTTP server.

use crate::{
    anthropic::{
        CountTokensResponse, MessageBatch, MessageBatchCreateRequest, MessageBatchDeleted,
        MessageBatchListResponse, MessageBatchRequest, MessageBatchRequestCounts,
        MessageBatchResult, MessageBatchResultType, MessagesRequest, content_block_stop,
        error_body, estimate_input_tokens, from_openai_response, from_openai_response_object,
        image_block_start, message_batch_list_response, message_delta_event, message_start_event,
        message_stop_event, models_response, text_block_start, text_delta, to_openai_request,
        tool_block_start, tool_json_delta,
    },
    codex::{convert::responses_to_codex_request, events::is_done_event},
    error::Result,
    openai::{
        response::{
            GeneratedImage, ImageGenerationResponse, ResponseCompaction, ResponseInputTokens,
            ResponseObject, generated_images_from_output, image_generation_response,
            response_function_call_item, response_image_generation_item, response_message_item,
        },
        types::{
            ChatCompletionRequest, ChatContent, ChatContentPart, ChatMessage, ChatTool,
            ImageGenerationRequest, ImageUrl, ResponseInput, ResponseInputContent,
            ResponseInputItem, ResponseMessageInputItem, ResponsesRequest,
        },
    },
    server::{AppState, auth::authorize, status_response::build_status_response},
};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header::HOST},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{Stream, StreamExt, stream};
use serde_json::{Value, json};
use std::{convert::Infallible, pin::Pin};

/// Lightweight healthcheck for the local service.
pub async fn health() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
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
                Json(models_response(&ids)).into_response()
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
/// continuation state while this Codexia process remains alive.
pub async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ResponsesRequest>,
) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return error.into_response();
    }

    let credentials = match state.token_manager.credentials().await {
        Ok(credentials) => credentials,
        Err(error) => return error.into_response(),
    };
    let previous =
        match load_previous_response(&state, request.previous_response_id.as_deref()).await {
            Ok(previous) => previous,
            Err(error) => return error.into_response(),
        };

    if response_request_requires_raw_mode(&request, previous.as_ref()) {
        let input_items = match collect_response_input_items(&request, previous.as_ref()) {
            Ok(items) => items,
            Err(error) => return error.into_response(),
        };
        let body = responses_to_codex_request(&request, &input_items);
        if request.wants_stream() {
            return match state.codex.stream_response(body, &credentials).await {
                Ok(stream) => {
                    openai_raw_responses_sse(stream, request, input_items, state.responses)
                        .into_response()
                }
                Err(error) => error.into_response(),
            };
        }
        return match state.codex.complete_response(body, &credentials).await {
            Ok(value) => {
                let response_object = response_object_from_upstream(&request, &value);
                maybe_store_response(&state, &request, response_object.clone(), input_items).await;
                Json(response_object).into_response()
            }
            Err(error) => error.into_response(),
        };
    }

    let (chat_request, input_items) = match responses_to_chat_request(&request, previous.as_ref()) {
        Ok(converted) => converted,
        Err(error) => return error.into_response(),
    };

    if request.wants_stream() {
        match state.codex.stream_chat(chat_request, &credentials).await {
            Ok(stream) => openai_responses_sse(
                stream,
                build_response_id(),
                request,
                input_items,
                state.responses,
            )
            .into_response(),
            Err(error) => error.into_response(),
        }
    } else {
        match state.codex.complete_chat(chat_request, &credentials).await {
            Ok(response) => {
                let response_object = response_object_from_chat(&request, response);
                maybe_store_response(&state, &request, response_object.clone(), input_items).await;
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
    Json(request): Json<ResponsesRequest>,
) -> Response {
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

    Json(ResponseInputTokens {
        input_tokens: estimate_response_input_tokens(&input_items),
    })
    .into_response()
}

/// Runs a local best-effort Responses API compaction pass.
pub async fn compact_response(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ResponsesRequest>,
) -> Response {
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

    Json(ResponseCompaction {
        output: compact_response_items(&input_items),
    })
    .into_response()
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
        return anthropic_error_response(&error);
    }

    let credentials = match state.token_manager.credentials().await {
        Ok(credentials) => credentials,
        Err(error) => return anthropic_error_response(&error),
    };

    if anthropic_request_uses_image_generation(&request) {
        let response_request = anthropic_image_generation_request(&request);
        let input_items = match collect_response_input_items(&response_request, None) {
            Ok(items) => items,
            Err(error) => return anthropic_error_response(&error),
        };
        let body = responses_to_codex_request(&response_request, &input_items);

        if request.wants_stream() {
            return match state.codex.stream_response(body, &credentials).await {
                Ok(stream) => anthropic_raw_image_sse_response(
                    stream,
                    request.model.clone(),
                    estimate_input_tokens(&request),
                )
                .into_response(),
                Err(error) => anthropic_error_response(&error),
            };
        }

        return match state.codex.complete_response(body, &credentials).await {
            Ok(value) => {
                let response_object = response_object_from_upstream(&response_request, &value);
                Json(from_openai_response_object(response_object)).into_response()
            }
            Err(error) => anthropic_error_response(&error),
        };
    }

    let openai_request = match to_openai_request(&request) {
        Ok(request) => request,
        Err(error) => return anthropic_error_response(&error),
    };

    let input_tokens = estimate_input_tokens(&request);
    let model = request.model.clone();

    if request.wants_stream() {
        match state.codex.stream_chat(openai_request, &credentials).await {
            Ok(stream) => anthropic_sse_response(stream, model, input_tokens).into_response(),
            Err(error) => anthropic_error_response(&error),
        }
    } else {
        match state
            .codex
            .complete_chat(openai_request, &credentials)
            .await
        {
            Ok(response) => Json(from_openai_response(response)).into_response(),
            Err(error) => anthropic_error_response(&error),
        }
    }
}

/// OpenAI-compatible Images API handler backed by the Codex Responses image tool.
pub async fn image_generations(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ImageGenerationRequest>,
) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return error.into_response();
    }

    let credentials = match state.token_manager.credentials().await {
        Ok(credentials) => credentials,
        Err(error) => return error.into_response(),
    };

    let response_request = image_generation_responses_request(&request);
    let input_items = match collect_response_input_items(&response_request, None) {
        Ok(items) => items,
        Err(error) => return error.into_response(),
    };
    let body = responses_to_codex_request(&response_request, &input_items);

    match state.codex.complete_response(body, &credentials).await {
        Ok(value) => {
            let images = generated_images_from_output(
                value
                    .get("output")
                    .and_then(Value::as_array)
                    .map_or(&[], Vec::as_slice),
            );
            Json::<ImageGenerationResponse>(image_generation_response(
                crate::config::now_unix(),
                images,
            ))
            .into_response()
        }
        Err(error) => error.into_response(),
    }
}

/// Anthropic-compatible token counting endpoint.
pub async fn count_message_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<MessagesRequest>,
) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return anthropic_error_response(&error);
    }

    Json(CountTokensResponse {
        input_tokens: estimate_input_tokens(&request),
    })
    .into_response()
}

/// Creates an Anthropic-compatible message batch and schedules background execution.
pub async fn create_message_batch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<MessageBatchCreateRequest>,
) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return anthropic_error_response(&error);
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
    let token_manager = state.token_manager.clone();
    let codex = state.codex.clone();
    tokio::spawn(async move {
        run_message_batch_worker(batches, token_manager, codex, batch_id, request.requests).await;
    });

    Json(batch).into_response()
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

async fn load_previous_response(
    state: &AppState,
    response_id: Option<&str>,
) -> Result<Option<crate::server::store::StoredResponse>> {
    match response_id {
        Some(id) => state
            .responses
            .get(id)
            .await
            .ok_or_else(|| crate::Error::config(format!("response `{id}` was not found")))
            .map(Some),
        None => Ok(None),
    }
}

fn responses_to_chat_request(
    request: &ResponsesRequest,
    previous: Option<&crate::server::store::StoredResponse>,
) -> Result<(ChatCompletionRequest, Vec<Value>)> {
    let mut messages = previous.map(stored_response_messages).unwrap_or_default();
    let input_items = collect_response_input_items(request, previous)?;
    messages.extend(response_input_items_to_chat_messages(&input_items));
    maybe_prepend_instructions(&mut messages, request.instructions.as_deref());

    Ok((
        ChatCompletionRequest {
            model: request.model.clone(),
            messages,
            stream: request.stream,
            temperature: request.temperature,
            top_p: request.top_p,
            tools: request.tools.clone(),
            tool_choice: request.tool_choice.clone(),
            service_tier: request.service_tier.clone(),
            reasoning_effort: request
                .reasoning
                .as_ref()
                .and_then(|value| value.get("effort"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            max_completion_tokens: request.max_output_tokens,
            max_tokens: request.max_output_tokens,
            parallel_tool_calls: request.parallel_tool_calls,
            stop: None,
            extra: request.extra.clone(),
        },
        input_items,
    ))
}

fn response_request_requires_raw_mode(
    request: &ResponsesRequest,
    previous: Option<&crate::server::store::StoredResponse>,
) -> bool {
    request
        .tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(is_image_generation_tool))
        || previous_stores_generated_images(previous)
}

fn previous_stores_generated_images(
    previous: Option<&crate::server::store::StoredResponse>,
) -> bool {
    previous.is_some_and(|stored| {
        stored.input_items.iter().any(|item| {
            item.get("type").and_then(Value::as_str) == Some("image_generation_call")
        })
    })
}

fn is_image_generation_tool(tool: &ChatTool) -> bool {
    tool.kind == "image_generation"
}

fn anthropic_request_uses_image_generation(request: &MessagesRequest) -> bool {
    request
        .tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(|tool| tool.name == "image_generation"))
}

fn anthropic_image_generation_request(request: &MessagesRequest) -> ResponsesRequest {
    let openai = to_openai_request(request).unwrap_or_else(|_| ChatCompletionRequest {
        model: request.model.clone(),
        messages: Vec::new(),
        stream: Some(false),
        temperature: request.temperature,
        top_p: request.top_p,
        tools: None,
        tool_choice: None,
        service_tier: None,
        reasoning_effort: None,
        max_completion_tokens: request.max_tokens,
        max_tokens: request.max_tokens,
        parallel_tool_calls: Some(false),
        stop: request.stop_sequences.clone(),
        extra: request.extra.clone(),
    });

    ResponsesRequest {
        model: openai.model,
        input: Some(ResponseInput::Items(
            openai
                .messages
                .into_iter()
                .map(chat_message_to_response_input_item)
                .collect(),
        )),
        instructions: None,
        stream: Some(false),
        temperature: openai.temperature,
        top_p: openai.top_p,
        tools: openai.tools,
        tool_choice: openai.tool_choice,
        service_tier: None,
        reasoning: None,
        max_output_tokens: request.max_tokens,
        parallel_tool_calls: Some(false),
        store: Some(false),
        previous_response_id: None,
        metadata: None,
        extra: request.extra.clone(),
    }
}

fn image_generation_responses_request(request: &ImageGenerationRequest) -> ResponsesRequest {
    let mut tool = ChatTool {
        kind: "image_generation".to_owned(),
        function: None,
        extra: serde_json::Map::new(),
    };
    if let Some(size) = &request.size {
        tool.extra
            .insert("size".to_owned(), Value::String(size.clone()));
    }
    if let Some(quality) = &request.quality {
        tool.extra
            .insert("quality".to_owned(), Value::String(quality.clone()));
    }
    if let Some(background) = &request.background {
        tool.extra
            .insert("background".to_owned(), Value::String(background.clone()));
    }
    if let Some(output_format) = &request.output_format {
        tool.extra.insert(
            "output_format".to_owned(),
            Value::String(output_format.clone()),
        );
    }
    if let Some(n) = request.n {
        tool.extra.insert("n".to_owned(), Value::from(n));
    }

    ResponsesRequest {
        model: request.model.clone(),
        input: Some(ResponseInput::Text(request.prompt.clone())),
        instructions: None,
        stream: Some(false),
        temperature: None,
        top_p: None,
        tools: Some(vec![tool]),
        tool_choice: Some(json!({"type": "image_generation"})),
        service_tier: None,
        reasoning: None,
        max_output_tokens: None,
        parallel_tool_calls: Some(false),
        store: Some(false),
        previous_response_id: None,
        metadata: None,
        extra: request.extra.clone(),
    }
}

fn chat_message_to_response_input_item(message: ChatMessage) -> ResponseInputItem {
    ResponseInputItem::Message(ResponseMessageInputItem {
        kind: Some("message".to_owned()),
        role: message.role,
        content: message.content.map_or_else(
            || ResponseInputContent::Parts(Vec::new()),
            chat_content_to_response_input_content,
        ),
        id: None,
        name: message.name,
        tool_call_id: message.tool_call_id,
    })
}

fn chat_content_to_response_input_content(content: ChatContent) -> ResponseInputContent {
    match content {
        ChatContent::Text(text) => ResponseInputContent::Parts(vec![json_text_input_part(&text)]),
        ChatContent::Parts(parts) => ResponseInputContent::Parts(
            parts
                .into_iter()
                .filter_map(|part| match part.kind.as_str() {
                    "text" => part.text.map(|text| json_text_input_part(&text)),
                    "image_url" => {
                        part.image_url
                            .map(|image| crate::openai::types::ResponseInputContentPart {
                                kind: "input_image".to_owned(),
                                text: None,
                                image_url: Some(image.url),
                                detail: image.detail,
                            })
                    }
                    _ => None,
                })
                .collect(),
        ),
    }
}

fn maybe_prepend_instructions(messages: &mut Vec<ChatMessage>, instructions: Option<&str>) {
    if let Some(instructions) = instructions {
        messages.insert(
            0,
            ChatMessage {
                role: "system".to_owned(),
                content: Some(ChatContent::Text(instructions.to_owned())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        );
    }
}

fn collect_response_input_items(
    request: &ResponsesRequest,
    previous: Option<&crate::server::store::StoredResponse>,
) -> Result<Vec<Value>> {
    let mut input_items = previous
        .map(|stored| stored.input_items.clone())
        .unwrap_or_default();

    match request.input.as_ref() {
        Some(ResponseInput::Text(text)) => {
            input_items.push(serde_json::to_value(ResponseInputItem::Message(
                ResponseMessageInputItem {
                    kind: Some("message".to_owned()),
                    role: "user".to_owned(),
                    content: ResponseInputContent::Parts(vec![json_text_input_part(text)]),
                    id: None,
                    name: None,
                    tool_call_id: None,
                },
            ))?);
        }
        Some(ResponseInput::Items(items)) => {
            for item in items {
                input_items.push(serde_json::to_value(item)?);
            }
        }
        None => {}
    }

    Ok(input_items)
}

fn response_input_items_to_chat_messages(input_items: &[Value]) -> Vec<ChatMessage> {
    input_items
        .iter()
        .filter_map(|item| serde_json::from_value::<ResponseInputItem>(item.clone()).ok())
        .filter_map(|item| response_input_item_to_chat_message(&item))
        .collect()
}

fn response_input_item_to_chat_message(item: &ResponseInputItem) -> Option<ChatMessage> {
    match item {
        ResponseInputItem::Message(message) => Some(ChatMessage {
            role: message.role.clone(),
            content: Some(response_input_content_to_chat(&message.content)),
            name: message.name.clone(),
            tool_call_id: message.tool_call_id.clone(),
            tool_calls: None,
        }),
        ResponseInputItem::Compaction(compaction) => {
            decode_compaction_summary(&compaction.encrypted_content).map(|summary| ChatMessage {
                role: "developer".to_owned(),
                content: Some(ChatContent::Text(summary)),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            })
        }
    }
}

fn response_input_content_to_chat(content: &ResponseInputContent) -> ChatContent {
    match content {
        ResponseInputContent::Text(text) => ChatContent::Text(text.clone()),
        ResponseInputContent::Parts(parts) => ChatContent::Parts(
            parts
                .iter()
                .filter_map(|part| match part.kind.as_str() {
                    "text" | "input_text" | "output_text" => Some(ChatContentPart {
                        kind: "text".to_owned(),
                        text: part.text.clone(),
                        image_url: None,
                    }),
                    "input_image" | "image_url" => {
                        part.image_url.as_ref().map(|image_url| ChatContentPart {
                            kind: "image_url".to_owned(),
                            text: None,
                            image_url: Some(ImageUrl {
                                url: image_url.clone(),
                                detail: part.detail.clone(),
                            }),
                        })
                    }
                    _ => None,
                })
                .collect(),
        ),
    }
}

fn stored_response_messages(stored: &crate::server::store::StoredResponse) -> Vec<ChatMessage> {
    response_input_items_to_chat_messages(&stored.input_items)
}

fn estimate_response_input_tokens(input_items: &[Value]) -> u32 {
    let mut text = String::new();
    for item in input_items {
        if item.get("type").and_then(Value::as_str) == Some("compaction") {
            if let Some(content) = item.get("encrypted_content").and_then(Value::as_str) {
                if let Some(summary) = decode_compaction_summary(content) {
                    text.push_str(&summary);
                } else {
                    text.push_str(content);
                }
            }
        } else {
            if let Some(role) = item.get("role").and_then(Value::as_str) {
                text.push_str(role);
            }
            if let Some(content) = item.get("content") {
                match content {
                    Value::String(value) => text.push_str(value),
                    Value::Array(parts) => {
                        for part in parts {
                            if let Some(value) = part.get("text").and_then(Value::as_str) {
                                text.push_str(value);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    let estimated = text.chars().count().saturating_div(4).max(1);
    u32::try_from(estimated).unwrap_or(u32::MAX)
}

fn json_text_input_part(text: &str) -> crate::openai::types::ResponseInputContentPart {
    crate::openai::types::ResponseInputContentPart {
        kind: "input_text".to_owned(),
        text: Some(text.to_owned()),
        image_url: None,
        detail: None,
    }
}

fn compact_response_items(input_items: &[Value]) -> Vec<Value> {
    let mut output = input_items
        .iter()
        .filter(|item| is_compactable_message(item))
        .cloned()
        .collect::<Vec<_>>();
    output.push(json!({
        "type": "compaction",
        "encrypted_content": local_compaction_payload(&output),
    }));
    output
}

fn is_compactable_message(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("message")
        && matches!(
            item.get("role").and_then(Value::as_str),
            Some("user" | "developer")
        )
}

fn local_compaction_payload(items: &[Value]) -> String {
    let summary = items
        .iter()
        .filter_map(compaction_text_for_item)
        .collect::<Vec<_>>()
        .join("\n");
    let summary = truncate_summary(&summary, 1_024);
    let payload = json!({
        "provider": "codexia",
        "version": 1,
        "summary": summary,
    });
    STANDARD.encode(payload.to_string())
}

fn compaction_text_for_item(item: &Value) -> Option<String> {
    let role = item.get("role").and_then(Value::as_str)?;
    let content = item.get("content")?;
    let text = match content {
        Value::String(value) => value.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    };
    Some(format!("{role}: {text}"))
}

fn truncate_summary(text: &str, max_len: usize) -> String {
    if text.chars().count() <= max_len {
        return text.to_owned();
    }

    text.chars().take(max_len).collect()
}

fn decode_compaction_summary(payload: &str) -> Option<String> {
    let decoded = STANDARD.decode(payload).ok()?;
    let value = serde_json::from_slice::<Value>(&decoded).ok()?;
    value
        .get("summary")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn response_object_from_chat(
    request: &ResponsesRequest,
    response: crate::openai::response::ChatCompletionResponse,
) -> ResponseObject {
    let choice = response.choices.into_iter().next().unwrap_or_else(|| {
        crate::openai::response::ChatChoice {
            index: 0,
            message: crate::openai::response::AssistantMessage {
                role: "assistant",
                content: None,
                tool_calls: None,
                images: None,
            },
            finish_reason: "stop".to_owned(),
        }
    });
    let output = build_responses_output_items(
        &response.id,
        choice.message.content.as_deref().unwrap_or_default(),
        choice.message.tool_calls.unwrap_or_default(),
        choice.message.images.unwrap_or_default(),
    );

    ResponseObject {
        id: response.id.replace("chatcmpl", "resp"),
        object: "response",
        created_at: response.created,
        status: "completed",
        error: None,
        incomplete_details: None,
        instructions: request.instructions.clone(),
        max_output_tokens: request.max_output_tokens,
        model: response.model,
        output,
        parallel_tool_calls: request.parallel_tool_calls(),
        store: request.should_store(),
        temperature: request.temperature,
        tool_choice: request.tool_choice.clone(),
        tools: request
            .tools
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|tool| serde_json::to_value(tool).ok())
            .collect(),
        usage: response.usage,
        metadata: request.metadata.clone(),
        previous_response_id: request.previous_response_id.clone(),
    }
}

fn response_object_from_upstream(request: &ResponsesRequest, response: &Value) -> ResponseObject {
    let created_at = response
        .get("created_at")
        .and_then(Value::as_i64)
        .unwrap_or_else(crate::config::now_unix);
    let output = response_output_items_from_upstream(
        response
            .get("output")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice),
    );

    ResponseObject {
        id: response
            .get("id")
            .and_then(Value::as_str)
            .map_or_else(build_response_id, str::to_owned),
        object: "response",
        created_at,
        status: "completed",
        error: None,
        incomplete_details: None,
        instructions: request.instructions.clone(),
        max_output_tokens: request.max_output_tokens,
        model: response
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(&request.model)
            .to_owned(),
        output,
        parallel_tool_calls: request.parallel_tool_calls(),
        store: request.should_store(),
        temperature: request.temperature,
        tool_choice: request.tool_choice.clone(),
        tools: response_tool_values(request.tools.clone()),
        usage: parse_upstream_usage(response.get("usage")),
        metadata: request.metadata.clone(),
        previous_response_id: request.previous_response_id.clone(),
    }
}

async fn maybe_store_response(
    state: &AppState,
    request: &ResponsesRequest,
    response: ResponseObject,
    input_items: Vec<Value>,
) {
    if request.should_store() {
        state
            .responses
            .insert(crate::server::store::StoredResponse {
                input_items: stored_response_input_items(input_items, &response),
                response,
            })
            .await;
    }
}

fn stored_response_input_items(
    mut input_items: Vec<Value>,
    response: &ResponseObject,
) -> Vec<Value> {
    input_items.extend(response_output_to_input_items(&response.output));
    input_items
}

fn build_response_id() -> String {
    format!(
        "resp_{}_{:08x}",
        crate::config::now_unix(),
        rand::random::<u32>()
    )
}

fn build_batch_id() -> String {
    format!(
        "msgbatch_{}_{:08x}",
        crate::config::now_unix(),
        rand::random::<u32>()
    )
}

fn batch_results_url(headers: &HeaderMap, batch_id: &str) -> String {
    let host = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("127.0.0.1:14550");
    format!("http://{host}/v1/messages/batches/{batch_id}/results")
}

async fn run_message_batch_worker(
    batches: crate::server::store::BatchStore,
    token_manager: crate::token::TokenManager,
    codex: crate::codex::client::CodexClient,
    batch_id: String,
    requests: Vec<MessageBatchRequest>,
) {
    let mut requests = std::collections::VecDeque::from(requests);
    while let Some(item) = requests.pop_front() {
        if batches.cancel_requested(&batch_id).await.unwrap_or(false) {
            let mut canceled = vec![MessageBatchResult {
                custom_id: item.custom_id,
                result: MessageBatchResultType::Canceled,
            }];
            canceled.extend(requests.into_iter().map(|pending| MessageBatchResult {
                custom_id: pending.custom_id,
                result: MessageBatchResultType::Canceled,
            }));
            finalize_canceled_batch(&batches, &batch_id, canceled).await;
            return;
        }

        let result = match to_openai_request(&item.params) {
            Ok(openai_request) => match token_manager.credentials().await {
                Ok(credentials) => match codex.complete_chat(openai_request, &credentials).await {
                    Ok(response) => MessageBatchResult {
                        custom_id: item.custom_id,
                        result: MessageBatchResultType::Succeeded {
                            message: from_openai_response(response),
                        },
                    },
                    Err(error) => MessageBatchResult {
                        custom_id: item.custom_id,
                        result: MessageBatchResultType::Errored {
                            error: error_body(&error),
                        },
                    },
                },
                Err(error) => MessageBatchResult {
                    custom_id: item.custom_id,
                    result: MessageBatchResultType::Errored {
                        error: error_body(&error),
                    },
                },
            },
            Err(error) => MessageBatchResult {
                custom_id: item.custom_id,
                result: MessageBatchResultType::Errored {
                    error: error_body(&error),
                },
            },
        };

        batches
            .update(&batch_id, move |stored| {
                stored.results.push(result.clone());
                match &result.result {
                    MessageBatchResultType::Succeeded { .. } => {
                        stored.batch.request_counts.succeeded =
                            stored.batch.request_counts.succeeded.saturating_add(1);
                    }
                    MessageBatchResultType::Errored { .. } => {
                        stored.batch.request_counts.errored =
                            stored.batch.request_counts.errored.saturating_add(1);
                    }
                    MessageBatchResultType::Canceled => {
                        stored.batch.request_counts.canceled =
                            stored.batch.request_counts.canceled.saturating_add(1);
                    }
                }
                stored.batch.request_counts.processing =
                    stored.batch.request_counts.processing.saturating_sub(1);
            })
            .await;
    }

    let _ = batches
        .update(&batch_id, |stored| {
            stored.batch.processing_status = "ended";
            stored.batch.ended_at = Some(chrono::Utc::now().to_rfc3339());
            stored.cancel_requested = false;
        })
        .await;
}

async fn finalize_canceled_batch(
    batches: &crate::server::store::BatchStore,
    batch_id: &str,
    canceled_results: Vec<MessageBatchResult>,
) {
    let remaining = u32::try_from(canceled_results.len()).unwrap_or(u32::MAX);
    let _ = batches
        .update(batch_id, move |stored| {
            stored.results.extend(canceled_results);
            stored.batch.request_counts.canceled = stored
                .batch
                .request_counts
                .canceled
                .saturating_add(remaining);
            stored.batch.request_counts.processing = stored
                .batch
                .request_counts
                .processing
                .saturating_sub(remaining);
            stored.batch.processing_status = "ended";
            stored.batch.ended_at = Some(chrono::Utc::now().to_rfc3339());
            stored.cancel_requested = false;
        })
        .await;
}

fn build_responses_output_items(
    response_id: &str,
    output_text: &str,
    tool_calls: Vec<crate::openai::types::ToolCall>,
    images: Vec<GeneratedImage>,
) -> Vec<crate::openai::response::ResponseOutputItem> {
    let mut output = vec![response_message_item(
        format!("msg_{response_id}"),
        Some(output_text.to_owned()),
    )];
    for (index, tool_call) in tool_calls.into_iter().enumerate() {
        output.push(response_function_call_item(
            format!("fc_{response_id}_{index}"),
            tool_call,
        ));
    }
    for (index, image) in images.into_iter().enumerate() {
        output.push(response_image_generation_item(
            format!("ig_{response_id}_{index}"),
            image.b64_json,
            image.revised_prompt,
        ));
    }
    output
}

fn response_tool_values(tools: Option<Vec<crate::openai::types::ChatTool>>) -> Vec<Value> {
    tools
        .unwrap_or_default()
        .into_iter()
        .filter_map(|tool| serde_json::to_value(tool).ok())
        .collect()
}

fn response_output_items_from_upstream(
    items: &[Value],
) -> Vec<crate::openai::response::ResponseOutputItem> {
    let mut output = Vec::new();
    for (index, item) in items.iter().enumerate() {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                let text = item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                        Some("output_text") => part.get("text").and_then(Value::as_str),
                        Some("refusal") => part.get("refusal").and_then(Value::as_str),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                output.push(response_message_item(
                    item.get("id")
                        .and_then(Value::as_str)
                        .map_or_else(|| format!("msg_{index}"), str::to_owned),
                    Some(text),
                ));
            }
            Some("function_call") => {
                let tool_call = crate::openai::types::ToolCall {
                    id: item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    kind: "function".to_owned(),
                    function: crate::openai::types::FunctionCall {
                        name: item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        arguments: item
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}")
                            .to_owned(),
                    },
                };
                output.push(response_function_call_item(
                    item.get("id")
                        .and_then(Value::as_str)
                        .map_or_else(|| format!("fc_{index}"), str::to_owned),
                    tool_call,
                ));
            }
            Some("image_generation_call") => {
                if let Some(image) = crate::openai::response::generated_image_from_item(item) {
                    output.push(response_image_generation_item(
                        item.get("id")
                            .and_then(Value::as_str)
                            .map_or_else(|| format!("ig_{index}"), str::to_owned),
                        image.b64_json,
                        image.revised_prompt,
                    ));
                }
            }
            _ => {}
        }
    }
    output
}

fn response_output_to_input_items(
    items: &[crate::openai::response::ResponseOutputItem],
) -> Vec<Value> {
    let mut output = Vec::new();
    for item in items {
        match item.kind {
            "message" => {
                let content = item
                    .content
                    .iter()
                    .map(|part| {
                        json!({
                            "type": "output_text",
                            "text": part.text,
                            "annotations": part.annotations,
                        })
                    })
                    .collect::<Vec<_>>();
                output.push(json!({
                    "type": "message",
                    "role": item.role.unwrap_or("assistant"),
                    "status": item.status,
                    "id": item.id,
                    "content": content,
                }));
            }
            "function_call" => output.push(json!({
                "type": "function_call",
                "id": item.id,
                "call_id": item.call_id,
                "name": item.name,
                "arguments": item.arguments,
            })),
            "image_generation_call" => output.push(json!({
                "type": "image_generation_call",
                "id": item.id,
                "result": item.result,
                "revised_prompt": item.revised_prompt,
            })),
            _ => {}
        }
    }
    output
}

fn parse_upstream_usage(value: Option<&Value>) -> Option<crate::openai::response::Usage> {
    let value = value?;
    let prompt_tokens = value
        .get("input_tokens")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0);
    let completion_tokens = value
        .get("output_tokens")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0);
    let total_tokens = value
        .get("total_tokens")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or_else(|| prompt_tokens.saturating_add(completion_tokens));

    Some(crate::openai::response::Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens,
    })
}

fn response_object(
    request: &ResponsesRequest,
    response_id: String,
    created_at: i64,
    status: &'static str,
    output: Vec<crate::openai::response::ResponseOutputItem>,
    usage: Option<crate::openai::response::Usage>,
) -> ResponseObject {
    ResponseObject {
        id: response_id,
        object: "response",
        created_at,
        status,
        error: None,
        incomplete_details: None,
        instructions: request.instructions.clone(),
        max_output_tokens: request.max_output_tokens,
        model: request.model.clone(),
        output,
        parallel_tool_calls: request.parallel_tool_calls(),
        store: request.should_store(),
        temperature: request.temperature,
        tool_choice: request.tool_choice.clone(),
        tools: response_tool_values(request.tools.clone()),
        usage,
        metadata: request.metadata.clone(),
        previous_response_id: request.previous_response_id.clone(),
    }
}

fn response_created_event(response: &ResponseObject) -> Event {
    Event::default().event("response.created").data(
        json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": response,
        })
        .to_string(),
    )
}

fn response_output_text_delta_event(
    sequence_number: u64,
    response_id: &str,
    item_id: &str,
    text: &str,
) -> Event {
    Event::default().event("response.output_text.delta").data(
        json!({
            "type": "response.output_text.delta",
            "sequence_number": sequence_number,
            "response_id": response_id,
            "item_id": item_id,
            "output_index": 0,
            "content_index": 0,
            "delta": text,
        })
        .to_string(),
    )
}

fn response_output_text_done_event(sequence_number: u64, item_id: &str, text: &str) -> Event {
    Event::default().event("response.output_text.done").data(
        json!({
            "type": "response.output_text.done",
            "sequence_number": sequence_number,
            "item_id": item_id,
            "output_index": 0,
            "content_index": 0,
            "text": text,
        })
        .to_string(),
    )
}

fn response_completed_event(
    sequence_number: u64,
    finish_reason: &str,
    response: &ResponseObject,
) -> Event {
    Event::default().event("response.completed").data(
        json!({
            "type": "response.completed",
            "sequence_number": sequence_number,
            "finish_reason": finish_reason,
            "response": response,
        })
        .to_string(),
    )
}

fn response_error_event(error: &crate::Error) -> Event {
    Event::default().event("error").data(
        json!({
            "type": "error",
            "error": {
                "message": error.to_string(),
                "type": "upstream_error",
            }
        })
        .to_string(),
    )
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

fn openai_responses_sse(
    stream: Pin<
        Box<dyn Stream<Item = Result<crate::openai::response::ChatCompletionChunk>> + Send>,
    >,
    response_id: String,
    request: ResponsesRequest,
    input_items: Vec<Value>,
    store: crate::server::store::ResponseStore,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>>> {
    let mapped = async_stream::stream! {
        let created_at = crate::config::now_unix();
        let output_item_id = format!("msg_{response_id}");
        let created_response = response_object(
            &request,
            response_id.clone(),
            created_at,
            "in_progress",
            Vec::new(),
            None,
        );
        yield Ok(response_created_event(&created_response));

        let mut stream = stream;
        let mut sequence_number = 1_u64;
        let mut output_text = String::new();
        let mut tool_calls = Vec::new();

        while let Some(item) = stream.next().await {
            match item {
                Ok(chunk) => {
                    let Some(choice) = chunk.choices.into_iter().next() else {
                        continue;
                    };

                    if let Some(text) = choice.delta.content {
                        if !text.is_empty() {
                            output_text.push_str(&text);
                            yield Ok(response_output_text_delta_event(
                                sequence_number,
                                &response_id,
                                &output_item_id,
                                &text,
                            ));
                            sequence_number = sequence_number.saturating_add(1);
                        }
                    }

                    for tool_call in choice.delta.tool_calls.into_iter().flatten() {
                        tool_calls.push(crate::openai::types::ToolCall {
                            id: tool_call.id,
                            kind: tool_call.kind.to_owned(),
                            function: tool_call.function,
                        });
                    }

                    if let Some(reason) = choice.finish_reason {
                        let output = build_responses_output_items(
                            &response_id,
                            &output_text,
                            tool_calls,
                            Vec::new(),
                        );
                        let completed = response_object(
                            &request,
                            response_id.clone(),
                            created_at,
                            "completed",
                            output,
                            None,
                        );
                        if request.should_store() {
                            let stored_items =
                                stored_response_input_items(input_items.clone(), &completed);
                            store.insert(crate::server::store::StoredResponse {
                                response: completed.clone(),
                                input_items: stored_items,
                            }).await;
                        }
                        yield Ok(response_output_text_done_event(
                            sequence_number,
                            &output_item_id,
                            &output_text,
                        ));
                        sequence_number = sequence_number.saturating_add(1);
                        yield Ok(response_completed_event(sequence_number, &reason, &completed));
                        return;
                    }
                }
                Err(error) => {
                    yield Ok(response_error_event(&error));
                    return;
                }
            }
        }
    };

    Sse::new(mapped).keep_alive(KeepAlive::default())
}

fn openai_raw_responses_sse(
    stream: Pin<Box<dyn Stream<Item = Result<crate::codex::sse::JsonSseEvent>> + Send>>,
    request: ResponsesRequest,
    input_items: Vec<Value>,
    store: crate::server::store::ResponseStore,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>>> {
    let mapped = async_stream::stream! {
        let mut stream = stream;

        while let Some(item) = stream.next().await {
            match item {
                Ok(item) => {
                    if is_done_event(&item.value) {
                        if let Some(response) = item.value.get("response").cloned() {
                            let completed = response_object_from_upstream(&request, &response);
                            if request.should_store() {
                                let stored_items =
                                    stored_response_input_items(input_items.clone(), &completed);
                                store.insert(crate::server::store::StoredResponse {
                                    response: completed,
                                    input_items: stored_items,
                                }).await;
                            }
                        }
                    }

                    let event_name = item
                        .event
                        .clone()
                        .or_else(|| item.value.get("type").and_then(Value::as_str).map(str::to_owned))
                        .unwrap_or_else(|| "message".to_owned());
                    yield Ok(Event::default()
                        .event(event_name)
                        .data(serde_json::to_string(&item.value).unwrap_or_default()));
                }
                Err(error) => {
                    yield Ok(response_error_event(&error));
                    return;
                }
            }
        }
    };

    Sse::new(mapped).keep_alive(KeepAlive::default())
}

fn anthropic_sse_response(
    stream: Pin<
        Box<dyn Stream<Item = Result<crate::openai::response::ChatCompletionChunk>> + Send>,
    >,
    model: String,
    input_tokens: u32,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>>> {
    let mapped = async_stream::stream! {
        macro_rules! yield_event_or_error {
            ($event:expr) => {
                match $event {
                    Ok(event) => yield Ok(event),
                    Err(error) => {
                        yield Ok(anthropic_error_event(&error));
                        return;
                    }
                }
            };
        }

        let id = format!("msg_{}", rand::random::<u64>());
        let mut stream = stream;
        let mut current_index = 0_u32;
        let mut text_open = false;
        let mut output_tokens = 0_u32;

        yield_event_or_error!(message_start_event(&id, &model, input_tokens));

        while let Some(item) = stream.next().await {
            match item {
                Ok(chunk) => {
                    let Some(choice) = chunk.choices.into_iter().next() else {
                        continue;
                    };

                    if let Some(text) = choice.delta.content {
                        if !text.is_empty() {
                            output_tokens =
                                output_tokens.saturating_add(estimate_stream_tokens(&text));
                            if !text_open {
                                yield_event_or_error!(text_block_start(current_index));
                                text_open = true;
                            }
                            yield_event_or_error!(text_delta(current_index, &text));
                        }
                    }

                    for tool_call in choice.delta.tool_calls.into_iter().flatten() {
                        if text_open {
                            yield_event_or_error!(content_block_stop(current_index));
                            current_index += 1;
                            text_open = false;
                        }

                        let tool_call = crate::openai::types::ToolCall {
                            id: tool_call.id,
                            kind: tool_call.kind.to_owned(),
                            function: tool_call.function,
                        };
                        output_tokens = output_tokens
                            .saturating_add(estimate_stream_tokens(&tool_call.function.arguments));

                        for event in [
                            tool_block_start(current_index, &tool_call),
                            tool_json_delta(current_index, &tool_call.function.arguments),
                            content_block_stop(current_index),
                        ] {
                            yield_event_or_error!(event);
                        }
                        current_index += 1;
                    }

                    if let Some(reason) = choice.finish_reason {
                        if text_open {
                            yield_event_or_error!(content_block_stop(current_index));
                        }
                        for event in [
                            message_delta_event(&reason, output_tokens),
                            message_stop_event(),
                        ] {
                            yield_event_or_error!(event);
                        }
                        return;
                    }
                }
                Err(error) => {
                    yield Ok(anthropic_error_event(&error));
                    return;
                }
            }
        }

        if text_open {
            yield_event_or_error!(content_block_stop(current_index));
        }
        for event in [message_delta_event("stop", output_tokens), message_stop_event()] {
            yield_event_or_error!(event);
        }
    };

    Sse::new(mapped).keep_alive(KeepAlive::default())
}

fn anthropic_raw_image_sse_response(
    stream: Pin<Box<dyn Stream<Item = Result<crate::codex::sse::JsonSseEvent>> + Send>>,
    model: String,
    input_tokens: u32,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>>> {
    let mapped = async_stream::stream! {
        macro_rules! yield_event_or_error {
            ($event:expr) => {
                match $event {
                    Ok(event) => yield Ok(event),
                    Err(error) => {
                        yield Ok(anthropic_error_event(&error));
                        return;
                    }
                }
            };
        }

        let id = format!("msg_{}", rand::random::<u64>());
        let mut stream = stream;
        let mut current_index = 0_u32;
        let mut output_tokens = 0_u32;

        yield_event_or_error!(message_start_event(&id, &model, input_tokens));

        while let Some(item) = stream.next().await {
            match item {
                Ok(item) => {
                    if !is_done_event(&item.value) {
                        continue;
                    }

                    let Some(response) = item.value.get("response") else {
                        continue;
                    };

                    output_tokens = parse_upstream_usage(response.get("usage"))
                        .map_or(0, |usage| usage.completion_tokens);

                    let output_items = response
                        .get("output")
                        .and_then(Value::as_array)
                        .map_or(&[] as &[Value], Vec::as_slice);
                    let images = generated_images_from_output(output_items);

                    for image in images {
                        let source = crate::anthropic::ImageSource {
                            kind: "base64".to_owned(),
                            media_type: image.media_type.or_else(|| Some("image/png".to_owned())),
                            data: Some(image.b64_json),
                        };
                        yield_event_or_error!(image_block_start(current_index, &source));
                        yield_event_or_error!(content_block_stop(current_index));
                        current_index += 1;
                    }

                    for event in [
                        message_delta_event("end_turn", output_tokens),
                        message_stop_event(),
                    ] {
                        yield_event_or_error!(event);
                    }
                    return;
                }
                Err(error) => {
                    yield Ok(anthropic_error_event(&error));
                    return;
                }
            }
        }

        for event in [message_delta_event("end_turn", output_tokens), message_stop_event()] {
            yield_event_or_error!(event);
        }
    };

    Sse::new(mapped).keep_alive(KeepAlive::default())
}

fn anthropic_error_response(error: &crate::Error) -> Response {
    (error.status_code(), Json(error_body(error))).into_response()
}

fn anthropic_error_event(error: &crate::Error) -> Event {
    Event::default()
        .event("error")
        .data(error_body(error).to_string())
}

fn estimate_stream_tokens(text: &str) -> u32 {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        0
    } else {
        u32::try_from(trimmed.chars().count())
            .unwrap_or(u32::MAX)
            .saturating_div(4)
            .max(1)
    }
}
