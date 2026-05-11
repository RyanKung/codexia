use super::responses::{
    build_responses_output_items, response_object, response_object_from_upstream,
    stored_response_input_items,
};
use crate::{
    anthropic::{
        content_block_stop, error_body, image_block_start, message_delta_event,
        message_start_event, message_stop_event, signature_delta, text_block_start, text_delta,
        thinking_block_start, thinking_delta, tool_block_start, tool_json_delta,
    },
    codex::events::{event_error, finish_reason, is_done_event},
    error::Result,
    openai::response::{GeneratedImage, ResponseObject, generated_images_from_output},
};
use axum::response::{
    IntoResponse,
    sse::{Event, KeepAlive, Sse},
};
use futures_util::{Stream, StreamExt, stream};
use serde_json::{Value, json};
use std::{convert::Infallible, pin::Pin};

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

pub(super) fn sse_response(
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

pub(super) fn openai_responses_sse(
    stream: Pin<
        Box<dyn Stream<Item = Result<crate::openai::response::ChatCompletionChunk>> + Send>,
    >,
    response_id: String,
    request: crate::openai::types::ResponsesRequest,
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
                            Vec::<GeneratedImage>::new(),
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

pub(super) fn openai_raw_responses_sse(
    stream: Pin<Box<dyn Stream<Item = Result<crate::codex::sse::JsonSseEvent>> + Send>>,
    request: crate::openai::types::ResponsesRequest,
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

#[allow(clippy::too_many_lines)]
pub(super) fn anthropic_raw_messages_sse_response(
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
        let mut open_text_blocks = std::collections::BTreeSet::<u32>::new();
        let mut open_tool_blocks = std::collections::BTreeSet::<u32>::new();
        let mut open_thinking_blocks = std::collections::BTreeSet::<u32>::new();
        let mut seen_tool_blocks = std::collections::BTreeSet::<u32>::new();
        let mut tool_meta = std::collections::HashMap::<String, ToolMeta>::new();
        let mut usage = crate::anthropic::ResponseUsage {
            input_tokens,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            output_tokens: 0,
            server_tool_use: None,
        };

        yield_event_or_error!(message_start_event(&id, &model, input_tokens));

        while let Some(item) = stream.next().await {
            match item {
                Ok(item) => {
                    if let Some(message) = event_error(&item.value) {
                        yield Ok(anthropic_error_event(&crate::Error::upstream(message)));
                        return;
                    }

                    match item.value.get("type").and_then(Value::as_str) {
                        Some("ping") => {
                            yield Ok(Event::default().event("ping").data("{\"type\":\"ping\"}"));
                        }
                        Some("response.output_text.delta") => {
                            let index = output_index(&item.value);
                            let delta = item
                                .value
                                .get("delta")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            if !delta.is_empty() {
                                if open_text_blocks.insert(index) {
                                    yield_event_or_error!(text_block_start(index));
                                }
                                usage.output_tokens =
                                    usage.output_tokens.saturating_add(estimate_stream_tokens(delta));
                                yield_event_or_error!(text_delta(index, delta));
                            }
                        }
                        Some("response.output_text.done") => {
                            let index = output_index(&item.value);
                            if open_text_blocks.remove(&index) {
                                yield_event_or_error!(content_block_stop(index));
                            }
                        }
                        Some("response.output_item.added") => {
                            if let Some(tool_call) = tool_call_from_event_item(&item.value) {
                                let index = output_index(&item.value);
                                cache_tool_meta(&mut tool_meta, &item.value, &tool_call);
                                if open_tool_blocks.insert(index) {
                                    seen_tool_blocks.insert(index);
                                    yield_event_or_error!(tool_block_start(index, &tool_call));
                                }
                            }
                        }
                        Some("response.function_call_arguments.delta") => {
                            let index = output_index(&item.value);
                            let item_id = item.value.get("item_id").and_then(Value::as_str);
                            let delta = item
                                .value
                                .get("delta")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            if let Some(meta) = item_id.and_then(|id| tool_meta.get(id)) {
                                if open_tool_blocks.insert(index) {
                                    seen_tool_blocks.insert(index);
                                    let tool_call = crate::openai::types::ToolCall {
                                        id: meta.id.clone(),
                                        kind: "function".to_owned(),
                                        function: crate::openai::types::FunctionCall {
                                            name: meta.name.clone(),
                                            arguments: String::new(),
                                        },
                                    };
                                    yield_event_or_error!(tool_block_start(index, &tool_call));
                                }
                            }
                            if !delta.is_empty() {
                                usage.output_tokens =
                                    usage.output_tokens.saturating_add(estimate_stream_tokens(delta));
                                yield_event_or_error!(tool_json_delta(index, delta));
                            }
                        }
                        Some("response.function_call_arguments.done") => {
                            let index = output_index(&item.value);
                            if open_tool_blocks.remove(&index) {
                                yield_event_or_error!(content_block_stop(index));
                            }
                        }
                        Some("response.output_item.done") => {
                            if let Some(tool_call) = tool_call_from_event_item(&item.value) {
                                let index = output_index(&item.value);
                                let first_seen = seen_tool_blocks.insert(index);
                                if first_seen {
                                    if open_tool_blocks.insert(index) {
                                        yield_event_or_error!(tool_block_start(index, &tool_call));
                                    }
                                    if !tool_call.function.arguments.is_empty() {
                                        usage.output_tokens = usage.output_tokens.saturating_add(
                                            estimate_stream_tokens(&tool_call.function.arguments),
                                        );
                                        yield_event_or_error!(tool_json_delta(
                                            index,
                                            &tool_call.function.arguments,
                                        ));
                                    }
                                }
                                if open_tool_blocks.remove(&index) || first_seen {
                                    yield_event_or_error!(content_block_stop(index));
                                }
                            } else if let Some(thinking) = reasoning_text_from_event_item(&item.value) {
                                let index = output_index(&item.value);
                                if open_thinking_blocks.insert(index) {
                                    yield_event_or_error!(thinking_block_start(index));
                                }
                                if !thinking.is_empty() {
                                    usage.output_tokens = usage
                                        .output_tokens
                                        .saturating_add(estimate_stream_tokens(&thinking));
                                    yield_event_or_error!(thinking_delta(index, &thinking));
                                }
                                if let Some(signature) = reasoning_signature_from_event_item(&item.value) {
                                    yield_event_or_error!(signature_delta(index, &signature));
                                }
                                if open_thinking_blocks.remove(&index) {
                                    yield_event_or_error!(content_block_stop(index));
                                }
                            }
                        }
                        Some(
                            "response.reasoning_text.delta"
                            | "response.reasoning_summary_text.delta",
                        ) => {
                            let index = output_index(&item.value);
                            let delta = item
                                .value
                                .get("delta")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            if !delta.is_empty() {
                                if open_thinking_blocks.insert(index) {
                                    yield_event_or_error!(thinking_block_start(index));
                                }
                                usage.output_tokens =
                                    usage.output_tokens.saturating_add(estimate_stream_tokens(delta));
                                yield_event_or_error!(thinking_delta(index, delta));
                            }
                        }
                        _ => {}
                    }
                    if is_done_event(&item.value) {
                        let mut stop_reason = finish_reason(&item.value);
                        if let Some(response) = item.value.get("response") {
                            merge_final_anthropic_usage(&mut usage, response.get("usage"));
                            if response_has_function_call(response) {
                                "tool_calls".clone_into(&mut stop_reason);
                            }

                            for (index, image) in generated_images_from_output(
                                response
                                    .get("output")
                                    .and_then(Value::as_array)
                                    .map_or(&[] as &[Value], Vec::as_slice),
                            )
                            .into_iter()
                            .enumerate()
                            {
                                let source = crate::anthropic::ImageSource {
                                    kind: "base64".to_owned(),
                                    media_type: image.media_type.or_else(|| Some("image/png".to_owned())),
                                    data: Some(image.b64_json),
                                    text: None,
                                    url: None,
                                    file_id: None,
                                };
                                let index = u32::try_from(index).unwrap_or(u32::MAX);
                                yield_event_or_error!(image_block_start(index, &source));
                                yield_event_or_error!(content_block_stop(index));
                            }
                        }

                        for index in std::mem::take(&mut open_text_blocks) {
                            yield_event_or_error!(content_block_stop(index));
                        }
                        for index in std::mem::take(&mut open_tool_blocks) {
                            yield_event_or_error!(content_block_stop(index));
                        }
                        for index in std::mem::take(&mut open_thinking_blocks) {
                            yield_event_or_error!(content_block_stop(index));
                        }

                        for event in [
                            message_delta_event(&stop_reason, &usage),
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

        for index in open_text_blocks {
            yield_event_or_error!(content_block_stop(index));
        }
        for index in open_tool_blocks {
            yield_event_or_error!(content_block_stop(index));
        }
        for index in open_thinking_blocks {
            yield_event_or_error!(content_block_stop(index));
        }
        for event in [message_delta_event("stop", &usage), message_stop_event()] {
            yield_event_or_error!(event);
        }
    };

    Sse::new(mapped).keep_alive(KeepAlive::default())
}

pub(super) fn anthropic_error_response(error: &crate::Error) -> axum::response::Response {
    (error.status_code(), axum::Json(error_body(error))).into_response()
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

fn output_index(event: &Value) -> u32 {
    event
        .get("output_index")
        .and_then(Value::as_u64)
        .and_then(|index| u32::try_from(index).ok())
        .unwrap_or(0)
}

fn tool_call_from_event_item(event: &Value) -> Option<crate::openai::types::ToolCall> {
    let item = event.get("item")?;
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return None;
    }

    Some(crate::openai::types::ToolCall {
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
                .unwrap_or_default()
                .to_owned(),
        },
    })
}

fn cache_tool_meta(
    tool_meta: &mut std::collections::HashMap<String, ToolMeta>,
    event: &Value,
    tool_call: &crate::openai::types::ToolCall,
) {
    tool_meta.insert(
        tool_call.id.clone(),
        ToolMeta {
            id: tool_call.id.clone(),
            name: tool_call.function.name.clone(),
        },
    );
    if let Some(item_id) = event
        .get("item")
        .and_then(|item| item.get("id"))
        .and_then(Value::as_str)
    {
        tool_meta.insert(
            item_id.to_owned(),
            ToolMeta {
                id: tool_call.id.clone(),
                name: tool_call.function.name.clone(),
            },
        );
    }
}

fn response_has_function_call(response: &Value) -> bool {
    response
        .get("output")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|entry| entry.get("type").and_then(Value::as_str) == Some("function_call"))
        })
}

fn merge_final_anthropic_usage(usage: &mut crate::anthropic::ResponseUsage, value: Option<&Value>) {
    if let Some(value) = value {
        if let Some(input_tokens) = value
            .get("input_tokens")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
        {
            usage.input_tokens = input_tokens;
        }
        if let Some(output_tokens) = value
            .get("output_tokens")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
        {
            usage.output_tokens = output_tokens;
        }
        usage.cache_creation_input_tokens = value
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok());
        usage.cache_read_input_tokens = value
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok());
        usage.server_tool_use = value
            .get("server_tool_use")
            .filter(|value| !value.is_null())
            .cloned();
    }
}

fn reasoning_text_from_event_item(event: &Value) -> Option<String> {
    let item = event.get("item")?;
    if item.get("type").and_then(Value::as_str) != Some("reasoning") {
        return None;
    }

    let summary_text = item
        .get("summary")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    if !summary_text.is_empty() {
        return Some(summary_text);
    }

    let content_text = item
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    (!content_text.is_empty()).then_some(content_text)
}

fn reasoning_signature_from_event_item(event: &Value) -> Option<String> {
    event
        .get("item")
        .and_then(|item| item.get("encrypted_content"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

#[derive(Clone)]
struct ToolMeta {
    id: String,
    name: String,
}
