use crate::{
    Error, Result,
    codex::{
        events::{ChatOutput, apply_event, is_done_event},
        sse::JsonSseEvent,
    },
    config::{Credentials, now_unix},
    oauth::{api_region_from_credentials, profile_arn_from_credentials},
    openai::types::{FunctionCall, ToolCall},
};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use reqwest::{
    Response,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT},
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{env, pin::Pin};

/// Default Kiro runtime endpoint. Requests derive the region from profileArn
/// when this default is used.
pub const DEFAULT_KIRO_BASE_URL: &str = "https://runtime.us-east-1.kiro.dev";

const KIRO_TARGET: &str = "AmazonCodeWhispererStreamingService.GenerateAssistantResponse";
const KIRO_EVENT_STREAM_CONTENT_TYPE: &str = "application/vnd.amazon.eventstream";
const EMPTY_PLACEHOLDER: &str = "(empty placeholder)";
const MAX_EVENT_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum KiroRole {
    #[default]
    User,
    Assistant,
}

#[derive(Debug, Clone, Default)]
struct KiroMessage {
    role: KiroRole,
    content: String,
    images: Vec<Value>,
    documents: Vec<Value>,
    tool_uses: Vec<KiroToolUse>,
    tool_results: Vec<Value>,
}

#[derive(Debug, Clone)]
struct KiroToolUse {
    id: String,
    name: String,
    input: Value,
}

#[derive(Debug, Default)]
struct AwsEventStreamDecoder {
    buffer: Vec<u8>,
}

#[derive(Debug, Clone)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Debug)]
struct KiroStreamState {
    request: Value,
    response_id: String,
    text_item_id: String,
    created_at: i64,
    sequence_number: u64,
    model: String,
    output_text: String,
    text_started: bool,
    reasoning_item_id: String,
    reasoning_text: String,
    reasoning_signature: Option<String>,
    reasoning_started: bool,
    tool_calls: Vec<ToolCall>,
    pending_tool: Option<PendingToolCall>,
    credit_usage: Option<f64>,
    context_usage_percentage: Option<f64>,
}

/// Builds Kiro runtime request headers from imported credentials.
///
/// # Errors
///
/// Returns an error when a header value cannot be encoded.
pub fn kiro_headers(credentials: &Credentials) -> Result<HeaderMap> {
    let fingerprint = machine_fingerprint(credentials);
    let user_agent = format!(
        "aws-sdk-js/1.0.27 ua/2.1 os/macos lang/js md/nodejs api/codewhispererstreaming#1.0.27 m/E KiroIDE-0.7.45-{fingerprint}"
    );
    let x_amz_user_agent = format!("aws-sdk-js/1.0.27 KiroIDE-0.7.45-{fingerprint}");

    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        header_value(&format!("Bearer {}", credentials.access_token))?,
    );
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/x-amz-json-1.0"),
    );
    headers.insert(
        ACCEPT,
        HeaderValue::from_static(KIRO_EVENT_STREAM_CONTENT_TYPE),
    );
    headers.insert(
        HeaderName::from_static("x-amz-target"),
        HeaderValue::from_static(KIRO_TARGET),
    );
    headers.insert(
        HeaderName::from_static("x-amzn-codewhisperer-optout"),
        HeaderValue::from_static("true"),
    );
    headers.insert(
        HeaderName::from_static("x-amzn-kiro-agent-mode"),
        HeaderValue::from_static("vibe"),
    );
    headers.insert(
        HeaderName::from_static("x-amz-user-agent"),
        header_value(&x_amz_user_agent)?,
    );
    headers.insert(
        HeaderName::from_static("amz-sdk-invocation-id"),
        header_value(&invocation_id())?,
    );
    headers.insert(
        HeaderName::from_static("amz-sdk-request"),
        HeaderValue::from_static("attempt=1; max=3"),
    );
    headers.insert(USER_AGENT, header_value(&user_agent)?);
    Ok(headers)
}

/// Resolves the Kiro runtime URL for the given credentials.
///
/// # Errors
///
/// Returns an error when stored credential metadata is malformed.
pub fn kiro_endpoint_url(base_url: &str, credentials: &Credentials) -> Result<String> {
    let normalized = base_url.trim_end_matches('/');
    if normalized == DEFAULT_KIRO_BASE_URL.trim_end_matches('/') {
        let region = api_region_from_credentials(credentials)?;
        return Ok(format!("https://runtime.{region}.kiro.dev/"));
    }
    Ok(format!("{normalized}/"))
}

/// Converts a rotom internal Responses-style request body to Kiro's runtime
/// `conversationState` payload.
///
/// # Errors
///
/// Returns an error when required Kiro credential metadata is missing or the
/// request does not contain usable input.
pub fn to_kiro_payload(request: &Value, credentials: &Credentials) -> Result<Value> {
    let profile_arn = profile_arn_from_credentials(credentials)?;
    let model = kiro_model_id(model_from_request(request));
    let mut messages = messages_from_request(request)?;
    let instructions = request
        .get("instructions")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();

    if messages.is_empty() {
        return Err(Error::config("Kiro request is missing input messages"));
    }
    if !instructions.is_empty() {
        prepend_instructions(&mut messages, &instructions);
    }
    messages = normalize_messages(messages);

    let mut history_messages = messages;
    let mut current_message = history_messages
        .pop()
        .ok_or_else(|| Error::config("Kiro request is missing current message"))?;
    if current_message.role == KiroRole::Assistant {
        history_messages.push(current_message);
        current_message = KiroMessage {
            role: KiroRole::User,
            content: EMPTY_PLACEHOLDER.to_owned(),
            ..KiroMessage::default()
        };
    }

    let mut conversation_state = json!({
        "chatTriggerType": "MANUAL",
        "conversationId": conversation_id_from_request(request),
        "currentMessage": {
            "userInputMessage": user_input_message(&current_message, &model, request)
        }
    });

    let history = history_messages
        .iter()
        .map(|message| history_item(message, &model))
        .collect::<Vec<_>>();
    if !history.is_empty() {
        conversation_state["history"] = Value::Array(history);
    }

    Ok(json!({
        "conversationState": conversation_state,
        "profileArn": profile_arn,
    }))
}

/// Converts a Kiro event-stream HTTP response into `OpenAI` Responses JSON events.
pub fn response_event_stream(
    response: Response,
    request: Value,
) -> Pin<Box<dyn Stream<Item = Result<JsonSseEvent>> + Send>> {
    let stream = async_stream::try_stream! {
        let mut body = response.bytes_stream();
        let mut decoder = AwsEventStreamDecoder::default();
        let mut state = KiroStreamState::new(request);

        for event in state.start_events() {
            yield event;
        }

        while let Some(chunk) = body.next().await {
            let chunk = chunk?;
            for payload in decoder.push_chunk(&chunk)? {
                for event in state.apply_payload(&payload) {
                    yield event;
                }
            }
        }

        for event in state.finish_events() {
            yield event;
        }
    };

    Box::pin(stream)
}

/// Collects a Kiro event-stream response into one `OpenAI` Responses value.
///
/// # Errors
///
/// Returns an error when the stream ends without a terminal response event.
pub async fn collect_response_value(response: Response, request: Value) -> Result<Value> {
    let mut stream = response_event_stream(response, request);
    while let Some(event) = stream.next().await {
        let event = event?;
        if is_done_event(&event.value) {
            return event.value.get("response").cloned().ok_or_else(|| {
                Error::upstream("Kiro response completed without response payload")
            });
        }
    }
    Err(Error::upstream(
        "Kiro response stream ended before completion",
    ))
}

/// Collects a Kiro event-stream response into chat-compatible output.
///
/// # Errors
///
/// Returns an error when the stream contains an upstream error or ends before
/// completion.
pub async fn collect_chat_output(response: Response, request: Value) -> Result<ChatOutput> {
    let mut stream = response_event_stream(response, request);
    let mut output = ChatOutput {
        finish_reason: "stop".to_owned(),
        ..ChatOutput::default()
    };

    while let Some(event) = stream.next().await {
        let event = event?;
        apply_event(&mut output, &event.value)?;
        if is_done_event(&event.value) {
            return Ok(output);
        }
    }

    Err(Error::upstream(
        "Kiro response stream ended before completion",
    ))
}

impl AwsEventStreamDecoder {
    fn push_chunk(&mut self, chunk: &Bytes) -> Result<Vec<Value>> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();

        loop {
            if self.buffer.len() < 12 {
                break;
            }

            let total_len = u32::from_be_bytes(
                self.buffer[0..4]
                    .try_into()
                    .map_err(|_| Error::upstream("invalid Kiro event-stream prelude"))?,
            ) as usize;
            let headers_len = u32::from_be_bytes(
                self.buffer[4..8]
                    .try_into()
                    .map_err(|_| Error::upstream("invalid Kiro event-stream prelude"))?,
            ) as usize;

            if !(16..=MAX_EVENT_FRAME_BYTES).contains(&total_len) {
                return Err(Error::upstream("invalid Kiro event-stream frame length"));
            }
            if self.buffer.len() < total_len {
                break;
            }

            let payload_start = 12usize
                .checked_add(headers_len)
                .ok_or_else(|| Error::upstream("invalid Kiro event-stream header length"))?;
            let payload_end = total_len
                .checked_sub(4)
                .ok_or_else(|| Error::upstream("invalid Kiro event-stream frame length"))?;
            if payload_start > payload_end || payload_end > total_len {
                return Err(Error::upstream("invalid Kiro event-stream payload bounds"));
            }

            let frame = self.buffer[..total_len].to_vec();
            self.buffer.drain(..total_len);
            let payload = &frame[payload_start..payload_end];
            if payload.iter().all(u8::is_ascii_whitespace) || payload.is_empty() {
                continue;
            }
            let value = serde_json::from_slice::<Value>(payload)?;
            crate::logging::trace_json("upstream.kiro.event", &value);
            events.push(value);
        }

        Ok(events)
    }
}

impl KiroStreamState {
    fn new(request: Value) -> Self {
        let response_id = format!("resp_{}_{:08x}", now_unix(), rand::random::<u32>());
        Self {
            text_item_id: format!("msg_{response_id}"),
            reasoning_item_id: format!("rs_{response_id}"),
            response_id,
            created_at: now_unix(),
            sequence_number: 2,
            model: kiro_model_id(model_from_request(&request)),
            request,
            output_text: String::new(),
            text_started: false,
            reasoning_text: String::new(),
            reasoning_signature: None,
            reasoning_started: false,
            tool_calls: Vec::new(),
            pending_tool: None,
            credit_usage: None,
            context_usage_percentage: None,
        }
    }

    fn start_events(&self) -> Vec<JsonSseEvent> {
        let response = self.response_json("in_progress", &[]);
        vec![
            json_event(
                "response.created",
                json!({
                    "type": "response.created",
                    "sequence_number": 0,
                    "response": response,
                }),
            ),
            json_event(
                "response.in_progress",
                json!({
                    "type": "response.in_progress",
                    "sequence_number": 1,
                    "response": response,
                }),
            ),
        ]
    }

    fn apply_payload(&mut self, payload: &Value) -> Vec<JsonSseEvent> {
        if let Some(message) = kiro_error_message(payload) {
            return vec![json_event(
                "error",
                json!({
                    "type": "error",
                    "message": message,
                }),
            )];
        }

        if let Some(event) = payload.get("assistantResponseEvent") {
            return event
                .get("content")
                .and_then(Value::as_str)
                .map_or_else(Vec::new, |content| self.text_delta_events(content));
        }
        if let Some(event) = payload.get("reasoningContentEvent") {
            let text = event
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let signature = event.get("signature").and_then(Value::as_str);
            return self.reasoning_delta_events(text, signature);
        }
        if let Some(event) = payload.get("toolUseEvent") {
            return self.apply_tool_payload(event);
        }
        if let Some(event) = payload.get("meteringEvent") {
            if let Some(usage) = event.get("usage").and_then(Value::as_f64) {
                self.credit_usage = Some(usage);
            }
            return Vec::new();
        }
        if let Some(event) = payload.get("contextUsageEvent") {
            if let Some(context) = event.get("contextUsagePercentage").and_then(Value::as_f64) {
                self.context_usage_percentage = Some(context);
            }
            return Vec::new();
        }

        if let Some(usage) = payload.get("usage").and_then(Value::as_f64) {
            self.credit_usage = Some(usage);
            return Vec::new();
        }
        if let Some(context) = payload
            .get("contextUsagePercentage")
            .and_then(Value::as_f64)
        {
            self.context_usage_percentage = Some(context);
            return Vec::new();
        }
        if payload.get("input").is_some() {
            return self.apply_tool_payload(payload);
        }
        if payload.get("stop").and_then(Value::as_bool) == Some(true) {
            return self.finish_tool_call();
        }
        if let Some(name) = payload.get("name").and_then(Value::as_str) {
            return self.start_tool(name, payload);
        }

        if let Some(content) = payload.get("content").and_then(Value::as_str) {
            return self.text_delta_events(content);
        }

        Vec::new()
    }

    fn apply_tool_payload(&mut self, payload: &Value) -> Vec<JsonSseEvent> {
        let mut events = Vec::new();
        if self.pending_tool.is_none()
            && let Some(name) = payload.get("name").and_then(Value::as_str)
        {
            events.extend(self.start_tool(name, payload));
        }
        if payload.get("input").is_some() {
            self.append_tool_input(payload);
        }
        if payload.get("stop").and_then(Value::as_bool) == Some(true) {
            events.extend(self.finish_tool_call());
        }
        events
    }

    fn reasoning_delta_events(&mut self, text: &str, signature: Option<&str>) -> Vec<JsonSseEvent> {
        if let Some(signature) = signature.filter(|value| !value.is_empty()) {
            self.reasoning_signature = Some(signature.to_owned());
        }
        if text.is_empty() {
            return Vec::new();
        }

        let mut events = Vec::new();
        if !self.reasoning_started {
            events.push(json_event(
                "response.output_item.added",
                json!({
                    "type": "response.output_item.added",
                    "sequence_number": self.next_sequence(),
                    "response_id": self.response_id,
                    "output_index": 0,
                    "item": {
                        "id": self.reasoning_item_id,
                        "type": "reasoning",
                        "status": "in_progress",
                        "summary": []
                    }
                }),
            ));
            self.reasoning_started = true;
        }

        self.reasoning_text.push_str(text);
        events.push(json_event(
            "response.reasoning_text.delta",
            json!({
                "type": "response.reasoning_text.delta",
                "sequence_number": self.next_sequence(),
                "response_id": self.response_id,
                "item_id": self.reasoning_item_id,
                "output_index": 0,
                "content_index": 0,
                "delta": text,
            }),
        ));
        events
    }

    fn text_delta_events(&mut self, text: &str) -> Vec<JsonSseEvent> {
        if text.is_empty() {
            return Vec::new();
        }

        let mut events = Vec::new();
        let output_index = self.text_output_index();
        if !self.text_started {
            events.push(json_event(
                "response.output_item.added",
                json!({
                    "type": "response.output_item.added",
                    "sequence_number": self.next_sequence(),
                    "response_id": self.response_id,
                    "output_index": output_index,
                    "item": {
                        "id": self.text_item_id,
                        "type": "message",
                        "status": "in_progress",
                        "role": "assistant",
                        "content": []
                    }
                }),
            ));
            events.push(json_event(
                "response.content_part.added",
                json!({
                    "type": "response.content_part.added",
                    "sequence_number": self.next_sequence(),
                    "response_id": self.response_id,
                    "item_id": self.text_item_id,
                    "output_index": output_index,
                    "content_index": 0,
                    "part": output_text_part("")
                }),
            ));
            self.text_started = true;
        }

        self.output_text.push_str(text);
        events.push(json_event(
            "response.output_text.delta",
            json!({
                "type": "response.output_text.delta",
                "sequence_number": self.next_sequence(),
                "response_id": self.response_id,
                "item_id": self.text_item_id,
                "output_index": output_index,
                "content_index": 0,
                "delta": text,
            }),
        ));
        events
    }

    fn start_tool(&mut self, name: &str, payload: &Value) -> Vec<JsonSseEvent> {
        let events = if self.pending_tool.is_some() {
            self.finish_tool_call()
        } else {
            Vec::new()
        };
        let input = payload.get("input").map(input_fragment).unwrap_or_default();
        self.pending_tool = Some(PendingToolCall {
            id: payload
                .get("toolUseId")
                .and_then(Value::as_str)
                .map_or_else(
                    || format!("call_{:08x}", rand::random::<u32>()),
                    str::to_owned,
                ),
            name: name.to_owned(),
            arguments: input,
        });
        events
    }

    fn append_tool_input(&mut self, payload: &Value) {
        if let Some(pending) = &mut self.pending_tool {
            pending.arguments.push_str(&input_fragment(
                payload.get("input").unwrap_or(&Value::Null),
            ));
        }
    }

    fn finish_tool_call(&mut self) -> Vec<JsonSseEvent> {
        let Some(pending) = self.pending_tool.take() else {
            return Vec::new();
        };
        let arguments = normalize_tool_arguments(&pending.arguments);
        let tool_call = ToolCall {
            id: pending.id,
            kind: "function".to_owned(),
            function: FunctionCall {
                name: pending.name,
                arguments,
            },
        };
        let output_index = self
            .tool_output_base_index()
            .saturating_add(self.tool_calls.len());
        let output_index = u32::try_from(output_index).unwrap_or(u32::MAX);
        let item_id = format!("fc_{}_{}", self.response_id, output_index);
        let events = self.tool_call_events(output_index, &item_id, &tool_call);
        self.tool_calls.push(tool_call);
        events
    }

    fn tool_call_events(
        &mut self,
        output_index: u32,
        item_id: &str,
        tool_call: &ToolCall,
    ) -> Vec<JsonSseEvent> {
        let mut events = vec![json_event(
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "sequence_number": self.next_sequence(),
                "response_id": self.response_id,
                "output_index": output_index,
                "item": {
                    "id": item_id,
                    "type": "function_call",
                    "status": "in_progress",
                    "call_id": tool_call.id,
                    "name": tool_call.function.name,
                    "arguments": ""
                }
            }),
        )];
        if !tool_call.function.arguments.is_empty() {
            events.push(json_event(
                "response.function_call_arguments.delta",
                json!({
                    "type": "response.function_call_arguments.delta",
                    "sequence_number": self.next_sequence(),
                    "response_id": self.response_id,
                    "output_index": output_index,
                    "item_id": item_id,
                    "delta": tool_call.function.arguments
                }),
            ));
        }
        events.push(json_event(
            "response.function_call_arguments.done",
            json!({
                "type": "response.function_call_arguments.done",
                "sequence_number": self.next_sequence(),
                "response_id": self.response_id,
                "output_index": output_index,
                "item_id": item_id,
                "name": tool_call.function.name,
                "arguments": tool_call.function.arguments
            }),
        ));
        events.push(json_event(
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "sequence_number": self.next_sequence(),
                "response_id": self.response_id,
                "output_index": output_index,
                "item": {
                    "id": item_id,
                    "type": "function_call",
                    "status": "completed",
                    "call_id": tool_call.id,
                    "name": tool_call.function.name,
                    "arguments": tool_call.function.arguments
                }
            }),
        ));
        events
    }

    fn text_output_index(&self) -> u32 {
        u32::from(self.reasoning_started || !self.reasoning_text.is_empty())
    }

    fn tool_output_base_index(&self) -> usize {
        let reasoning_count =
            usize::from(self.reasoning_started || !self.reasoning_text.is_empty());
        let text_count = usize::from(self.text_started);
        reasoning_count + text_count
    }

    fn finish_events(&mut self) -> Vec<JsonSseEvent> {
        let mut events = self.finish_tool_call();
        if self.reasoning_started || !self.reasoning_text.is_empty() {
            events.push(json_event(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "sequence_number": self.next_sequence(),
                    "response_id": self.response_id,
                    "output_index": 0,
                    "item": self.reasoning_output_item(),
                }),
            ));
        }
        if self.text_started {
            let text = self.output_text.clone();
            let output_index = self.text_output_index();
            events.push(json_event(
                "response.output_text.done",
                json!({
                    "type": "response.output_text.done",
                    "sequence_number": self.next_sequence(),
                    "response_id": self.response_id,
                    "item_id": self.text_item_id,
                    "output_index": output_index,
                    "content_index": 0,
                    "text": text,
                }),
            ));
            events.push(json_event(
                "response.content_part.done",
                json!({
                    "type": "response.content_part.done",
                    "sequence_number": self.next_sequence(),
                    "response_id": self.response_id,
                    "item_id": self.text_item_id,
                    "output_index": output_index,
                    "content_index": 0,
                    "part": output_text_part(&text),
                }),
            ));
            events.push(json_event(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "sequence_number": self.next_sequence(),
                    "response_id": self.response_id,
                    "output_index": output_index,
                    "item": {
                        "id": self.text_item_id,
                        "type": "message",
                        "status": "completed",
                        "role": "assistant",
                        "content": [output_text_part(&text)]
                    }
                }),
            ));
        }

        let output = self.response_output();
        let response = self.response_json("completed", &output);
        events.push(json_event(
            "response.completed",
            json!({
                "type": "response.completed",
                "sequence_number": self.next_sequence(),
                "response": response,
            }),
        ));
        events
    }

    fn response_output(&self) -> Vec<Value> {
        let mut output = Vec::new();
        if self.reasoning_started || !self.reasoning_text.is_empty() {
            output.push(self.reasoning_output_item());
        }
        if !self.output_text.is_empty() || self.tool_calls.is_empty() {
            output.push(json!({
                "id": self.text_item_id,
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [output_text_part(&self.output_text)]
            }));
        }
        let tool_output_base = self.tool_output_base_index();
        for (index, tool_call) in self.tool_calls.iter().enumerate() {
            let output_index = tool_output_base.saturating_add(index);
            output.push(json!({
                "id": format!("fc_{}_{}", self.response_id, output_index),
                "type": "function_call",
                "status": "completed",
                "call_id": tool_call.id,
                "name": tool_call.function.name,
                "arguments": tool_call.function.arguments
            }));
        }
        output
    }

    fn reasoning_output_item(&self) -> Value {
        let mut item = json!({
            "id": self.reasoning_item_id,
            "type": "reasoning",
            "status": "completed",
            "summary": [{
                "type": "summary_text",
                "text": self.reasoning_text
            }]
        });
        if let Some(signature) = self.reasoning_signature.as_deref() {
            item["encrypted_content"] = Value::String(signature.to_owned());
        }
        item
    }

    fn response_json(&self, status: &str, output: &[Value]) -> Value {
        let mut response = json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "error": null,
            "incomplete_details": null,
            "model": self.model,
            "output": output,
            "parallel_tool_calls": self.request.get("parallel_tool_calls").and_then(Value::as_bool).unwrap_or(true),
            "store": false,
            "text": self.request.get("text").cloned().unwrap_or_else(|| json!({"verbosity": "medium"})),
            "tool_choice": self.request.get("tool_choice").cloned().unwrap_or(Value::Null),
            "tools": self.request.get("tools").cloned().unwrap_or_else(|| json!([])),
            "truncation": self.request.get("truncation").and_then(Value::as_str).unwrap_or("disabled"),
            "metadata": kiro_metadata(self.credit_usage, self.context_usage_percentage),
            "usage": null,
        });
        copy_optional_response_field(&mut response, &self.request, "instructions");
        response
    }

    const fn next_sequence(&mut self) -> u64 {
        let current = self.sequence_number;
        self.sequence_number = self.sequence_number.saturating_add(1);
        current
    }
}

fn model_from_request(request: &Value) -> &str {
    request
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("auto")
}

fn kiro_model_id(model: &str) -> String {
    model
        .strip_prefix("kiro/")
        .or_else(|| model.strip_prefix("openai-codex/kiro/"))
        .unwrap_or(model)
        .to_owned()
}

fn messages_from_request(request: &Value) -> Result<Vec<KiroMessage>> {
    match request.get("input") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| message_from_input_item(item).transpose())
            .collect(),
        Some(Value::String(text)) => Ok(vec![KiroMessage {
            role: KiroRole::User,
            content: text.clone(),
            ..KiroMessage::default()
        }]),
        _ => Ok(Vec::new()),
    }
}

fn message_from_input_item(item: &Value) -> Result<Option<KiroMessage>> {
    match item.get("type").and_then(Value::as_str) {
        Some("function_call") => Ok(Some(KiroMessage {
            role: KiroRole::Assistant,
            content: EMPTY_PLACEHOLDER.to_owned(),
            tool_uses: vec![KiroToolUse {
                id: item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("call")
                    .to_owned(),
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_owned(),
                input: item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .and_then(|arguments| serde_json::from_str(arguments).ok())
                    .unwrap_or_else(|| json!({})),
            }],
            ..KiroMessage::default()
        })),
        Some("function_call_output") => Ok(Some(KiroMessage {
            role: KiroRole::User,
            content: tool_output_text(item),
            tool_results: vec![tool_result_from_item(item)],
            ..KiroMessage::default()
        })),
        Some("message") | None => {
            let role = match item.get("role").and_then(Value::as_str) {
                Some("assistant") => KiroRole::Assistant,
                _ => KiroRole::User,
            };
            let content = item.get("content");
            Ok(Some(KiroMessage {
                role,
                content: content_text(content),
                images: if role == KiroRole::User {
                    image_blocks(content)?
                } else {
                    Vec::new()
                },
                documents: if role == KiroRole::User {
                    document_blocks(content)?
                } else {
                    Vec::new()
                },
                ..KiroMessage::default()
            }))
        }
        Some("reasoning" | "image_generation_call") => Ok(None),
        Some(_) => item
            .get("content")
            .map(|content| {
                Ok(KiroMessage {
                    role: KiroRole::User,
                    content: content_text(Some(content)),
                    images: image_blocks(Some(content))?,
                    documents: document_blocks(Some(content))?,
                    ..KiroMessage::default()
                })
            })
            .transpose(),
    }
}

fn tool_output_text(item: &Value) -> String {
    item.get("output")
        .and_then(Value::as_str)
        .or_else(|| item.get("content").and_then(Value::as_str))
        .filter(|value| !value.is_empty())
        .unwrap_or(EMPTY_PLACEHOLDER)
        .to_owned()
}

fn tool_result_from_item(item: &Value) -> Value {
    json!({
        "content": [{"text": tool_output_text(item)}],
        "status": "success",
        "toolUseId": item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
    })
}

fn content_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts.iter().filter_map(content_part_text).collect(),
        Some(Value::Object(_)) => content_part_text(value.unwrap())
            .unwrap_or_default()
            .to_owned(),
        Some(value) => value.as_str().unwrap_or_default().to_owned(),
        None => String::new(),
    }
}

fn content_part_text(part: &Value) -> Option<&str> {
    part.get("text")
        .or_else(|| part.get("input_text"))
        .or_else(|| part.get("output_text"))
        .or_else(|| part.get("refusal"))
        .and_then(Value::as_str)
}

fn image_blocks(value: Option<&Value>) -> Result<Vec<Value>> {
    let mut images = Vec::new();
    for part in content_parts(value) {
        let kind = part.get("type").and_then(Value::as_str);
        if !matches!(kind, Some("input_image" | "image_url" | "image")) {
            continue;
        }
        let Some(url) = image_url_from_part(part) else {
            continue;
        };
        let (mime_type, data) = data_url_payload(url, "image")?;
        let format = match mime_type.as_str() {
            "image/png" => "png",
            "image/jpeg" | "image/jpg" => "jpeg",
            "image/gif" => "gif",
            "image/webp" => "webp",
            _ => {
                return Err(Error::config(format!(
                    "Kiro image input does not support MIME type {mime_type}"
                )));
            }
        };
        images.push(json!({
            "format": format,
            "source": { "bytes": data }
        }));
    }
    Ok(images)
}

fn document_blocks(value: Option<&Value>) -> Result<Vec<Value>> {
    let mut documents = Vec::new();
    for part in content_parts(value) {
        let kind = part.get("type").and_then(Value::as_str);
        if !matches!(kind, Some("input_file" | "document_url" | "document")) {
            continue;
        }
        if references_external_document(part) {
            return Err(Error::config(
                "Kiro document input supports only inline base64 document data; remote URLs and file IDs are not fetched",
            ));
        }
        let Some((name, mime_type, data)) = document_data_from_part(part)? else {
            continue;
        };
        let format = match mime_type.as_str() {
            "application/pdf" => "pdf",
            "text/csv" => "csv",
            "application/msword" => "doc",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
            "application/vnd.ms-excel" => "xls",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
            "text/html" => "html",
            "text/plain" => "txt",
            "text/markdown" => "md",
            _ => {
                return Err(Error::config(format!(
                    "Kiro document input does not support MIME type {mime_type}"
                )));
            }
        };
        documents.push(json!({
            "name": sanitize_document_name(&name),
            "format": format,
            "source": { "bytes": data }
        }));
    }
    Ok(documents)
}

fn content_parts(value: Option<&Value>) -> Vec<&Value> {
    match value {
        Some(Value::Array(parts)) => parts.iter().collect(),
        Some(Value::Object(_)) => value.into_iter().collect(),
        _ => Vec::new(),
    }
}

fn image_url_from_part(part: &Value) -> Option<&str> {
    part.get("image_url")
        .and_then(string_or_url_object)
        .or_else(|| part.pointer("/source/url").and_then(Value::as_str))
}

fn string_or_url_object(value: &Value) -> Option<&str> {
    value
        .as_str()
        .or_else(|| value.get("url").and_then(Value::as_str))
}

fn document_data_from_part(part: &Value) -> Result<Option<(String, String, String)>> {
    if let Some(file_data) = part.get("file_data").and_then(Value::as_str) {
        let (mime_type, data) = data_url_payload(file_data, "document")?;
        return Ok(Some((document_name(part), mime_type, data)));
    }

    if let Some(source) = part.get("source") {
        if let Some(data) = source.get("data").and_then(Value::as_str) {
            let mime_type = source
                .get("media_type")
                .or_else(|| source.get("mimeType"))
                .or_else(|| source.get("mime_type"))
                .and_then(Value::as_str)
                .unwrap_or("text/plain")
                .to_ascii_lowercase();
            return Ok(Some((document_name(part), mime_type, data.to_owned())));
        }
    }

    if let Some(document_url) = part.get("document_url") {
        if let Some(data) = document_url.get("data").and_then(Value::as_str) {
            let mime_type = document_url
                .get("mimeType")
                .or_else(|| document_url.get("mime_type"))
                .and_then(Value::as_str)
                .unwrap_or("text/plain")
                .to_ascii_lowercase();
            let name = document_url
                .get("name")
                .or_else(|| document_url.get("filename"))
                .and_then(Value::as_str)
                .map_or_else(|| document_name(part), str::to_owned);
            return Ok(Some((name, mime_type, data.to_owned())));
        }
    }

    Ok(None)
}

fn references_external_document(part: &Value) -> bool {
    part.get("file_id").and_then(Value::as_str).is_some()
        || part.get("document_url").and_then(Value::as_str).is_some()
        || part
            .get("document_url")
            .and_then(|value| value.get("url"))
            .and_then(Value::as_str)
            .is_some()
        || part
            .get("source")
            .and_then(|value| value.get("url").or_else(|| value.get("file_id")))
            .and_then(Value::as_str)
            .is_some()
}

fn document_name(part: &Value) -> String {
    part.get("filename")
        .or_else(|| part.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("document")
        .to_owned()
}

fn sanitize_document_name(name: &str) -> String {
    let sanitized = name
        .rsplit_once('.')
        .map_or(name, |(stem, _)| stem)
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric()
                || matches!(character, ' ' | '-' | '_' | '(' | ')' | '[' | ']')
            {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let sanitized = sanitized.trim_matches('-').trim();
    if sanitized.is_empty() {
        "document".to_owned()
    } else {
        sanitized.chars().take(200).collect()
    }
}

fn data_url_payload(url: &str, kind: &str) -> Result<(String, String)> {
    let Some(rest) = url.strip_prefix("data:") else {
        return Err(Error::config(format!(
            "Kiro {kind} input supports only inline data URLs"
        )));
    };
    let Some((metadata, data)) = rest.split_once(',') else {
        return Err(Error::config(format!(
            "Kiro {kind} data URL is missing data"
        )));
    };
    if !metadata
        .split(';')
        .any(|part| part.eq_ignore_ascii_case("base64"))
    {
        return Err(Error::config(format!(
            "Kiro {kind} data URL must be base64 encoded"
        )));
    }
    if data.trim().is_empty() {
        return Err(Error::config(format!("Kiro {kind} data URL is empty")));
    }
    let mime_type = metadata
        .split(';')
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or("text/plain")
        .to_ascii_lowercase();
    Ok((mime_type, data.to_owned()))
}

fn prepend_instructions(messages: &mut [KiroMessage], instructions: &str) {
    if let Some(message) = messages
        .iter_mut()
        .find(|message| message.role == KiroRole::User)
    {
        message.content = if message.content.trim().is_empty() {
            instructions.to_owned()
        } else {
            format!("{instructions}\n\n{}", message.content)
        };
    }
}

fn normalize_messages(messages: Vec<KiroMessage>) -> Vec<KiroMessage> {
    let mut merged = Vec::<KiroMessage>::new();
    for mut message in messages {
        if message.content.trim().is_empty()
            && message.images.is_empty()
            && message.documents.is_empty()
            && message.tool_uses.is_empty()
            && message.tool_results.is_empty()
        {
            EMPTY_PLACEHOLDER.clone_into(&mut message.content);
        }
        if let Some(index) = merged.len().checked_sub(1)
            && merged[index].role == message.role
        {
            if can_merge_adjacent(&merged[index], &message) {
                merge_message(&mut merged[index], message);
            } else {
                let role = merged[index].role.opposite();
                merged.push(KiroMessage {
                    role,
                    content: EMPTY_PLACEHOLDER.to_owned(),
                    ..KiroMessage::default()
                });
                merged.push(message);
            }
            continue;
        }
        merged.push(message);
    }

    if merged
        .first()
        .is_some_and(|message| message.role != KiroRole::User)
    {
        merged.insert(
            0,
            KiroMessage {
                role: KiroRole::User,
                content: EMPTY_PLACEHOLDER.to_owned(),
                ..KiroMessage::default()
            },
        );
    }
    merged
}

impl KiroRole {
    const fn opposite(self) -> Self {
        match self {
            Self::User => Self::Assistant,
            Self::Assistant => Self::User,
        }
    }
}

fn can_merge_adjacent(previous: &KiroMessage, message: &KiroMessage) -> bool {
    previous.tool_uses.is_empty()
        && previous.tool_results.is_empty()
        && message.tool_uses.is_empty()
        && message.tool_results.is_empty()
}

fn merge_message(previous: &mut KiroMessage, message: KiroMessage) {
    if !message.content.trim().is_empty() {
        if previous.content.trim().is_empty() || previous.content == EMPTY_PLACEHOLDER {
            previous.content = message.content;
        } else {
            previous.content.push_str("\n\n");
            previous.content.push_str(&message.content);
        }
    }
    previous.images.extend(message.images);
    previous.documents.extend(message.documents);
    previous.tool_uses.extend(message.tool_uses);
    previous.tool_results.extend(message.tool_results);
}

fn history_item(message: &KiroMessage, model: &str) -> Value {
    match message.role {
        KiroRole::User => {
            json!({ "userInputMessage": user_input_message(message, model, &json!({})) })
        }
        KiroRole::Assistant => {
            let mut assistant = json!({
                "content": non_empty_content(&message.content),
            });
            if !message.tool_uses.is_empty() {
                assistant["toolUses"] = Value::Array(
                    message
                        .tool_uses
                        .iter()
                        .map(|tool| {
                            json!({
                                "name": tool.name,
                                "input": tool.input,
                                "toolUseId": tool.id,
                            })
                        })
                        .collect(),
                );
            }
            json!({ "assistantResponseMessage": assistant })
        }
    }
}

fn user_input_message(message: &KiroMessage, model: &str, request: &Value) -> Value {
    let mut user_input = json!({
        "content": non_empty_content(&message.content),
        "modelId": model,
        "origin": "AI_EDITOR",
    });
    if !message.images.is_empty() {
        user_input["images"] = Value::Array(message.images.clone());
    }
    if !message.documents.is_empty() {
        user_input["documents"] = Value::Array(message.documents.clone());
    }
    let mut context = Map::new();
    let tools = tools_for_request(request);
    if !tools.is_empty() {
        context.insert("tools".to_owned(), Value::Array(tools));
    }
    if !message.tool_results.is_empty() {
        context.insert(
            "toolResults".to_owned(),
            Value::Array(message.tool_results.clone()),
        );
    }
    if !context.is_empty() {
        user_input["userInputMessageContext"] = Value::Object(context);
    }
    user_input
}

fn tools_for_request(request: &Value) -> Vec<Value> {
    if tool_choice_disables_tools(request.get("tool_choice")) {
        return Vec::new();
    }
    let selected_tool = selected_tool_name(request.get("tool_choice"));
    request
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|tool| {
            selected_tool.is_none_or(|name| {
                tool.get("name")
                    .or_else(|| tool.pointer("/function/name"))
                    .and_then(Value::as_str)
                    == Some(name)
            })
        })
        .filter_map(convert_tool_to_kiro)
        .collect()
}

fn tool_choice_disables_tools(choice: Option<&Value>) -> bool {
    choice.and_then(Value::as_str) == Some("none")
}

fn selected_tool_name(choice: Option<&Value>) -> Option<&str> {
    let choice = choice?.as_object()?;
    if choice.get("type").and_then(Value::as_str) != Some("function") {
        return None;
    }
    choice
        .get("name")
        .or_else(|| {
            choice
                .get("function")
                .and_then(|function| function.get("name"))
        })
        .and_then(Value::as_str)
}

fn convert_tool_to_kiro(tool: &Value) -> Option<Value> {
    let name = tool
        .get("name")
        .or_else(|| tool.pointer("/function/name"))?
        .as_str()?;
    let description = tool
        .get("description")
        .or_else(|| tool.pointer("/function/description"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("Tool");
    let parameters = tool
        .get("parameters")
        .or_else(|| tool.pointer("/function/parameters"))
        .cloned()
        .unwrap_or_else(|| json!({"type": "object"}));
    Some(json!({
        "toolSpecification": {
            "name": name,
            "description": description,
            "inputSchema": {
                "json": sanitize_schema(parameters)
            }
        }
    }))
}

fn sanitize_schema(value: Value) -> Value {
    match value {
        Value::Object(mut object) => {
            object.remove("additionalProperties");
            if object
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
            {
                object.remove("required");
            }
            let sanitized = object
                .into_iter()
                .map(|(key, value)| (key, sanitize_schema(value)))
                .collect();
            Value::Object(sanitized)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sanitize_schema).collect()),
        other => other,
    }
}

fn non_empty_content(content: &str) -> String {
    if content.trim().is_empty() {
        EMPTY_PLACEHOLDER.to_owned()
    } else {
        content.to_owned()
    }
}

fn conversation_id_from_request(request: &Value) -> String {
    request
        .get("conversation_id")
        .or_else(|| request.get("previous_response_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map_or_else(random_conversation_id, str::to_owned)
}

fn random_conversation_id() -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        rand::random::<u32>(),
        rand::random::<u16>(),
        rand::random::<u16>(),
        rand::random::<u16>(),
        rand::random::<u64>() & 0x0000_ffff_ffff_ffff
    )
}

fn input_fragment(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Object(object) if object.is_empty() => String::new(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn normalize_tool_arguments(arguments: &str) -> String {
    if arguments.trim().is_empty() {
        return "{}".to_owned();
    }
    serde_json::from_str::<Value>(arguments)
        .map_or_else(|_| "{}".to_owned(), |value| value.to_string())
}

fn output_text_part(text: &str) -> Value {
    json!({
        "type": "output_text",
        "text": text,
        "annotations": []
    })
}

fn json_event(event: &str, value: Value) -> JsonSseEvent {
    JsonSseEvent {
        event: Some(event.to_owned()),
        value,
    }
}

fn kiro_metadata(credit_usage: Option<f64>, context_usage_percentage: Option<f64>) -> Value {
    let mut metadata = Map::new();
    if let Some(usage) = credit_usage {
        metadata.insert("kiro_credit_usage".to_owned(), json!(usage));
    }
    if let Some(percentage) = context_usage_percentage {
        metadata.insert(
            "kiro_context_usage_percentage".to_owned(),
            json!(percentage),
        );
    }
    Value::Object(metadata)
}

fn copy_optional_response_field(response: &mut Value, request: &Value, key: &str) {
    if let Some(value) = request.get(key)
        && let Some(object) = response.as_object_mut()
    {
        object.insert(key.to_owned(), value.clone());
    }
}

fn kiro_error_message(value: &Value) -> Option<String> {
    value
        .pointer("/error/message")
        .or_else(|| value.get("message"))
        .or_else(|| value.get("__type"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn machine_fingerprint(credentials: &Credentials) -> String {
    let hostname = env::var("HOSTNAME").unwrap_or_default();
    let username = env::var("USER").unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(hostname.as_bytes());
    hasher.update(b":");
    hasher.update(username.as_bytes());
    hasher.update(b":");
    hasher.update(credentials.account_id.as_bytes());
    let digest = hasher.finalize();
    hex::encode(digest)
}

fn invocation_id() -> String {
    random_conversation_id()
}

fn header_value(value: &str) -> Result<HeaderValue> {
    HeaderValue::from_str(value).map_err(|_| Error::config("invalid header value"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> Credentials {
        Credentials {
            provider: crate::config::Provider::Kiro,
            access_token: "access".to_owned(),
            refresh_token: serde_json::to_string(&json!({
                "kind": "desktop",
                "refresh_token": "refresh",
                "region": "us-east-1",
                "user_agent": "KiroIDE",
                "profile_arn": "arn:aws:codewhisperer:us-east-1:123:profile/test"
            }))
            .unwrap(),
            expires_at: 1,
            account_id: "kiro-desktop:us-east-1".to_owned(),
        }
    }

    #[test]
    fn builds_kiro_payload_from_responses_body() {
        let request = json!({
            "model": "kiro/claude-sonnet-4.5",
            "instructions": "be terse",
            "input": [{
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]
            }]
        });

        let payload = to_kiro_payload(&request, &credentials()).unwrap();

        assert_eq!(
            payload["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
            "claude-sonnet-4.5"
        );
        assert_eq!(
            payload["conversationState"]["currentMessage"]["userInputMessage"]["content"],
            "be terse\n\nhello"
        );
        assert_eq!(
            payload["profileArn"],
            "arn:aws:codewhisperer:us-east-1:123:profile/test"
        );
    }

    #[test]
    fn builds_kiro_payload_from_openai_responses_string_input() {
        let request = json!({
            "model": "auto",
            "input": "hello from responses"
        });

        let payload = to_kiro_payload(&request, &credentials()).unwrap();

        assert_eq!(
            payload["conversationState"]["currentMessage"]["userInputMessage"]["content"],
            "hello from responses"
        );
        assert_eq!(
            payload["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
            "auto"
        );
    }

    #[test]
    fn builds_kiro_payload_from_openai_chat_history_and_tools() {
        let request = json!({
            "model": "claude-sonnet-4.5",
            "tools": [{
                "type": "function",
                "name": "lookup",
                "description": "fetch data",
                "parameters": {
                    "type": "object",
                    "properties": {"q": {"type": "string"}},
                    "required": [],
                    "additionalProperties": false
                }
            }],
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "find x"}]
                },
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "lookup",
                    "arguments": "{\"q\":\"x\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "done"
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "summarize"}]
                }
            ]
        });

        let payload = to_kiro_payload(&request, &credentials()).unwrap();
        let state = &payload["conversationState"];

        assert_eq!(state["history"][0]["userInputMessage"]["content"], "find x");
        assert_eq!(
            state["history"][1]["assistantResponseMessage"]["toolUses"][0]["name"],
            "lookup"
        );
        assert_eq!(
            state["history"][1]["assistantResponseMessage"]["toolUses"][0]["input"],
            json!({"q": "x"})
        );
        assert_eq!(
            state["history"][2]["userInputMessage"]["userInputMessageContext"]["toolResults"][0]["toolUseId"],
            "call_1"
        );
        assert_eq!(
            state["currentMessage"]["userInputMessage"]["userInputMessageContext"]["tools"][0]["toolSpecification"]
                ["inputSchema"]["json"],
            json!({
                "type": "object",
                "properties": {"q": {"type": "string"}}
            })
        );
    }

    #[test]
    fn kiro_payload_uses_only_supported_runtime_fields() {
        let request = json!({
            "model": "claude-sonnet-4.5",
            "temperature": 0.2,
            "top_p": 0.8,
            "max_output_tokens": 128,
            "stop": ["DONE"],
            "reasoning": {"effort": "high"},
            "tool_choice": "none",
            "tools": [{
                "type": "function",
                "name": "lookup",
                "description": "fetch data",
                "parameters": {"type": "object"}
            }],
            "input": [{
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]
            }]
        });

        let payload = to_kiro_payload(&request, &credentials()).unwrap();
        let serialized = serde_json::to_string(&payload).unwrap();

        assert_eq!(
            payload
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["conversationState", "profileArn"]
        );
        assert!(serialized.contains("conversationState"));
        assert!(!serialized.contains("temperature"));
        assert!(!serialized.contains("top_p"));
        assert!(!serialized.contains("max_output_tokens"));
        assert!(!serialized.contains("\"stop\""));
        assert!(!serialized.contains("reasoning"));
        assert!(
            payload["conversationState"]["currentMessage"]["userInputMessage"]
                ["userInputMessageContext"]
                .is_null()
        );
    }

    #[test]
    fn maps_inline_images_and_documents_to_kiro_blocks() {
        let request = json!({
            "model": "claude-sonnet-4.5",
            "input": [{
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "inspect"},
                    {"type": "input_image", "image_url": "data:image/png;base64,aGVsbG8="},
                    {
                        "type": "input_file",
                        "filename": "notes.md",
                        "file_data": "data:text/markdown;base64,IyBOb3Rlcw=="
                    }
                ]
            }]
        });

        let payload = to_kiro_payload(&request, &credentials()).unwrap();
        let message = &payload["conversationState"]["currentMessage"]["userInputMessage"];

        assert_eq!(message["images"][0]["format"], "png");
        assert_eq!(message["images"][0]["source"]["bytes"], "aGVsbG8=");
        assert_eq!(message["documents"][0]["name"], "notes");
        assert_eq!(message["documents"][0]["format"], "md");
        assert_eq!(message["documents"][0]["source"]["bytes"], "IyBOb3Rlcw==");
    }

    #[test]
    fn rejects_remote_images_for_kiro_payload() {
        let request = json!({
            "model": "claude-sonnet-4.5",
            "input": [{
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "inspect"},
                    {"type": "input_image", "image_url": "https://example.com/a.png"}
                ]
            }]
        });

        let error = to_kiro_payload(&request, &credentials()).unwrap_err();

        assert!(error.to_string().contains("inline data URLs"));
    }

    #[test]
    fn builds_kiro_payload_from_anthropic_converted_blocks() {
        let request = json!({
            "model": "claude-sonnet-4.5",
            "input": [
                {
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "hello "},
                        {"type": "input_text", "text": "claude"}
                    ]
                },
                {
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "hi"}]
                },
                {
                    "role": "user",
                    "content": [{"type": "input_text", "text": "continue"}]
                }
            ]
        });

        let payload = to_kiro_payload(&request, &credentials()).unwrap();
        let state = &payload["conversationState"];

        assert_eq!(
            state["history"][0]["userInputMessage"]["content"],
            "hello claude"
        );
        assert_eq!(
            state["history"][1]["assistantResponseMessage"]["content"],
            "hi"
        );
        assert_eq!(
            state["currentMessage"]["userInputMessage"]["content"],
            "continue"
        );
    }

    #[test]
    fn maps_kiro_tool_events_to_responses_function_call() {
        let request = json!({"model": "claude-sonnet-4.5", "input": "use lookup"});
        let mut state = KiroStreamState::new(request);

        assert!(
            state
                .apply_payload(&json!({"name": "lookup", "input": {}, "toolUseId": "call_1"}))
                .is_empty()
        );
        assert!(
            state
                .apply_payload(&json!({"input": "{\"q\":\"x\"}"}))
                .is_empty()
        );
        let events = state.apply_payload(&json!({"stop": true}));

        let done = events
            .iter()
            .find(|event| {
                event.value.get("type").and_then(Value::as_str) == Some("response.output_item.done")
            })
            .unwrap();
        assert_eq!(done.value["item"]["type"], "function_call");
        assert_eq!(done.value["item"]["call_id"], "call_1");
        assert_eq!(done.value["item"]["name"], "lookup");
        assert_eq!(done.value["item"]["arguments"], "{\"q\":\"x\"}");
    }

    #[test]
    fn maps_wrapped_kiro_reasoning_events() {
        let request = json!({"model": "claude-sonnet-4.5", "input": "think"});
        let mut state = KiroStreamState::new(request);

        let events = state.apply_payload(&json!({
            "reasoningContentEvent": {"text": "step", "signature": "sig"}
        }));
        let final_events = state.finish_events();

        assert!(events.iter().any(|event| {
            event.value.get("type").and_then(Value::as_str) == Some("response.reasoning_text.delta")
                && event.value.get("delta").and_then(Value::as_str) == Some("step")
        }));
        let completed = final_events
            .iter()
            .find(|event| {
                event.value.get("type").and_then(Value::as_str) == Some("response.completed")
            })
            .unwrap();
        assert_eq!(
            completed.value["response"]["output"][0]["type"],
            "reasoning"
        );
        assert_eq!(
            completed.value["response"]["output"][0]["encrypted_content"],
            "sig"
        );
    }

    #[test]
    fn decodes_aws_event_stream_frame() {
        let payload = br#"{"content":"OK","modelId":"auto"}"#;
        let total_len = 12 + payload.len() + 4;
        let mut frame = Vec::new();
        frame.extend_from_slice(&u32::try_from(total_len).unwrap().to_be_bytes());
        frame.extend_from_slice(&0_u32.to_be_bytes());
        frame.extend_from_slice(&0_u32.to_be_bytes());
        frame.extend_from_slice(payload);
        frame.extend_from_slice(&0_u32.to_be_bytes());
        let mut decoder = AwsEventStreamDecoder::default();

        let events = decoder.push_chunk(&Bytes::from(frame)).unwrap();

        assert_eq!(events, vec![json!({"content": "OK", "modelId": "auto"})]);
    }
}
