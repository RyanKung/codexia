use crate::openai::types::{
    ChatCompletionRequest, ChatContent, ChatContentPart, ChatMessage, ChatTool, ResponsesRequest,
};
use serde_json::{Value, json};

/// Normalizes model IDs by removing the `openai-codex/` prefix when present.
#[must_use]
pub fn normalize_model(model: &str) -> String {
    model
        .strip_prefix("openai-codex/")
        .unwrap_or(model)
        .to_owned()
}

/// Converts a chat completions request into the JSON body expected by Codex.
pub fn to_codex_request(request: &ChatCompletionRequest) -> Value {
    let (instructions, input) = split_messages(&request.messages);
    let mut body = json!({
        "model": normalize_model(&request.model),
        "store": false,
        "stream": true,
        "instructions": instructions,
        "input": input,
        "text": { "verbosity": text_verbosity(request) },
        "include": ["reasoning.encrypted_content"],
        "tool_choice": convert_tool_choice(request.tool_choice.as_ref()).unwrap_or_else(|| json!("auto")),
        "parallel_tool_calls": request.parallel_tool_calls.unwrap_or(true)
    });

    insert_optional(
        &mut body,
        "service_tier",
        request.service_tier.clone().map(Value::from),
    );
    insert_optional(
        &mut body,
        "stop",
        request
            .stop
            .as_ref()
            .filter(|stop| !stop.is_empty())
            .cloned()
            .map(|stop| json!(stop)),
    );
    if let Some(tools) = request.tools.as_ref().filter(|tools| !tools.is_empty()) {
        body["tools"] = Value::Array(tools.iter().map(convert_tool).collect());
    }

    if let Some(effort) = request.reasoning_effort.as_deref() {
        body["reasoning"] = json!({
            "effort": clamp_reasoning_effort(&request.model, effort),
            "summary": "auto"
        });
    }

    body
}

fn split_messages(messages: &[ChatMessage]) -> (String, Vec<Value>) {
    messages
        .iter()
        .enumerate()
        .fold((Vec::new(), Vec::new()), |mut acc, (index, message)| {
            match message.role.as_str() {
                "system" | "developer" => {
                    if let Some(text) = message_text(message) {
                        acc.0.push(text);
                    }
                }
                "user" => acc.1.push(json!({
                    "role": "user",
                    "content": content_to_input_parts(message.content.as_ref())
                })),
                "assistant" => append_assistant_message(&mut acc.1, message, index),
                "tool" => {
                    if let Some(call_id) = message.tool_call_id.as_deref() {
                        acc.1.push(json!({
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": message_text(message).unwrap_or_default()
                        }));
                    }
                }
                _ => {}
            }
            acc
        })
        .map_first(|parts| parts.join("\n\n"))
}

fn append_assistant_message(input: &mut Vec<Value>, message: &ChatMessage, index: usize) {
    if let Some(text) = message_text(message).filter(|text| !text.is_empty()) {
        input.push(json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
            "status": "completed",
            "id": format!("msg_{index}")
        }));
    }

    for tool_call in message.tool_calls.iter().flatten() {
        input.push(json!({
            "type": "function_call",
            "call_id": &tool_call.id,
            "name": &tool_call.function.name,
            "arguments": &tool_call.function.arguments
        }));
    }
}

fn content_to_input_parts(content: Option<&ChatContent>) -> Vec<Value> {
    match content {
        Some(ChatContent::Text(text)) => vec![json!({ "type": "input_text", "text": text })],
        Some(ChatContent::Parts(parts)) => parts.iter().filter_map(convert_content_part).collect(),
        None => Vec::new(),
    }
}

fn convert_content_part(part: &ChatContentPart) -> Option<Value> {
    match part.kind.as_str() {
        "text" => part
            .text
            .as_ref()
            .map(|text| json!({ "type": "input_text", "text": text })),
        "image_url" => part.image_url.as_ref().map(|image| {
            json!({
                "type": "input_image",
                "detail": image.detail.as_deref().unwrap_or("auto"),
                "image_url": image.url
            })
        }),
        _ => None,
    }
}

/// Converts one compatibility-layer tool definition into the upstream Codex shape.
///
/// # Panics
///
/// Panics when a tool declares `"function"` as its type but omits the
/// corresponding function metadata payload.
#[must_use]
pub fn convert_tool(tool: &ChatTool) -> Value {
    if tool.kind == "function" {
        let function = tool
            .function
            .as_ref()
            .expect("function tool metadata missing");
        json!({
            "type": "function",
            "name": &function.name,
            "description": function.description.clone().unwrap_or_default(),
            "parameters": function.parameters.clone().unwrap_or_else(|| json!({ "type": "object" })),
            "strict": function.strict
        })
    } else {
        let mut value = Value::Object(tool.extra.clone());
        value["type"] = Value::String(tool.kind.clone());
        value
    }
}

/// Normalizes compatibility-layer tool choice values into the upstream Codex shape.
pub fn convert_tool_choice(tool_choice: Option<&Value>) -> Option<Value> {
    let choice = tool_choice?;
    if choice.is_string() {
        return Some(choice.clone());
    }

    let kind = choice.get("type").and_then(Value::as_str)?;
    if kind != "function" {
        return Some(choice.clone());
    }

    let name = choice
        .get("name")
        .or_else(|| choice.pointer("/function/name"))?
        .as_str()?;

    Some(json!({
        "type": "function",
        "name": name
    }))
}

/// Converts a Responses request plus normalized input items into the Codex body.
#[must_use]
pub fn responses_to_codex_request(request: &ResponsesRequest, input: &[Value]) -> Value {
    let mut body = json!({
        "model": normalize_model(&request.model),
        "store": request.should_store(),
        "stream": request.wants_stream(),
        "input": input,
        "text": { "verbosity": request.extra.get("text_verbosity").and_then(Value::as_str).unwrap_or("medium") },
        "include": ["reasoning.encrypted_content"],
        "tool_choice": convert_tool_choice(request.tool_choice.as_ref()).unwrap_or_else(|| json!("auto")),
        "parallel_tool_calls": request.parallel_tool_calls()
    });

    insert_optional(
        &mut body,
        "instructions",
        request.instructions.clone().map(Value::from),
    );
    insert_optional(
        &mut body,
        "service_tier",
        request.service_tier.clone().map(Value::from),
    );
    if let Some(tools) = request.tools.as_ref().filter(|tools| !tools.is_empty()) {
        body["tools"] = Value::Array(tools.iter().map(convert_tool).collect());
    }
    if let Some(reasoning) = request.reasoning.clone() {
        body["reasoning"] = reasoning;
    }
    insert_optional(
        &mut body,
        "max_output_tokens",
        request.max_output_tokens.map(Value::from),
    );

    body
}

fn message_text(message: &ChatMessage) -> Option<String> {
    match message.content.as_ref()? {
        ChatContent::Text(text) => Some(text.clone()),
        ChatContent::Parts(parts) => {
            let text = parts
                .iter()
                .filter_map(|part| match part.kind.as_str() {
                    "text" => part.text.as_deref(),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            Some(text)
        }
    }
}

fn text_verbosity(request: &ChatCompletionRequest) -> String {
    request
        .extra
        .get("text_verbosity")
        .and_then(Value::as_str)
        .unwrap_or("medium")
        .to_owned()
}

fn clamp_reasoning_effort(model: &str, effort: &str) -> String {
    let id = normalize_model(model);
    if (id.starts_with("gpt-5.2")
        || id.starts_with("gpt-5.3")
        || id.starts_with("gpt-5.4")
        || id.starts_with("gpt-5.5"))
        && effort == "minimal"
    {
        "low".to_owned()
    } else if id == "gpt-5.1" && effort == "xhigh" {
        "high".to_owned()
    } else {
        effort.to_owned()
    }
}

fn insert_optional(body: &mut Value, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        body[key] = value;
    }
}

trait TupleMapFirst<A, B> {
    fn map_first<C>(self, map: impl FnOnce(A) -> C) -> (C, B);
}

impl<A, B> TupleMapFirst<A, B> for (A, B) {
    fn map_first<C>(self, map: impl FnOnce(A) -> C) -> (C, B) {
        (map(self.0), self.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai::types::ChatCompletionRequest;

    fn request(value: Value) -> ChatCompletionRequest {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn strips_openai_codex_prefix() {
        assert_eq!(normalize_model("openai-codex/gpt-5.4"), "gpt-5.4");
        assert_eq!(normalize_model("gpt-5.4"), "gpt-5.4");
    }

    #[test]
    fn converts_chat_messages_to_responses_body() {
        let body = to_codex_request(&request(json!({
            "model": "openai-codex/gpt-5.4",
            "messages": [
                {"role": "system", "content": "be terse"},
                {"role": "user", "content": "hello"}
            ],
            "temperature": 0.2
        })));

        assert_eq!(body["model"], "gpt-5.4");
        assert_eq!(body["instructions"], "be terse");
        assert!(body.get("temperature").is_none());
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
    }

    #[test]
    fn does_not_forward_unsupported_sampling_controls() {
        let body = to_codex_request(&request(json!({
            "model": "gpt-5.5",
            "messages": [{"role": "user", "content": "hello"}],
            "temperature": 0.2,
            "top_p": 0.7,
            "parallel_tool_calls": false,
            "stop": ["DONE"]
        })));

        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["stop"], json!(["DONE"]));
    }

    #[test]
    fn converts_assistant_tool_calls_and_tool_results() {
        let body = to_codex_request(&request(json!({
            "model": "gpt-5.4",
            "messages": [
                {"role": "assistant", "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "lookup", "arguments": "{\"q\":\"x\"}"}
                }]},
                {"role": "tool", "tool_call_id": "call_1", "content": "done"}
            ]
        })));

        assert_eq!(body["input"][0]["type"], "function_call");
        assert_eq!(body["input"][1]["type"], "function_call_output");
    }

    #[test]
    fn clamps_minimal_reasoning_for_new_codex_models() {
        let body = to_codex_request(&request(json!({
            "model": "gpt-5.5",
            "messages": [],
            "reasoning_effort": "minimal"
        })));

        assert_eq!(body["reasoning"]["effort"], "low");
    }

    #[test]
    fn does_not_forward_unsupported_chat_completion_token_limit() {
        let body = to_codex_request(&request(json!({
            "model": "gpt-5.4",
            "messages": [],
            "max_completion_tokens": 42
        })));

        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn maps_chat_tool_choice_to_responses_tool_choice() {
        let body = to_codex_request(&request(json!({
            "model": "gpt-5.4",
            "messages": [],
            "tool_choice": {"type": "function", "function": {"name": "lookup"}}
        })));

        assert_eq!(
            body["tool_choice"],
            json!({"type": "function", "name": "lookup"})
        );
    }

    #[test]
    fn preserves_string_tool_choice() {
        let body = to_codex_request(&request(json!({
            "model": "gpt-5.4",
            "messages": [],
            "tool_choice": "required"
        })));

        assert_eq!(body["tool_choice"], "required");
    }
}
