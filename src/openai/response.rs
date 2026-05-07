use crate::openai::types::{FunctionCall, ToolCall};
use serde::Serialize;
use serde_json::{Map, Value};

/// List response returned by the models endpoint.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ModelList {
    /// Object kind, always `list`.
    pub object: &'static str,
    /// Models included in the listing.
    pub data: Vec<ModelObject>,
}

/// Single model entry in a models list response.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ModelObject {
    /// Model identifier.
    pub id: String,
    /// Object kind, always `model`.
    pub object: &'static str,
    /// Owning organization label exposed to clients.
    pub owned_by: &'static str,
}

/// OpenAI-compatible non-streaming chat completion response body.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ChatCompletionResponse {
    /// Response identifier.
    pub id: String,
    /// Object kind, always `chat.completion`.
    pub object: &'static str,
    /// Unix timestamp when the response was created.
    pub created: i64,
    /// Model identifier that produced the response.
    pub model: String,
    /// Completion choices returned by the model.
    pub choices: Vec<ChatChoice>,
    /// Optional token accounting information.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

/// Single completion choice in a chat completion response.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ChatChoice {
    /// Choice index in the response.
    pub index: u32,
    /// Assistant message produced for this choice.
    pub message: AssistantMessage,
    /// Reason generation stopped for this choice.
    pub finish_reason: String,
}

/// Assistant message returned in a non-streaming response.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AssistantMessage {
    /// Message role, always `assistant`.
    pub role: &'static str,
    /// Assistant text content, when present.
    pub content: Option<String>,
    /// Tool calls emitted alongside or instead of text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

/// Token usage metadata reported by the upstream provider.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Usage {
    /// Tokens consumed by the prompt.
    pub prompt_tokens: u32,
    /// Tokens generated in the completion.
    pub completion_tokens: u32,
    /// Total tokens consumed by the request.
    pub total_tokens: u32,
}

/// OpenAI-compatible Responses API object.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ResponseObject {
    /// Response identifier.
    pub id: String,
    /// Object kind, always `response`.
    pub object: &'static str,
    /// Unix timestamp when the response was created.
    pub created_at: i64,
    /// Terminal response status.
    pub status: &'static str,
    /// Error details when generation failed.
    pub error: Option<Value>,
    /// Incomplete details when generation ended early.
    pub incomplete_details: Option<Value>,
    /// Top-level instructions associated with this response.
    pub instructions: Option<String>,
    /// Preferred upper bound for generated tokens.
    pub max_output_tokens: Option<u32>,
    /// Model identifier that produced the response.
    pub model: String,
    /// Output items emitted by the response.
    pub output: Vec<ResponseOutputItem>,
    /// Whether tool calls may run in parallel.
    pub parallel_tool_calls: bool,
    /// Whether the response was stored for later retrieval.
    pub store: bool,
    /// Optional sampling temperature recorded on the response.
    pub temperature: Option<f64>,
    /// Tool choice recorded on the response.
    pub tool_choice: Option<Value>,
    /// Tool definitions recorded on the response.
    pub tools: Vec<Value>,
    /// Optional token accounting information.
    pub usage: Option<Usage>,
    /// Optional user metadata preserved on the response.
    pub metadata: Option<Map<String, Value>>,
    /// Identifier of the referenced previous response, when supplied.
    pub previous_response_id: Option<String>,
}

/// Single output item returned by the Responses API.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ResponseOutputItem {
    /// Output item identifier.
    pub id: String,
    /// Object type, such as `message` or `function_call`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Role associated with the output item when it is a message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<&'static str>,
    /// Item status, always `completed` for fully collected local responses.
    pub status: &'static str,
    /// Message content blocks, when the output item is a message.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ResponseOutputContent>,
    /// Tool call identifier, when the output item is a function call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    /// Tool name, when the output item is a function call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// JSON-encoded tool arguments, when the output item is a function call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

/// Message content block within a Responses API output item.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ResponseOutputContent {
    /// Content type, currently `output_text`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Text payload emitted by the model.
    pub text: String,
    /// Output annotations attached to the text.
    pub annotations: Vec<Value>,
}

/// Response returned by `POST /v1/responses/input_tokens`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ResponseInputTokens {
    /// Estimated input token count for the submitted request.
    pub input_tokens: u32,
}

/// OpenAI-compatible streamed chat completion chunk.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ChatCompletionChunk {
    /// Response identifier shared across chunks.
    pub id: String,
    /// Object kind, always `chat.completion.chunk`.
    pub object: &'static str,
    /// Unix timestamp when the stream was created.
    pub created: i64,
    /// Model identifier that produced the stream.
    pub model: String,
    /// Incremental choice updates contained in this chunk.
    pub choices: Vec<ChunkChoice>,
}

/// Single choice delta inside a streaming chunk.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ChunkChoice {
    /// Choice index in the response.
    pub index: u32,
    /// Incremental message delta for this choice.
    pub delta: DeltaMessage,
    /// Optional terminal stop reason when the choice finishes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

/// Incremental assistant message payload carried by a stream chunk.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeltaMessage {
    /// Role emitted at the start of the stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<&'static str>,
    /// Text delta appended to the current assistant message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Incremental tool call deltas emitted by the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

/// Streaming representation of a single tool call delta.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ToolCallDelta {
    /// Zero-based tool call index within the assistant turn.
    pub index: u32,
    /// Stable identifier for correlating tool responses.
    pub id: String,
    /// Tool call type, currently `function`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Incremental function call payload.
    pub function: FunctionCall,
}

impl ModelList {
    /// Builds a model list from a sequence of model identifiers.
    #[must_use]
    pub fn from_ids(ids: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        Self {
            object: "list",
            data: ids
                .into_iter()
                .map(|id| ModelObject {
                    id: id.as_ref().to_owned(),
                    object: "model",
                    owned_by: "openai-codex",
                })
                .collect(),
        }
    }
}

/// Builds a message output item for the Responses API.
#[must_use]
pub fn response_message_item(id: String, text: Option<String>) -> ResponseOutputItem {
    let content = text
        .filter(|value| !value.is_empty())
        .map(|value| {
            vec![ResponseOutputContent {
                kind: "output_text",
                text: value,
                annotations: Vec::new(),
            }]
        })
        .unwrap_or_default();

    ResponseOutputItem {
        id,
        kind: "message",
        role: Some("assistant"),
        status: "completed",
        content,
        call_id: None,
        name: None,
        arguments: None,
    }
}

/// Builds a function call output item for the Responses API.
#[must_use]
pub fn response_function_call_item(id: String, tool_call: ToolCall) -> ResponseOutputItem {
    ResponseOutputItem {
        id,
        kind: "function_call",
        role: None,
        status: "completed",
        content: Vec::new(),
        call_id: Some(tool_call.id),
        name: Some(tool_call.function.name),
        arguments: Some(tool_call.function.arguments),
    }
}

/// Builds the initial stream chunk that introduces the assistant role.
#[must_use]
pub fn chunk_with_role(id: &str, created: i64, model: &str) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_owned(),
        object: "chat.completion.chunk",
        created,
        model: model.to_owned(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: DeltaMessage {
                role: Some("assistant"),
                content: None,
                tool_calls: None,
            },
            finish_reason: None,
        }],
    }
}

/// Builds a stream chunk that carries an assistant text delta.
#[must_use]
pub fn chunk_with_content(
    id: &str,
    created: i64,
    model: &str,
    content: String,
) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_owned(),
        object: "chat.completion.chunk",
        created,
        model: model.to_owned(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: DeltaMessage {
                role: None,
                content: Some(content),
                tool_calls: None,
            },
            finish_reason: None,
        }],
    }
}

/// Builds a stream chunk that carries a tool call delta.
#[must_use]
pub fn chunk_with_tool_call(
    id: &str,
    created: i64,
    model: &str,
    index: u32,
    tool_call: ToolCall,
) -> ChatCompletionChunk {
    // OpenAI streams tool calls as deltas, so convert the completed tool call
    // shape into a single incremental entry for this chunk.
    ChatCompletionChunk {
        id: id.to_owned(),
        object: "chat.completion.chunk",
        created,
        model: model.to_owned(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: DeltaMessage {
                role: None,
                content: None,
                tool_calls: Some(vec![ToolCallDelta {
                    index,
                    id: tool_call.id,
                    kind: "function",
                    function: tool_call.function,
                }]),
            },
            finish_reason: None,
        }],
    }
}

/// Builds the terminal stream chunk for a finished choice.
#[must_use]
pub fn chunk_finished(id: &str, created: i64, model: &str, reason: &str) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_owned(),
        object: "chat.completion.chunk",
        created,
        model: model.to_owned(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: DeltaMessage {
                role: None,
                content: None,
                tool_calls: None,
            },
            finish_reason: Some(reason.to_owned()),
        }],
    }
}
