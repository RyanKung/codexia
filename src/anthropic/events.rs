use super::{
    ImageSource, ResponseUsage, StreamingMessage, map_stop_reason, types::MessageStartEvent,
};
use crate::{Error, Result, openai::types::ToolCall};
use axum::response::sse::Event;
use serde::Serialize;
use serde_json::{Value, json};

/// Builds the `message_start` payload emitted ahead of streaming deltas.
///
/// # Errors
///
/// Returns an error when the SSE payload cannot be serialized to JSON.
pub fn message_start_event(id: &str, model: &str, input_tokens: u32) -> Result<Event> {
    sse_event(
        "message_start",
        &MessageStartEvent {
            kind: "message_start",
            message: StreamingMessage {
                id: id.to_owned(),
                kind: "message",
                role: "assistant",
                content: Vec::new(),
                model: model.to_owned(),
                stop_reason: None,
                stop_sequence: None,
                usage: ResponseUsage {
                    input_tokens,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                    output_tokens: 0,
                    server_tool_use: None,
                },
            },
        },
    )
}

/// Builds a `content_block_start` event for a text block.
///
/// # Errors
///
/// Returns an error when the SSE payload cannot be serialized to JSON.
pub fn text_block_start(index: u32) -> Result<Event> {
    sse_event(
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {"type": "text", "text": ""}
        }),
    )
}

/// Builds a `content_block_start` event for a tool use block.
///
/// # Errors
///
/// Returns an error when the SSE payload cannot be serialized to JSON.
pub fn tool_block_start(index: u32, tool_call: &ToolCall) -> Result<Event> {
    sse_event(
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {
                "type": "tool_use",
                "id": tool_call.id,
                "name": tool_call.function.name,
                "input": {}
            }
        }),
    )
}

/// Builds a `content_block_start` event for an image block.
///
/// # Errors
///
/// Returns an error when the SSE payload cannot be serialized to JSON.
pub fn image_block_start(index: u32, source: &ImageSource) -> Result<Event> {
    sse_event(
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {
                "type": "image",
                "source": source
            }
        }),
    )
}

/// Builds a `content_block_start` event for a thinking block.
///
/// # Errors
///
/// Returns an error when the SSE payload cannot be serialized to JSON.
pub fn thinking_block_start(index: u32) -> Result<Event> {
    sse_event(
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {"type": "thinking", "thinking": ""}
        }),
    )
}

/// Builds a `content_block_delta` event for text content.
///
/// # Errors
///
/// Returns an error when the SSE payload cannot be serialized to JSON.
pub fn text_delta(index: u32, text: &str) -> Result<Event> {
    sse_event(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "text_delta", "text": text}
        }),
    )
}

/// Builds a `content_block_delta` event for tool input JSON.
///
/// # Errors
///
/// Returns an error when the SSE payload cannot be serialized to JSON.
pub fn tool_json_delta(index: u32, arguments: &str) -> Result<Event> {
    sse_event(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "input_json_delta", "partial_json": arguments}
        }),
    )
}

/// Builds a `content_block_delta` event for thinking content.
///
/// # Errors
///
/// Returns an error when the SSE payload cannot be serialized to JSON.
pub fn thinking_delta(index: u32, thinking: &str) -> Result<Event> {
    sse_event(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "thinking_delta", "thinking": thinking}
        }),
    )
}

/// Builds a `content_block_delta` signature event for a thinking block.
///
/// # Errors
///
/// Returns an error when the SSE payload cannot be serialized to JSON.
pub fn signature_delta(index: u32, signature: &str) -> Result<Event> {
    sse_event(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "signature_delta", "signature": signature}
        }),
    )
}

/// Builds a `content_block_stop` event.
///
/// # Errors
///
/// Returns an error when the SSE payload cannot be serialized to JSON.
pub fn content_block_stop(index: u32) -> Result<Event> {
    sse_event(
        "content_block_stop",
        &json!({
            "type": "content_block_stop",
            "index": index
        }),
    )
}

/// Builds the terminal `message_delta` event with cumulative usage.
///
/// # Errors
///
/// Returns an error when the SSE payload cannot be serialized to JSON.
pub fn message_delta_event(stop_reason: &str, usage: &ResponseUsage) -> Result<Event> {
    sse_event(
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": map_stop_reason(stop_reason),
                "stop_sequence": null
            },
            "usage": usage
        }),
    )
}

/// Builds the terminal `message_stop` event.
///
/// # Errors
///
/// Returns an error when the SSE payload cannot be serialized to JSON.
pub fn message_stop_event() -> Result<Event> {
    sse_event("message_stop", &json!({ "type": "message_stop" }))
}

/// Builds an Anthropic-shaped error response body.
#[must_use]
pub fn error_body(error: &Error) -> Value {
    json!({
        "type": "error",
        "error": {
            "type": anthropic_error_type(error),
            "message": error.to_string()
        }
    })
}

fn anthropic_error_type(error: &Error) -> &'static str {
    match error.status_code() {
        axum::http::StatusCode::UNAUTHORIZED | axum::http::StatusCode::FORBIDDEN => {
            "authentication_error"
        }
        axum::http::StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
        status if status.is_server_error() || matches!(error, Error::Http(_)) => "api_error",
        _ => "invalid_request_error",
    }
}

fn sse_event(event: &str, payload: &impl Serialize) -> Result<Event> {
    // Axum's SSE helper takes a string payload, so serialize the Anthropic
    // event envelope once here before attaching the event name.
    let data = serde_json::to_string(payload)?;
    Ok(Event::default().event(event).data(data))
}
