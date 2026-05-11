use crate::{
    openai::response::{
        GeneratedImage, ResponseObject, Usage, response_function_call_item,
        response_image_generation_item, response_message_item,
    },
    openai::types::ResponsesRequest,
    server::AppState,
};
use serde_json::{Value, json};

pub(in crate::server::handlers) fn response_object_from_chat(
    request: &ResponsesRequest,
    response: crate::openai::response::ChatCompletionResponse,
) -> ResponseObject {
    let choice = response.choices.into_iter().next().unwrap_or_else(|| {
        crate::openai::response::ChatChoice {
            index: 0,
            message: crate::openai::response::AssistantMessage {
                role: "assistant",
                content: None,
                tool_calls: None,
                images: None,
            },
            finish_reason: "stop".to_owned(),
        }
    });
    let output = build_responses_output_items(
        &response.id,
        choice.message.content.as_deref().unwrap_or_default(),
        choice.message.tool_calls.unwrap_or_default(),
        choice.message.images.unwrap_or_default(),
    );

    ResponseObject {
        id: response.id.replace("chatcmpl", "resp"),
        object: "response",
        created_at: response.created,
        status: "completed".to_owned(),
        error: None,
        incomplete_details: None,
        instructions: request.instructions.clone(),
        max_output_tokens: request.max_output_tokens,
        model: response.model,
        output,
        parallel_tool_calls: request.parallel_tool_calls(),
        store: request.should_store(),
        temperature: request.temperature,
        tool_choice: request.tool_choice.clone(),
        tools: request
            .tools
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|tool| serde_json::to_value(tool).ok())
            .collect(),
        usage: response.usage,
        metadata: request.metadata.clone(),
        previous_response_id: request.previous_response_id.clone(),
    }
}

pub(in crate::server::handlers) fn response_object_from_upstream(
    request: &ResponsesRequest,
    response: &Value,
) -> ResponseObject {
    let created_at = response
        .get("created_at")
        .and_then(Value::as_i64)
        .unwrap_or_else(crate::config::now_unix);
    let output = response_output_items_from_upstream(
        response
            .get("output")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice),
    );

    ResponseObject {
        id: response
            .get("id")
            .and_then(Value::as_str)
            .map_or_else(build_response_id, str::to_owned),
        object: "response",
        created_at,
        status: response
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("completed")
            .to_owned(),
        error: response
            .get("error")
            .filter(|value| !value.is_null())
            .cloned(),
        incomplete_details: response
            .get("incomplete_details")
            .filter(|value| !value.is_null())
            .cloned(),
        instructions: request.instructions.clone(),
        max_output_tokens: request.max_output_tokens,
        model: response
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(&request.model)
            .to_owned(),
        output,
        parallel_tool_calls: request.parallel_tool_calls(),
        store: request.should_store(),
        temperature: request.temperature,
        tool_choice: request.tool_choice.clone(),
        tools: response_tool_values(request.tools.clone()),
        usage: parse_upstream_usage(response.get("usage")),
        metadata: request.metadata.clone(),
        previous_response_id: request.previous_response_id.clone(),
    }
}

pub(in crate::server::handlers) async fn maybe_store_response(
    state: &AppState,
    request: &ResponsesRequest,
    response: ResponseObject,
    input_items: Vec<Value>,
) {
    if request.should_store() {
        state
            .responses
            .insert(crate::server::store::StoredResponse {
                input_items: stored_response_input_items(input_items, &response),
                response,
            })
            .await;
    }
}

pub(in crate::server::handlers) fn stored_response_input_items(
    mut input_items: Vec<Value>,
    response: &ResponseObject,
) -> Vec<Value> {
    input_items.extend(response_output_to_input_items(&response.output));
    input_items
}

pub(in crate::server::handlers) fn build_response_id() -> String {
    format!(
        "resp_{}_{:08x}",
        crate::config::now_unix(),
        rand::random::<u32>()
    )
}

pub(in crate::server::handlers) fn build_responses_output_items(
    response_id: &str,
    output_text: &str,
    tool_calls: Vec<crate::openai::types::ToolCall>,
    images: Vec<GeneratedImage>,
) -> Vec<crate::openai::response::ResponseOutputItem> {
    let mut output = vec![response_message_item(
        format!("msg_{response_id}"),
        Some(output_text.to_owned()),
    )];
    for (index, tool_call) in tool_calls.into_iter().enumerate() {
        output.push(response_function_call_item(
            format!("fc_{response_id}_{index}"),
            tool_call,
        ));
    }
    for (index, image) in images.into_iter().enumerate() {
        output.push(response_image_generation_item(
            format!("ig_{response_id}_{index}"),
            image.b64_json,
            image.revised_prompt,
        ));
    }
    output
}

fn response_tool_values(tools: Option<Vec<crate::openai::types::ChatTool>>) -> Vec<Value> {
    tools
        .unwrap_or_default()
        .into_iter()
        .filter_map(|tool| serde_json::to_value(tool).ok())
        .collect()
}

fn response_output_items_from_upstream(
    items: &[Value],
) -> Vec<crate::openai::response::ResponseOutputItem> {
    let mut output = Vec::new();
    for (index, item) in items.iter().enumerate() {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
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
                output.push(response_message_item(
                    item.get("id")
                        .and_then(Value::as_str)
                        .map_or_else(|| format!("msg_{index}"), str::to_owned),
                    Some(text),
                ));
            }
            Some("function_call") => {
                let tool_call = crate::openai::types::ToolCall {
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
                            .unwrap_or("{}")
                            .to_owned(),
                    },
                };
                output.push(response_function_call_item(
                    item.get("id")
                        .and_then(Value::as_str)
                        .map_or_else(|| format!("fc_{index}"), str::to_owned),
                    tool_call,
                ));
            }
            Some("image_generation_call") => {
                if let Some(image) = crate::openai::response::generated_image_from_item(item) {
                    output.push(response_image_generation_item(
                        item.get("id")
                            .and_then(Value::as_str)
                            .map_or_else(|| format!("ig_{index}"), str::to_owned),
                        image.b64_json,
                        image.revised_prompt,
                    ));
                }
            }
            _ => {}
        }
    }
    output
}

fn response_output_to_input_items(
    items: &[crate::openai::response::ResponseOutputItem],
) -> Vec<Value> {
    let mut output = Vec::new();
    for item in items {
        match item.kind {
            "message" => {
                let content = item
                    .content
                    .iter()
                    .map(|part| {
                        json!({
                            "type": "output_text",
                            "text": part.text,
                            "annotations": part.annotations,
                        })
                    })
                    .collect::<Vec<_>>();
                output.push(json!({
                    "type": "message",
                    "role": item.role.unwrap_or("assistant"),
                    "status": item.status,
                    "id": item.id,
                    "content": content,
                }));
            }
            "function_call" => output.push(json!({
                "type": "function_call",
                "id": item.id,
                "call_id": item.call_id,
                "name": item.name,
                "arguments": item.arguments,
            })),
            "image_generation_call" => output.push(json!({
                "type": "image_generation_call",
                "id": item.id,
                "result": item.result,
                "revised_prompt": item.revised_prompt,
            })),
            _ => {}
        }
    }
    output
}

pub(in crate::server::handlers) fn parse_upstream_usage(value: Option<&Value>) -> Option<Usage> {
    let value = value?;
    let prompt_tokens = value
        .get("input_tokens")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0);
    let completion_tokens = value
        .get("output_tokens")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0);
    let total_tokens = value
        .get("total_tokens")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or_else(|| prompt_tokens.saturating_add(completion_tokens));

    Some(Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens,
    })
}

pub(in crate::server::handlers) fn response_object(
    request: &ResponsesRequest,
    response_id: String,
    created_at: i64,
    status: &str,
    output: Vec<crate::openai::response::ResponseOutputItem>,
    usage: Option<Usage>,
) -> ResponseObject {
    ResponseObject {
        id: response_id,
        object: "response",
        created_at,
        status: status.to_owned(),
        error: None,
        incomplete_details: None,
        instructions: request.instructions.clone(),
        max_output_tokens: request.max_output_tokens,
        model: request.model.clone(),
        output,
        parallel_tool_calls: request.parallel_tool_calls(),
        store: request.should_store(),
        temperature: request.temperature,
        tool_choice: request.tool_choice.clone(),
        tools: response_tool_values(request.tools.clone()),
        usage,
        metadata: request.metadata.clone(),
        previous_response_id: request.previous_response_id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn response_object_from_upstream_preserves_status_and_incomplete_details() {
        let request = ResponsesRequest {
            model: "gpt-5.5".to_owned(),
            input: None,
            instructions: Some("Be terse.".to_owned()),
            stream: Some(false),
            temperature: None,
            top_p: None,
            tools: None,
            tool_choice: None,
            service_tier: None,
            reasoning: None,
            max_output_tokens: Some(128),
            parallel_tool_calls: Some(false),
            store: Some(false),
            previous_response_id: None,
            metadata: None,
            extra: serde_json::Map::new(),
        };
        let response = json!({
            "id": "resp_incomplete",
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "model": "gpt-5.5",
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "partial"}]
            }]
        });

        let mapped = response_object_from_upstream(&request, &response);

        assert_eq!(mapped.status, "incomplete");
        assert_eq!(
            mapped
                .incomplete_details
                .as_ref()
                .and_then(|details| details.get("reason"))
                .and_then(Value::as_str),
            Some("max_output_tokens")
        );
        assert_eq!(mapped.output[0].kind, "message");
    }
}
