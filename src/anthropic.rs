//! Anthropic-compatible request, response, and streaming adapters.
//!
//! This module exposes a minimal Messages API surface that matches the parts
//! Claude Code and Anthropic SDKs commonly rely on: `/v1/messages` and
//! `/v1/messages/count_tokens`.

use crate::{
    Error, Result,
    openai::{
        response::{ChatCompletionResponse, Usage},
        types::{
            ChatCompletionRequest, ChatContent, ChatContentPart, ChatMessage, ChatTool,
            FunctionTool, ImageUrl, ToolCall,
        },
    },
};
use axum::response::sse::Event;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// Anthropic-compatible Messages API request body.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct MessagesRequest {
    pub model: String,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub system: Option<SystemPrompt>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub tools: Option<Vec<ToolDefinition>>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    pub thinking: Option<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl MessagesRequest {
    /// Returns whether the request should use streaming responses.
    pub fn wants_stream(&self) -> bool {
        self.stream.unwrap_or(false)
    }
}

/// Anthropic-compatible input message.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Message {
    pub role: String,
    pub content: MessageContent,
}

/// Anthropic accepts either a string or an array of blocks for message content.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// Top-level system prompt can be a string or an array of blocks.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum SystemPrompt {
    Text(String),
    Blocks(Vec<SystemBlock>),
}

/// Supported Anthropic system block shape.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct SystemBlock {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
}

/// Minimal Anthropic content block support needed by SDKs and Claude Code.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub source: Option<ImageSource>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub input: Option<Value>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
    #[serde(default)]
    pub content: Option<ToolResultContent>,
    #[serde(default)]
    pub thinking: Option<String>,
    #[serde(default)]
    pub signature: Option<String>,
}

/// Base64 image source accepted by the Anthropic Messages API.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ImageSource {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub media_type: Option<String>,
    #[serde(default)]
    pub data: Option<String>,
}

/// Tool result content may arrive as a string or as a list of text blocks.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// Anthropic tool definition.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: Option<Value>,
}

/// Anthropic Messages API response body.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MessageResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub role: &'static str,
    pub model: String,
    pub content: Vec<ResponseContentBlock>,
    pub stop_reason: &'static str,
    pub stop_sequence: Option<String>,
    pub usage: ResponseUsage,
}

/// Anthropic response content block.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type")]
pub enum ResponseContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
}

/// Anthropic usage fields for Messages responses.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ResponseUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// Anthropic token counting response body.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CountTokensResponse {
    pub input_tokens: u32,
}

/// SSE payload for `message_start`.
#[derive(Debug, Clone, Serialize)]
pub struct MessageStartEvent {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub message: StreamingMessage,
}

/// Partial message object used by stream events.
#[derive(Debug, Clone, Serialize)]
pub struct StreamingMessage {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub role: &'static str,
    pub content: Vec<Value>,
    pub model: String,
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
    pub usage: ResponseUsage,
}

/// Converts an Anthropic Messages request into the existing OpenAI-like request
/// type used by the Codex upstream adapter.
pub fn to_openai_request(request: &MessagesRequest) -> Result<ChatCompletionRequest> {
    let mut messages = Vec::new();
    if let Some(system) = system_prompt_text(request.system.as_ref()) {
        messages.push(ChatMessage {
            role: "system".to_owned(),
            content: Some(ChatContent::Text(system)),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        });
    }

    for message in &request.messages {
        append_message(&mut messages, message)?;
    }

    Ok(ChatCompletionRequest {
        model: request.model.clone(),
        messages,
        stream: request.stream,
        temperature: request.temperature,
        tools: request.tools.as_ref().map(|tools| convert_tools(tools)),
        tool_choice: request.tool_choice.clone().map(convert_tool_choice),
        service_tier: None,
        reasoning_effort: None,
        max_completion_tokens: request.max_tokens,
        max_tokens: request.max_tokens,
        extra: request.extra.clone(),
    })
}

/// Maps the OpenAI-compatible chat response into an Anthropic Messages response.
pub fn from_openai_response(response: ChatCompletionResponse) -> MessageResponse {
    let choice = response
        .choices
        .into_iter()
        .next()
        .unwrap_or_else(empty_choice);
    let mut content = Vec::new();

    if let Some(text) = choice.message.content.filter(|text| !text.is_empty()) {
        content.push(ResponseContentBlock::Text { text });
    }

    for tool_call in choice.message.tool_calls.into_iter().flatten() {
        content.push(ResponseContentBlock::ToolUse {
            id: tool_call.id,
            name: tool_call.function.name,
            input: parse_arguments(&tool_call.function.arguments),
        });
    }

    let usage = response.usage.unwrap_or(Usage {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
    });

    MessageResponse {
        id: response.id.replace("chatcmpl", "msg"),
        kind: "message",
        role: "assistant",
        model: response.model,
        content,
        stop_reason: map_stop_reason(&choice.finish_reason),
        stop_sequence: None,
        usage: ResponseUsage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
        },
    }
}

/// Produces a compact token estimate for `/v1/messages/count_tokens`.
pub fn estimate_input_tokens(request: &MessagesRequest) -> u32 {
    let mut text = String::new();
    if let Some(system) = system_prompt_text(request.system.as_ref()) {
        text.push_str(&system);
    }

    for message in &request.messages {
        text.push_str(&message.role);
        match &message.content {
            MessageContent::Text(value) => text.push_str(value),
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    append_block_text(&mut text, block);
                }
            }
        }
    }

    if let Some(tools) = request.tools.as_ref() {
        for tool in tools {
            text.push_str(&tool.name);
            if let Some(description) = &tool.description {
                text.push_str(description);
            }
            if let Some(schema) = &tool.input_schema {
                text.push_str(&schema.to_string());
            }
        }
    }

    estimate_tokens_from_text(&text)
}

/// Builds the `message_start` payload emitted ahead of streaming deltas.
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
                    output_tokens: 0,
                },
            },
        },
    )
}

/// Builds a `content_block_start` event for a text block.
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

/// Builds a `content_block_delta` event for text content.
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

/// Builds a `content_block_stop` event.
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
pub fn message_delta_event(stop_reason: &str, output_tokens: u32) -> Result<Event> {
    sse_event(
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": map_stop_reason(stop_reason),
                "stop_sequence": null
            },
            "usage": {
                "output_tokens": output_tokens
            }
        }),
    )
}

/// Builds the terminal `message_stop` event.
pub fn message_stop_event() -> Result<Event> {
    sse_event("message_stop", &json!({ "type": "message_stop" }))
}

/// Builds an Anthropic-shaped error response body.
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
    match error {
        Error::Unauthorized => "authentication_error",
        Error::Upstream(_) | Error::Http(_) => "api_error",
        _ => "invalid_request_error",
    }
}

fn append_message(messages: &mut Vec<ChatMessage>, message: &Message) -> Result<()> {
    match message.role.as_str() {
        "user" => append_user_message(messages, message),
        "assistant" => append_assistant_message(messages, message),
        role => Err(Error::config(format!("unsupported Anthropic role: {role}"))),
    }
}

fn append_user_message(messages: &mut Vec<ChatMessage>, message: &Message) -> Result<()> {
    match &message.content {
        MessageContent::Text(text) => messages.push(ChatMessage {
            role: "user".to_owned(),
            content: Some(ChatContent::Text(text.clone())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }),
        MessageContent::Blocks(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                match block.kind.as_str() {
                    "text" => parts.push(ChatContentPart {
                        kind: "text".to_owned(),
                        text: block.text.clone(),
                        image_url: None,
                    }),
                    "image" => {
                        if let Some(url) = image_data_url(block.source.as_ref()) {
                            parts.push(ChatContentPart {
                                kind: "image_url".to_owned(),
                                text: None,
                                image_url: Some(ImageUrl { url, detail: None }),
                            });
                        }
                    }
                    "tool_result" => messages.push(ChatMessage {
                        role: "tool".to_owned(),
                        content: Some(ChatContent::Text(tool_result_text(block.content.as_ref()))),
                        name: None,
                        tool_call_id: block.tool_use_id.clone(),
                        tool_calls: None,
                    }),
                    "thinking" => {}
                    _ => {}
                }
            }

            if !parts.is_empty() {
                messages.push(ChatMessage {
                    role: "user".to_owned(),
                    content: Some(ChatContent::Parts(parts)),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                });
            }
        }
    }
    Ok(())
}

fn append_assistant_message(messages: &mut Vec<ChatMessage>, message: &Message) -> Result<()> {
    match &message.content {
        MessageContent::Text(text) => messages.push(ChatMessage {
            role: "assistant".to_owned(),
            content: Some(ChatContent::Text(text.clone())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }),
        MessageContent::Blocks(blocks) => {
            let text = blocks
                .iter()
                .filter_map(|block| match block.kind.as_str() {
                    "text" => block.text.clone(),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let tool_calls = blocks
                .iter()
                .filter(|block| block.kind == "tool_use")
                .map(tool_call_from_block)
                .collect::<Result<Vec<_>>>()?;

            messages.push(ChatMessage {
                role: "assistant".to_owned(),
                content: (!text.is_empty()).then_some(ChatContent::Text(text)),
                name: None,
                tool_call_id: None,
                tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            });
        }
    }
    Ok(())
}

fn tool_call_from_block(block: &ContentBlock) -> Result<ToolCall> {
    Ok(ToolCall {
        id: block
            .id
            .clone()
            .ok_or_else(|| Error::config("tool_use block missing id"))?,
        kind: "function".to_owned(),
        function: crate::openai::types::FunctionCall {
            name: block
                .name
                .clone()
                .ok_or_else(|| Error::config("tool_use block missing name"))?,
            arguments: block.input.clone().unwrap_or_else(|| json!({})).to_string(),
        },
    })
}

fn convert_tools(tools: &[ToolDefinition]) -> Vec<ChatTool> {
    tools
        .iter()
        .map(|tool| ChatTool {
            kind: "function".to_owned(),
            function: FunctionTool {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.input_schema.clone(),
                strict: None,
            },
        })
        .collect()
}

fn convert_tool_choice(value: Value) -> Value {
    match value {
        Value::String(string) => match string.as_str() {
            "any" => json!("required"),
            "auto" => json!("auto"),
            "none" => json!("none"),
            _ => Value::String(string),
        },
        Value::Object(object) if object.get("type").and_then(Value::as_str) == Some("tool") => {
            let name = object.get("name").cloned().unwrap_or(Value::Null);
            json!({
                "type": "function",
                "function": {"name": name}
            })
        }
        other => other,
    }
}

fn system_prompt_text(system: Option<&SystemPrompt>) -> Option<String> {
    match system? {
        SystemPrompt::Text(text) => Some(text.clone()),
        SystemPrompt::Blocks(blocks) => {
            let text = blocks
                .iter()
                .filter(|block| block.kind == "text")
                .filter_map(|block| block.text.as_deref())
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
    }
}

fn image_data_url(source: Option<&ImageSource>) -> Option<String> {
    let source = source?;
    let media_type = source.media_type.as_deref().unwrap_or("image/png");
    let data = source.data.as_deref()?;
    Some(format!("data:{media_type};base64,{data}"))
}

fn tool_result_text(content: Option<&ToolResultContent>) -> String {
    match content {
        Some(ToolResultContent::Text(text)) => text.clone(),
        Some(ToolResultContent::Blocks(blocks)) => blocks
            .iter()
            .filter_map(|block| block.text.as_deref())
            .collect::<Vec<_>>()
            .join("\n"),
        None => String::new(),
    }
}

fn parse_arguments(arguments: &str) -> Value {
    serde_json::from_str(arguments).unwrap_or_else(|_| json!({ "_raw": arguments }))
}

fn map_stop_reason(reason: &str) -> &'static str {
    match reason {
        "tool_calls" => "tool_use",
        "length" => "max_tokens",
        _ => "end_turn",
    }
}

fn estimate_tokens_from_text(text: &str) -> u32 {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        0
    } else {
        ((trimmed.chars().count() as u32) / 4).max(1)
    }
}

fn append_block_text(text: &mut String, block: &ContentBlock) {
    match block.kind.as_str() {
        "text" => {
            if let Some(value) = &block.text {
                text.push_str(value);
            }
        }
        "tool_use" => {
            if let Some(name) = &block.name {
                text.push_str(name);
            }
            if let Some(input) = &block.input {
                text.push_str(&input.to_string());
            }
        }
        "tool_result" => text.push_str(&tool_result_text(block.content.as_ref())),
        "thinking" => {
            if let Some(value) = &block.thinking {
                text.push_str(value);
            }
        }
        _ => {}
    }
}

fn sse_event(event: &str, payload: &impl Serialize) -> Result<Event> {
    let data = serde_json::to_string(payload)?;
    Ok(Event::default().event(event).data(data))
}

fn empty_choice() -> crate::openai::response::ChatChoice {
    crate::openai::response::ChatChoice {
        index: 0,
        message: crate::openai::response::AssistantMessage {
            role: "assistant",
            content: Some(String::new()),
            tool_calls: None,
        },
        finish_reason: "stop".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn converts_anthropic_request_to_openai_shape() {
        let request: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "system": "be terse",
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "abc"}}
                ]}
            ],
            "tools": [{"name": "lookup", "input_schema": {"type": "object"}}],
            "tool_choice": "any"
        }))
        .unwrap();

        let converted = to_openai_request(&request).unwrap();
        assert_eq!(converted.messages[0].role, "system");
        assert_eq!(converted.messages[1].role, "user");
        assert_eq!(converted.tools.as_ref().unwrap()[0].function.name, "lookup");
        assert_eq!(converted.tool_choice, Some(json!("required")));
    }

    #[test]
    fn converts_tool_result_blocks_to_tool_messages() {
        let request: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "done"}]
            }]
        }))
        .unwrap();

        let converted = to_openai_request(&request).unwrap();
        assert_eq!(converted.messages[0].role, "tool");
        assert_eq!(
            converted.messages[0].tool_call_id.as_deref(),
            Some("call_1")
        );
    }

    #[test]
    fn converts_openai_response_to_anthropic_message() {
        let response = ChatCompletionResponse {
            id: "chatcmpl-1".into(),
            object: "chat.completion",
            created: 1,
            model: "gpt-5.5".into(),
            choices: vec![crate::openai::response::ChatChoice {
                index: 0,
                message: crate::openai::response::AssistantMessage {
                    role: "assistant",
                    content: Some("hello".into()),
                    tool_calls: Some(vec![ToolCall {
                        id: "call_1".into(),
                        kind: "function".into(),
                        function: crate::openai::types::FunctionCall {
                            name: "lookup".into(),
                            arguments: "{\"q\":\"x\"}".into(),
                        },
                    }]),
                },
                finish_reason: "tool_calls".into(),
            }],
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 4,
                total_tokens: 14,
            }),
        };

        let message = from_openai_response(response);
        assert_eq!(message.stop_reason, "tool_use");
        assert_eq!(message.usage.input_tokens, 10);
        assert_eq!(message.content.len(), 2);
    }

    #[test]
    fn estimates_input_tokens_from_blocks() {
        let request: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hello world"}]}]
        }))
        .unwrap();

        assert!(estimate_input_tokens(&request) > 0);
    }
}
