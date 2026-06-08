use crate::{
    Error, Result,
    codex::{convert::to_codex_request, sse::JsonSseEvent},
    config::{Credentials, now_unix},
    openai::{
        response::{
            AssistantMessage, ChatChoice, ChatCompletionChunk, ChatCompletionResponse, Usage,
            chunk_finished, chunk_with_content, chunk_with_role, chunk_with_tool_call,
        },
        types::{ChatCompletionRequest, FunctionCall, ToolCall},
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
const ROTOM_CURSOR_AGENT_MODE_ENV: &str = "ROTOM_CURSOR_AGENT_MODE";
const CURSOR_AGENT_MODE_UNSPECIFIED: i32 = 0;
const CURSOR_AGENT_MODE_AGENT: i32 = 1;
const CURSOR_AGENT_MODE_ASK: i32 = 2;
const CURSOR_AGENT_MODE_PLAN: i32 = 3;
const CURSOR_AGENT_MODE_DEBUG: i32 = 4;
const CURSOR_AGENT_MODE_TRIAGE: i32 = 5;
const CURSOR_AGENT_MODE_PROJECT: i32 = 6;
const CURSOR_AGENT_MODE_MULTITASK: i32 = 7;

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

/// Sends a chat completion through Cursor's `AgentService` protocol.
///
/// # Errors
///
/// Returns an error when the `OpenAI` request cannot be adapted to text-only
/// agent input, Cursor rejects the request, or the `AgentService` response cannot
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
    let CursorOutput {
        text,
        tool_calls,
        usage,
        request_id: _,
    } = output;
    let has_tool_calls = !tool_calls.is_empty();

    Ok(ChatCompletionResponse {
        id,
        object: "chat.completion",
        created,
        model,
        choices: vec![ChatChoice {
            index: 0,
            message: AssistantMessage {
                role: "assistant",
                content: text,
                tool_calls: has_tool_calls.then_some(tool_calls),
                images: None,
            },
            finish_reason: if has_tool_calls {
                "tool_calls".to_owned()
            } else {
                "stop".to_owned()
            },
        }],
        usage,
    })
}

/// Sends a chat completion and adapts the final answer into chunks.
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
        let finish_reason = if output.tool_calls.is_empty() {
            "stop"
        } else {
            for (index, tool_call) in output.tool_calls.into_iter().enumerate() {
                let index = u32::try_from(index).unwrap_or(u32::MAX);
                yield chunk_with_tool_call(&id, created, &model, index, tool_call);
            }
            "tool_calls"
        };
        yield chunk_finished(&id, created, &model, finish_reason);
    })
}

/// Sends a Responses request through Cursor's `AgentService` protocol.
///
/// # Errors
///
/// Returns an error when the Responses payload cannot be adapted to text-only
/// agent input, Cursor rejects the request, or the `AgentService` response cannot
/// be decoded.
pub async fn complete_response(body: &Value, credentials: &Credentials) -> Result<Value> {
    let output = run_cursor_api(body, credentials).await?;
    Ok(response_value(body, &output))
}

/// Sends a Responses request and adapts the final answer into raw SSE events.
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
    has_client_tools: bool,
}

impl CursorRequest {
    fn from_body(body: &Value) -> Result<Self> {
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("cursor/auto")
            .to_owned();
        let has_client_tools = body_has_client_tools(body);
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

        Ok(Self {
            model,
            prompt,
            has_client_tools,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CursorOutput {
    text: String,
    tool_calls: Vec<ToolCall>,
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
        let agent_mode = resolve_cursor_agent_mode()?;
        let request_id = cursor_uuid();
        let conversation_id = cursor_uuid();
        let message_id = cursor_uuid();
        let run_request = build_agent_client_message_with_mode(
            request,
            &model,
            &conversation_id,
            &message_id,
            agent_mode,
        );

        crate::logging::trace_json(
            "upstream.cursor.request",
            &json!({
                "endpoint": "AgentService/Run",
                "request_id": request_id,
                "conversation_id": conversation_id,
                "model": model.model_id,
                "mode": cursor_agent_mode_name(agent_mode),
                "mode_value": agent_mode,
                "has_client_tools": request.has_client_tools,
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

fn resolve_cursor_agent_mode() -> Result<i32> {
    parse_cursor_agent_mode(std::env::var(ROTOM_CURSOR_AGENT_MODE_ENV).ok().as_deref())
        .map_err(Error::config)
}

fn parse_cursor_agent_mode(value: Option<&str>) -> std::result::Result<i32, String> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(CURSOR_AGENT_MODE_AGENT);
    };

    if let Ok(mode) = value.parse::<i32>() {
        return match mode {
            CURSOR_AGENT_MODE_UNSPECIFIED
            | CURSOR_AGENT_MODE_AGENT
            | CURSOR_AGENT_MODE_ASK
            | CURSOR_AGENT_MODE_PLAN
            | CURSOR_AGENT_MODE_DEBUG
            | CURSOR_AGENT_MODE_TRIAGE
            | CURSOR_AGENT_MODE_PROJECT
            | CURSOR_AGENT_MODE_MULTITASK => Ok(mode),
            _ => Err(format!(
                "{ROTOM_CURSOR_AGENT_MODE_ENV} must be one of 0-7 or agent/ask/plan/debug/triage/project/multitask"
            )),
        };
    }

    match value.to_ascii_lowercase().replace('-', "_").as_str() {
        "unspecified" => Ok(CURSOR_AGENT_MODE_UNSPECIFIED),
        "agent" => Ok(CURSOR_AGENT_MODE_AGENT),
        "ask" => Ok(CURSOR_AGENT_MODE_ASK),
        "plan" => Ok(CURSOR_AGENT_MODE_PLAN),
        "debug" => Ok(CURSOR_AGENT_MODE_DEBUG),
        "triage" => Ok(CURSOR_AGENT_MODE_TRIAGE),
        "project" => Ok(CURSOR_AGENT_MODE_PROJECT),
        "multitask" | "multi_task" => Ok(CURSOR_AGENT_MODE_MULTITASK),
        _ => Err(format!(
            "{ROTOM_CURSOR_AGENT_MODE_ENV} must be one of 0-7 or agent/ask/plan/debug/triage/project/multitask"
        )),
    }
}

const fn cursor_agent_mode_name(mode: i32) -> &'static str {
    match mode {
        CURSOR_AGENT_MODE_UNSPECIFIED => "unspecified",
        CURSOR_AGENT_MODE_AGENT => "agent",
        CURSOR_AGENT_MODE_ASK => "ask",
        CURSOR_AGENT_MODE_PLAN => "plan",
        CURSOR_AGENT_MODE_DEBUG => "debug",
        CURSOR_AGENT_MODE_TRIAGE => "triage",
        CURSOR_AGENT_MODE_PROJECT => "project",
        CURSOR_AGENT_MODE_MULTITASK => "multitask",
        _ => "unknown",
    }
}

fn build_agent_client_message_with_mode(
    request: &CursorRequest,
    model: &CursorModel,
    conversation_id: &str,
    message_id: &str,
    mode: i32,
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
                mode: Some(mode),
                conversation_started_timestamp_ms: Some(now_unix_millis()),
                conversation_started_time_zone: Some(cursor_time_zone()),
            }),
            action: Some(ConversationAction {
                user_message_action: Some(UserMessageAction {
                    user_message: Some(UserMessage {
                        text: request.prompt.clone(),
                        message_id: message_id.to_owned(),
                        selected_context: Some(SelectedContext {}),
                        mode,
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
    let mut tool_calls = Vec::new();
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
                    match cursor_frame_action(&message) {
                        CursorFrameAction::ToolCall(tool_call) => {
                            if !tool_calls
                                .iter()
                                .any(|existing: &ToolCall| existing.id == tool_call.id)
                            {
                                tool_calls.push(tool_call);
                            }
                            return cursor_output_from_parts(text, tool_calls, request_id, usage);
                        }
                        CursorFrameAction::UnsupportedInteraction(kind) => {
                            return Err(Error::upstream_with_status(
                                StatusCode::NOT_IMPLEMENTED,
                                format!(
                                    "Cursor provider requested unsupported interactive tool response type `{kind}`"
                                ),
                            ));
                        }
                        CursorFrameAction::None => {}
                    }
                    // Cursor keeps some Run streams open for follow-up heartbeats after
                    // the ask-mode answer has been emitted.
                    if !text.trim().is_empty() && cursor_frame_kind(&message) == "heartbeat" {
                        return cursor_output_from_parts(text, tool_calls, request_id, usage);
                    }
                    if let Some(delta) = cursor_text_delta(&message) {
                        text.push_str(delta);
                    }
                    if let Some(turn_usage) = cursor_turn_usage(&message) {
                        usage = Some(turn_usage);
                        if !text.trim().is_empty() {
                            return cursor_output_from_parts(text, tool_calls, request_id, usage);
                        }
                    }
                }
                ConnectFrame::End => {
                    return cursor_output_from_parts(text, tool_calls, request_id, usage);
                }
            }
        }
    }
    decoder.finish()?;
    cursor_output_from_parts(text, tool_calls, request_id, usage)
}

fn cursor_output_from_parts(
    text: String,
    tool_calls: Vec<ToolCall>,
    request_id: String,
    usage: Option<Usage>,
) -> Result<CursorOutput> {
    if text.trim().is_empty() && tool_calls.is_empty() {
        return Err(Error::upstream(
            "Cursor provider completed without a text or tool response",
        ));
    }
    Ok(CursorOutput {
        text,
        tool_calls,
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

enum CursorFrameAction {
    ToolCall(ToolCall),
    UnsupportedInteraction(&'static str),
    None,
}

fn cursor_frame_action(message: &AgentServerMessage) -> CursorFrameAction {
    if let Some(exec) = message.exec_server_message.as_ref()
        && let Some(tool_call) = cursor_exec_tool_call(exec)
    {
        return CursorFrameAction::ToolCall(tool_call);
    }

    let Some(query) = message.interaction_query.as_ref() else {
        return CursorFrameAction::None;
    };

    if let Some(tool_call) = cursor_interaction_tool_call(query) {
        CursorFrameAction::ToolCall(tool_call)
    } else {
        CursorFrameAction::UnsupportedInteraction(cursor_interaction_query_kind(query))
    }
}

fn cursor_exec_tool_call(exec: &ExecServerMessage) -> Option<ToolCall> {
    let (name, arguments, tool_call_id) = if let Some(args) = exec.shell_stream_args.as_ref() {
        (
            "shell_stream",
            shell_args_value(args),
            args.tool_call_id.as_str(),
        )
    } else if let Some(args) = exec.shell_args.as_ref() {
        ("shell", shell_args_value(args), args.tool_call_id.as_str())
    } else if let Some(args) = exec.read_args.as_ref() {
        ("read", read_args_value(args), args.tool_call_id.as_str())
    } else if let Some(args) = exec.ls_args.as_ref() {
        ("ls", ls_args_value(args), args.tool_call_id.as_str())
    } else if let Some(args) = exec.grep_args.as_ref() {
        ("grep", grep_args_value(args), args.tool_call_id.as_str())
    } else if let Some(args) = exec.fetch_args.as_ref() {
        ("fetch", fetch_args_value(args), args.tool_call_id.as_str())
    } else if let Some(args) = exec.write_args.as_ref() {
        ("write", write_args_value(args), args.tool_call_id.as_str())
    } else if let Some(args) = exec.delete_args.as_ref() {
        (
            "delete",
            delete_args_value(args),
            args.tool_call_id.as_str(),
        )
    } else if let Some(args) = exec.request_context_args.as_ref() {
        ("request_context", request_context_args_value(args), "")
    } else {
        return None;
    };

    Some(cursor_function_tool_call(
        cursor_tool_call_id(exec, tool_call_id),
        name,
        arguments,
    ))
}

fn cursor_interaction_tool_call(query: &InteractionQuery) -> Option<ToolCall> {
    if let Some(ask) = query.ask_question_interaction_query.as_ref() {
        return Some(cursor_function_tool_call(
            cursor_interaction_tool_call_id(query.id, &ask.tool_call_id),
            "ask_question",
            ask_question_args_value(ask.args.as_ref()),
        ));
    }
    if let Some(fetch) = query.web_fetch_request_query.as_ref() {
        return Some(cursor_function_tool_call(
            cursor_interaction_tool_call_id(
                query.id,
                fetch
                    .args
                    .as_ref()
                    .map_or("", |args| args.tool_call_id.as_str()),
            ),
            "web_fetch_request",
            web_fetch_request_query_value(fetch),
        ));
    }
    None
}

fn cursor_function_tool_call(id: String, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id,
        kind: "function".to_owned(),
        function: FunctionCall {
            name: name.to_owned(),
            arguments: arguments.to_string(),
        },
    }
}

fn cursor_tool_call_id(exec: &ExecServerMessage, tool_call_id: &str) -> String {
    if !tool_call_id.is_empty() {
        return tool_call_id.to_owned();
    }
    if !exec.exec_id.is_empty() {
        return exec.exec_id.clone();
    }
    format!("cursor_exec_{}", exec.id)
}

fn cursor_interaction_tool_call_id(interaction_id: u32, tool_call_id: &str) -> String {
    if !tool_call_id.is_empty() {
        return tool_call_id.to_owned();
    }
    format!("cursor_interaction_{interaction_id}")
}

fn shell_args_value(args: &ShellArgs) -> Value {
    let mut object = serde_json::Map::new();
    insert_string(&mut object, "command", &args.command);
    insert_string(&mut object, "working_directory", &args.working_directory);
    insert_u32(&mut object, "timeout", args.timeout);
    insert_string_array(&mut object, "simple_commands", &args.simple_commands);
    insert_bool(&mut object, "skip_approval", args.skip_approval);
    insert_optional_string(&mut object, "description", args.description.as_deref());
    Value::Object(object)
}

fn write_args_value(args: &WriteArgs) -> Value {
    let mut object = serde_json::Map::new();
    insert_string(&mut object, "path", &args.path);
    insert_string(&mut object, "file_text", &args.file_text);
    insert_bool(
        &mut object,
        "return_file_content_after_write",
        args.return_file_content_after_write,
    );
    insert_optional_string(&mut object, "encoding_hint", args.encoding_hint.as_deref());
    Value::Object(object)
}

fn delete_args_value(args: &DeleteArgs) -> Value {
    let mut object = serde_json::Map::new();
    insert_string(&mut object, "path", &args.path);
    Value::Object(object)
}

fn grep_args_value(args: &GrepArgs) -> Value {
    let mut object = serde_json::Map::new();
    insert_string(&mut object, "pattern", &args.pattern);
    insert_optional_string(&mut object, "path", args.path.as_deref());
    insert_optional_string(&mut object, "glob", args.glob.as_deref());
    insert_optional_string(&mut object, "output_mode", args.output_mode.as_deref());
    insert_i32(&mut object, "context_before", args.context_before);
    insert_i32(&mut object, "context_after", args.context_after);
    insert_i32(&mut object, "context", args.context);
    insert_bool(&mut object, "case_insensitive", args.case_insensitive);
    insert_optional_string(&mut object, "type_name", args.type_name.as_deref());
    insert_i32(&mut object, "head_limit", args.head_limit);
    insert_bool(&mut object, "multiline", args.multiline);
    insert_optional_string(&mut object, "sort", args.sort.as_deref());
    insert_bool(&mut object, "sort_ascending", args.sort_ascending);
    insert_i32(&mut object, "offset", args.offset);
    Value::Object(object)
}

fn read_args_value(args: &ReadArgs) -> Value {
    let mut object = serde_json::Map::new();
    insert_string(&mut object, "path", &args.path);
    insert_i32(&mut object, "offset", args.offset);
    insert_u32(&mut object, "limit", args.limit);
    insert_optional_string(&mut object, "encoding_hint", args.encoding_hint.as_deref());
    Value::Object(object)
}

fn ls_args_value(args: &LsArgs) -> Value {
    let mut object = serde_json::Map::new();
    insert_string(&mut object, "path", &args.path);
    insert_string_array(&mut object, "ignore", &args.ignore);
    insert_u32(&mut object, "timeout_ms", args.timeout_ms);
    Value::Object(object)
}

fn request_context_args_value(args: &RequestContextArgs) -> Value {
    let mut object = serde_json::Map::new();
    insert_optional_string(
        &mut object,
        "notes_session_id",
        args.notes_session_id.as_deref(),
    );
    insert_optional_string(&mut object, "workspace_id", args.workspace_id.as_deref());
    insert_optional_string(
        &mut object,
        "read_only_pinned_tree_sha",
        args.read_only_pinned_tree_sha.as_deref(),
    );
    insert_optional_string(
        &mut object,
        "read_only_plugin_cache_root",
        args.read_only_plugin_cache_root.as_deref(),
    );
    Value::Object(object)
}

fn fetch_args_value(args: &WebFetchArgs) -> Value {
    let mut object = serde_json::Map::new();
    insert_string(&mut object, "url", &args.url);
    Value::Object(object)
}

fn ask_question_args_value(args: Option<&AskQuestionArgs>) -> Value {
    let Some(args) = args else {
        return Value::Object(serde_json::Map::new());
    };

    let mut object = serde_json::Map::new();
    insert_string(&mut object, "title", &args.title);
    if !args.questions.is_empty() {
        object.insert(
            "questions".to_owned(),
            Value::Array(
                args.questions
                    .iter()
                    .map(ask_question_question_value)
                    .collect(),
            ),
        );
    }
    insert_bool(&mut object, "run_async", args.run_async);
    insert_optional_string(
        &mut object,
        "async_original_tool_call_id",
        args.async_original_tool_call_id.as_deref(),
    );
    Value::Object(object)
}

fn ask_question_question_value(question: &AskQuestionQuestion) -> Value {
    let mut object = serde_json::Map::new();
    insert_string(&mut object, "id", &question.id);
    insert_string(&mut object, "prompt", &question.prompt);
    if !question.options.is_empty() {
        object.insert(
            "options".to_owned(),
            Value::Array(
                question
                    .options
                    .iter()
                    .map(ask_question_option_value)
                    .collect(),
            ),
        );
    }
    insert_bool(&mut object, "allow_multiple", question.allow_multiple);
    Value::Object(object)
}

fn ask_question_option_value(option: &AskQuestionOption) -> Value {
    let mut object = serde_json::Map::new();
    insert_string(&mut object, "id", &option.id);
    insert_string(&mut object, "label", &option.label);
    Value::Object(object)
}

fn web_fetch_request_query_value(query: &WebFetchRequestQuery) -> Value {
    let mut object = serde_json::Map::new();
    if let Some(args) = query.args.as_ref() {
        insert_string(&mut object, "url", &args.url);
    }
    insert_bool(&mut object, "skip_approval", query.skip_approval);
    Value::Object(object)
}

fn insert_string(object: &mut serde_json::Map<String, Value>, key: &str, value: &str) {
    if !value.is_empty() {
        object.insert(key.to_owned(), Value::String(value.to_owned()));
    }
}

fn insert_optional_string(
    object: &mut serde_json::Map<String, Value>,
    key: &str,
    value: Option<&str>,
) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        object.insert(key.to_owned(), Value::String(value.to_owned()));
    }
}

fn insert_string_array(object: &mut serde_json::Map<String, Value>, key: &str, values: &[String]) {
    if !values.is_empty() {
        object.insert(
            key.to_owned(),
            Value::Array(values.iter().cloned().map(Value::String).collect()),
        );
    }
}

fn insert_bool(object: &mut serde_json::Map<String, Value>, key: &str, value: Option<bool>) {
    if let Some(value) = value {
        object.insert(key.to_owned(), Value::Bool(value));
    }
}

fn insert_i32(object: &mut serde_json::Map<String, Value>, key: &str, value: Option<i32>) {
    if let Some(value) = value {
        object.insert(key.to_owned(), Value::Number(value.into()));
    }
}

fn insert_u32(object: &mut serde_json::Map<String, Value>, key: &str, value: Option<u32>) {
    if let Some(value) = value {
        object.insert(key.to_owned(), Value::Number(value.into()));
    }
}

const fn cursor_interaction_query_kind(query: &InteractionQuery) -> &'static str {
    if query.ask_question_interaction_query.is_some() {
        return "ask_question_interaction_query";
    }
    if query.web_fetch_request_query.is_some() {
        return "web_fetch_request_query";
    }
    "unknown"
}

#[cfg(test)]
fn cursor_probe_signal(message: &AgentServerMessage) -> Option<String> {
    if let Some(kind) = cursor_exec_kind(message) {
        return Some(format!("exec:{kind}"));
    }
    match cursor_frame_action(message) {
        CursorFrameAction::ToolCall(tool_call) => Some(format!("tool:{}", tool_call.function.name)),
        CursorFrameAction::UnsupportedInteraction(kind) => {
            Some(format!("interaction_query:{kind}"))
        }
        CursorFrameAction::None => None,
    }
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
