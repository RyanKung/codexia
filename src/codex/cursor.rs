use crate::{
    Error, Result,
    codex::{convert::to_codex_request, sse::JsonSseEvent},
    config::{Credentials, now_unix},
    openai::{
        response::{
            AssistantMessage, ChatChoice, ChatCompletionChunk, ChatCompletionResponse, Usage,
            chunk_finished, chunk_with_content, chunk_with_role,
        },
        types::ChatCompletionRequest,
    },
};
use futures_util::{Stream, StreamExt};
use prost::Message;
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    pin::Pin,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::timeout;

const CURSOR_API_BASE_URL: &str = "https://api2.cursor.sh";
const CURSOR_CLIENT_VERSION: &str = "cli-2026.05.07-42ddaca";
const CURSOR_API_TIMEOUT: Duration = Duration::from_secs(180);
const CONNECT_DATA_FLAG_END_STREAM: u8 = 0x02;
const CURSOR_AGENT_MODE_ASK: i32 = 2;

/// Fetches the authenticated Cursor account's usable `AgentService` models.
///
/// # Errors
///
/// Returns an error when credentials are invalid, Cursor rejects the request,
/// or the `AgentService` model registry response cannot be decoded.
pub async fn list_model_ids(credentials: &Credentials) -> Result<Vec<String>> {
    let client = CursorApiClient::new(credentials);
    let models = client.get_usable_models().await?;
    Ok(cursor_model_ids(&models))
}

/// Sends a text-only chat completion through Cursor's `AgentService` protocol.
///
/// # Errors
///
/// Returns an error when the `OpenAI` request cannot be adapted to text-only
/// ask mode, Cursor rejects the request, or the `AgentService` response cannot
/// be decoded.
pub async fn complete_chat(
    request: ChatCompletionRequest,
    credentials: &Credentials,
) -> Result<ChatCompletionResponse> {
    let id = chat_completion_id();
    let created = now_unix();
    let model = request.model.clone();
    let body = to_codex_request(&request)?;
    let output = run_cursor_api(&body, credentials).await?;

    Ok(ChatCompletionResponse {
        id,
        object: "chat.completion",
        created,
        model,
        choices: vec![ChatChoice {
            index: 0,
            message: AssistantMessage {
                role: "assistant",
                content: output.text,
                tool_calls: None,
                images: None,
            },
            finish_reason: "stop".to_owned(),
        }],
        usage: output.usage,
    })
}

/// Sends a text-only chat completion and adapts the final answer into chunks.
#[must_use]
pub fn stream_chat(
    request: ChatCompletionRequest,
    credentials: Credentials,
) -> Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk>> + Send>> {
    Box::pin(async_stream::try_stream! {
        let id = chat_completion_id();
        let created = now_unix();
        let model = request.model.clone();
        yield chunk_with_role(&id, created, &model);

        let body = to_codex_request(&request)?;
        let output = run_cursor_api(&body, &credentials).await?;
        if !output.text.is_empty() {
            yield chunk_with_content(&id, created, &model, output.text);
        }
        yield chunk_finished(&id, created, &model, "stop");
    })
}

/// Sends a text-only Responses request through Cursor's `AgentService` protocol.
///
/// # Errors
///
/// Returns an error when the Responses payload cannot be adapted to text-only
/// ask mode, Cursor rejects the request, or the `AgentService` response cannot
/// be decoded.
pub async fn complete_response(body: &Value, credentials: &Credentials) -> Result<Value> {
    let output = run_cursor_api(body, credentials).await?;
    Ok(response_value(body, &output))
}

/// Sends a text-only Responses request and adapts the final answer into raw SSE events.
#[must_use]
pub fn response_event_stream(
    body: Value,
    credentials: Credentials,
) -> Pin<Box<dyn Stream<Item = Result<JsonSseEvent>> + Send>> {
    Box::pin(async_stream::try_stream! {
        let output = run_cursor_api(&body, &credentials).await?;
        for event in response_events(&body, &output) {
            yield event;
        }
    })
}

async fn run_cursor_api(body: &Value, credentials: &Credentials) -> Result<CursorOutput> {
    let request = CursorRequest::from_body(body)?;
    let client = CursorApiClient::new(credentials);
    timeout(CURSOR_API_TIMEOUT, client.run_prompt(&request))
        .await
        .map_err(|_| Error::upstream("Cursor provider timed out"))?
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CursorRequest {
    model: String,
    prompt: String,
}

impl CursorRequest {
    fn from_body(body: &Value) -> Result<Self> {
        reject_client_tools(body)?;
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("cursor/auto")
            .to_owned();
        let mut sections = Vec::new();

        if let Some(instructions) = body
            .get("instructions")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            sections.push(format!("System:\n{instructions}"));
        }

        let input = body
            .get("input")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::config("Cursor requests require text input"))?;
        for item in input {
            if let Some(section) = input_item_prompt_section(item)? {
                sections.push(section);
            }
        }

        let prompt = sections.join("\n\n");
        if prompt.trim().is_empty() {
            return Err(Error::config(
                "Cursor requests require non-empty text input",
            ));
        }

        Ok(Self { model, prompt })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CursorOutput {
    text: String,
    usage: Option<Usage>,
    request_id: Option<String>,
}

#[derive(Clone)]
struct CursorApiClient {
    http: reqwest::Client,
    access_token: String,
    base_url: String,
}

impl CursorApiClient {
    fn new(credentials: &Credentials) -> Self {
        let base_url = std::env::var("ROTOM_CURSOR_BASE_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| CURSOR_API_BASE_URL.to_owned());
        Self {
            http: reqwest::Client::new(),
            access_token: credentials.access_token.clone(),
            base_url: base_url.trim_end_matches('/').to_owned(),
        }
    }

    async fn run_prompt(&self, request: &CursorRequest) -> Result<CursorOutput> {
        let models = self.get_usable_models().await?;
        let model = select_cursor_model(&request.model, &models)?;
        let request_id = cursor_uuid();
        let conversation_id = cursor_uuid();
        let message_id = cursor_uuid();
        let run_request =
            build_agent_client_message(request, &model, &conversation_id, &message_id);

        crate::logging::trace_json(
            "upstream.cursor.request",
            &json!({
                "endpoint": "AgentService/Run",
                "request_id": request_id,
                "conversation_id": conversation_id,
                "model": model.model_id,
                "prompt_chars": request.prompt.len(),
            }),
        );

        let response = self.post_run(&request_id, run_request).await?;
        collect_cursor_output(response, request_id).await
    }

    async fn get_usable_models(&self) -> Result<Vec<CursorModel>> {
        let response = self
            .post_json(
                "agent.v1.AgentService/GetUsableModels",
                None,
                &json!({ "customModelIds": [] }),
            )
            .await?;
        let value = response.json::<CursorModelsResponse>().await?;
        Ok(value.models)
    }

    async fn post_run(
        &self,
        request_id: &str,
        run_request: AgentClientMessage,
    ) -> Result<reqwest::Response> {
        let encoded_request = run_request.encode_to_vec();
        let initial_frame = connect_proto_envelope(&encoded_request);
        let response = self
            .http
            .post(self.url("agent.v1.AgentService/Run"))
            .version(reqwest::Version::HTTP_2)
            .headers(self.headers(Some(request_id), CursorPayloadFormat::Proto)?)
            .body(initial_frame)
            .send()
            .await?;
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(cursor_response_error("Run", response).await)
        }
    }

    async fn post_json(
        &self,
        path: &str,
        request_id: Option<&str>,
        body: &Value,
    ) -> Result<reqwest::Response> {
        let response = self
            .http
            .post(self.url(path))
            .headers(self.headers(request_id, CursorPayloadFormat::Json)?)
            .json(body)
            .send()
            .await?;
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(cursor_response_error(path, response).await)
        }
    }

    fn headers(
        &self,
        request_id: Option<&str>,
        payload_format: CursorPayloadFormat,
    ) -> Result<reqwest::header::HeaderMap> {
        let mut headers = reqwest::header::HeaderMap::new();
        let generated_request_id;
        let request_id = if let Some(request_id) = request_id {
            request_id
        } else {
            generated_request_id = cursor_uuid();
            &generated_request_id
        };
        headers.insert(
            reqwest::header::AUTHORIZATION,
            header_value(&format!("Bearer {}", self.access_token))?,
        );
        headers.insert(
            "connect-protocol-version",
            reqwest::header::HeaderValue::from_static("1"),
        );
        headers.insert(
            "x-ghost-mode",
            reqwest::header::HeaderValue::from_static("true"),
        );
        headers.insert(
            "x-cursor-client-type",
            reqwest::header::HeaderValue::from_static("cli"),
        );
        headers.insert(
            "x-cursor-client-version",
            header_value(&cursor_client_version())?,
        );
        headers.insert("x-request-id", header_value(request_id)?);
        headers.insert("x-original-request-id", header_value(request_id)?);
        match payload_format {
            CursorPayloadFormat::Json => {}
            CursorPayloadFormat::Proto => {
                headers.insert(
                    reqwest::header::CONTENT_TYPE,
                    reqwest::header::HeaderValue::from_static("application/connect+proto"),
                );
                headers.insert(
                    reqwest::header::ACCEPT,
                    reqwest::header::HeaderValue::from_static("application/connect+proto"),
                );
            }
        }
        Ok(headers)
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.base_url)
    }
}

#[derive(Clone, Copy)]
enum CursorPayloadFormat {
    Json,
    Proto,
}

fn header_value(value: &str) -> Result<reqwest::header::HeaderValue> {
    reqwest::header::HeaderValue::from_str(value)
        .map_err(|error| Error::config(format!("invalid Cursor header value: {error}")))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorModelsResponse {
    #[serde(default)]
    models: Vec<CursorModel>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorModel {
    model_id: String,
    #[serde(default)]
    display_model_id: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    display_name_short: String,
    #[serde(default)]
    aliases: Vec<String>,
    max_mode: Option<bool>,
}

impl CursorModel {
    fn matches(&self, requested: &str) -> bool {
        self.model_id == requested
            || self.display_model_id == requested
            || self.aliases.iter().any(|alias| alias == requested)
    }
}

fn select_cursor_model(requested_model: &str, models: &[CursorModel]) -> Result<CursorModel> {
    let requested = cursor_model_name(requested_model);
    let is_auto = requested.is_empty() || requested == "auto";
    let canonical = match requested.as_str() {
        "" | "auto" => "default",
        "gpt-5" => "gpt",
        "sonnet-4" => "claude-4-sonnet",
        "sonnet-4-thinking" => "claude-4-sonnet-thinking",
        other => other,
    };
    let selected = models
        .iter()
        .find(|model| model.matches(canonical))
        .or_else(|| {
            is_auto
                .then(|| models.iter().find(|model| model.model_id == "default"))
                .flatten()
        })
        .cloned();
    selected.ok_or_else(|| {
        Error::upstream_with_status(
            StatusCode::BAD_REQUEST,
            format!("Cursor provider does not support model `{requested_model}`"),
        )
    })
}

fn cursor_model_name(model: &str) -> String {
    model
        .strip_prefix("cursor/")
        .unwrap_or(model)
        .trim()
        .to_owned()
}

fn cursor_model_ids(models: &[CursorModel]) -> Vec<String> {
    let mut ids = Vec::new();
    push_cursor_model_id(&mut ids, "auto");
    for model in models {
        if model.model_id == "default" {
            continue;
        }
        push_cursor_model_id(&mut ids, &model.model_id);
    }
    for alias in ["gpt-5", "sonnet-4", "sonnet-4-thinking"] {
        push_cursor_model_id(&mut ids, alias);
    }
    ids
}

fn push_cursor_model_id(ids: &mut Vec<String>, id: &str) {
    let normalized = id.trim();
    if normalized.is_empty() {
        return;
    }
    let prefixed = if normalized.starts_with("cursor/") {
        normalized.to_owned()
    } else {
        format!("cursor/{normalized}")
    };
    if !ids.contains(&prefixed) {
        ids.push(prefixed);
    }
}

fn cursor_client_version() -> String {
    std::env::var("ROTOM_CURSOR_CLIENT_VERSION")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| CURSOR_CLIENT_VERSION.to_owned())
}

fn cursor_time_zone() -> String {
    std::env::var("TZ")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "UTC".to_owned())
}

fn build_agent_client_message(
    request: &CursorRequest,
    model: &CursorModel,
    conversation_id: &str,
    message_id: &str,
) -> AgentClientMessage {
    let model_details = ModelDetails {
        model_id: model.model_id.clone(),
        display_model_id: non_empty_or(model.display_model_id.clone(), model.model_id.clone()),
        display_name: non_empty_or(model.display_name.clone(), model.model_id.clone()),
        display_name_short: non_empty_or(model.display_name_short.clone(), model.model_id.clone()),
        aliases: model.aliases.clone(),
        max_mode: model.max_mode,
    };
    let requested_model = RequestedModel {
        model_id: model.model_id.clone(),
        max_mode: model.max_mode.unwrap_or(false),
        built_in_model: true,
        is_variant_string_representation: false,
    };
    AgentClientMessage {
        run_request: Some(AgentRunRequest {
            conversation_state: Some(ConversationStateStructure {
                mode: Some(CURSOR_AGENT_MODE_ASK),
                conversation_started_timestamp_ms: Some(now_unix_millis()),
                conversation_started_time_zone: Some(cursor_time_zone()),
            }),
            action: Some(ConversationAction {
                user_message_action: Some(UserMessageAction {
                    user_message: Some(UserMessage {
                        text: request.prompt.clone(),
                        message_id: message_id.to_owned(),
                        selected_context: Some(SelectedContext {}),
                        mode: CURSOR_AGENT_MODE_ASK,
                        conversation_state_blob_id: Vec::new(),
                    }),
                    request_context: Some(minimal_request_context()),
                }),
            }),
            model_details: Some(model_details),
            requested_model: Some(requested_model),
            mcp_tools: Some(McpTools {}),
            conversation_id: Some(conversation_id.to_owned()),
            exclude_workspace_context: None,
            conversation_group_id: Some(conversation_id.to_owned()),
            client_supports_inline_images: Some(false),
        }),
        client_heartbeat: None,
    }
}

fn non_empty_or(value: String, fallback: String) -> String {
    if value.is_empty() { fallback } else { value }
}

fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn connect_proto_envelope(payload: &[u8]) -> Vec<u8> {
    let len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    let mut output = Vec::with_capacity(payload.len() + 5);
    output.push(0);
    output.extend_from_slice(&len.to_be_bytes());
    output.extend_from_slice(payload);
    output
}

async fn collect_cursor_output(
    response: reqwest::Response,
    request_id: String,
) -> Result<CursorOutput> {
    let mut stream = response.bytes_stream();
    let mut decoder = ConnectFrameDecoder::default();
    let mut text = String::new();
    let mut usage = None;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        for frame in decoder.push(&chunk)? {
            match frame {
                ConnectFrame::Data(payload) => {
                    let message =
                        AgentServerMessage::decode(payload.as_slice()).map_err(|error| {
                            Error::upstream(format!(
                                "Cursor provider returned an invalid AgentServerMessage: {error}"
                            ))
                        })?;
                    tracing::debug!(
                        event = "upstream.cursor.frame",
                        kind = cursor_frame_kind(&message),
                        exec_kind = cursor_exec_kind(&message).unwrap_or("")
                    );
                    crate::logging::trace_json(
                        "upstream.cursor.frame",
                        &json!({ "kind": cursor_frame_kind(&message) }),
                    );
                    if cursor_frame_requires_interaction(&message) {
                        return Err(Error::upstream_with_status(
                            StatusCode::NOT_IMPLEMENTED,
                            "Cursor provider requested an interactive tool response, which rotom does not support",
                        ));
                    }
                    // Cursor keeps some Run streams open for follow-up heartbeats after
                    // the ask-mode answer has been emitted.
                    if !text.trim().is_empty() && cursor_frame_kind(&message) == "heartbeat" {
                        return cursor_output_from_text(text, request_id, usage);
                    }
                    if let Some(delta) = cursor_text_delta(&message) {
                        text.push_str(delta);
                    }
                    if let Some(turn_usage) = cursor_turn_usage(&message) {
                        usage = Some(turn_usage);
                        if !text.trim().is_empty() {
                            return cursor_output_from_text(text, request_id, usage);
                        }
                    }
                }
                ConnectFrame::End => {
                    return cursor_output_from_text(text, request_id, usage);
                }
            }
        }
    }
    decoder.finish()?;
    cursor_output_from_text(text, request_id, usage)
}

fn cursor_output_from_text(
    text: String,
    request_id: String,
    usage: Option<Usage>,
) -> Result<CursorOutput> {
    if text.trim().is_empty() {
        return Err(Error::upstream(
            "Cursor provider completed without a text response",
        ));
    }
    Ok(CursorOutput {
        text,
        usage,
        request_id: Some(request_id),
    })
}

#[derive(Default)]
struct ConnectFrameDecoder {
    buffer: Vec<u8>,
}

impl ConnectFrameDecoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<ConnectFrame>> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        loop {
            if self.buffer.len() < 5 {
                break;
            }
            let flags = self.buffer[0];
            let len = u32::from_be_bytes([
                self.buffer[1],
                self.buffer[2],
                self.buffer[3],
                self.buffer[4],
            ]) as usize;
            if self.buffer.len() < len + 5 {
                break;
            }
            let payload = self.buffer[5..len + 5].to_vec();
            self.buffer.drain(..len + 5);
            if flags & CONNECT_DATA_FLAG_END_STREAM != 0 {
                if !payload.is_empty() {
                    return Err(cursor_connect_error(&payload));
                }
                frames.push(ConnectFrame::End);
            } else {
                frames.push(ConnectFrame::Data(payload));
            }
        }
        Ok(frames)
    }

    fn finish(&self) -> Result<()> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            Err(Error::upstream(
                "Cursor provider ended with a truncated Connect frame",
            ))
        }
    }
}

enum ConnectFrame {
    Data(Vec<u8>),
    End,
}

fn cursor_text_delta(message: &AgentServerMessage) -> Option<&str> {
    message
        .interaction_update
        .as_ref()
        .and_then(|update| update.text_delta.as_ref())
        .map(|delta| delta.text.as_str())
        .filter(|text| !text.is_empty())
}

fn cursor_turn_usage(message: &AgentServerMessage) -> Option<Usage> {
    let turn = message
        .interaction_update
        .as_ref()
        .and_then(|update| update.turn_ended.as_ref())?;
    let prompt_tokens = u32::try_from(turn.input_tokens.unwrap_or_default()).unwrap_or_default();
    let completion_tokens =
        u32::try_from(turn.output_tokens.unwrap_or_default()).unwrap_or_default();
    let total_tokens = prompt_tokens.saturating_add(completion_tokens);
    (total_tokens > 0).then_some(Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens,
    })
}

const fn cursor_frame_requires_interaction(message: &AgentServerMessage) -> bool {
    if message.interaction_query.is_some() {
        return true;
    }
    let Some(update) = message.interaction_update.as_ref() else {
        return false;
    };
    update.partial_tool_call.is_some()
        || update.tool_call_started.is_some()
        || update.tool_call_completed.is_some()
        || update.tool_call_delta.is_some()
        || update.shell_output_delta.is_some()
}

const fn cursor_frame_kind(message: &AgentServerMessage) -> &'static str {
    if let Some(update) = message.interaction_update.as_ref() {
        if update.text_delta.is_some() {
            return "text_delta";
        }
        if update.thinking_delta.is_some() {
            return "thinking_delta";
        }
        if update.user_message_appended.is_some() {
            return "user_message_appended";
        }
        if update.token_delta.is_some() {
            return "token_delta";
        }
        if update.summary.is_some() {
            return "summary";
        }
        if update.summary_started.is_some() {
            return "summary_started";
        }
        if update.summary_completed.is_some() {
            return "summary_completed";
        }
        if update.shell_output_delta.is_some() {
            return "shell_output_delta";
        }
        if update.heartbeat.is_some() {
            return "heartbeat";
        }
        if update.turn_ended.is_some() {
            return "turn_ended";
        }
        if update.step_started.is_some() {
            return "step_started";
        }
        if update.step_completed.is_some() {
            return "step_completed";
        }
        if update.prompt_suggestion.is_some() {
            return "prompt_suggestion";
        }
        if update.post_request_prompt.is_some() {
            return "post_request_prompt";
        }
        if update.active_branch_change.is_some() {
            return "active_branch_change";
        }
        if update.feedback_request.is_some() {
            return "feedback_request";
        }
        if update.partial_tool_call.is_some()
            || update.tool_call_started.is_some()
            || update.tool_call_completed.is_some()
            || update.tool_call_delta.is_some()
        {
            return "tool_update";
        }
        return "interaction_update";
    }
    if message.interaction_query.is_some() {
        return "interaction_query";
    }
    if message.conversation_checkpoint_update.is_some() {
        return "conversation_checkpoint_update";
    }
    if message.exec_server_message.is_some() {
        return "exec_server_message";
    }
    if message.exec_server_control_message.is_some() {
        return "exec_server_control_message";
    }
    if message.kv_server_message.is_some() {
        return "kv_server_message";
    }
    "other"
}

fn cursor_exec_kind(message: &AgentServerMessage) -> Option<&'static str> {
    let exec = message.exec_server_message.as_ref()?;
    if exec.request_context_args.is_some() {
        return Some("request_context_args");
    }
    if exec.shell_args.is_some() {
        return Some("shell_args");
    }
    if exec.shell_stream_args.is_some() {
        return Some("shell_stream_args");
    }
    if exec.read_args.is_some() || exec.redacted_read_args.is_some() {
        return Some("read_args");
    }
    if exec.ls_args.is_some() {
        return Some("ls_args");
    }
    if exec.grep_args.is_some() {
        return Some("grep_args");
    }
    if exec.fetch_args.is_some() {
        return Some("fetch_args");
    }
    if exec.mcp_args.is_some()
        || exec.list_mcp_resources_exec_args.is_some()
        || exec.read_mcp_resource_exec_args.is_some()
        || exec.mcp_state_exec_args.is_some()
    {
        return Some("mcp_args");
    }
    if exec.execute_hook_args.is_some() {
        return Some("execute_hook_args");
    }
    if exec.subagent_args.is_some()
        || exec.force_background_subagent_args.is_some()
        || exec.subagent_await_args.is_some()
    {
        return Some("subagent_args");
    }
    Some("other")
}

fn cursor_connect_error(payload: &[u8]) -> Error {
    let value = serde_json::from_slice::<Value>(payload).unwrap_or(Value::Null);
    let message = value
        .pointer("/error/message")
        .or_else(|| value.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("Cursor provider returned a Connect stream error");
    Error::upstream(message)
}

async fn cursor_response_error(operation: &str, response: reqwest::Response) -> Error {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let downstream_status = if status == StatusCode::UNAUTHORIZED {
        StatusCode::UNAUTHORIZED
    } else if status.is_client_error() {
        status
    } else {
        StatusCode::BAD_GATEWAY
    };
    Error::upstream_with_status(
        downstream_status,
        format!("Cursor {operation} failed with status {status}: {text}"),
    )
}

include!("cursor_adapters.rs");

fn chat_completion_id() -> String {
    format!("chatcmpl-{}-{:08x}", now_unix(), rand::random::<u32>())
}

fn cursor_uuid() -> String {
    let high = rand::random::<u64>();
    let low = rand::random::<u64>();
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (high >> 32) as u32,
        ((high >> 16) & 0xffff) as u16,
        (high & 0xffff) as u16,
        (low >> 48) as u16,
        low & 0x0000_ffff_ffff_ffff
    )
}

trait TitleCase {
    fn to_ascii_titlecase(&self) -> String;
}

impl TitleCase for str {
    fn to_ascii_titlecase(&self) -> String {
        let mut chars = self.chars();
        let Some(first) = chars.next() else {
            return String::new();
        };
        format!("{}{}", first.to_ascii_uppercase(), chars.as_str())
    }
}

include!("cursor_proto.rs");

#[cfg(test)]
mod cursor_tests {
    include!("cursor_tests.rs");
}
