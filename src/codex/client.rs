use crate::{
    Error, Result,
    codex::{
        convert::to_codex_request,
        events::{
            collect_output, collect_response_value, event_error, event_tool_call, finish_reason, is_done_event,
            response_tool_calls, text_delta,
        },
        sse,
    },
    config::{Credentials, now_unix},
    openai::response::{
        AssistantMessage, ChatChoice, ChatCompletionChunk, ChatCompletionResponse, chunk_finished,
        chunk_with_content, chunk_with_role, chunk_with_tool_call,
    },
};
use futures_util::{Stream, StreamExt};
use reqwest::{
    Client, Response,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT},
};
use serde_json::Value;
use std::pin::Pin;

/// Default upstream base URL for `Codex` response requests.
pub const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";

#[derive(Clone)]
/// HTTP client wrapper for the `ChatGPT` `Codex` responses backend.
pub struct CodexClient {
    http: Client,
    base_url: String,
}

impl CodexClient {
    /// Creates a Codex client with the provided HTTP client and backend base URL.
    #[must_use]
    pub fn new(http: Client, base_url: impl Into<String>) -> Self {
        Self {
            http,
            base_url: base_url.into(),
        }
    }

    /// Returns the default upstream base URL used by the client.
    #[must_use]
    pub const fn default_base_url() -> &'static str {
        DEFAULT_CODEX_BASE_URL
    }

    /// Returns the configured upstream base URL.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Sends a non-streaming chat completion request and collects the full response body.
    ///
    /// # Errors
    ///
    /// Returns an error when the upstream request fails or the Codex response
    /// stream cannot be collected into a final completion payload.
    pub async fn complete_chat(
        &self,
        request: crate::openai::types::ChatCompletionRequest,
        credentials: &Credentials,
    ) -> Result<ChatCompletionResponse> {
        let id = chat_completion_id();
        let created = now_unix();
        let model = request.model.clone();
        let response = self.send_chat(&request, credentials).await?;
        let output = collect_output(response).await?;

        Ok(ChatCompletionResponse {
            id,
            object: "chat.completion",
            created,
            model,
            choices: vec![ChatChoice {
                index: 0,
                message: AssistantMessage {
                    role: "assistant",
                    content: if output.text.is_empty() && !output.tool_calls.is_empty() {
                        None
                    } else {
                        Some(output.text)
                    },
                    tool_calls: (!output.tool_calls.is_empty()).then_some(output.tool_calls),
                    images: (!output.images.is_empty()).then_some(output.images),
                },
                finish_reason: output.finish_reason,
            }],
            usage: output.usage,
        })
    }

    /// Sends a streaming chat completion request and yields OpenAI-compatible chunks.
    ///
    /// # Errors
    ///
    /// Returns an error when the upstream request fails before the response
    /// stream can be handed back to the caller.
    pub async fn stream_chat(
        &self,
        request: crate::openai::types::ChatCompletionRequest,
        credentials: &Credentials,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk>> + Send>>> {
        let id = chat_completion_id();
        let created = now_unix();
        let model = request.model.clone();
        let response = self.send_chat(&request, credentials).await?;
        let mut events = Box::pin(sse::json_events(Box::pin(response.bytes_stream())));

        let stream = async_stream::try_stream! {
            yield chunk_with_role(&id, created, &model);
            let mut finished = false;
            let mut tool_call_count = 0_u32;
            let mut seen_tool_call_ids = std::collections::HashSet::<String>::new();

            while let Some(event) = events.next().await {
                let event = event?;
                if let Some(message) = event_error(&event) {
                    Err(Error::upstream(message))?;
                }
                if let Some(delta) = text_delta(&event) {
                    if !delta.is_empty() {
                        yield chunk_with_content(&id, created, &model, delta);
                    }
                }
                if let Some(tool_call) = event_tool_call(&event) {
                    if seen_tool_call_ids.insert(tool_call.id.clone()) {
                        // The SSE stream can repeat tool calls across incremental and completed events.
                        yield chunk_with_tool_call(&id, created, &model, tool_call_count, tool_call);
                        tool_call_count += 1;
                    }
                }
                if is_done_event(&event) {
                    // Some tool calls appear only on the terminal completed event.
                    for tool_call in response_tool_calls(&event) {
                        if seen_tool_call_ids.insert(tool_call.id.clone()) {
                            yield chunk_with_tool_call(&id, created, &model, tool_call_count, tool_call);
                            tool_call_count += 1;
                        }
                    }
                    finished = true;
                    let reason = if tool_call_count > 0 {
                        "tool_calls"
                    } else {
                        finish_reason(&event)
                    };
                    yield chunk_finished(&id, created, &model, reason);
                    break;
                }
            }

            if !finished {
                yield chunk_finished(&id, created, &model, "stop");
            }
        };

        Ok(Box::pin(stream))
    }

    /// Sends a non-streaming Responses-style request body and returns the final response envelope.
    pub async fn complete_response(
        &self,
        request: Value,
        credentials: &Credentials,
    ) -> Result<Value> {
        let response = self.send_body(&request, credentials).await?;
        collect_response_value(response).await
    }

    /// Sends a streaming Responses-style request body and yields raw upstream JSON SSE events.
    pub async fn stream_response(
        &self,
        request: Value,
        credentials: &Credentials,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<crate::codex::sse::JsonSseEvent>> + Send>>> {
        let response = self.send_body(&request, credentials).await?;
        Ok(Box::pin(sse::json_named_events(Box::pin(response.bytes_stream()))))
    }

    async fn send_chat(
        &self,
        request: &crate::openai::types::ChatCompletionRequest,
        credentials: &Credentials,
    ) -> Result<Response> {
        self.send_body(&to_codex_request(request), credentials).await
    }

    async fn send_body(&self, body: &Value, credentials: &Credentials) -> Result<Response> {
        let response = self
            .http
            .post(resolve_codex_url(&self.base_url))
            .headers(codex_headers(credentials)?)
            .json(body)
            .send()
            .await?;
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(parse_error_response(response).await)
        }
    }
}

/// Resolves a configured base URL into the concrete Codex responses endpoint.
#[must_use]
pub fn resolve_codex_url(base_url: &str) -> String {
    let normalized = base_url.trim_end_matches('/');
    if normalized.ends_with("/codex/responses") {
        normalized.to_owned()
    } else if normalized.ends_with("/codex") {
        format!("{normalized}/responses")
    } else {
        format!("{normalized}/codex/responses")
    }
}

/// Builds the required HTTP headers for authenticated Codex backend requests.
///
/// # Errors
///
/// Returns an error when any derived header value is invalid.
pub fn codex_headers(credentials: &Credentials) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        header_value(&format!("Bearer {}", credentials.access_token))?,
    );
    headers.insert(
        HeaderName::from_static("chatgpt-account-id"),
        header_value(&credentials.account_id)?,
    );
    headers.insert(
        HeaderName::from_static("originator"),
        HeaderValue::from_static("pi"),
    );
    headers.insert(USER_AGENT, HeaderValue::from_static("pi (rust; codexia)"));
    headers.insert(
        HeaderName::from_static("openai-beta"),
        HeaderValue::from_static("responses=experimental"),
    );
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(headers)
}

async fn parse_error_response(response: Response) -> Error {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    // Prefer the structured upstream error message when the backend provides one.
    let message = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or(text);

    Error::upstream(format!("Codex backend returned {status}: {message}"))
}

fn header_value(value: &str) -> Result<HeaderValue> {
    HeaderValue::from_str(value).map_err(|_| Error::config("invalid header value"))
}

#[must_use]
fn chat_completion_id() -> String {
    format!("chatcmpl-{}-{:08x}", now_unix(), rand::random::<u32>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Credentials;

    #[test]
    fn resolves_codex_url_variants() {
        assert_eq!(
            resolve_codex_url("https://chatgpt.com/backend-api"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_codex_url("https://example.com/codex"),
            "https://example.com/codex/responses"
        );
        assert_eq!(
            resolve_codex_url("https://example.com/codex/responses"),
            "https://example.com/codex/responses"
        );
    }

    #[test]
    fn builds_required_codex_headers() {
        let credentials = Credentials {
            access_token: "token".into(),
            refresh_token: "refresh".into(),
            expires_at: 1,
            account_id: "acc".into(),
        };
        let headers = codex_headers(&credentials).unwrap();

        assert_eq!(headers["authorization"], "Bearer token");
        assert_eq!(headers["chatgpt-account-id"], "acc");
        assert_eq!(headers["openai-beta"], "responses=experimental");
    }
}
