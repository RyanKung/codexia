fn reject_client_tools(_body: &Value) -> Result<()> {
    Ok(())
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
    let message_id = format!("msg_{id}");
    let usage = output.usage.as_ref().map(usage_value);
    json!({
        "id": id,
        "object": "response",
        "created_at": created_at,
        "status": "completed",
        "model": model,
        "output": [{
            "id": message_id,
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{
                "type": "output_text",
                "text": output.text,
                "annotations": []
            }]
        }],
        "usage": usage,
    })
}

fn response_events(body: &Value, output: &CursorOutput) -> Vec<JsonSseEvent> {
    let response = response_value(body, output);
    let response_id = response
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("resp_cursor");
    let item = response
        .get("output")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .cloned()
        .unwrap_or_else(|| json!({"id": format!("msg_{response_id}"), "type": "message"}));
    let item_id = item
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("msg_cursor");

    vec![
        named_event(
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
        ),
        named_event(
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "id": item_id,
                    "type": "message",
                    "role": "assistant",
                    "status": "in_progress",
                    "content": []
                }
            }),
        ),
        named_event(
            "response.output_text.delta",
            json!({
                "type": "response.output_text.delta",
                "output_index": 0,
                "item_id": item_id,
                "content_index": 0,
                "delta": output.text,
            }),
        ),
        named_event(
            "response.output_text.done",
            json!({
                "type": "response.output_text.done",
                "output_index": 0,
                "item_id": item_id,
                "content_index": 0,
                "text": output.text,
            }),
        ),
        named_event(
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": item,
            }),
        ),
        named_event(
            "response.completed",
            json!({
                "type": "response.completed",
                "response": response,
            }),
        ),
    ]
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
