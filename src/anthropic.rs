//! Anthropic-compatible request, response, and streaming adapters.
//!
//! This module exposes a minimal Messages API surface that matches the parts
//! Claude Code and Anthropic SDKs commonly rely on: `/v1/messages` and
//! `/v1/messages/count_tokens`.

use crate::{
    Error, Result,
    openai::{
        response::{
            ChatCompletionResponse, ResponseObject, Usage, generated_images_from_output,
            generated_images_from_response_items,
        },
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
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MessagesRequest {
    /// Target model identifier.
    pub model: String,
    /// Conversation history supplied to the model.
    #[serde(default)]
    pub messages: Vec<Message>,
    /// Optional top-level system prompt.
    #[serde(default)]
    pub system: Option<SystemPrompt>,
    /// Maximum number of output tokens requested.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Whether the caller requested SSE streaming.
    #[serde(default)]
    pub stream: Option<bool>,
    /// Sampling temperature, when supported upstream.
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Nucleus sampling parameter, preserved for compatibility.
    #[serde(default)]
    pub top_p: Option<f64>,
    /// Tools exposed to the model.
    #[serde(default)]
    pub tools: Option<Vec<ToolDefinition>>,
    /// Requested tool selection mode or explicit tool choice.
    #[serde(default)]
    pub tool_choice: Option<Value>,
    /// Optional stop sequences understood by Anthropic clients.
    #[serde(default)]
    pub stop_sequences: Option<Vec<String>>,
    /// Optional thinking configuration passed through by compatible clients.
    #[serde(default)]
    pub thinking: Option<Value>,
    /// Optional Anthropic output configuration, including effort.
    #[serde(default)]
    pub output_config: Option<Value>,
    /// Optional Anthropic service tier hint such as `auto` or `standard_only`.
    #[serde(default)]
    pub service_tier: Option<String>,
    /// Optional Anthropic beta speed hint such as `fast`.
    #[serde(default)]
    pub speed: Option<String>,
    /// Additional provider-specific fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

const CODEX_PRIORITY_SERVICE_TIER: &str = "priority";

impl MessagesRequest {
    /// Returns whether the request should use streaming responses.
    #[must_use]
    pub fn wants_stream(&self) -> bool {
        self.stream.unwrap_or(false)
    }

    /// Maps Anthropic priority/speed controls to the upstream Codex service tier.
    #[must_use]
    pub(crate) fn upstream_service_tier(&self) -> Option<String> {
        if self
            .speed
            .as_deref()
            .map(str::trim)
            .is_some_and(|speed| speed.eq_ignore_ascii_case("fast"))
        {
            return Some(CODEX_PRIORITY_SERVICE_TIER.to_owned());
        }

        let tier = self.service_tier.as_deref()?.trim();
        if tier.eq_ignore_ascii_case("auto")
            || tier.eq_ignore_ascii_case("priority")
            || tier.eq_ignore_ascii_case("fast")
        {
            Some(CODEX_PRIORITY_SERVICE_TIER.to_owned())
        } else {
            None
        }
    }

    /// Returns the normalized Anthropic output effort, when the caller supplied one.
    #[must_use]
    pub(crate) fn output_effort(&self) -> Option<String> {
        self.output_config
            .as_ref()
            .and_then(|config| config.get("effort"))
            .and_then(Value::as_str)
            .and_then(normalize_anthropic_effort)
    }
}

/// Converts Anthropic effort aliases into Codex reasoning effort values.
pub(crate) fn normalize_anthropic_effort(value: &str) -> Option<String> {
    let normalized = value.trim().to_ascii_lowercase().replace(['-', ' '], "_");
    match normalized.as_str() {
        "low" | "medium" | "high" | "xhigh" => Some(normalized),
        "extra_high" | "extra" | "max" => Some("xhigh".to_owned()),
        "minimal" | "min" => Some("low".to_owned()),
        _ => None,
    }
}

const CLAUDE_CODE_BILLING_HEADER_PREFIX: &str = "x-anthropic-billing-header:";

/// Anthropic-compatible input message.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Message {
    /// Message role such as `user` or `assistant`.
    pub role: String,
    /// Message payload as text or structured blocks.
    pub content: MessageContent,
}

/// Anthropic accepts either a string or an array of blocks for message content.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum MessageContent {
    /// Plain text message content.
    Text(String),
    /// Structured content blocks.
    Blocks(Vec<ContentBlock>),
}

/// Top-level system prompt can be a string or an array of blocks.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum SystemPrompt {
    /// Plain text system prompt.
    Text(String),
    /// Structured system prompt blocks.
    Blocks(Vec<SystemBlock>),
}

/// Supported Anthropic system block shape.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SystemBlock {
    /// Block type, typically `text`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Optional text payload carried by the block.
    #[serde(default)]
    pub text: Option<String>,
    /// Optional prompt caching directive attached to this block.
    #[serde(default)]
    pub cache_control: Option<Value>,
}

/// Minimal Anthropic content block support needed by SDKs and Claude Code.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContentBlock {
    /// Content block type such as `text`, `image`, `tool_use`, or `tool_result`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Inline text payload for text-like blocks.
    #[serde(default)]
    pub text: Option<String>,
    /// Optional prompt caching directive attached to this block.
    #[serde(default)]
    pub cache_control: Option<Value>,
    /// Image source for image blocks.
    #[serde(default)]
    pub source: Option<ImageSource>,
    /// Optional nested content for document-like blocks.
    #[serde(default)]
    pub document_content: Option<Vec<Self>>,
    /// Tool use identifier for `tool_use` blocks.
    #[serde(default)]
    pub id: Option<String>,
    /// Tool name for `tool_use` blocks.
    #[serde(default)]
    pub name: Option<String>,
    /// JSON input payload for `tool_use` blocks.
    #[serde(default)]
    pub input: Option<Value>,
    /// Referenced tool call identifier for `tool_result` blocks.
    #[serde(default)]
    pub tool_use_id: Option<String>,
    /// Structured or plain-text tool result content.
    #[serde(default)]
    pub content: Option<ToolResultContent>,
    /// Whether a tool result represents an error.
    #[serde(default)]
    pub is_error: Option<bool>,
    /// Reasoning text for `thinking` blocks.
    #[serde(default)]
    pub thinking: Option<String>,
    /// Optional signature attached to signed thinking blocks.
    #[serde(default)]
    pub signature: Option<String>,
}

/// Base64 image source accepted by the Anthropic Messages API.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ImageSource {
    /// Source type, typically `base64`.
    #[serde(rename = "type")]
    pub kind: String,
    /// MIME type for the encoded image.
    #[serde(default)]
    pub media_type: Option<String>,
    /// Base64-encoded image payload.
    #[serde(default)]
    pub data: Option<String>,
    /// Inline text payload for text-backed sources.
    #[serde(default)]
    pub text: Option<String>,
    /// URL-backed sources when the caller references a remote asset.
    #[serde(default)]
    pub url: Option<String>,
    /// File-backed sources when the caller references an uploaded file.
    #[serde(default)]
    pub file_id: Option<String>,
}

/// Tool result content may arrive as a string or as a list of text blocks.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    /// Plain-text tool output.
    Text(String),
    /// Structured tool output blocks.
    Blocks(Vec<ContentBlock>),
}

/// Anthropic tool definition.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolDefinition {
    /// Tool name exposed to the model.
    pub name: String,
    /// Optional human-readable description.
    #[serde(default)]
    pub description: Option<String>,
    /// JSON Schema describing accepted tool input.
    #[serde(default)]
    pub input_schema: Option<Value>,
    /// Optional prompt caching directive attached to this tool definition.
    #[serde(default)]
    pub cache_control: Option<Value>,
}

/// Anthropic Messages API response body.
#[derive(Debug, Clone, Serialize)]
pub struct MessageResponse {
    /// Anthropic message identifier.
    pub id: String,
    /// Object kind, always `message`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Message role, always `assistant` for generated responses.
    pub role: &'static str,
    /// Model identifier that produced the response.
    pub model: String,
    /// Response content blocks in Anthropic format.
    pub content: Vec<ResponseContentBlock>,
    /// Terminal reason reported to Anthropic clients.
    pub stop_reason: &'static str,
    /// Matching stop sequence when one caused termination.
    pub stop_sequence: Option<String>,
    /// Token usage metadata.
    pub usage: ResponseUsage,
}

/// Anthropic response content block.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ResponseContentBlock {
    /// Plain text response content.
    #[serde(rename = "text")]
    Text {
        /// Text emitted for this response block.
        text: String,
    },
    /// Tool invocation content.
    #[serde(rename = "tool_use")]
    ToolUse {
        /// Tool call identifier.
        id: String,
        /// Tool name.
        name: String,
        /// Parsed JSON tool input.
        input: Value,
    },
    /// Non-standard rotom extension for generated image output.
    #[serde(rename = "image")]
    Image {
        /// Base64 image payload.
        source: ImageSource,
    },
    /// Thinking output surfaced by reasoning-capable upstream models.
    #[serde(rename = "thinking")]
    Thinking {
        /// Reasoning text emitted by the model.
        thinking: String,
        /// Opaque signature carried by Anthropic thinking blocks.
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
}

/// Anthropic usage fields for Messages responses.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ResponseUsage {
    /// Estimated or reported prompt token count.
    pub input_tokens: u32,
    /// Number of prompt tokens written into cache on this request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u32>,
    /// Number of prompt tokens read from cache on this request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u32>,
    /// Estimated or reported output token count.
    pub output_tokens: u32,
    /// Optional hosted server-tool usage summary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<Value>,
}

/// Anthropic token counting response body.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CountTokensResponse {
    /// Estimated input token count for the submitted request.
    pub input_tokens: u32,
}

/// Anthropic-compatible models list response.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ModelsResponse {
    /// Listed models.
    pub data: Vec<ModelInfo>,
    /// First model identifier when available.
    pub first_id: Option<String>,
    /// Whether more models remain after this page.
    pub has_more: bool,
    /// Last model identifier when available.
    pub last_id: Option<String>,
}

/// Anthropic-compatible model object.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ModelInfo {
    /// Model creation timestamp in RFC 3339 form.
    pub created_at: String,
    /// Human-readable model name.
    pub display_name: String,
    /// Model identifier.
    pub id: String,
    /// Object kind, always `model`.
    #[serde(rename = "type")]
    pub kind: &'static str,
}

/// Anthropic message batch creation request body.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MessageBatchCreateRequest {
    /// Individual message creation requests to process in the batch.
    pub requests: Vec<MessageBatchRequest>,
}

/// Single request inside an Anthropic message batch.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MessageBatchRequest {
    /// Caller-defined identifier used to match results back to inputs.
    pub custom_id: String,
    /// Parameters for the embedded message creation request.
    pub params: MessagesRequest,
}

/// Anthropic-compatible message batch object.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MessageBatch {
    /// Time at which the batch is archived, if ever.
    pub archived_at: Option<String>,
    /// Time at which cancellation was initiated, if ever.
    pub cancel_initiated_at: Option<String>,
    /// Time at which the batch was created.
    pub created_at: String,
    /// Time at which batch processing ended.
    pub ended_at: Option<String>,
    /// Time at which the batch will expire.
    pub expires_at: String,
    /// Batch identifier.
    pub id: String,
    /// Current processing status.
    pub processing_status: &'static str,
    /// Counts grouped by terminal request state.
    pub request_counts: MessageBatchRequestCounts,
    /// URL where JSONL batch results can be fetched.
    pub results_url: Option<String>,
    /// Object kind, always `message_batch`.
    #[serde(rename = "type")]
    pub kind: &'static str,
}

/// Anthropic-compatible list response for message batches.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MessageBatchListResponse {
    /// Listed message batches, ordered newest first.
    pub data: Vec<MessageBatch>,
    /// First batch identifier when available.
    pub first_id: Option<String>,
    /// Whether more batches remain after this page.
    pub has_more: bool,
    /// Last batch identifier when available.
    pub last_id: Option<String>,
}

/// Anthropic-compatible request state counters for a batch.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MessageBatchRequestCounts {
    /// Number of canceled requests.
    pub canceled: u32,
    /// Number of errored requests.
    pub errored: u32,
    /// Number of expired requests.
    pub expired: u32,
    /// Number of requests still processing.
    pub processing: u32,
    /// Number of succeeded requests.
    pub succeeded: u32,
}

/// Single line emitted by the Anthropic message batch results endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct MessageBatchResult {
    /// Caller-defined identifier copied from the request.
    pub custom_id: String,
    /// Terminal result payload for the request.
    pub result: MessageBatchResultType,
}

/// Terminal result variants for Anthropic message batch items.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum MessageBatchResultType {
    /// Successful message creation result.
    #[serde(rename = "succeeded")]
    Succeeded {
        /// Completed message object.
        message: MessageResponse,
    },
    /// Failed message creation result.
    #[serde(rename = "errored")]
    Errored {
        /// Error object returned for the failed request.
        error: Value,
    },
    /// Request canceled before execution completed.
    #[serde(rename = "canceled")]
    Canceled,
}

/// Anthropic-compatible deletion confirmation for a message batch.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MessageBatchDeleted {
    /// Identifier of the deleted message batch.
    pub id: String,
    /// Object kind, always `message_batch_deleted`.
    #[serde(rename = "type")]
    pub kind: &'static str,
}

/// SSE payload for `message_start`.
#[derive(Debug, Clone, Serialize)]
pub struct MessageStartEvent {
    /// Event payload type, always `message_start`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Initial partial message object for the stream.
    pub message: StreamingMessage,
}

/// Partial message object used by stream events.
#[derive(Debug, Clone, Serialize)]
pub struct StreamingMessage {
    /// Anthropic message identifier shared across the stream.
    pub id: String,
    /// Object kind, always `message`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Message role, always `assistant`.
    pub role: &'static str,
    /// Content blocks accumulated so far.
    pub content: Vec<Value>,
    /// Model identifier producing the stream.
    pub model: String,
    /// Final stop reason, omitted until the stream ends.
    pub stop_reason: Option<String>,
    /// Final stop sequence, if any.
    pub stop_sequence: Option<String>,
    /// Incremental token usage metadata.
    pub usage: ResponseUsage,
}

/// Converts an Anthropic Messages request into the existing OpenAI-like request
/// type used by the Codex upstream adapter.
///
/// # Errors
///
/// Returns an error when the request contains unsupported Anthropic roles or
/// malformed tool blocks that cannot be mapped into the OpenAI-compatible
/// request shape.
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
        top_p: request.top_p,
        tools: request.tools.as_ref().map(|tools| convert_tools(tools)),
        tool_choice: request.tool_choice.clone().map(convert_tool_choice),
        service_tier: request.upstream_service_tier(),
        reasoning_effort: request.output_effort(),
        max_completion_tokens: request.max_tokens,
        max_tokens: request.max_tokens,
        parallel_tool_calls: Some(parallel_tool_calls_enabled(request.tool_choice.as_ref())),
        stop: request.stop_sequences.clone(),
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

    for image in choice.message.images.into_iter().flatten() {
        content.push(ResponseContentBlock::Image {
            source: ImageSource {
                kind: "base64".to_owned(),
                media_type: image.media_type.or_else(|| Some("image/png".to_owned())),
                data: Some(image.b64_json),
                text: None,
                url: None,
                file_id: None,
            },
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
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            output_tokens: usage.completion_tokens,
            server_tool_use: None,
        },
    }
}

/// Maps an OpenAI-compatible Responses object into an Anthropic Messages response.
#[must_use]
pub fn from_openai_response_object(response: ResponseObject) -> MessageResponse {
    let mut content = Vec::new();

    for item in &response.output {
        match item.kind {
            "message" => {
                let text = item
                    .content
                    .iter()
                    .map(|part| part.text.clone())
                    .collect::<String>();
                if !text.is_empty() {
                    content.push(ResponseContentBlock::Text { text });
                }
            }
            "function_call" => {
                content.push(ResponseContentBlock::ToolUse {
                    id: item.call_id.clone().unwrap_or_else(|| item.id.clone()),
                    name: item.name.clone().unwrap_or_default(),
                    input: parse_arguments(item.arguments.as_deref().unwrap_or("{}")),
                });
            }
            _ => {}
        }
    }

    for image in generated_images_from_response_items(&response.output) {
        content.push(ResponseContentBlock::Image {
            source: ImageSource {
                kind: "base64".to_owned(),
                media_type: image.media_type.or_else(|| Some("image/png".to_owned())),
                data: Some(image.b64_json),
                text: None,
                url: None,
                file_id: None,
            },
        });
    }

    let stop_reason = response_stop_reason(&response);
    let usage = response.usage.unwrap_or(Usage {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
    });

    MessageResponse {
        id: response.id.replace("resp", "msg"),
        kind: "message",
        role: "assistant",
        model: response.model,
        content,
        stop_reason,
        stop_sequence: None,
        usage: ResponseUsage {
            input_tokens: usage.prompt_tokens,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            output_tokens: usage.completion_tokens,
            server_tool_use: None,
        },
    }
}

/// Maps a raw OpenAI/Codex Responses payload into an Anthropic Messages response.
#[must_use]
pub fn from_openai_response_value(response: &Value, fallback_model: &str) -> MessageResponse {
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .map_or(&[] as &[Value], Vec::as_slice);
    let mut content = output
        .iter()
        .flat_map(response_content_blocks_from_value)
        .collect::<Vec<_>>();
    content.extend(
        generated_images_from_output(output)
            .into_iter()
            .map(|image| ResponseContentBlock::Image {
                source: ImageSource {
                    kind: "base64".to_owned(),
                    media_type: image.media_type.or_else(|| Some("image/png".to_owned())),
                    data: Some(image.b64_json),
                    text: None,
                    url: None,
                    file_id: None,
                },
            }),
    );

    let usage = response
        .get("usage")
        .and_then(parse_anthropic_usage_value)
        .unwrap_or_else(|| default_response_usage(0, 0));

    MessageResponse {
        id: response
            .get("id")
            .and_then(Value::as_str)
            .map_or_else(|| "msg_local".to_owned(), |id| id.replace("resp", "msg")),
        kind: "message",
        role: "assistant",
        model: response
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(fallback_model)
            .to_owned(),
        content,
        stop_reason: response_stop_reason_value(response),
        stop_sequence: None,
        usage,
    }
}

fn response_content_blocks_from_value(item: &Value) -> Vec<ResponseContentBlock> {
    match item.get("type").and_then(Value::as_str) {
        Some("message") => response_message_blocks_from_value(item),
        Some("function_call") => vec![ResponseContentBlock::ToolUse {
            id: item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            name: item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            input: parse_arguments(
                item.get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}"),
            ),
        }],
        Some("reasoning") => response_reasoning_blocks_from_value(item),
        _ => Vec::new(),
    }
}

fn response_message_blocks_from_value(item: &Value) -> Vec<ResponseContentBlock> {
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
    if text.is_empty() {
        Vec::new()
    } else {
        vec![ResponseContentBlock::Text { text }]
    }
}

fn response_reasoning_blocks_from_value(item: &Value) -> Vec<ResponseContentBlock> {
    let thinking = item
        .get("summary")
        .and_then(Value::as_array)
        .map_or_else(String::new, |parts| joined_reasoning_text(parts));
    let thinking = if thinking.is_empty() {
        item.get("content")
            .and_then(Value::as_array)
            .map_or_else(String::new, |parts| joined_reasoning_text(parts))
    } else {
        thinking
    };

    if thinking.is_empty() {
        Vec::new()
    } else {
        vec![ResponseContentBlock::Thinking {
            thinking,
            signature: item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }]
    }
}

fn joined_reasoning_text(parts: &[Value]) -> String {
    parts
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

/// Produces a compact token estimate for `/v1/messages/count_tokens`.
#[must_use]
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

    if let Some(thinking) = request.thinking.as_ref() {
        text.push_str(&thinking.to_string());
    }
    if let Some(output_config) = request.output_config.as_ref() {
        text.push_str(&output_config.to_string());
    }
    if let Some(service_tier) = request.service_tier.as_deref() {
        text.push_str(service_tier);
    }
    if let Some(speed) = request.speed.as_deref() {
        text.push_str(speed);
    }

    estimate_tokens_from_text(&text)
}

/// Builds an Anthropic-compatible models response from the configured model IDs.
#[must_use]
pub fn models_response(ids: &[String]) -> ModelsResponse {
    let data = ids
        .iter()
        .map(|id| ModelInfo {
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            display_name: anthropic_display_name(id),
            id: id.clone(),
            kind: "model",
        })
        .collect::<Vec<_>>();
    let first_id = data.first().map(|model| model.id.clone());
    let last_id = data.last().map(|model| model.id.clone());

    ModelsResponse {
        data,
        first_id,
        has_more: false,
        last_id,
    }
}

/// Builds an Anthropic-compatible list response from stored message batches.
#[must_use]
pub fn message_batch_list_response(batches: Vec<MessageBatch>) -> MessageBatchListResponse {
    let first_id = batches.first().map(|batch| batch.id.clone());
    let last_id = batches.last().map(|batch| batch.id.clone());

    MessageBatchListResponse {
        data: batches,
        first_id,
        has_more: false,
        last_id,
    }
}

fn anthropic_display_name(id: &str) -> String {
    id.split('-')
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().collect::<String>() + chars.as_str()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

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

fn append_message(messages: &mut Vec<ChatMessage>, message: &Message) -> Result<()> {
    match message.role.as_str() {
        "user" => {
            append_user_message(messages, message);
            Ok(())
        }
        "assistant" => append_assistant_message(messages, message),
        role => Err(Error::config(format!("unsupported Anthropic role: {role}"))),
    }
}

fn append_user_message(messages: &mut Vec<ChatMessage>, message: &Message) {
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
                    "text" | "input_text" => parts.push(ChatContentPart {
                        kind: "text".to_owned(),
                        text: text_like_block_text(block),
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
                    "document" => {
                        if let Some(text) = document_text(block) {
                            parts.push(ChatContentPart {
                                kind: "text".to_owned(),
                                text: Some(text),
                                image_url: None,
                            });
                        }
                    }
                    "tool_result" => {
                        flush_user_parts(messages, &mut parts);
                        messages.push(ChatMessage {
                            role: "tool".to_owned(),
                            content: Some(ChatContent::Text(tool_result_text(
                                block.content.as_ref(),
                            ))),
                            name: None,
                            tool_call_id: block.tool_use_id.clone(),
                            tool_calls: None,
                        });
                    }
                    _ => {}
                }
            }

            flush_user_parts(messages, &mut parts);
        }
    }
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
            // Anthropic can interleave text and tool_use blocks in one assistant
            // turn; flatten text into one assistant message and preserve tool
            // calls in the dedicated OpenAI-compatible field.
            let text = blocks
                .iter()
                .filter_map(|block| match block.kind.as_str() {
                    "text" | "input_text" => text_like_block_text(block),
                    "document" => document_text(block),
                    "thinking" => block.thinking.clone(),
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
            // Preserve the JSON object as a string because the OpenAI-style
            // request shape stores function arguments as encoded JSON text.
            arguments: block.input.clone().unwrap_or_else(|| json!({})).to_string(),
        },
    })
}

fn convert_tools(tools: &[ToolDefinition]) -> Vec<ChatTool> {
    tools
        .iter()
        .map(|tool| ChatTool {
            kind: if tool.name == "image_generation" {
                "image_generation".to_owned()
            } else {
                "function".to_owned()
            },
            function: (tool.name != "image_generation").then_some(FunctionTool {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.input_schema.clone(),
                strict: None,
            }),
            extra: tool
                .cache_control
                .as_ref()
                .map_or_else(Map::new, |cache_control| {
                    let mut extra = Map::new();
                    extra.insert("cache_control".to_owned(), cache_control.clone());
                    extra
                }),
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

fn parallel_tool_calls_enabled(tool_choice: Option<&Value>) -> bool {
    match tool_choice {
        Some(Value::String(choice)) => choice != "none" && choice != "any",
        Some(Value::Object(object)) => object.get("type").and_then(Value::as_str) != Some("tool"),
        None | Some(_) => true,
    }
}

pub(crate) fn system_prompt_text(system: Option<&SystemPrompt>) -> Option<String> {
    match system? {
        SystemPrompt::Text(text) => sanitize_system_text(text),
        SystemPrompt::Blocks(blocks) => {
            let text = blocks
                .iter()
                .filter(|block| block.kind == "text")
                .filter_map(|block| block.text.as_deref())
                .filter_map(sanitize_system_text)
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
    }
}

pub(crate) fn sanitize_system_text(text: &str) -> Option<String> {
    let stripped = strip_claude_code_billing_header_line(text)
        .trim()
        .to_owned();
    (!stripped.is_empty()).then_some(stripped)
}

fn strip_claude_code_billing_header_line(text: &str) -> &str {
    let trimmed = text.trim_start();
    let Some(rest) = trimmed.strip_prefix(CLAUDE_CODE_BILLING_HEADER_PREFIX) else {
        return text;
    };

    rest.find('\n').map_or("", |index| {
        rest[index + 1..].trim_start_matches(['\r', '\n'])
    })
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

fn flush_user_parts(messages: &mut Vec<ChatMessage>, parts: &mut Vec<ChatContentPart>) {
    if parts.is_empty() {
        return;
    }

    messages.push(ChatMessage {
        role: "user".to_owned(),
        content: Some(ChatContent::Parts(std::mem::take(parts))),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    });
}

fn text_like_block_text(block: &ContentBlock) -> Option<String> {
    block
        .text
        .clone()
        .or_else(|| block.thinking.clone())
        .or_else(|| block.source.as_ref().and_then(|source| source.text.clone()))
}

fn document_text(block: &ContentBlock) -> Option<String> {
    let mut fragments = Vec::new();
    if let Some(text) = block.text.as_deref() {
        fragments.push(text.to_owned());
    }
    if let Some(source) = block.source.as_ref() {
        if let Some(text) = source.text.as_deref() {
            fragments.push(text.to_owned());
        }
        if let Some(data) = source.data.as_deref() {
            fragments.push(data.to_owned());
        }
        if let Some(url) = source.url.as_deref() {
            fragments.push(format!("[document:{url}]"));
        }
        if let Some(file_id) = source.file_id.as_deref() {
            fragments.push(format!("[document_file:{file_id}]"));
        }
    }
    if let Some(content) = block.document_content.as_ref() {
        for nested in content {
            let mut text = String::new();
            append_block_text(&mut text, nested);
            if !text.is_empty() {
                fragments.push(text);
            }
        }
    }

    let combined = fragments.join("\n");
    (!combined.is_empty()).then_some(combined)
}

fn parse_arguments(arguments: &str) -> Value {
    // Anthropic expects parsed JSON input for tool_use blocks; keep malformed
    // arguments accessible instead of failing the entire response conversion.
    serde_json::from_str(arguments).unwrap_or_else(|_| json!({ "_raw": arguments }))
}

fn map_stop_reason(reason: &str) -> &'static str {
    match reason {
        "tool_calls" => "tool_use",
        "length" | "max_output_tokens" | "max_tokens" => "max_tokens",
        "content_filter" => "refusal",
        _ => "end_turn",
    }
}

fn response_stop_reason(response: &ResponseObject) -> &'static str {
    if response
        .output
        .iter()
        .any(|item| item.kind == "function_call")
    {
        return "tool_use";
    }

    if response.status == "incomplete" {
        return response
            .incomplete_details
            .as_ref()
            .and_then(|details| details.get("reason"))
            .and_then(Value::as_str)
            .map_or("max_tokens", map_stop_reason);
    }

    "end_turn"
}

fn response_stop_reason_value(response: &Value) -> &'static str {
    if response
        .get("output")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
        })
    {
        return "tool_use";
    }

    if response.get("status").and_then(Value::as_str) == Some("incomplete") {
        return response
            .get("incomplete_details")
            .and_then(|details| details.get("reason"))
            .and_then(Value::as_str)
            .map_or("max_tokens", map_stop_reason);
    }

    response
        .get("stop_reason")
        .or_else(|| response.get("finish_reason"))
        .and_then(Value::as_str)
        .map_or("end_turn", map_stop_reason)
}

const fn default_response_usage(input_tokens: u32, output_tokens: u32) -> ResponseUsage {
    ResponseUsage {
        input_tokens,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
        output_tokens,
        server_tool_use: None,
    }
}

fn parse_anthropic_usage_value(value: &Value) -> Option<ResponseUsage> {
    let prompt_tokens = value
        .get("input_tokens")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())?;
    let completion_tokens = value
        .get("output_tokens")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0);

    Some(ResponseUsage {
        input_tokens: prompt_tokens,
        cache_creation_input_tokens: value
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        cache_read_input_tokens: value
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        output_tokens: completion_tokens,
        server_tool_use: value
            .get("server_tool_use")
            .filter(|value| !value.is_null())
            .cloned(),
    })
}

fn estimate_tokens_from_text(text: &str) -> u32 {
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

fn append_block_text(text: &mut String, block: &ContentBlock) {
    match block.kind.as_str() {
        "text" | "input_text" => {
            if let Some(value) = text_like_block_text(block) {
                text.push_str(&value);
            }
        }
        "document" => {
            if let Some(value) = document_text(block) {
                text.push_str(&value);
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
    // Axum's SSE helper takes a string payload, so serialize the Anthropic
    // event envelope once here before attaching the event name.
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
            images: None,
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
        assert_eq!(
            converted.tools.as_ref().unwrap()[0]
                .function
                .as_ref()
                .unwrap()
                .name,
            "lookup"
        );
        assert_eq!(converted.tool_choice, Some(json!("required")));
        assert_eq!(converted.parallel_tool_calls, Some(false));
    }

    #[test]
    fn explicit_tool_choice_disables_parallel_tool_calls() {
        let request: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role": "user", "content": "hello"}],
            "tool_choice": {"type": "tool", "name": "lookup"}
        }))
        .unwrap();

        let converted = to_openai_request(&request).unwrap();
        assert_eq!(converted.parallel_tool_calls, Some(false));
        assert_eq!(
            converted.tool_choice,
            Some(json!({"type": "function", "function": {"name": "lookup"}}))
        );
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
    fn preserves_user_content_around_tool_results() {
        let request: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "prefix"},
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "done"},
                    {"type": "text", "text": "suffix"}
                ]
            }]
        }))
        .unwrap();

        let converted = to_openai_request(&request).unwrap();
        assert_eq!(converted.messages.len(), 3);
        assert_eq!(converted.messages[0].role, "user");
        assert_eq!(converted.messages[1].role, "tool");
        assert_eq!(converted.messages[2].role, "user");
    }

    #[test]
    fn supports_input_text_and_document_blocks() {
        let request: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "hello"},
                    {"type": "document", "source": {"type": "text", "text": "doc body"}}
                ]
            }]
        }))
        .unwrap();

        let converted = to_openai_request(&request).unwrap();
        let parts = match converted.messages[0].content.as_ref().unwrap() {
            ChatContent::Parts(parts) => parts,
            ChatContent::Text(_) => panic!("expected multimodal parts"),
        };
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].text.as_deref(), Some("hello"));
        assert_eq!(parts[1].text.as_deref(), Some("doc body"));
    }

    #[test]
    fn maps_raw_reasoning_responses_to_thinking_blocks() {
        let response = json!({
            "id": "resp_reasoning",
            "model": "gpt-5.5",
            "status": "completed",
            "output": [
                {
                    "type": "reasoning",
                    "id": "rs_1",
                    "summary": [{"type": "summary_text", "text": "work"}],
                    "encrypted_content": "sig"
                },
                {
                    "type": "message",
                    "content": [{"type": "output_text", "text": "OK"}]
                }
            ],
            "usage": {"input_tokens": 12, "output_tokens": 5, "total_tokens": 17}
        });

        let mapped = from_openai_response_value(&response, "gpt-5.5");
        assert_eq!(mapped.content.len(), 2);
        assert_eq!(
            serde_json::to_value(&mapped.content[0]).unwrap()["type"],
            "thinking"
        );
        assert_eq!(
            serde_json::to_value(&mapped.content[0]).unwrap()["thinking"],
            "work"
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
                    images: None,
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

    #[test]
    fn estimates_input_tokens_include_documents_tools_and_thinking() {
        let request: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "thinking": {"type": "enabled", "budget_tokens": 2048},
            "tools": [{"name": "lookup", "description": "fetch docs", "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}}}],
            "messages": [{
                "role": "user",
                "content": [{"type": "document", "source": {"type": "text", "text": "document body"}}]
            }]
        }))
        .unwrap();

        assert!(estimate_input_tokens(&request) > 0);
    }

    #[test]
    fn strips_claude_code_billing_header_from_system_text() {
        let system = SystemPrompt::Text(
            "x-anthropic-billing-header: cc_version=2.1.38; cc_entrypoint=cli; cch=4873d;\n\nbe terse"
                .into(),
        );

        assert_eq!(
            system_prompt_text(Some(&system)).as_deref(),
            Some("be terse")
        );
    }

    #[test]
    fn strips_claude_code_billing_header_blocks_from_system_prompt() {
        let system = SystemPrompt::Blocks(vec![
            SystemBlock {
                kind: "text".into(),
                text: Some(
                    "x-anthropic-billing-header: cc_version=2.1.38; cc_entrypoint=cli; cch=4873d;"
                        .into(),
                ),
                cache_control: None,
            },
            SystemBlock {
                kind: "text".into(),
                text: Some("be terse".into()),
                cache_control: None,
            },
        ]);

        assert_eq!(
            system_prompt_text(Some(&system)).as_deref(),
            Some("be terse")
        );
    }
}
