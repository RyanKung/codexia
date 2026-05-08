use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// OpenAI-compatible chat completions request body.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatCompletionRequest {
    /// Target model identifier.
    pub model: String,
    /// Conversation history sent to the model.
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
    /// Whether the upstream should return SSE chunks.
    #[serde(default)]
    pub stream: Option<bool>,
    /// Sampling temperature, when supported by the model.
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Nucleus sampling parameter, when supported by the model.
    #[serde(default)]
    pub top_p: Option<f64>,
    /// Callable tools exposed to the model.
    #[serde(default)]
    pub tools: Option<Vec<ChatTool>>,
    /// Tool selection mode or an explicitly chosen tool.
    #[serde(default)]
    pub tool_choice: Option<Value>,
    /// Optional service tier hint passed through to the upstream.
    #[serde(default)]
    pub service_tier: Option<String>,
    /// Optional reasoning effort level for reasoning-capable models.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Preferred upper bound for generated completion tokens.
    #[serde(default)]
    pub max_completion_tokens: Option<u32>,
    /// Legacy upper bound for generated tokens.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Whether tool calls may execute in parallel.
    #[serde(default)]
    pub parallel_tool_calls: Option<bool>,
    /// Optional stop sequences understood by compatible clients.
    #[serde(default)]
    pub stop: Option<Vec<String>>,
    /// Provider-specific extra request fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ChatCompletionRequest {
    /// Returns whether the request should use streaming responses.
    #[must_use]
    pub fn wants_stream(&self) -> bool {
        self.stream.unwrap_or(false)
    }
}

/// OpenAI-compatible Responses API request body.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResponsesRequest {
    /// Target model identifier.
    pub model: String,
    /// Input payload represented as text or structured input items.
    #[serde(default)]
    pub input: Option<ResponseInput>,
    /// Optional top-level instructions inserted ahead of the conversation.
    #[serde(default)]
    pub instructions: Option<String>,
    /// Whether the response should be streamed as semantic SSE events.
    #[serde(default)]
    pub stream: Option<bool>,
    /// Sampling temperature, when supported by the model.
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Nucleus sampling parameter, when supported by the model.
    #[serde(default)]
    pub top_p: Option<f64>,
    /// Callable tools exposed to the model.
    #[serde(default)]
    pub tools: Option<Vec<ChatTool>>,
    /// Tool selection mode or an explicitly chosen tool.
    #[serde(default)]
    pub tool_choice: Option<Value>,
    /// Optional service tier hint passed through to the upstream.
    #[serde(default)]
    pub service_tier: Option<String>,
    /// Optional reasoning configuration for reasoning-capable models.
    #[serde(default)]
    pub reasoning: Option<Value>,
    /// Preferred upper bound for generated completion tokens.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// Whether tool calls may execute in parallel.
    #[serde(default)]
    pub parallel_tool_calls: Option<bool>,
    /// Whether the created response should be retrievable later.
    #[serde(default)]
    pub store: Option<bool>,
    /// Identifier of a previous response to continue from.
    ///
    /// In Codexia this is implemented as best-effort in-memory continuation
    /// within the same running process rather than as a durable response
    /// resource lifecycle.
    #[serde(default)]
    pub previous_response_id: Option<String>,
    /// User metadata preserved on the stored response object.
    #[serde(default)]
    pub metadata: Option<Map<String, Value>>,
    /// Provider-specific extra request fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ResponsesRequest {
    /// Returns whether the request should use streaming responses.
    #[must_use]
    pub fn wants_stream(&self) -> bool {
        self.stream.unwrap_or(false)
    }

    /// Returns whether the response should be stored for later retrieval.
    #[must_use]
    pub fn should_store(&self) -> bool {
        self.store.unwrap_or(true)
    }

    /// Returns whether tool calls may execute in parallel.
    #[must_use]
    pub fn parallel_tool_calls(&self) -> bool {
        self.parallel_tool_calls.unwrap_or(true)
    }
}

/// Responses API input can be a plain string or a list of structured items.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ResponseInput {
    /// Plain text input sent as a single user turn.
    Text(String),
    /// Structured list of message-like input items.
    Items(Vec<ResponseInputItem>),
}

/// Structured input item accepted by the Responses API.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ResponseInputItem {
    /// Message-like input item carrying a role and content payload.
    Message(ResponseMessageInputItem),
    /// Opaque compaction item returned by `/v1/responses/compact`.
    Compaction(ResponseCompactionItem),
}

/// Message-like structured input item accepted by the Responses API.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResponseMessageInputItem {
    /// Object type, commonly `message`.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    /// Message role such as `user`, `assistant`, `developer`, `system`, or `tool`.
    pub role: String,
    /// Message payload represented as text or structured parts.
    pub content: ResponseInputContent,
    /// Optional item identifier supplied by the caller.
    #[serde(default)]
    pub id: Option<String>,
    /// Optional participant name for providers that support named messages.
    #[serde(default)]
    pub name: Option<String>,
    /// Tool call identifier referenced by a `tool` role item.
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

/// Opaque compaction item returned by the Responses compaction endpoint.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ResponseCompactionItem {
    /// Object type, always `compaction`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Opaque compaction payload preserved for later reuse.
    pub encrypted_content: String,
}

/// Structured input item content accepted by the Responses API.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ResponseInputContent {
    /// Plain text content.
    Text(String),
    /// Structured content parts.
    Parts(Vec<ResponseInputContentPart>),
}

/// Single structured content part within a Responses input item.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ResponseInputContentPart {
    /// Part type such as `input_text`, `output_text`, `text`, or `input_image`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Text payload for text parts.
    #[serde(default)]
    pub text: Option<String>,
    /// Image URL or data URL payload for image parts.
    #[serde(default)]
    pub image_url: Option<String>,
    /// Optional image detail hint such as `low`, `high`, or `auto`.
    #[serde(default)]
    pub detail: Option<String>,
}

/// Single chat message in an OpenAI-compatible conversation.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ChatMessage {
    /// Message role such as `system`, `user`, `assistant`, or `tool`.
    pub role: String,
    /// Message payload as plain text or multimodal parts.
    #[serde(default)]
    pub content: Option<ChatContent>,
    /// Optional participant name for providers that support named messages.
    #[serde(default)]
    pub name: Option<String>,
    /// Tool call identifier referenced by a `tool` role message.
    #[serde(default)]
    pub tool_call_id: Option<String>,
    /// Tool calls emitted by an assistant message.
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCall>>,
}

/// Chat message content represented as a plain string or structured parts.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum ChatContent {
    /// Plain text content.
    Text(String),
    /// Structured multimodal content parts.
    Parts(Vec<ChatContentPart>),
}

/// Single structured content part within a multimodal chat message.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ChatContentPart {
    /// Part type such as `text` or `image_url`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Text payload for text parts.
    #[serde(default)]
    pub text: Option<String>,
    /// Image payload for image parts.
    #[serde(default)]
    pub image_url: Option<ImageUrl>,
}

/// OpenAI-compatible image reference embedded in message content.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ImageUrl {
    /// Image URL or data URL.
    pub url: String,
    /// Optional detail hint such as `low`, `high`, or `auto`.
    #[serde(default)]
    pub detail: Option<String>,
}

/// Tool call emitted by an assistant response.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ToolCall {
    /// Stable identifier for correlating tool responses.
    pub id: String,
    /// Tool call type, currently `function`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Function invocation payload.
    pub function: FunctionCall,
}

/// Function call payload embedded in a tool call.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct FunctionCall {
    /// Function name selected by the model.
    pub name: String,
    /// JSON-encoded function arguments.
    pub arguments: String,
}

/// Tool definition exposed to the model.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatTool {
    /// Tool type such as `function` or `image_generation`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Function tool metadata when the tool is a callable function.
    #[serde(default)]
    pub function: Option<FunctionTool>,
    /// Additional hosted-tool fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Metadata describing a callable function tool.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FunctionTool {
    /// Function name advertised to the model.
    pub name: String,
    /// Optional human-readable description.
    #[serde(default)]
    pub description: Option<String>,
    /// JSON Schema describing accepted arguments.
    #[serde(default)]
    pub parameters: Option<Value>,
    /// Whether the model should adhere strictly to the schema.
    #[serde(default)]
    pub strict: Option<bool>,
}

/// Classic OpenAI-compatible Images API generation request body.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImageGenerationRequest {
    /// Target model identifier.
    pub model: String,
    /// Prompt describing the desired image.
    pub prompt: String,
    /// Number of images requested.
    #[serde(default)]
    pub n: Option<u32>,
    /// Requested image size such as `1024x1024`.
    #[serde(default)]
    pub size: Option<String>,
    /// Requested quality such as `low`, `medium`, or `high`.
    #[serde(default)]
    pub quality: Option<String>,
    /// Background hint such as `transparent`.
    #[serde(default)]
    pub background: Option<String>,
    /// Preferred output format such as `png`, `jpeg`, or `webp`.
    #[serde(default)]
    pub output_format: Option<String>,
    /// Response format, typically `b64_json` or `url`.
    #[serde(default)]
    pub response_format: Option<String>,
    /// Provider-specific extra request fields preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_string_message() {
        let request: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model": "gpt-5.4",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();

        assert!(!request.wants_stream());
        assert_eq!(
            request.messages[0].content,
            Some(ChatContent::Text("hello".into()))
        );
    }

    #[test]
    fn parses_multimodal_content_parts() {
        let message: ChatMessage = serde_json::from_value(serde_json::json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "look"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,abc"}}
            ]
        }))
        .unwrap();

        let parts = match message.content.unwrap() {
            ChatContent::Parts(parts) => parts,
            ChatContent::Text(_) => panic!("expected parts"),
        };
        assert_eq!(parts.len(), 2);
        assert_eq!(
            parts[1].image_url.as_ref().unwrap().url,
            "data:image/png;base64,abc"
        );
    }

    #[test]
    fn parses_compaction_input_item() {
        let item: ResponseInputItem = serde_json::from_value(serde_json::json!({
            "type": "compaction",
            "encrypted_content": "opaque"
        }))
        .unwrap();

        match item {
            ResponseInputItem::Compaction(compaction) => {
                assert_eq!(compaction.kind, "compaction");
                assert_eq!(compaction.encrypted_content, "opaque");
            }
            ResponseInputItem::Message(_) => panic!("expected compaction item"),
        }
    }
}
