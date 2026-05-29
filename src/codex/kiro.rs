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

include!("kiro_request.rs");

#[cfg(test)]
mod kiro_tests {
    include!("kiro_tests.rs");
}
