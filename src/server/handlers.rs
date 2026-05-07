//! Route handlers and streaming response helpers for the HTTP server.

use crate::{
    anthropic::{
        CountTokensResponse, MessageBatch, MessageBatchCreateRequest, MessageBatchDeleted,
        MessageBatchListResponse, MessageBatchRequest, MessageBatchRequestCounts,
        MessageBatchResult, MessageBatchResultType, MessagesRequest, content_block_stop,
        error_body, estimate_input_tokens, from_openai_response, message_batch_list_response,
        message_delta_event, message_start_event, message_stop_event, models_response,
        text_block_start, text_delta, to_openai_request, tool_block_start, tool_json_delta,
    },
    error::Result,
    openai::{
        response::{
            ResponseCompaction, ResponseInputTokens, ResponseObject, response_function_call_item,
            response_message_item,
        },
        types::{
            ChatCompletionRequest, ChatContent, ChatContentPart, ChatMessage, ImageUrl,
            ResponseInput, ResponseInputContent, ResponseInputItem, ResponseMessageInputItem,
            ResponsesRequest,
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
            },
            finish_reason: "stop".to_owned(),
        }
    });
    let output = build_responses_output_items(
        &response.id,
        choice.message.content.as_deref().unwrap_or_default(),
        choice.message.tool_calls.unwrap_or_default(),
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
                response,
                input_items,
            })
            .await;
    }
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
    output
}

fn response_tool_values(tools: Option<Vec<crate::openai::types::ChatTool>>) -> Vec<Value> {
    tools
        .unwrap_or_default()
        .into_iter()
        .filter_map(|tool| serde_json::to_value(tool).ok())
        .collect()
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
                        let output = build_responses_output_items(&response_id, &output_text, tool_calls);
                        let completed = response_object(
                            &request,
                            response_id.clone(),
                            created_at,
                            "completed",
                            output,
                            None,
                        );
                        if request.should_store() {
                            store.insert(crate::server::store::StoredResponse {
                                response: completed.clone(),
                                input_items: input_items.clone(),
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
