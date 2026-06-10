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
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use prost::Message;
use prost_types::{ListValue, Struct, Value as ProtoValue, value::Kind as ProtoKind};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    collections::HashMap,
    pin::Pin,
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{Mutex, RwLock},
    time::timeout,
};

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
const ROTOM_CURSOR_MCP_PROVIDER: &str = "rotom-tools";
const ROTOM_CURSOR_MCP_SERVER_NAME: &str = "Rotom Tools";
const CURSOR_REMOTE_TOOL_STEERING: &str = "CRITICAL EXECUTION ENVIRONMENT NOTICE: You are operating through a remote bridge and are NOT running on the user's local machine. Cursor's built-in local tools (terminal/shell, run command, read file, list directory, codebase search, grep/ripgrep search, glob/file search, edit, write, delete, diagnostics, fetch) are DISABLED in this environment and WILL FAIL if you call them. The ONLY way to inspect files, run commands, search, or take any action is to call the externally provided tools listed below (each prefixed with `rotom-tools-`). For every single step, you MUST select the appropriate `rotom-tools-` tool instead of any built-in tool. Never attempt to use a built-in tool.";
const CURSOR_BUILTIN_TOOL_DECLINE_REASON: &str = "This built-in tool is unavailable: the agent runs through a remote bridge, not on the user's machine. Do not call built-in tools. Use the externally provided `rotom-tools-` tools to read files, run commands, search, or take any action.";

type CursorBodyStream =
    Pin<Box<dyn Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send>>;

struct CursorActiveStream {
    stream: CursorBodyStream,
    decoder: ConnectFrameDecoder,
    text: String,
    usage: Option<Usage>,
    blob_store: HashMap<String, Vec<u8>>,
}

#[derive(Clone)]
struct CursorToolSession {
    client: Option<CursorApiClient>,
    request_id: String,
    exec_id: Option<String>,
    exec_numeric_id: u32,
    next_append_seqno: i64,
    stream_state: Option<Arc<Mutex<CursorActiveStream>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CursorToolResult {
    call_id: String,
    output: String,
    is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CursorStreamContext {
    request_id: String,
    next_append_seqno: i64,
}

fn cursor_tool_sessions() -> &'static RwLock<HashMap<String, CursorToolSession>> {
    static TOOL_SESSIONS: OnceLock<RwLock<HashMap<String, CursorToolSession>>> = OnceLock::new();
    TOOL_SESSIONS.get_or_init(|| RwLock::new(HashMap::new()))
}

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
    let tool_result = cursor_tool_result_input(body).await;
    timeout(CURSOR_API_TIMEOUT, client.run_prompt(&request, tool_result))
        .await
        .map_err(|_| Error::upstream("Cursor provider timed out"))?
}

#[derive(Debug, Clone, PartialEq)]
struct CursorClientTool {
    name: String,
    description: String,
    parameters: Value,
}

#[derive(Debug, Clone, PartialEq)]
struct CursorRequest {
    model: String,
    prompt: String,
    has_client_tools: bool,
    client_tools: Vec<CursorClientTool>,
}

impl CursorRequest {
    fn from_body(body: &Value) -> Result<Self> {
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("cursor/auto")
            .to_owned();
        let client_tools = cursor_client_tools(body);
        let has_client_tools = !client_tools.is_empty();
        let mut sections = Vec::new();

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
            client_tools,
        })
    }
}

fn cursor_client_tools(body: &Value) -> Vec<CursorClientTool> {
    body.get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(cursor_client_tool)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn cursor_client_tool(tool: &Value) -> Option<CursorClientTool> {
    let kind = tool
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("function");
    if kind != "function" {
        return None;
    }

    let function = tool.get("function");
    let name = tool
        .get("name")
        .or_else(|| function.and_then(|value| value.get("name")))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_owned();
    let description = tool
        .get("description")
        .or_else(|| function.and_then(|value| value.get("description")))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let parameters = tool
        .get("parameters")
        .or_else(|| tool.get("input_schema"))
        .or_else(|| function.and_then(|value| value.get("parameters")))
        .or_else(|| function.and_then(|value| value.get("input_schema")))
        .cloned()
        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));

    Some(CursorClientTool {
        name,
        description,
        parameters,
    })
}

async fn cursor_tool_result_input(body: &Value) -> Option<(CursorToolSession, CursorToolResult)> {
    let input = body.get("input")?.as_array()?;
    let result = input.iter().rev().find_map(|item| {
        (item.get("type").and_then(Value::as_str) == Some("function_call_output")).then(|| {
            CursorToolResult {
                call_id: item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                output: item.get("output").and_then(Value::as_str).map_or_else(
                    || {
                        item.get("output")
                            .cloned()
                            .unwrap_or(Value::Null)
                            .to_string()
                    },
                    str::to_owned,
                ),
                is_error: item
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }
        })
    })?;
    let session = cursor_tool_sessions()
        .write()
        .await
        .remove(&result.call_id)?;
    Some((session, result))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CursorOutput {
    text: String,
    tool_calls: Vec<ToolCall>,
    usage: Option<Usage>,
    request_id: Option<String>,
}

impl CursorActiveStream {
    fn new(response: reqwest::Response) -> Self {
        Self {
            stream: Box::pin(response.bytes_stream()),
            decoder: ConnectFrameDecoder::default(),
            text: String::new(),
            usage: None,
            blob_store: HashMap::new(),
        }
    }
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

    async fn run_prompt(
        &self,
        request: &CursorRequest,
        tool_result: Option<(CursorToolSession, CursorToolResult)>,
    ) -> Result<CursorOutput> {
        if let Some((session, result)) = tool_result {
            return self
                .continue_after_tool_result(request, &session, &result)
                .await;
        }

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
                "endpoint": "AgentService/RunSSE+BidiAppend",
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
        let stream_state = Arc::new(Mutex::new(CursorActiveStream::new(response)));
        collect_cursor_output(
            stream_state,
            CursorStreamContext {
                request_id,
                next_append_seqno: 1,
            },
            self.clone(),
            request.client_tools.clone(),
        )
        .await
    }

    async fn continue_after_tool_result(
        &self,
        request: &CursorRequest,
        session: &CursorToolSession,
        result: &CursorToolResult,
    ) -> Result<CursorOutput> {
        crate::logging::trace_json(
            "upstream.cursor.request",
            &json!({
                "endpoint": "BidiAppendResume",
                "request_id": session.request_id,
                "model": request.model,
                "tool_call_id": result.call_id,
                "is_error": result.is_error,
                "result_chars": result.output.len(),
            }),
        );

        let Some(stream_state) = session.stream_state.clone() else {
            return Err(Error::upstream(
                "Cursor provider lost the pending tool stream before tool_result continuation",
            ));
        };
        let Some(client) = session.client.clone() else {
            return Err(Error::upstream(
                "Cursor provider lost the pending tool client before tool_result continuation",
            ));
        };

        client
            .post_continue(
                &session.request_id,
                session.next_append_seqno,
                vec![
                    agent_client_message_with_mcp_result(
                        session.exec_numeric_id,
                        session.exec_id.clone(),
                        result,
                    ),
                    agent_client_message_with_exec_control(session.exec_numeric_id),
                ],
            )
            .await?;
        collect_cursor_output(
            stream_state,
            CursorStreamContext {
                request_id: session.request_id.clone(),
                next_append_seqno: session.next_append_seqno.saturating_add(2),
            },
            client,
            request.client_tools.clone(),
        )
        .await
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
        let run_sse = self.open_run_sse(request_id);
        let bidi_append = self.bidi_append(request_id, 0, run_request);
        let (response, ()) = tokio::try_join!(run_sse, bidi_append)?;
        Ok(response)
    }

    async fn post_continue(
        &self,
        request_id: &str,
        append_seqno: i64,
        messages: Vec<AgentClientMessage>,
    ) -> Result<()> {
        for (index, message) in messages.into_iter().enumerate() {
            let index = i64::try_from(index).unwrap_or(i64::MAX);
            self.bidi_append(request_id, append_seqno.saturating_add(index), message)
                .await?;
        }
        Ok(())
    }

    async fn open_run_sse(&self, request_id: &str) -> Result<reqwest::Response> {
        let request_id_frame = connect_proto_envelope(
            &BidiRequestId {
                request_id: request_id.to_owned(),
            }
            .encode_to_vec(),
        );
        let response = self
            .http
            .post(self.url("agent.v1.AgentService/RunSSE"))
            .version(reqwest::Version::HTTP_2)
            .headers(self.headers(Some(request_id), CursorPayloadFormat::ProtoStream)?)
            .body(request_id_frame)
            .send()
            .await?;
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(cursor_response_error("RunSSE", response).await)
        }
    }

    async fn bidi_append(
        &self,
        request_id: &str,
        append_seqno: i64,
        message: AgentClientMessage,
    ) -> Result<()> {
        let bidi_append_request = BidiAppendRequest {
            data: hex::encode(message.encode_to_vec()),
            request_id: Some(BidiRequestId {
                request_id: request_id.to_owned(),
            }),
            append_seqno,
        }
        .encode_to_vec();
        let response = self
            .http
            .post(self.url("aiserver.v1.BidiService/BidiAppend"))
            .version(reqwest::Version::HTTP_2)
            .headers(self.headers(Some(request_id), CursorPayloadFormat::ProtoUnary)?)
            .body(bidi_append_request)
            .send()
            .await?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(cursor_response_error("BidiAppend", response).await)
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
            CursorPayloadFormat::ProtoStream => {
                headers.insert(
                    reqwest::header::CONTENT_TYPE,
                    reqwest::header::HeaderValue::from_static("application/connect+proto"),
                );
                headers.insert(
                    reqwest::header::ACCEPT,
                    reqwest::header::HeaderValue::from_static("application/connect+proto"),
                );
                headers.insert(
                    "x-cursor-streaming",
                    reqwest::header::HeaderValue::from_static("true"),
                );
            }
            CursorPayloadFormat::ProtoUnary => {
                headers.insert(
                    reqwest::header::CONTENT_TYPE,
                    reqwest::header::HeaderValue::from_static("application/proto"),
                );
                headers.insert(
                    reqwest::header::ACCEPT,
                    reqwest::header::HeaderValue::from_static("application/proto"),
                );
                headers.insert(
                    "x-cursor-streaming",
                    reqwest::header::HeaderValue::from_static("true"),
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
    ProtoStream,
    ProtoUnary,
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

fn cursor_os_version() -> String {
    #[cfg(unix)]
    {
        let mut uts = std::mem::MaybeUninit::<libc::utsname>::uninit();
        // SAFETY: `uname` initializes the provided `utsname` struct on success.
        let ok = unsafe { libc::uname(uts.as_mut_ptr()) == 0 };
        if ok {
            // SAFETY: `uname` succeeded, so `uts` is initialized.
            let uts = unsafe { uts.assume_init() };
            let sysname = unsafe { std::ffi::CStr::from_ptr(uts.sysname.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            let release = unsafe { std::ffi::CStr::from_ptr(uts.release.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            let sysname = if sysname.eq_ignore_ascii_case("darwin") {
                "darwin".to_owned()
            } else {
                sysname.to_ascii_lowercase()
            };
            return format!("{sysname} {release}");
        }
    }
    std::env::consts::OS.to_owned()
}

fn cursor_workspace_path() -> Option<String> {
    std::env::current_dir()
        .ok()
        .and_then(|path| path.into_os_string().into_string().ok())
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
    let workspace_path = cursor_workspace_path();
    let request_context = request_context_for_client_tools(&request.client_tools);
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
                        selected_context: None,
                        mode,
                        conversation_state_blob_id: Vec::new(),
                    }),
                    request_context: Some(request_context),
                }),
                resume_action: None,
            }),
            model_details: Some(model_details),
            requested_model: Some(requested_model),
            mcp_tools: mcp_tools_for_client_tools(&request.client_tools),
            conversation_id: Some(conversation_id.to_owned()),
            mcp_file_system_options: mcp_file_system_options_for_client_tools(
                &request.client_tools,
                workspace_path,
            ),
            exclude_workspace_context: None,
            conversation_group_id: Some(conversation_id.to_owned()),
            client_supports_inline_images: Some(false),
        }),
        exec_client_message: None,
        kv_client_message: None,
        conversation_action: None,
        exec_client_control_message: None,
        client_heartbeat: None,
    }
}

fn request_context_for_client_tools(client_tools: &[CursorClientTool]) -> RequestContext {
    let mut context = minimal_request_context();
    if client_tools.is_empty() {
        return context;
    }

    context.mcp_tools = client_tools
        .iter()
        .map(cursor_mcp_tool_definition)
        .collect();
    context.mcp_instructions.push(McpInstructions {
        server_name: ROTOM_CURSOR_MCP_SERVER_NAME.to_owned(),
        instructions: cursor_mcp_instructions(client_tools),
    });
    context
}

fn mcp_tools_for_client_tools(client_tools: &[CursorClientTool]) -> Option<McpTools> {
    (!client_tools.is_empty()).then(|| McpTools {
        tools: client_tools
            .iter()
            .map(cursor_mcp_tool_definition)
            .collect(),
    })
}

fn mcp_file_system_options_for_client_tools(
    client_tools: &[CursorClientTool],
    workspace_path: Option<String>,
) -> Option<McpFileSystemOptions> {
    (!client_tools.is_empty()).then(|| McpFileSystemOptions {
        enabled: Some(true),
        workspace_project_dir: workspace_path.clone(),
        descriptors: vec![McpDescriptor {
            server_name: ROTOM_CURSOR_MCP_SERVER_NAME.to_owned(),
            server_identifier: ROTOM_CURSOR_MCP_PROVIDER.to_owned(),
            folder_path: workspace_path,
            server_use_instructions: Some(CURSOR_REMOTE_TOOL_STEERING.to_owned()),
        }],
    })
}

fn cursor_mcp_tool_definition(tool: &CursorClientTool) -> McpToolDefinition {
    McpToolDefinition {
        name: format!("{}-{}", ROTOM_CURSOR_MCP_PROVIDER, tool.name),
        description: tool.description.clone(),
        input_schema: Some(json_to_proto_value(&tool.parameters)),
        provider_identifier: ROTOM_CURSOR_MCP_PROVIDER.to_owned(),
        tool_name: tool.name.clone(),
    }
}

fn cursor_mcp_instructions(client_tools: &[CursorClientTool]) -> String {
    let tool_descriptions = client_tools
        .iter()
        .map(|tool| {
            if tool.description.is_empty() {
                format!("- {}", tool.name)
            } else {
                format!("- {}: {}", tool.name, tool.description)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{CURSOR_REMOTE_TOOL_STEERING}\n\nAvailable tools:\n{tool_descriptions}")
}

fn json_to_proto_value(value: &Value) -> ProtoValue {
    let kind = match value {
        Value::Null => ProtoKind::NullValue(0),
        Value::Bool(value) => ProtoKind::BoolValue(*value),
        Value::Number(value) => ProtoKind::NumberValue(value.as_f64().unwrap_or_default()),
        Value::String(value) => ProtoKind::StringValue(value.clone()),
        Value::Array(values) => ProtoKind::ListValue(ListValue {
            values: values.iter().map(json_to_proto_value).collect(),
        }),
        Value::Object(object) => ProtoKind::StructValue(Struct {
            fields: object
                .iter()
                .map(|(key, value)| (key.clone(), json_to_proto_value(value)))
                .collect(),
        }),
    };

    ProtoValue { kind: Some(kind) }
}

fn exec_client_message_base(id: u32, exec_id: Option<String>) -> ExecClientMessage {
    ExecClientMessage {
        id,
        exec_id,
        ..ExecClientMessage::default()
    }
}

/// Declines a Cursor built-in exec tool by returning its matching result variant
/// carrying the remote-environment steering reason, so the model retries with an
/// externally provided MCP tool instead of aborting the turn.
///
/// Returns `None` for exec frames that are not declinable built-in tools (for
/// example the bridged `mcp_args` tool call, which must be surfaced upstream).
fn cursor_builtin_exec_decline(exec: &ExecServerMessage) -> Option<AgentClientMessage> {
    if exec.mcp_args.is_some() {
        return None;
    }
    let reason = CURSOR_BUILTIN_TOOL_DECLINE_REASON.to_owned();
    let exec_id = (!exec.exec_id.is_empty()).then(|| exec.exec_id.clone());
    let mut message = exec_client_message_base(exec.id, exec_id);

    if let Some(shell) = exec.shell_args.as_ref() {
        message.shell_result = Some(ShellResult {
            rejected: Some(shell_rejected(shell, reason)),
        });
    } else if let Some(shell) = exec.shell_stream_args.as_ref() {
        message.shell_stream = Some(ShellStream {
            rejected: Some(shell_rejected(shell, reason)),
        });
    } else if exec.grep_args.is_some() {
        message.grep_result = Some(GrepResult {
            error: Some(GrepError { error: reason }),
        });
    } else if let Some(read) = exec.read_args.as_ref() {
        message.read_result = Some(read_decline(&read.path, reason));
    } else if exec.redacted_read_args.is_some() {
        message.redacted_read_result = Some(read_decline("", reason));
    } else if let Some(ls) = exec.ls_args.as_ref() {
        message.ls_result = Some(LsResult {
            error: Some(LsError {
                path: ls.path.clone(),
                error: reason,
            }),
        });
    } else if let Some(write) = exec.write_args.as_ref() {
        message.write_result = Some(WriteResult {
            error: Some(WriteError {
                path: write.path.clone(),
                error: reason,
            }),
        });
    } else if let Some(delete) = exec.delete_args.as_ref() {
        message.delete_result = Some(DeleteResult {
            error: Some(DeleteError {
                path: delete.path.clone(),
                error: reason,
            }),
        });
    } else if exec.diagnostics_args.is_some() {
        message.diagnostics_result = Some(DiagnosticsResult {
            error: Some(DiagnosticsError {
                path: String::new(),
                error: reason,
            }),
        });
    } else if let Some(fetch) = exec.fetch_args.as_ref() {
        message.fetch_result = Some(FetchResult {
            error: Some(FetchError {
                url: fetch.url.clone(),
                error: reason,
            }),
        });
    } else {
        return None;
    }

    Some(AgentClientMessage {
        run_request: None,
        exec_client_message: Some(message),
        kv_client_message: None,
        conversation_action: None,
        exec_client_control_message: None,
        client_heartbeat: None,
    })
}

fn read_decline(path: &str, reason: String) -> ReadResult {
    ReadResult {
        error: Some(ReadError {
            path: path.to_owned(),
            error: reason,
        }),
    }
}

fn shell_rejected(shell: &ShellArgs, reason: String) -> ShellRejected {
    ShellRejected {
        command: shell.command.clone(),
        working_directory: shell.working_directory.clone(),
        reason,
        is_readonly: false,
    }
}

fn agent_client_message_with_mcp_result(
    exec_numeric_id: u32,
    exec_id: Option<String>,
    result: &CursorToolResult,
) -> AgentClientMessage {
    AgentClientMessage {
        run_request: None,
        exec_client_message: Some(ExecClientMessage {
            mcp_result: Some(McpResult {
                success: (!result.is_error).then(|| McpSuccess {
                    content: vec![McpToolResultContentItem {
                        text: Some(McpTextContent {
                            text: result.output.clone(),
                        }),
                    }],
                    is_error: None,
                }),
                error: result.is_error.then(|| McpError {
                    error: result.output.clone(),
                }),
            }),
            ..exec_client_message_base(exec_numeric_id, exec_id)
        }),
        kv_client_message: None,
        conversation_action: None,
        exec_client_control_message: None,
        client_heartbeat: None,
    }
}

fn agent_client_message_with_request_context_result(
    exec_numeric_id: u32,
    exec_id: Option<String>,
    client_tools: &[CursorClientTool],
) -> AgentClientMessage {
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|path| path.into_os_string().into_string().ok())
        .unwrap_or_default();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_owned());
    let time_zone = std::env::var("TZ").unwrap_or_else(|_| cursor_time_zone());
    let mut request_context = request_context_for_client_tools(client_tools);
    if let Some(env) = request_context.env.as_mut() {
        env.os_version = cursor_os_version();
        env.workspace_paths = (!cwd.is_empty()).then(|| cwd.clone()).into_iter().collect();
        env.shell = shell;
        env.time_zone = time_zone;
        env.workspace_project_dir = (!cwd.is_empty()).then(|| cwd.clone());
        env.process_working_directory = (!cwd.is_empty()).then_some(cwd);
    }
    AgentClientMessage {
        run_request: None,
        exec_client_message: Some(ExecClientMessage {
            request_context_result: Some(RequestContextExecResult {
                success: Some(RequestContextSuccess {
                    request_context: Some(request_context),
                }),
            }),
            ..exec_client_message_base(exec_numeric_id, exec_id)
        }),
        kv_client_message: None,
        conversation_action: None,
        exec_client_control_message: None,
        client_heartbeat: None,
    }
}

const fn agent_client_message_with_exec_control(exec_numeric_id: u32) -> AgentClientMessage {
    AgentClientMessage {
        run_request: None,
        exec_client_message: None,
        kv_client_message: None,
        conversation_action: None,
        exec_client_control_message: Some(ExecClientControlMessage {
            stream_close: Some(ExecClientStreamClose {
                id: exec_numeric_id,
            }),
        }),
        client_heartbeat: None,
    }
}

fn agent_client_message_with_kv_get_result(id: u32, data: Option<&[u8]>) -> AgentClientMessage {
    AgentClientMessage {
        run_request: None,
        exec_client_message: None,
        kv_client_message: Some(KvClientMessage {
            id,
            get_blob_result: Some(KvGetBlobResult {
                data: data.map(<[u8]>::to_vec),
            }),
            set_blob_result: None,
        }),
        conversation_action: None,
        exec_client_control_message: None,
        client_heartbeat: None,
    }
}

const fn agent_client_message_with_kv_set_result(id: u32) -> AgentClientMessage {
    AgentClientMessage {
        run_request: None,
        exec_client_message: None,
        kv_client_message: Some(KvClientMessage {
            id,
            get_blob_result: None,
            set_blob_result: Some(KvSetBlobResult {}),
        }),
        conversation_action: None,
        exec_client_control_message: None,
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

#[allow(clippy::too_many_lines, clippy::significant_drop_tightening)]
async fn collect_cursor_output(
    stream_state: Arc<Mutex<CursorActiveStream>>,
    stream_context: CursorStreamContext,
    client: CursorApiClient,
    client_tools: Vec<CursorClientTool>,
) -> Result<CursorOutput> {
    let mut active_stream = stream_state.lock().await;
    let request_id = stream_context.request_id;
    let mut next_append_seqno = stream_context.next_append_seqno;
    let mut tool_calls = Vec::new();

    while let Some(chunk) = active_stream.stream.next().await {
        let chunk = chunk?;
        for frame in active_stream.decoder.push(&chunk)? {
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
                    if let Some(kv_message) = message.kv_server_message.as_ref() {
                        next_append_seqno = handle_cursor_kv_message(
                            &client,
                            &request_id,
                            next_append_seqno,
                            kv_message,
                            &mut active_stream.blob_store,
                        )
                        .await?;
                        continue;
                    }
                    if let Some(exec_message) = message.exec_server_message.as_ref() {
                        if exec_message.request_context_args.is_some() {
                            client
                                .bidi_append(
                                    &request_id,
                                    next_append_seqno,
                                    agent_client_message_with_request_context_result(
                                        exec_message.id,
                                        (!exec_message.exec_id.is_empty())
                                            .then(|| exec_message.exec_id.clone()),
                                        &client_tools,
                                    ),
                                )
                                .await?;
                            next_append_seqno = next_append_seqno.saturating_add(1);
                            client
                                .bidi_append(
                                    &request_id,
                                    next_append_seqno,
                                    agent_client_message_with_exec_control(exec_message.id),
                                )
                                .await?;
                            next_append_seqno = next_append_seqno.saturating_add(1);
                            continue;
                        }
                        if let Some(decline) = cursor_builtin_exec_decline(exec_message) {
                            tracing::debug!(
                                event = "upstream.cursor.builtin_tool_declined",
                                exec_kind = cursor_exec_kind(&message).unwrap_or("")
                            );
                            client
                                .bidi_append(&request_id, next_append_seqno, decline)
                                .await?;
                            next_append_seqno = next_append_seqno.saturating_add(1);
                            client
                                .bidi_append(
                                    &request_id,
                                    next_append_seqno,
                                    agent_client_message_with_exec_control(exec_message.id),
                                )
                                .await?;
                            next_append_seqno = next_append_seqno.saturating_add(1);
                            continue;
                        }
                    }
                    let frame_context = CursorStreamContext {
                        request_id: request_id.clone(),
                        next_append_seqno,
                    };
                    match cursor_frame_action(&message, &frame_context) {
                        CursorFrameAction::ToolCall(tool_call, mut session) => {
                            let returned_text = std::mem::take(&mut active_stream.text);
                            let returned_usage = active_stream.usage.take();
                            session.client = Some(client.clone());
                            session.stream_state = Some(stream_state.clone());
                            remember_cursor_tool_session(&tool_call.id, *session).await;
                            tool_calls.push(tool_call);
                            return cursor_output_from_parts(
                                returned_text,
                                tool_calls,
                                request_id,
                                returned_usage,
                            );
                        }
                        CursorFrameAction::UnsupportedTool(kind) => {
                            return Err(Error::upstream_with_status(
                                StatusCode::BAD_REQUEST,
                                format!(
                                    "Cursor provider requested unsupported tool response type `{kind}`"
                                ),
                            ));
                        }
                        CursorFrameAction::None => {}
                    }
                    // Cursor keeps some RunSSE streams open for follow-up heartbeats after
                    // the ask-mode answer has been emitted.
                    if !active_stream.text.trim().is_empty()
                        && cursor_frame_kind(&message) == "heartbeat"
                    {
                        let returned_text = std::mem::take(&mut active_stream.text);
                        let returned_usage = active_stream.usage.take();
                        return cursor_output_from_parts(
                            returned_text,
                            tool_calls,
                            request_id,
                            returned_usage,
                        );
                    }
                    if let Some(delta) = cursor_text_delta(&message) {
                        active_stream.text.push_str(delta);
                    }
                    if let Some(turn_usage) = cursor_turn_usage(&message) {
                        active_stream.usage = Some(turn_usage);
                        if !active_stream.text.trim().is_empty() {
                            let returned_text = std::mem::take(&mut active_stream.text);
                            let returned_usage = active_stream.usage.take();
                            return cursor_output_from_parts(
                                returned_text,
                                tool_calls,
                                request_id,
                                returned_usage,
                            );
                        }
                    }
                }
                ConnectFrame::End => {
                    let returned_text = std::mem::take(&mut active_stream.text);
                    let returned_usage = active_stream.usage.take();
                    return cursor_output_from_parts(
                        returned_text,
                        tool_calls,
                        request_id,
                        returned_usage,
                    );
                }
            }
        }
    }
    active_stream.decoder.finish()?;
    let returned_text = std::mem::take(&mut active_stream.text);
    let returned_usage = active_stream.usage.take();
    cursor_output_from_parts(returned_text, tool_calls, request_id, returned_usage)
}

async fn remember_cursor_tool_session(call_id: &str, session: CursorToolSession) {
    if call_id.trim().is_empty() {
        return;
    }
    let call_id = call_id.to_owned();
    cursor_tool_sessions()
        .write()
        .await
        .insert(call_id.clone(), session);
    tokio::spawn(async move {
        tokio::time::sleep(CURSOR_API_TIMEOUT).await;
        cursor_tool_sessions().write().await.remove(&call_id);
    });
}

async fn handle_cursor_kv_message(
    client: &CursorApiClient,
    request_id: &str,
    append_seqno: i64,
    kv_message: &KvServerMessage,
    blob_store: &mut HashMap<String, Vec<u8>>,
) -> Result<i64> {
    if let Some(args) = kv_message.get_blob_args.as_ref() {
        let blob_id = hex::encode(&args.blob_id);
        let response = agent_client_message_with_kv_get_result(
            kv_message.id,
            blob_store.get(&blob_id).map(Vec::as_slice),
        );
        client
            .bidi_append(request_id, append_seqno, response)
            .await?;
        return Ok(append_seqno.saturating_add(1));
    }

    if let Some(args) = kv_message.set_blob_args.as_ref() {
        let blob_id = hex::encode(&args.blob_id);
        blob_store.insert(blob_id, args.blob_data.clone());
        let response = agent_client_message_with_kv_set_result(kv_message.id);
        client
            .bidi_append(request_id, append_seqno, response)
            .await?;
        return Ok(append_seqno.saturating_add(1));
    }

    Ok(append_seqno)
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
    ToolCall(ToolCall, Box<CursorToolSession>),
    UnsupportedTool(&'static str),
    None,
}

fn cursor_frame_action(
    message: &AgentServerMessage,
    stream_context: &CursorStreamContext,
) -> CursorFrameAction {
    if let Some((tool_call, session)) = cursor_mcp_tool_call(message, stream_context) {
        return CursorFrameAction::ToolCall(tool_call, Box::new(session));
    }

    if message.exec_server_message.is_some() {
        return CursorFrameAction::UnsupportedTool(cursor_exec_kind(message).unwrap_or("unknown"));
    }

    let Some(query) = message.interaction_query.as_ref() else {
        return CursorFrameAction::None;
    };

    CursorFrameAction::UnsupportedTool(cursor_interaction_query_kind(query))
}

fn cursor_mcp_tool_call(
    message: &AgentServerMessage,
    stream_context: &CursorStreamContext,
) -> Option<(ToolCall, CursorToolSession)> {
    let exec = message.exec_server_message.as_ref()?;
    let mcp = exec.mcp_args.as_ref()?;
    if mcp.provider_identifier != ROTOM_CURSOR_MCP_PROVIDER {
        return None;
    }

    let name = if mcp.tool_name.is_empty() {
        mcp.name
            .strip_prefix(&format!("{ROTOM_CURSOR_MCP_PROVIDER}-"))
            .unwrap_or(mcp.name.as_str())
            .to_owned()
    } else {
        mcp.tool_name.clone()
    };
    if name.trim().is_empty() {
        return None;
    }

    let arguments = serde_json::to_string(&Value::Object(
        mcp.args
            .iter()
            .map(|(key, value)| (key.clone(), proto_value_to_json(value)))
            .collect::<Map<_, _>>(),
    ))
    .unwrap_or_else(|_| "{}".to_owned());
    let call_id = if mcp.tool_call_id.is_empty() {
        format!("call_{:08x}", rand::random::<u32>())
    } else {
        mcp.tool_call_id.clone()
    };

    Some((
        ToolCall {
            id: call_id,
            kind: "function".to_owned(),
            function: FunctionCall { name, arguments },
        },
        CursorToolSession {
            client: None,
            request_id: stream_context.request_id.clone(),
            exec_id: (!exec.exec_id.is_empty()).then(|| exec.exec_id.clone()),
            exec_numeric_id: exec.id,
            next_append_seqno: stream_context.next_append_seqno,
            stream_state: None,
        },
    ))
}

fn proto_value_to_json(value: &ProtoValue) -> Value {
    match value.kind.as_ref() {
        Some(ProtoKind::NullValue(_)) | None => Value::Null,
        Some(ProtoKind::NumberValue(number)) => {
            serde_json::Number::from_f64(*number).map_or(Value::Null, Value::Number)
        }
        Some(ProtoKind::StringValue(text)) => Value::String(text.clone()),
        Some(ProtoKind::BoolValue(value)) => Value::Bool(*value),
        Some(ProtoKind::StructValue(struct_value)) => Value::Object(
            struct_value
                .fields
                .iter()
                .map(|(key, value)| (key.clone(), proto_value_to_json(value)))
                .collect(),
        ),
        Some(ProtoKind::ListValue(list_value)) => {
            Value::Array(list_value.values.iter().map(proto_value_to_json).collect())
        }
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
    if let Some((tool_call, _)) = cursor_mcp_tool_call(message, &cursor_test_stream_context()) {
        return Some(format!("tool_call:{}", tool_call.function.name));
    }
    if let Some(kind) = cursor_exec_kind(message) {
        return Some(format!("unsupported_tool:{kind}"));
    }
    match cursor_frame_action(message, &cursor_test_stream_context()) {
        CursorFrameAction::ToolCall(tool_call, _) => {
            Some(format!("tool_call:{}", tool_call.function.name))
        }
        CursorFrameAction::UnsupportedTool(kind) => Some(format!("unsupported_tool:{kind}")),
        CursorFrameAction::None => None,
    }
}

#[cfg(test)]
fn cursor_test_stream_context() -> CursorStreamContext {
    CursorStreamContext {
        request_id: "req_test".to_owned(),
        next_append_seqno: 1,
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
