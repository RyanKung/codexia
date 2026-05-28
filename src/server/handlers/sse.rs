use super::responses::{
    build_responses_output_items, response_object, response_object_from_upstream,
    stored_response_input_items,
};
use crate::{
    anthropic::{
        content_block_stop, error_body, image_block_start, message_delta_event,
        message_start_event, message_stop_event, signature_delta, text_block_start, text_delta,
        thinking_block_start, thinking_delta, tool_block_start, tool_json_delta,
    },
    codex::events::{
        event_error, finish_reason, is_done_event, normalize_incomplete_result_response,
    },
    error::Result,
    openai::response::{GeneratedImage, ResponseObject, generated_images_from_output},
};
use axum::response::{
    IntoResponse,
    sse::{Event, KeepAlive, Sse},
};
use futures_util::{Stream, StreamExt, stream};
use serde_json::{Value, json};
use std::{convert::Infallible, pin::Pin};

use crate::openai::types::ToolCall;

fn response_created_event(response: &ResponseObject) -> Event {
    Event::default().event("response.created").data(
        json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": response,
        })
        .to_string(),
    )
}

fn response_in_progress_event(response: &ResponseObject) -> Event {
    Event::default().event("response.in_progress").data(
        json!({
            "type": "response.in_progress",
            "sequence_number": 1,
            "response": response,
        })
        .to_string(),
    )
}

fn response_output_item_added_event(
    sequence_number: u64,
    response_id: &str,
    item_id: &str,
) -> Event {
    Event::default().event("response.output_item.added").data(
        json!({
            "type": "response.output_item.added",
            "sequence_number": sequence_number,
            "response_id": response_id,
            "output_index": 0,
            "item": {
                "id": item_id,
                "type": "message",
                "status": "in_progress",
                "role": "assistant",
                "content": []
            }
        })
        .to_string(),
    )
}

fn output_text_part(text: &str) -> Value {
    json!({
        "type": "output_text",
        "text": text,
        "annotations": []
    })
}

fn response_content_part_added_event(
    sequence_number: u64,
    response_id: &str,
    item_id: &str,
) -> Event {
    Event::default().event("response.content_part.added").data(
        json!({
            "type": "response.content_part.added",
            "sequence_number": sequence_number,
            "response_id": response_id,
            "item_id": item_id,
            "output_index": 0,
            "content_index": 0,
            "part": output_text_part("")
        })
        .to_string(),
    )
}

fn response_started_lifecycle_events(
    sequence_number: u64,
    response_id: &str,
    item_id: &str,
) -> [Event; 2] {
    [
        response_output_item_added_event(sequence_number, response_id, item_id),
        response_content_part_added_event(sequence_number.saturating_add(1), response_id, item_id),
    ]
}

fn response_output_text_delta_event(
    sequence_number: u64,
    response_id: &str,
    item_id: &str,
    text: &str,
) -> Event {
    Event::default().event("response.output_text.delta").data(
        json!({
            "type": "response.output_text.delta",
            "sequence_number": sequence_number,
            "response_id": response_id,
            "item_id": item_id,
            "output_index": 0,
            "content_index": 0,
            "delta": text,
        })
        .to_string(),
    )
}

fn response_output_text_done_event(
    sequence_number: u64,
    response_id: &str,
    item_id: &str,
    text: &str,
) -> Event {
    Event::default().event("response.output_text.done").data(
        json!({
            "type": "response.output_text.done",
            "sequence_number": sequence_number,
            "response_id": response_id,
            "item_id": item_id,
            "output_index": 0,
            "content_index": 0,
            "text": text,
        })
        .to_string(),
    )
}

fn response_content_part_done_event(
    sequence_number: u64,
    response_id: &str,
    item_id: &str,
    text: &str,
) -> Event {
    Event::default().event("response.content_part.done").data(
        json!({
            "type": "response.content_part.done",
            "sequence_number": sequence_number,
            "response_id": response_id,
            "item_id": item_id,
            "output_index": 0,
            "content_index": 0,
            "part": output_text_part(text)
        })
        .to_string(),
    )
}

fn response_output_item_done_event(
    sequence_number: u64,
    response_id: &str,
    item_id: &str,
    text: &str,
) -> Event {
    Event::default().event("response.output_item.done").data(
        json!({
            "type": "response.output_item.done",
            "sequence_number": sequence_number,
            "response_id": response_id,
            "output_index": 0,
            "item": {
                "id": item_id,
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [output_text_part(text)]
            }
        })
        .to_string(),
    )
}

fn response_finished_lifecycle_events(
    sequence_number: u64,
    response_id: &str,
    item_id: &str,
    text: &str,
) -> [Event; 3] {
    [
        response_output_text_done_event(sequence_number, response_id, item_id, text),
        response_content_part_done_event(
            sequence_number.saturating_add(1),
            response_id,
            item_id,
            text,
        ),
        response_output_item_done_event(
            sequence_number.saturating_add(2),
            response_id,
            item_id,
            text,
        ),
    ]
}

fn response_completed_event(sequence_number: u64, response: &ResponseObject) -> Event {
    Event::default().event("response.completed").data(
        json!({
            "type": "response.completed",
            "sequence_number": sequence_number,
            "response": response,
        })
        .to_string(),
    )
}

fn response_function_call_item_added_event(
    sequence_number: u64,
    response_id: &str,
    output_index: u32,
    item_id: &str,
    tool_call: &ToolCall,
) -> Event {
    Event::default().event("response.output_item.added").data(
        json!({
            "type": "response.output_item.added",
            "sequence_number": sequence_number,
            "response_id": response_id,
            "output_index": output_index,
            "item": {
                "id": item_id,
                "type": "function_call",
                "status": "in_progress",
                "call_id": tool_call.id,
                "name": tool_call.function.name,
                "arguments": ""
            }
        })
        .to_string(),
    )
}

fn response_function_call_arguments_delta_event(
    sequence_number: u64,
    response_id: &str,
    output_index: u32,
    item_id: &str,
    arguments: &str,
) -> Event {
    Event::default()
        .event("response.function_call_arguments.delta")
        .data(
            json!({
                "type": "response.function_call_arguments.delta",
                "sequence_number": sequence_number,
                "response_id": response_id,
                "output_index": output_index,
                "item_id": item_id,
                "delta": arguments
            })
            .to_string(),
        )
}

fn response_function_call_arguments_done_event(
    sequence_number: u64,
    response_id: &str,
    output_index: u32,
    item_id: &str,
    name: &str,
    arguments: &str,
) -> Event {
    Event::default()
        .event("response.function_call_arguments.done")
        .data(
            json!({
                "type": "response.function_call_arguments.done",
                "sequence_number": sequence_number,
                "response_id": response_id,
                "output_index": output_index,
                "item_id": item_id,
                "name": name,
                "arguments": arguments
            })
            .to_string(),
        )
}

fn response_function_call_item_done_event(
    sequence_number: u64,
    response_id: &str,
    output_index: u32,
    item_id: &str,
    tool_call: &ToolCall,
) -> Event {
    Event::default().event("response.output_item.done").data(
        json!({
            "type": "response.output_item.done",
            "sequence_number": sequence_number,
            "response_id": response_id,
            "output_index": output_index,
            "item": {
                "id": item_id,
                "type": "function_call",
                "status": "completed",
                "call_id": tool_call.id,
                "name": tool_call.function.name,
                "arguments": tool_call.function.arguments
            }
        })
        .to_string(),
    )
}

fn response_function_call_events(
    sequence_number: u64,
    response_id: &str,
    output_index: u32,
    item_id: &str,
    tool_call: &ToolCall,
) -> Vec<Event> {
    let mut events = vec![response_function_call_item_added_event(
        sequence_number,
        response_id,
        output_index,
        item_id,
        tool_call,
    )];
    let mut next_sequence = sequence_number.saturating_add(1);
    if !tool_call.function.arguments.is_empty() {
        events.push(response_function_call_arguments_delta_event(
            next_sequence,
            response_id,
            output_index,
            item_id,
            &tool_call.function.arguments,
        ));
        next_sequence = next_sequence.saturating_add(1);
    }
    events.push(response_function_call_arguments_done_event(
        next_sequence,
        response_id,
        output_index,
        item_id,
        &tool_call.function.name,
        &tool_call.function.arguments,
    ));
    events.push(response_function_call_item_done_event(
        next_sequence.saturating_add(1),
        response_id,
        output_index,
        item_id,
        tool_call,
    ));
    events
}

fn response_error_event(error: &crate::Error) -> Event {
    Event::default().event("error").data(
        json!({
            "type": "error",
            "error": {
                "message": error.to_string(),
                "type": "upstream_error",
            }
        })
        .to_string(),
    )
}

pub(super) fn sse_response(
    stream: Pin<
        Box<dyn Stream<Item = Result<crate::openai::response::ChatCompletionChunk>> + Send>,
    >,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>>> {
    let mapped = stream.map(|item| {
        let event = match item {
            Ok(chunk) => Event::default().data(serde_json::to_string(&chunk).unwrap_or_default()),
            Err(error) => Event::default().data(
                json!({"error": {"message": error.to_string(), "type": "upstream_error"}})
                    .to_string(),
            ),
        };
        Ok(event)
    });

    let done = stream::once(async { Ok(Event::default().data("[DONE]")) });
    Sse::new(mapped.chain(done)).keep_alive(KeepAlive::default())
}

struct OpenAiResponsesStreamState {
    response_id: String,
    text_item_id: String,
    sequence_number: u64,
    output_text: String,
    tool_calls: Vec<ToolCall>,
    text_item_started: bool,
}

impl OpenAiResponsesStreamState {
    fn new(response_id: String) -> Self {
        Self {
            text_item_id: format!("msg_{response_id}"),
            response_id,
            sequence_number: 2,
            output_text: String::new(),
            tool_calls: Vec::new(),
            text_item_started: false,
        }
    }

    fn text_delta_events(&mut self, text: &str) -> Vec<Event> {
        if text.is_empty() {
            return Vec::new();
        }

        let mut events = Vec::new();
        if !self.text_item_started {
            events.extend(response_started_lifecycle_events(
                self.sequence_number,
                &self.response_id,
                &self.text_item_id,
            ));
            self.sequence_number = self.sequence_number.saturating_add(2);
            self.text_item_started = true;
        }

        self.output_text.push_str(text);
        events.push(response_output_text_delta_event(
            self.sequence_number,
            &self.response_id,
            &self.text_item_id,
            text,
        ));
        self.sequence_number = self.sequence_number.saturating_add(1);
        events
    }

    fn tool_call_events(&mut self, tool_call: ToolCall) -> Vec<Event> {
        let tool_index = if self.text_item_started {
            self.tool_calls.len().saturating_add(1)
        } else {
            self.tool_calls.len()
        };
        let output_index = u32::try_from(tool_index).unwrap_or(u32::MAX);
        let item_id = format!("fc_{}_{tool_index}", self.response_id);
        let events = response_function_call_events(
            self.sequence_number,
            &self.response_id,
            output_index,
            &item_id,
            &tool_call,
        );
        self.sequence_number = self
            .sequence_number
            .saturating_add(u64::try_from(events.len()).unwrap_or(u64::MAX));
        self.tool_calls.push(tool_call);
        events
    }

    fn text_done_events(&mut self) -> Vec<Event> {
        if !self.text_item_started {
            return Vec::new();
        }
        let events = response_finished_lifecycle_events(
            self.sequence_number,
            &self.response_id,
            &self.text_item_id,
            &self.output_text,
        );
        self.sequence_number = self.sequence_number.saturating_add(3);
        events.into()
    }
}

pub(super) fn openai_responses_sse(
    stream: Pin<
        Box<dyn Stream<Item = Result<crate::openai::response::ChatCompletionChunk>> + Send>,
    >,
    response_id: String,
    request: crate::openai::types::ResponsesRequest,
    input_items: Vec<Value>,
    store: crate::server::store::ResponseStore,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>>> {
    let mapped = async_stream::stream! {
        let created_at = crate::config::now_unix();
        let created_response = response_object(
            &request,
            response_id.clone(),
            created_at,
            "in_progress",
            Vec::new(),
            None,
        );
        yield Ok(response_created_event(&created_response));
        yield Ok(response_in_progress_event(&created_response));

        let mut stream = stream;
        let mut stream_state = OpenAiResponsesStreamState::new(response_id.clone());

        while let Some(item) = stream.next().await {
            match item {
                Ok(chunk) => {
                    let Some(choice) = chunk.choices.into_iter().next() else {
                        continue;
                    };

                    if let Some(text) = choice.delta.content {
                        for event in stream_state.text_delta_events(&text) {
                            yield Ok(event);
                        }
                    }

                    for tool_call in choice.delta.tool_calls.into_iter().flatten() {
                        let tool_call = crate::openai::types::ToolCall {
                            id: tool_call.id,
                            kind: tool_call.kind.to_owned(),
                            function: tool_call.function,
                        };
                        for event in stream_state.tool_call_events(tool_call) {
                            yield Ok(event);
                        }
                    }

                    if choice.finish_reason.is_some() {
                        let output_text = stream_state.output_text.clone();
                        let tool_calls = std::mem::take(&mut stream_state.tool_calls);
                        let output = build_responses_output_items(
                            &response_id,
                            &output_text,
                            tool_calls,
                            Vec::<GeneratedImage>::new(),
                        );
                        let completed = response_object(
                            &request,
                            response_id.clone(),
                            created_at,
                            "completed",
                            output,
                            None,
                        );
                        if request.should_store() {
                            let stored_items =
                                stored_response_input_items(input_items.clone(), &completed);
                            store.insert(crate::server::store::StoredResponse {
                                response: completed.clone(),
                                input_items: stored_items,
                            }).await;
                        }
                        for event in stream_state.text_done_events() {
                            yield Ok(event);
                        }
                        yield Ok(response_completed_event(stream_state.sequence_number, &completed));
                        return;
                    }
                }
                Err(error) => {
                    yield Ok(response_error_event(&error));
                    return;
                }
            }
        }
    };

    Sse::new(mapped).keep_alive(KeepAlive::default())
}

pub(super) fn openai_raw_responses_sse(
    stream: Pin<Box<dyn Stream<Item = Result<crate::codex::sse::JsonSseEvent>> + Send>>,
    request: crate::openai::types::ResponsesRequest,
    input_items: Vec<Value>,
    store: crate::server::store::ResponseStore,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>>> {
    let mapped = async_stream::stream! {
        let mut stream = stream;
        let mut state = RawResponsesStreamState::default();

        while let Some(item) = stream.next().await {
            match item {
                Ok(mut item) => {
                    sanitize_raw_responses_event(&mut item.value, &mut state);
                    if is_done_event(&item.value) {
                        if let Some(response) = item.value.get("response").cloned() {
                            let completed = response_object_from_upstream(&request, &response);
                            if request.should_store() {
                                let stored_items =
                                    stored_response_input_items(input_items.clone(), &completed);
                                store.insert(crate::server::store::StoredResponse {
                                    response: completed,
                                    input_items: stored_items,
                                }).await;
                            }
                        }
                    }

                    let event_name = item
                        .value
                        .get("type")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or(item.event)
                        .unwrap_or_else(|| "message".to_owned());
                    yield Ok(Event::default()
                        .event(event_name)
                        .data(serde_json::to_string(&item.value).unwrap_or_default()));
                }
                Err(error) => {
                    yield Ok(response_error_event(&error));
                    return;
                }
            }
        }
    };

    Sse::new(mapped).keep_alive(KeepAlive::default())
}

#[derive(Default)]
struct RawResponsesStreamState {
    output_items: std::collections::BTreeMap<u32, Value>,
    text_items: std::collections::BTreeMap<u32, RawTextOutput>,
    function_names: std::collections::BTreeMap<u32, String>,
}

#[derive(Default)]
struct RawTextOutput {
    item_id: Option<String>,
    text: String,
}

fn sanitize_raw_responses_event(event: &mut Value, state: &mut RawResponsesStreamState) {
    let is_terminal = is_done_event(event);
    match event.get("type").and_then(Value::as_str) {
        Some("response.output_text.delta") => {
            let index = output_index(event);
            let text = event
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let item = state.text_items.entry(index).or_default();
            remember_item_id(item, event);
            item.text.push_str(text);
        }
        Some("response.output_text.done") => {
            let index = output_index(event);
            let text = event.get("text").and_then(Value::as_str);
            let item = state.text_items.entry(index).or_default();
            remember_item_id(item, event);
            if let Some(text) = text {
                text.clone_into(&mut item.text);
            }
        }
        Some("response.output_item.added") => remember_function_name(state, event),
        Some("response.output_item.done") => {
            remember_function_name(state, event);
            if let Some(item) = event.get("item").cloned() {
                state.output_items.insert(output_index(event), item);
            }
        }
        Some("response.function_call_arguments.done") => {
            let index = output_index(event);
            if event.get("name").and_then(Value::as_str).is_none()
                && let Some(name) = state.function_names.get(&index)
                && let Some(object) = event.as_object_mut()
            {
                object.insert("name".to_owned(), Value::String(name.clone()));
            }
        }
        _ => {}
    }

    {
        let Some(response) = event.get_mut("response").and_then(Value::as_object_mut) else {
            return;
        };
        if is_terminal && response_output_needs_backfill(response.get("output")) {
            response.insert("output".to_owned(), Value::Array(state.recovered_output()));
        } else if response_output_needs_array(response.get("output")) {
            response.insert("output".to_owned(), Value::Array(Vec::new()));
        }
    }

    if is_terminal
        && event.get("type").and_then(Value::as_str) == Some("response.incomplete")
        && event
            .get_mut("response")
            .is_some_and(normalize_incomplete_result_response)
    {
        event["type"] = Value::String("response.completed".to_owned());
    }
}

fn remember_item_id(item: &mut RawTextOutput, event: &Value) {
    if item.item_id.is_none() {
        item.item_id = event
            .get("item_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
    }
}

fn remember_function_name(state: &mut RawResponsesStreamState, event: &Value) {
    let Some(item) = event.get("item") else {
        return;
    };
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return;
    }
    if let Some(name) = item.get("name").and_then(Value::as_str) {
        state
            .function_names
            .insert(output_index(event), name.to_owned());
    }
}

fn response_output_needs_backfill(output: Option<&Value>) -> bool {
    match output {
        Some(Value::Array(items)) => items.is_empty(),
        Some(Value::Null) | None => true,
        Some(_) => false,
    }
}

const fn response_output_needs_array(output: Option<&Value>) -> bool {
    matches!(output, Some(Value::Null) | None)
}

impl RawResponsesStreamState {
    fn recovered_output(&self) -> Vec<Value> {
        let mut output = self.output_items.clone();
        for (index, text_item) in &self.text_items {
            if text_item.text.is_empty() {
                continue;
            }
            output
                .entry(*index)
                .or_insert_with(|| synthesized_message_item(*index, text_item));
        }

        output.into_values().collect()
    }
}

fn synthesized_message_item(index: u32, text_item: &RawTextOutput) -> Value {
    let id = text_item
        .item_id
        .clone()
        .unwrap_or_else(|| format!("msg_{index}"));
    json!({
        "type": "message",
        "id": id,
        "status": "completed",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": text_item.text,
            "annotations": []
        }]
    })
}

#[allow(clippy::too_many_lines)]
pub(super) fn anthropic_raw_messages_sse_response(
    stream: Pin<Box<dyn Stream<Item = Result<crate::codex::sse::JsonSseEvent>> + Send>>,
    model: String,
    input_tokens: u32,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>>> {
    let mapped = async_stream::stream! {
        macro_rules! yield_event_or_error {
            ($event:expr) => {
                match $event {
                    Ok(event) => yield Ok(event),
                    Err(error) => {
                        yield Ok(anthropic_error_event(&error));
                        return;
                    }
                }
            };
        }

        let id = format!("msg_{}", rand::random::<u64>());
        let mut stream = stream;
        let mut open_text_blocks = std::collections::BTreeSet::<u32>::new();
        let mut open_tool_blocks = std::collections::BTreeSet::<u32>::new();
        let mut open_thinking_blocks = std::collections::BTreeSet::<u32>::new();
        let mut seen_tool_blocks = std::collections::BTreeSet::<u32>::new();
        let mut seen_thinking_text_blocks = std::collections::BTreeSet::<u32>::new();
        let mut tool_meta = std::collections::HashMap::<String, ToolMeta>::new();
        let mut usage = crate::anthropic::ResponseUsage {
            input_tokens,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            output_tokens: 0,
            server_tool_use: None,
        };

        yield_event_or_error!(message_start_event(&id, &model, input_tokens));

        while let Some(item) = stream.next().await {
            match item {
                Ok(item) => {
                    if let Some(message) = event_error(&item.value) {
                        yield Ok(anthropic_error_event(&crate::Error::upstream(message)));
                        return;
                    }

                    match item.value.get("type").and_then(Value::as_str) {
                        Some("ping") => {
                            yield Ok(Event::default().event("ping").data("{\"type\":\"ping\"}"));
                        }
                        Some("response.output_text.delta") => {
                            let index = output_index(&item.value);
                            let delta = item
                                .value
                                .get("delta")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            if !delta.is_empty() {
                                if open_text_blocks.insert(index) {
                                    yield_event_or_error!(text_block_start(index));
                                }
                                usage.output_tokens =
                                    usage.output_tokens.saturating_add(estimate_stream_tokens(delta));
                                yield_event_or_error!(text_delta(index, delta));
                            }
                        }
                        Some("response.output_text.done") => {
                            let index = output_index(&item.value);
                            if open_text_blocks.remove(&index) {
                                yield_event_or_error!(content_block_stop(index));
                            }
                        }
                        Some("response.output_item.added") => {
                            if let Some(tool_call) = tool_call_from_event_item(&item.value) {
                                let index = output_index(&item.value);
                                cache_tool_meta(&mut tool_meta, &item.value, &tool_call);
                                if open_tool_blocks.insert(index) {
                                    seen_tool_blocks.insert(index);
                                    yield_event_or_error!(tool_block_start(index, &tool_call));
                                }
                            }
                        }
                        Some("response.function_call_arguments.delta") => {
                            let index = output_index(&item.value);
                            let item_id = item.value.get("item_id").and_then(Value::as_str);
                            let delta = item
                                .value
                                .get("delta")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            if let Some(meta) = item_id.and_then(|id| tool_meta.get(id)) {
                                if open_tool_blocks.insert(index) {
                                    seen_tool_blocks.insert(index);
                                    let tool_call = crate::openai::types::ToolCall {
                                        id: meta.id.clone(),
                                        kind: "function".to_owned(),
                                        function: crate::openai::types::FunctionCall {
                                            name: meta.name.clone(),
                                            arguments: String::new(),
                                        },
                                    };
                                    yield_event_or_error!(tool_block_start(index, &tool_call));
                                }
                            }
                            if !delta.is_empty() {
                                usage.output_tokens =
                                    usage.output_tokens.saturating_add(estimate_stream_tokens(delta));
                                yield_event_or_error!(tool_json_delta(index, delta));
                            }
                        }
                        Some("response.function_call_arguments.done") => {
                            let index = output_index(&item.value);
                            if open_tool_blocks.remove(&index) {
                                yield_event_or_error!(content_block_stop(index));
                            }
                        }
                        Some("response.output_item.done") => {
                            if let Some(tool_call) = tool_call_from_event_item(&item.value) {
                                let index = output_index(&item.value);
                                let first_seen = seen_tool_blocks.insert(index);
                                if first_seen {
                                    if open_tool_blocks.insert(index) {
                                        yield_event_or_error!(tool_block_start(index, &tool_call));
                                    }
                                    if !tool_call.function.arguments.is_empty() {
                                        usage.output_tokens = usage.output_tokens.saturating_add(
                                            estimate_stream_tokens(&tool_call.function.arguments),
                                        );
                                        yield_event_or_error!(tool_json_delta(
                                            index,
                                            &tool_call.function.arguments,
                                        ));
                                    }
                                }
                                if open_tool_blocks.remove(&index) || first_seen {
                                    yield_event_or_error!(content_block_stop(index));
                                }
                            } else if let Some(thinking) = reasoning_text_from_event_item(&item.value) {
                                let index = output_index(&item.value);
                                if open_thinking_blocks.insert(index) {
                                    yield_event_or_error!(thinking_block_start(index));
                                }
                                if !thinking.is_empty() && !seen_thinking_text_blocks.contains(&index) {
                                    seen_thinking_text_blocks.insert(index);
                                    usage.output_tokens = usage
                                        .output_tokens
                                        .saturating_add(estimate_stream_tokens(&thinking));
                                    yield_event_or_error!(thinking_delta(index, &thinking));
                                }
                                if let Some(signature) = reasoning_signature_from_event_item(&item.value) {
                                    yield_event_or_error!(signature_delta(index, &signature));
                                }
                                if open_thinking_blocks.remove(&index) {
                                    yield_event_or_error!(content_block_stop(index));
                                }
                            }
                        }
                        Some(
                            "response.reasoning_text.delta"
                            | "response.reasoning_summary_text.delta",
                        ) => {
                            let index = output_index(&item.value);
                            let delta = item
                                .value
                                .get("delta")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            if !delta.is_empty() {
                                if open_thinking_blocks.insert(index) {
                                    yield_event_or_error!(thinking_block_start(index));
                                }
                                seen_thinking_text_blocks.insert(index);
                                usage.output_tokens =
                                    usage.output_tokens.saturating_add(estimate_stream_tokens(delta));
                                yield_event_or_error!(thinking_delta(index, delta));
                            }
                        }
                        _ => {}
                    }
                    if is_done_event(&item.value) {
                        let mut stop_reason = finish_reason(&item.value);
                        if let Some(response) = item.value.get("response") {
                            merge_final_anthropic_usage(&mut usage, response.get("usage"));
                            if response_has_function_call(response) {
                                "tool_calls".clone_into(&mut stop_reason);
                            }

                            for (index, image) in generated_images_from_output(
                                response
                                    .get("output")
                                    .and_then(Value::as_array)
                                    .map_or(&[] as &[Value], Vec::as_slice),
                            )
                            .into_iter()
                            .enumerate()
                            {
                                let source = crate::anthropic::ImageSource {
                                    kind: "base64".to_owned(),
                                    media_type: image.media_type.or_else(|| Some("image/png".to_owned())),
                                    data: Some(image.b64_json),
                                    text: None,
                                    url: None,
                                    file_id: None,
                                };
                                let index = u32::try_from(index).unwrap_or(u32::MAX);
                                yield_event_or_error!(image_block_start(index, &source));
                                yield_event_or_error!(content_block_stop(index));
                            }
                        }

                        for index in std::mem::take(&mut open_text_blocks) {
                            yield_event_or_error!(content_block_stop(index));
                        }
                        for index in std::mem::take(&mut open_tool_blocks) {
                            yield_event_or_error!(content_block_stop(index));
                        }
                        for index in std::mem::take(&mut open_thinking_blocks) {
                            yield_event_or_error!(content_block_stop(index));
                        }

                        for event in [
                            message_delta_event(&stop_reason, &usage),
                            message_stop_event(),
                        ] {
                            yield_event_or_error!(event);
                        }
                        return;
                    }
                }
                Err(error) => {
                    yield Ok(anthropic_error_event(&error));
                    return;
                }
            }
        }

        for index in open_text_blocks {
            yield_event_or_error!(content_block_stop(index));
        }
        for index in open_tool_blocks {
            yield_event_or_error!(content_block_stop(index));
        }
        for index in open_thinking_blocks {
            yield_event_or_error!(content_block_stop(index));
        }
        for event in [message_delta_event("stop", &usage), message_stop_event()] {
            yield_event_or_error!(event);
        }
    };

    Sse::new(mapped).keep_alive(KeepAlive::default())
}

pub(super) fn anthropic_error_response(error: &crate::Error) -> axum::response::Response {
    (error.status_code(), axum::Json(error_body(error))).into_response()
}

fn anthropic_error_event(error: &crate::Error) -> Event {
    Event::default()
        .event("error")
        .data(error_body(error).to_string())
}

fn estimate_stream_tokens(text: &str) -> u32 {
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

fn output_index(event: &Value) -> u32 {
    event
        .get("output_index")
        .and_then(Value::as_u64)
        .and_then(|index| u32::try_from(index).ok())
        .unwrap_or(0)
}

fn tool_call_from_event_item(event: &Value) -> Option<crate::openai::types::ToolCall> {
    let item = event.get("item")?;
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return None;
    }

    Some(crate::openai::types::ToolCall {
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
                .unwrap_or_default()
                .to_owned(),
        },
    })
}

fn cache_tool_meta(
    tool_meta: &mut std::collections::HashMap<String, ToolMeta>,
    event: &Value,
    tool_call: &crate::openai::types::ToolCall,
) {
    tool_meta.insert(
        tool_call.id.clone(),
        ToolMeta {
            id: tool_call.id.clone(),
            name: tool_call.function.name.clone(),
        },
    );
    if let Some(item_id) = event
        .get("item")
        .and_then(|item| item.get("id"))
        .and_then(Value::as_str)
    {
        tool_meta.insert(
            item_id.to_owned(),
            ToolMeta {
                id: tool_call.id.clone(),
                name: tool_call.function.name.clone(),
            },
        );
    }
}

fn response_has_function_call(response: &Value) -> bool {
    response
        .get("output")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|entry| entry.get("type").and_then(Value::as_str) == Some("function_call"))
        })
}

fn merge_final_anthropic_usage(usage: &mut crate::anthropic::ResponseUsage, value: Option<&Value>) {
    if let Some(value) = value {
        if let Some(input_tokens) = value
            .get("input_tokens")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
        {
            usage.input_tokens = input_tokens;
        }
        if let Some(output_tokens) = value
            .get("output_tokens")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
        {
            usage.output_tokens = output_tokens;
        }
        usage.cache_creation_input_tokens = value
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok());
        usage.cache_read_input_tokens = value
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok());
        usage.server_tool_use = value
            .get("server_tool_use")
            .filter(|value| !value.is_null())
            .cloned();
    }
}

fn reasoning_text_from_event_item(event: &Value) -> Option<String> {
    let item = event.get("item")?;
    if item.get("type").and_then(Value::as_str) != Some("reasoning") {
        return None;
    }

    let summary_text = item
        .get("summary")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    if !summary_text.is_empty() {
        return Some(summary_text);
    }

    let content_text = item
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    (!content_text.is_empty()).then_some(content_text)
}

fn reasoning_signature_from_event_item(event: &Value) -> Option<String> {
    event
        .get("item")
        .and_then(|item| item.get("encrypted_content"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

#[derive(Clone)]
struct ToolMeta {
    id: String,
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn raw_responses_sanitizer_backfills_null_output_from_done_items() {
        let mut state = RawResponsesStreamState::default();
        let mut done_item = json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "lookup",
                "arguments": "{\"q\":\"x\"}"
            }
        });
        sanitize_raw_responses_event(&mut done_item, &mut state);

        let mut completed = json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "model": "gpt-5.5",
                "status": "completed",
                "output": null
            }
        });
        sanitize_raw_responses_event(&mut completed, &mut state);

        assert_eq!(completed["response"]["output"][0]["type"], "function_call");
        assert_eq!(completed["response"]["output"][0]["call_id"], "call_1");
    }

    #[test]
    fn raw_responses_sanitizer_synthesizes_output_from_text_deltas() {
        let mut state = RawResponsesStreamState::default();
        let mut delta = json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "item_id": "msg_1",
            "delta": "OK"
        });
        sanitize_raw_responses_event(&mut delta, &mut state);

        let mut completed = json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "model": "gpt-5.5",
                "status": "completed",
                "output": null
            }
        });
        sanitize_raw_responses_event(&mut completed, &mut state);

        assert_eq!(completed["response"]["output"][0]["type"], "message");
        assert_eq!(completed["response"]["output"][0]["id"], "msg_1");
        assert_eq!(
            completed["response"]["output"][0]["content"][0]["text"],
            "OK"
        );
    }
}
