use crate::openai::types::{FunctionCall, ToolCall};
use serde::Serialize;

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
#[derive(Debug, Clone, Serialize, PartialEq)]
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
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ChatChoice {
    /// Choice index in the response.
    pub index: u32,
    /// Assistant message produced for this choice.
    pub message: AssistantMessage,
    /// Reason generation stopped for this choice.
    pub finish_reason: String,
}

/// Assistant message returned in a non-streaming response.
#[derive(Debug, Clone, Serialize, PartialEq)]
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

/// OpenAI-compatible streamed chat completion chunk.
#[derive(Debug, Clone, Serialize, PartialEq)]
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
#[derive(Debug, Clone, Serialize, PartialEq)]
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
#[derive(Debug, Clone, Serialize, PartialEq)]
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
#[derive(Debug, Clone, Serialize, PartialEq)]
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

/// Builds the initial stream chunk that introduces the assistant role.
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
