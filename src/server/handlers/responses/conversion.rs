use crate::{
    anthropic::{MessagesRequest, to_openai_request},
    error::Result,
    openai::types::{
        ChatCompletionRequest, ChatContent, ChatContentPart, ChatMessage, ChatTool,
        ImageGenerationRequest, ImageUrl, ResponseInput, ResponseInputContent, ResponseInputItem,
        ResponseMessageInputItem, ResponsesRequest,
    },
    server::AppState,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

pub(in crate::server::handlers) async fn load_previous_response(
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

pub(in crate::server::handlers) fn responses_to_chat_request(
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

pub(in crate::server::handlers) fn response_request_requires_raw_mode(
    request: &ResponsesRequest,
    previous: Option<&crate::server::store::StoredResponse>,
) -> bool {
    request
        .tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(is_image_generation_tool))
        || previous_stores_generated_images(previous)
}

fn previous_stores_generated_images(
    previous: Option<&crate::server::store::StoredResponse>,
) -> bool {
    previous.is_some_and(|stored| {
        stored
            .input_items
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("image_generation_call"))
    })
}

fn is_image_generation_tool(tool: &ChatTool) -> bool {
    tool.kind == "image_generation"
}

pub(in crate::server::handlers) fn anthropic_responses_request(
    request: &MessagesRequest,
) -> Result<ResponsesRequest> {
    let openai = to_openai_request(request)?;

    Ok(ResponsesRequest {
        model: openai.model,
        input: Some(ResponseInput::Items(
            openai
                .messages
                .into_iter()
                .map(chat_message_to_response_input_item)
                .collect(),
        )),
        instructions: None,
        stream: openai.stream,
        temperature: openai.temperature,
        top_p: openai.top_p,
        tools: openai.tools,
        tool_choice: openai.tool_choice,
        service_tier: None,
        reasoning: None,
        max_output_tokens: request.max_tokens,
        parallel_tool_calls: openai.parallel_tool_calls,
        store: Some(false),
        previous_response_id: None,
        metadata: None,
        extra: request.extra.clone(),
    })
}

pub(in crate::server::handlers) fn image_generation_responses_request(
    request: &ImageGenerationRequest,
) -> ResponsesRequest {
    let mut tool = ChatTool {
        kind: "image_generation".to_owned(),
        function: None,
        extra: serde_json::Map::new(),
    };
    if let Some(size) = &request.size {
        tool.extra
            .insert("size".to_owned(), Value::String(size.clone()));
    }
    if let Some(quality) = &request.quality {
        tool.extra
            .insert("quality".to_owned(), Value::String(quality.clone()));
    }
    if let Some(background) = &request.background {
        tool.extra
            .insert("background".to_owned(), Value::String(background.clone()));
    }
    if let Some(output_format) = &request.output_format {
        tool.extra.insert(
            "output_format".to_owned(),
            Value::String(output_format.clone()),
        );
    }
    if let Some(n) = request.n {
        tool.extra.insert("n".to_owned(), Value::from(n));
    }

    ResponsesRequest {
        model: request.model.clone(),
        input: Some(ResponseInput::Text(request.prompt.clone())),
        instructions: None,
        stream: Some(false),
        temperature: None,
        top_p: None,
        tools: Some(vec![tool]),
        tool_choice: Some(json!({"type": "image_generation"})),
        service_tier: None,
        reasoning: None,
        max_output_tokens: None,
        parallel_tool_calls: Some(false),
        store: Some(false),
        previous_response_id: None,
        metadata: None,
        extra: request.extra.clone(),
    }
}

fn chat_message_to_response_input_item(message: ChatMessage) -> ResponseInputItem {
    ResponseInputItem::Message(ResponseMessageInputItem {
        kind: Some("message".to_owned()),
        role: message.role,
        content: message.content.map_or_else(
            || ResponseInputContent::Parts(Vec::new()),
            chat_content_to_response_input_content,
        ),
        id: None,
        name: message.name,
        tool_call_id: message.tool_call_id,
    })
}

fn chat_content_to_response_input_content(content: ChatContent) -> ResponseInputContent {
    match content {
        ChatContent::Text(text) => ResponseInputContent::Parts(vec![json_text_input_part(&text)]),
        ChatContent::Parts(parts) => ResponseInputContent::Parts(
            parts
                .into_iter()
                .filter_map(|part| match part.kind.as_str() {
                    "text" => part.text.map(|text| json_text_input_part(&text)),
                    "image_url" => {
                        part.image_url
                            .map(|image| crate::openai::types::ResponseInputContentPart {
                                kind: "input_image".to_owned(),
                                text: None,
                                image_url: Some(image.url),
                                detail: image.detail,
                            })
                    }
                    _ => None,
                })
                .collect(),
        ),
    }
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

pub(in crate::server::handlers) fn collect_response_input_items(
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

pub(in crate::server::handlers) fn estimate_response_input_tokens(input_items: &[Value]) -> u32 {
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

pub(in crate::server::handlers) fn compact_response_items(input_items: &[Value]) -> Vec<Value> {
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
