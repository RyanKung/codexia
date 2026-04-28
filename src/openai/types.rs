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
    /// Tool type, currently `function`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Function tool metadata.
    pub function: FunctionTool,
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
}
