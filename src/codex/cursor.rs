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
use futures_util::Stream;
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    time::Duration,
};
use tokio::{process::Command, time::timeout};

const CURSOR_AGENT_ENV: &str = "ROTOM_CURSOR_AGENT";
const CURSOR_AGENT_WORKSPACE_ENV: &str = "ROTOM_CURSOR_WORKSPACE";
const CURSOR_AGENT_TIMEOUT: Duration = Duration::from_secs(180);

/// Sends a text-only chat completion through the installed Cursor Agent CLI.
pub async fn complete_chat(
    request: ChatCompletionRequest,
    credentials: &Credentials,
) -> Result<ChatCompletionResponse> {
    let id = chat_completion_id();
    let created = now_unix();
    let model = request.model.clone();
    let body = to_codex_request(&request)?;
    let output = run_cursor_agent(&body, credentials).await?;

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
        let output = run_cursor_agent(&body, &credentials).await?;
        if !output.text.is_empty() {
            yield chunk_with_content(&id, created, &model, output.text);
        }
        yield chunk_finished(&id, created, &model, "stop");
    })
}

/// Sends a text-only Responses request through the installed Cursor Agent CLI.
pub async fn complete_response(body: &Value, credentials: &Credentials) -> Result<Value> {
    let output = run_cursor_agent(body, credentials).await?;
    Ok(response_value(body, &output))
}

/// Sends a text-only Responses request and adapts the final answer into raw SSE events.
pub fn response_event_stream(
    body: Value,
    credentials: Credentials,
) -> Pin<Box<dyn Stream<Item = Result<JsonSseEvent>> + Send>> {
    Box::pin(async_stream::try_stream! {
        let output = run_cursor_agent(&body, &credentials).await?;
        for event in response_events(&body, &output) {
            yield event;
        }
    })
}

async fn run_cursor_agent(body: &Value, credentials: &Credentials) -> Result<CursorAgentOutput> {
    let request = CursorAgentRequest::from_body(body)?;
    let executable = cursor_agent_executable();
    let workspace = cursor_agent_workspace()?;
    let args = cursor_agent_args(&request, &workspace, "json");
    crate::logging::trace_json(
        "upstream.cursor.request",
        &json!({
            "executable": executable.display().to_string(),
            "args": redacted_cursor_agent_args(&args),
            "prompt_chars": request.prompt.len(),
        }),
    );

    let mut command = Command::new(&executable);
    command
        .args(&args)
        .env("CURSOR_AUTH_TOKEN", &credentials.access_token)
        .env("NO_OPEN_BROWSER", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = timeout(CURSOR_AGENT_TIMEOUT, command.output())
        .await
        .map_err(|_| Error::upstream("Cursor Agent timed out"))??;
    parse_cursor_agent_output(output.status, &output.stdout, &output.stderr)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CursorAgentRequest {
    model: String,
    prompt: String,
}

impl CursorAgentRequest {
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
struct CursorAgentOutput {
    text: String,
    usage: Option<Usage>,
    request_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CursorAgentJsonLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    subtype: Option<String>,
    #[serde(default)]
    is_error: bool,
    result: Option<String>,
    usage: Option<CursorAgentUsage>,
    request_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CursorAgentUsage {
    #[serde(default, rename = "inputTokens", alias = "input_tokens")]
    input: u32,
    #[serde(default, rename = "outputTokens", alias = "output_tokens")]
    output: u32,
    #[serde(default, rename = "cacheReadTokens", alias = "cache_read_tokens")]
    cache_read: u32,
    #[serde(default, rename = "cacheWriteTokens", alias = "cache_write_tokens")]
    cache_write: u32,
}

impl CursorAgentUsage {
    const fn into_openai_usage(self) -> Usage {
        let prompt_tokens = self
            .input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write);
        Usage {
            prompt_tokens,
            completion_tokens: self.output,
            total_tokens: prompt_tokens.saturating_add(self.output),
        }
    }
}

fn reject_client_tools(body: &Value) -> Result<()> {
    let has_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    let tool_choice_none = body
        .get("tool_choice")
        .and_then(Value::as_str)
        .is_some_and(|value| value == "none");

    if has_tools && !tool_choice_none {
        return Err(Error::upstream_with_status(
            StatusCode::NOT_IMPLEMENTED,
            "Cursor Agent runtime does not support OpenAI-compatible client-supplied tools",
        ));
    }

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
            format!("Cursor Agent runtime does not support Responses input item type `{kind}`"),
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
            "Cursor Agent runtime does not support multimodal inputs through rotom",
        )),
        Some(kind) => Err(Error::upstream_with_status(
            StatusCode::NOT_IMPLEMENTED,
            format!("Cursor Agent runtime does not support content part type `{kind}`"),
        )),
    }
}

fn parse_cursor_agent_output(
    status: std::process::ExitStatus,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<CursorAgentOutput> {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    let mut result = None;

    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<CursorAgentJsonLine>(line) else {
            continue;
        };
        if value.kind.as_deref() == Some("result") {
            if value.is_error || value.subtype.as_deref() == Some("error") {
                return Err(cursor_agent_error(
                    value
                        .result
                        .as_deref()
                        .unwrap_or("Cursor Agent returned an error"),
                ));
            }
            result = Some(CursorAgentOutput {
                text: value.result.unwrap_or_default(),
                usage: value.usage.map(CursorAgentUsage::into_openai_usage),
                request_id: value.request_id,
            });
        }
    }

    if let Some(result) = result {
        return Ok(result);
    }

    let message = [stdout.trim(), stderr.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if !message.is_empty() || !status.success() {
        return Err(cursor_agent_error(&message));
    }

    Err(Error::upstream(
        "Cursor Agent completed without a result payload",
    ))
}

fn cursor_agent_error(message: &str) -> Error {
    let message = if message.trim().is_empty() {
        "Cursor Agent failed without an error message"
    } else {
        message.trim()
    };
    let lowercase_message = message.to_ascii_lowercase();
    let status = if lowercase_message.contains("authentication required")
        || lowercase_message.contains("cursor_auth_token")
        || lowercase_message.contains("invalid auth")
        || lowercase_message.contains("authentication is invalid")
        || lowercase_message.contains("please log in")
        || lowercase_message.contains("not logged in")
    {
        StatusCode::UNAUTHORIZED
    } else {
        StatusCode::BAD_GATEWAY
    };
    Error::upstream_with_status(status, format!("Cursor Agent failed: {message}"))
}

fn response_value(body: &Value, output: &CursorAgentOutput) -> Value {
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

fn response_events(body: &Value, output: &CursorAgentOutput) -> Vec<JsonSseEvent> {
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

fn response_id(output: &CursorAgentOutput) -> String {
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

fn cursor_agent_args(
    request: &CursorAgentRequest,
    workspace: &Path,
    output_format: &str,
) -> Vec<String> {
    let mut args = vec![
        "--print".to_owned(),
        "--output-format".to_owned(),
        output_format.to_owned(),
        "--mode".to_owned(),
        "ask".to_owned(),
        "--sandbox".to_owned(),
        "enabled".to_owned(),
        "--trust".to_owned(),
        "--workspace".to_owned(),
        workspace.display().to_string(),
    ];
    if let Some(model) = cursor_agent_model_arg(&request.model) {
        args.push("--model".to_owned());
        args.push(model);
    }
    args.push(request.prompt.clone());
    args
}

fn redacted_cursor_agent_args(args: &[String]) -> Vec<String> {
    args.iter()
        .enumerate()
        .map(|(index, value)| {
            if index + 1 == args.len() {
                "<prompt>".to_owned()
            } else {
                value.clone()
            }
        })
        .collect()
}

fn cursor_agent_model_arg(model: &str) -> Option<String> {
    let model = model.strip_prefix("cursor/").unwrap_or(model).trim();
    (!model.is_empty() && model != "auto").then(|| model.to_owned())
}

fn cursor_agent_executable() -> PathBuf {
    if let Some(path) = std::env::var_os(CURSOR_AGENT_ENV).filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let candidate = PathBuf::from(home).join(".local/bin/cursor-agent");
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from("cursor-agent")
}

fn cursor_agent_workspace() -> Result<PathBuf> {
    let workspace = std::env::var_os(CURSOR_AGENT_WORKSPACE_ENV)
        .filter(|value| !value.is_empty())
        .map_or_else(
            || std::env::temp_dir().join("rotom-cursor-agent-workspace"),
            PathBuf::from,
        );
    std::fs::create_dir_all(&workspace)?;
    Ok(workspace)
}

fn chat_completion_id() -> String {
    format!("chatcmpl-{}-{:08x}", now_unix(), rand::random::<u32>())
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn builds_text_prompt_from_responses_body() {
        let body = json!({
            "model": "cursor/sonnet-4",
            "instructions": "Be concise.",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hello"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Hi"}]},
                {"type": "function_call_output", "call_id": "call_1", "output": "42"}
            ]
        });

        let request = CursorAgentRequest::from_body(&body).unwrap();

        assert_eq!(request.model, "cursor/sonnet-4");
        assert_eq!(
            request.prompt,
            "System:\nBe concise.\n\nUser:\nHello\n\nAssistant:\nHi\n\nTool result call_1:\n42"
        );
    }

    #[test]
    fn rejects_cursor_tools_without_none_choice() {
        let body = json!({
            "model": "cursor/auto",
            "input": [{"type": "message", "role": "user", "content": "Hello"}],
            "tool_choice": "auto",
            "tools": [{"type": "function", "name": "lookup"}]
        });

        let error = CursorAgentRequest::from_body(&body).unwrap_err();

        assert!(error.to_string().contains("client-supplied tools"));
    }

    #[test]
    fn allows_tools_when_tool_choice_is_none() {
        let body = json!({
            "model": "cursor/auto",
            "input": [{"type": "message", "role": "user", "content": "Hello"}],
            "tool_choice": "none",
            "tools": [{"type": "function", "name": "lookup"}]
        });

        let request = CursorAgentRequest::from_body(&body).unwrap();

        assert_eq!(request.prompt, "User:\nHello");
    }

    #[test]
    fn rejects_multimodal_cursor_content() {
        let body = json!({
            "model": "cursor/auto",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_image", "image_url": "data:image/png;base64,AA=="}]
            }]
        });

        let error = CursorAgentRequest::from_body(&body).unwrap_err();

        assert!(error.to_string().contains("multimodal inputs"));
    }

    #[test]
    fn builds_cursor_agent_args_without_auto_model() {
        let request = CursorAgentRequest {
            model: "cursor/auto".to_owned(),
            prompt: "Hello".to_owned(),
        };
        let args = cursor_agent_args(&request, Path::new("/tmp/rotom-cursor"), "json");

        assert!(!args.iter().any(|arg| arg == "--model"));
        assert_eq!(args.last().map(String::as_str), Some("Hello"));
    }

    #[test]
    fn builds_cursor_agent_args_with_stripped_model() {
        let request = CursorAgentRequest {
            model: "cursor/sonnet-4".to_owned(),
            prompt: "Hello".to_owned(),
        };
        let args = cursor_agent_args(&request, Path::new("/tmp/rotom-cursor"), "json");
        let model_index = args.iter().position(|arg| arg == "--model").unwrap();

        assert_eq!(
            args.get(model_index + 1).map(String::as_str),
            Some("sonnet-4")
        );
    }

    #[test]
    #[cfg(unix)]
    fn parses_cursor_agent_json_result() {
        let stdout = br#"{"type":"result","subtype":"success","is_error":false,"result":"OK","request_id":"req_1","usage":{"inputTokens":10,"outputTokens":2,"cacheReadTokens":3,"cacheWriteTokens":4}}"#;
        let output =
            parse_cursor_agent_output(std::process::ExitStatus::from_raw(0), stdout, b"").unwrap();

        assert_eq!(output.text, "OK");
        assert_eq!(output.request_id.as_deref(), Some("req_1"));
        assert_eq!(
            output.usage,
            Some(Usage {
                prompt_tokens: 17,
                completion_tokens: 2,
                total_tokens: 19
            })
        );
    }

    #[test]
    #[cfg(unix)]
    fn maps_non_json_auth_output_to_unauthorized() {
        let stdout = b"Your stored authentication is invalid. Please log in again.";
        let error = parse_cursor_agent_output(std::process::ExitStatus::from_raw(0), stdout, b"")
            .unwrap_err();

        assert_eq!(error.status_code(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn response_value_uses_openai_usage_shape() {
        let body = json!({"model": "cursor/gpt-5", "input": []});
        let output = CursorAgentOutput {
            text: "OK".to_owned(),
            usage: Some(Usage {
                prompt_tokens: 1,
                completion_tokens: 2,
                total_tokens: 3,
            }),
            request_id: Some("req_1".to_owned()),
        };

        let response = response_value(&body, &output);

        assert_eq!(response["id"], "resp_cursor_req_1");
        assert_eq!(response["output"][0]["content"][0]["text"], "OK");
        assert_eq!(response["usage"]["input_tokens"], 1);
    }
}
