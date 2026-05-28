use crate::{
    Error, Result,
    codex::{
        convert::to_codex_request,
        events::{
            collect_output, collect_response_value, event_error, event_tool_call, finish_reason,
            is_done_event, normalize_incomplete_result_finish_reason, response_has_usable_output,
            response_tool_calls, text_delta,
        },
        sse,
        upstream::{UpstreamProvider, adapter_for_provider},
    },
    config::{Credentials, Provider, now_unix},
    openai::response::{
        AssistantMessage, ChatChoice, ChatCompletionChunk, ChatCompletionResponse, chunk_finished,
        chunk_with_content, chunk_with_role, chunk_with_tool_call,
    },
};
use futures_util::{Stream, StreamExt};
use reqwest::{
    Client, Method, Response,
    header::{ACCEPT, HeaderValue},
};
use serde_json::Value;
use std::pin::Pin;

/// Default upstream base URL for `Codex` response requests.
pub use crate::codex::upstream::DEFAULT_CODEX_BASE_URL;
pub use crate::codex::upstream::{
    ResponseResourceCapabilities, ResponseResourceCapability, codex_headers, grok_headers,
    resolve_codex_url, resolve_grok_responses_url,
};

#[derive(Clone)]
/// HTTP client wrapper for the `ChatGPT` `Codex` responses backend.
pub struct CodexClient {
    http: Client,
    base_url: String,
    provider: Provider,
}

impl CodexClient {
    /// Creates a Codex client with the provided HTTP client and backend base URL.
    #[must_use]
    pub fn new(http: Client, base_url: impl Into<String>) -> Self {
        Self {
            http,
            base_url: base_url.into(),
            provider: Provider::Codex,
        }
    }

    /// Creates an upstream client for the selected provider.
    #[must_use]
    pub fn new_for_provider(http: Client, provider: Provider) -> Self {
        let base_url = adapter_for_provider(provider).default_base_url();
        Self::new_for_provider_base_url(http, provider, base_url)
    }

    /// Creates an upstream client for the selected provider and base URL.
    #[must_use]
    pub fn new_for_provider_base_url(
        http: Client,
        provider: Provider,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            http,
            base_url: base_url.into(),
            provider,
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

    /// Returns the upstream provider used by this client.
    #[must_use]
    pub const fn provider(&self) -> Provider {
        self.provider
    }

    /// Returns the provider's Responses resource lifecycle support matrix.
    #[must_use]
    pub fn response_resource_capabilities(&self) -> ResponseResourceCapabilities {
        self.upstream_provider().resource_capabilities()
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

        let usage = output.usage;
        let message = AssistantMessage {
            role: "assistant",
            content: output.text,
            tool_calls: (!output.tool_calls.is_empty()).then_some(output.tool_calls),
            images: (!output.images.is_empty()).then_some(output.images),
        };

        Ok(ChatCompletionResponse {
            id,
            object: "chat.completion",
            created,
            model,
            choices: vec![ChatChoice {
                index: 0,
                message,
                finish_reason: output.finish_reason,
            }],
            usage,
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
            let mut saw_usable_output = false;

            while let Some(event) = events.next().await {
                let event = event?;
                if let Some(message) = event_error(&event) {
                    Err(Error::upstream(message))?;
                }
                if let Some(delta) = text_delta(&event) {
                    if !delta.is_empty() {
                        saw_usable_output = true;
                        yield chunk_with_content(&id, created, &model, delta);
                    }
                }
                if let Some(tool_call) = event_tool_call(&event) {
                    if seen_tool_call_ids.insert(tool_call.id.clone()) {
                        // The SSE stream can repeat tool calls across incremental and completed events.
                        yield chunk_with_tool_call(&id, created, &model, tool_call_count, tool_call);
                        tool_call_count += 1;
                        saw_usable_output = true;
                    }
                }
                if is_done_event(&event) {
                    // Some tool calls appear only on the terminal completed event.
                    for tool_call in response_tool_calls(&event) {
                        if seen_tool_call_ids.insert(tool_call.id.clone()) {
                            yield chunk_with_tool_call(&id, created, &model, tool_call_count, tool_call);
                            tool_call_count += 1;
                            saw_usable_output = true;
                        }
                    }
                    if event
                        .get("response")
                        .is_some_and(response_has_usable_output)
                    {
                        saw_usable_output = true;
                    }
                    finished = true;
                    let reason = if tool_call_count > 0 {
                        "tool_calls".to_owned()
                    } else {
                        normalize_incomplete_result_finish_reason(
                            finish_reason(&event),
                            saw_usable_output,
                        )
                    };
                    yield chunk_finished(&id, created, &model, &reason);
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
    ///
    /// # Errors
    ///
    /// Returns an error when the upstream request fails or the response stream
    /// cannot be collected into one final response value.
    pub async fn complete_response(
        &self,
        request: Value,
        credentials: &Credentials,
    ) -> Result<Value> {
        let response = self.send_body(&request, credentials).await?;
        collect_response_value(response).await
    }

    /// Sends a streaming Responses-style request body and yields raw upstream JSON SSE events.
    ///
    /// # Errors
    ///
    /// Returns an error when the upstream request cannot be started.
    pub async fn stream_response(
        &self,
        request: Value,
        credentials: &Credentials,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<crate::codex::sse::JsonSseEvent>> + Send>>> {
        let response = self.send_body(&request, credentials).await?;
        Ok(Box::pin(sse::json_named_events(Box::pin(
            response.bytes_stream(),
        ))))
    }

    /// Retrieves an upstream-stored Responses API object when the provider supports it.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider does not expose retrieval or the
    /// upstream request fails.
    pub async fn retrieve_response(
        &self,
        response_id: &str,
        credentials: &Credentials,
    ) -> Result<Value> {
        let response = self
            .send_response_resource(Method::GET, response_id, credentials)
            .await?;
        response.json::<Value>().await.map_err(Into::into)
    }

    /// Deletes an upstream-stored Responses API object when the provider supports it.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider does not expose deletion or the
    /// upstream request fails.
    pub async fn delete_response(
        &self,
        response_id: &str,
        credentials: &Credentials,
    ) -> Result<Value> {
        let response = self
            .send_response_resource(Method::DELETE, response_id, credentials)
            .await?;
        let status = response.status();
        let text = response.text().await?;
        if text.trim().is_empty() {
            Ok(serde_json::json!({
                "id": response_id,
                "object": "response",
                "deleted": status.is_success()
            }))
        } else {
            serde_json::from_str(&text).map_err(Into::into)
        }
    }

    async fn send_chat(
        &self,
        request: &crate::openai::types::ChatCompletionRequest,
        credentials: &Credentials,
    ) -> Result<Response> {
        self.send_body(&to_codex_request(request)?, credentials)
            .await
    }

    async fn send_body(&self, body: &Value, credentials: &Credentials) -> Result<Response> {
        let mut upstream_body = body.clone();
        let upstream = self.upstream_provider();
        upstream.prepare_request(&mut upstream_body);
        crate::logging::trace_json("upstream.request", &upstream_body);
        let url = upstream.responses_url(&self.base_url);
        let response = self
            .http
            .post(&url)
            .headers(upstream.headers(credentials)?)
            .json(&upstream_body)
            .send()
            .await?;
        tracing::trace!(
            event = "upstream.response_started",
            url = %url,
            status = response.status().as_u16()
        );
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(parse_error_response(response, upstream.provider()).await)
        }
    }

    async fn send_response_resource(
        &self,
        method: Method,
        response_id: &str,
        credentials: &Credentials,
    ) -> Result<Response> {
        let upstream = self.upstream_provider();
        let Some(url) = upstream.response_resource_url(&self.base_url, response_id) else {
            return Err(unsupported_response_resource(
                upstream.provider(),
                method.as_str(),
            ));
        };
        let mut headers = upstream.headers(credentials)?;
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        let response = self
            .http
            .request(method, &url)
            .headers(headers)
            .send()
            .await?;
        tracing::trace!(
            event = "upstream.response_resource_started",
            url = %url,
            status = response.status().as_u16()
        );
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(parse_error_response(response, upstream.provider()).await)
        }
    }

    fn upstream_provider(&self) -> &'static dyn UpstreamProvider {
        adapter_for_provider(self.provider)
    }
}

fn unsupported_response_resource(provider: Provider, operation: &str) -> Error {
    Error::upstream_with_status(
        reqwest::StatusCode::NOT_IMPLEMENTED,
        format!(
            "{} upstream does not support Responses resource {operation}",
            provider.display_name()
        ),
    )
}

async fn parse_error_response(response: Response, provider: Provider) -> Error {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    crate::logging::trace_text("upstream.codex.error_body", &text);
    // Prefer the structured upstream error message when the backend provides one.
    let message = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.pointer("/detail"))
                .or_else(|| value.pointer("/message"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or(text);

    let downstream_status = if status.is_client_error() {
        status
    } else {
        reqwest::StatusCode::BAD_GATEWAY
    };

    Error::upstream_with_status(
        downstream_status,
        format!(
            "{} backend returned {status}: {message}",
            provider.display_name()
        ),
    )
}

#[must_use]
fn chat_completion_id() -> String {
    format!("chatcmpl-{}-{:08x}", now_unix(), rand::random::<u32>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_upstream_provider_adapter() {
        let http = Client::new();
        let codex = CodexClient::new_for_provider(http.clone(), Provider::Codex);
        let grok = CodexClient::new_for_provider(http, Provider::Grok);

        assert_eq!(codex.upstream_provider().provider(), Provider::Codex);
        assert_eq!(grok.upstream_provider().provider(), Provider::Grok);
    }
}
