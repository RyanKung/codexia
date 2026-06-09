fn body_has_client_tools(body: &Value) -> bool {
    body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty())
}

fn input_item_prompt_section(item: &Value) -> Result<Option<String>> {
    match item.get("type").and_then(Value::as_str) {
        Some("message") | None => message_prompt_section(item),
        Some("function_call") => Ok(Some(format!(
            "Assistant tool call:\n{}({})",
            item.get("name").and_then(Value::as_str).unwrap_or("tool"),
            item.get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default()
        ))),
        Some("function_call_output") => Ok(Some(format!(
            "Tool result{}:\n{}",
            item.get("call_id")
                .and_then(Value::as_str)
                .map(|id| format!(" {id}"))
                .unwrap_or_default(),
            item.get("output")
                .and_then(Value::as_str)
                .unwrap_or_default()
        ))),
        Some("reasoning" | "compaction") => Ok(None),
        Some(kind) => Err(Error::upstream_with_status(
            StatusCode::NOT_IMPLEMENTED,
            format!("Cursor provider does not support Responses input item type `{kind}`"),
        )),
    }
}

fn message_prompt_section(item: &Value) -> Result<Option<String>> {
    let role = item
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("user")
        .to_ascii_titlecase();
    let text = input_content_text(item.get("content"))?;
    Ok((!text.is_empty()).then(|| format!("{role}:\n{text}")))
}

fn input_content_text(content: Option<&Value>) -> Result<String> {
    match content {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(input_content_part_text)
            .collect::<Result<Vec<_>>>()
            .map(|parts| {
                parts
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n")
            }),
        Some(Value::Null) | None => Ok(String::new()),
        Some(other) => Ok(other.to_string()),
    }
}

fn input_content_part_text(part: &Value) -> Result<String> {
    match part.get("type").and_then(Value::as_str) {
        Some("input_text" | "output_text" | "text") | None => Ok(part
            .get("text")
            .or_else(|| part.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()),
        Some("input_image" | "image_url" | "input_file") => Err(Error::upstream_with_status(
            StatusCode::NOT_IMPLEMENTED,
            "Cursor provider does not support multimodal inputs through rotom",
        )),
        Some(kind) => Err(Error::upstream_with_status(
            StatusCode::NOT_IMPLEMENTED,
            format!("Cursor provider does not support content part type `{kind}`"),
        )),
    }
}

fn response_value(body: &Value, output: &CursorOutput) -> Value {
    let id = response_id(output);
    let created_at = now_unix();
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("cursor/auto");
    let usage = output.usage.as_ref().map(usage_value);
    let response_output = response_output_items(&id, output);
    json!({
        "id": id,
        "object": "response",
        "created_at": created_at,
        "status": "completed",
        "model": model,
        "output": response_output,
        "usage": usage,
    })
}

fn response_events(body: &Value, output: &CursorOutput) -> Vec<JsonSseEvent> {
    let response = response_value(body, output);
    let response_id = response
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("resp_cursor");
    let mut events = vec![named_event(
        "response.created",
        json!({
            "type": "response.created",
            "response": {
                "id": response_id,
                "object": "response",
                "created_at": response.get("created_at").cloned().unwrap_or_else(|| json!(now_unix())),
                "status": "in_progress",
                "model": response.get("model").cloned().unwrap_or_else(|| json!("cursor/auto")),
                "output": []
            }
        }),
    )];

    for (output_index, item) in response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let output_index = u32::try_from(output_index).unwrap_or(u32::MAX);
        push_response_output_item_events(&mut events, output_index, item);
    }

    events.push(named_event(
        "response.completed",
        json!({
            "type": "response.completed",
            "response": response,
        }),
    ));
    events
}

fn push_response_output_item_events(
    events: &mut Vec<JsonSseEvent>,
    output_index: u32,
    item: &Value,
) {
    if item.get("type").and_then(Value::as_str) == Some("function_call") {
        push_function_call_events(events, output_index, item);
    } else {
        push_message_output_events(events, output_index, item);
    }
}

fn push_function_call_events(events: &mut Vec<JsonSseEvent>, output_index: u32, item: &Value) {
    let item_id = item
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("cursor_item");
    let name = item.get("name").and_then(Value::as_str).unwrap_or("tool");
    let arguments = item
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let call_id = item
        .get("call_id")
        .and_then(Value::as_str)
        .unwrap_or(item_id);
    events.push(named_event(
        "response.output_item.added",
        json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": {
                "id": item_id,
                "type": "function_call",
                "status": "in_progress",
                "call_id": call_id,
                "name": name,
                "arguments": ""
            }
        }),
    ));
    if !arguments.is_empty() {
        events.push(named_event(
            "response.function_call_arguments.delta",
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": output_index,
                "item_id": item_id,
                "delta": arguments,
            }),
        ));
    }
    events.push(named_event(
        "response.function_call_arguments.done",
        json!({
            "type": "response.function_call_arguments.done",
            "output_index": output_index,
            "item_id": item_id,
            "name": name,
            "arguments": arguments,
        }),
    ));
    push_output_item_done(events, output_index, item);
}

fn push_message_output_events(events: &mut Vec<JsonSseEvent>, output_index: u32, item: &Value) {
    let item_id = item
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("cursor_item");
    let text = item
        .get("content")
        .and_then(Value::as_array)
        .and_then(|parts| parts.first())
        .and_then(|part| part.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    events.push(named_event(
        "response.output_item.added",
        json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": {
                "id": item_id,
                "type": "message",
                "role": "assistant",
                "status": "in_progress",
                "content": []
            }
        }),
    ));
    if !text.is_empty() {
        events.push(named_event(
            "response.output_text.delta",
            json!({
                "type": "response.output_text.delta",
                "output_index": output_index,
                "item_id": item_id,
                "content_index": 0,
                "delta": text,
            }),
        ));
        events.push(named_event(
            "response.output_text.done",
            json!({
                "type": "response.output_text.done",
                "output_index": output_index,
                "item_id": item_id,
                "content_index": 0,
                "text": text,
            }),
        ));
    }
    push_output_item_done(events, output_index, item);
}

fn push_output_item_done(events: &mut Vec<JsonSseEvent>, output_index: u32, item: &Value) {
    events.push(named_event(
        "response.output_item.done",
        json!({
            "type": "response.output_item.done",
            "output_index": output_index,
            "item": item,
        }),
    ));
}

fn named_event(event: &str, value: Value) -> JsonSseEvent {
    JsonSseEvent {
        event: Some(event.to_owned()),
        value,
    }
}

fn response_id(output: &CursorOutput) -> String {
    output
        .request_id
        .as_ref()
        .filter(|id| !id.is_empty())
        .map_or_else(
            || format!("resp_cursor_{}_{:08x}", now_unix(), rand::random::<u32>()),
            |id| format!("resp_cursor_{id}"),
        )
}

fn usage_value(usage: &Usage) -> Value {
    json!({
        "input_tokens": usage.prompt_tokens,
        "output_tokens": usage.completion_tokens,
        "total_tokens": usage.total_tokens,
    })
}

fn response_output_items(response_id: &str, output: &CursorOutput) -> Vec<Value> {
    let mut items = Vec::new();
    if !output.text.is_empty() || output.tool_calls.is_empty() {
        items.push(json!({
            "id": format!("msg_{response_id}"),
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{
                "type": "output_text",
                "text": output.text,
                "annotations": []
            }]
        }));
    }
    let tool_output_base = items.len();
    for (index, tool_call) in output.tool_calls.iter().enumerate() {
        let output_index = tool_output_base.saturating_add(index);
        items.push(json!({
            "id": format!("fc_{response_id}_{output_index}"),
            "type": "function_call",
            "status": "completed",
            "call_id": tool_call.id,
            "name": tool_call.function.name,
            "arguments": tool_call.function.arguments
        }));
    }
    items
}
