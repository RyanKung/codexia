//! HTTP server state, router wiring, and endpoint tests.

use crate::{
    codex::{client::CodexClient, convert::normalize_model},
    config::Provider,
    error::Result,
    models::provider_for_model,
    openai::response::ModelList,
    status::StatusClient,
    token::TokenManager,
};
use axum::{
    Router,
    extract::Request,
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
};
use reqwest::Client;
use std::{net::SocketAddr, sync::Arc};
use tokio::net::TcpListener;

mod auth;
mod handlers;
mod status_response;
mod store;

pub use auth::authorize;

/// Shared application state used by all HTTP route handlers.
#[derive(Clone)]
pub struct AppState {
    upstreams: Arc<Vec<UpstreamState>>,
    status: StatusClient,
    api_key: Option<Arc<str>>,
    models: ModelList,
    model_fallback: Option<Arc<str>>,
    responses: store::ResponseStore,
    batches: store::BatchStore,
}

#[derive(Clone)]
/// Runtime state for one authenticated upstream provider.
pub struct UpstreamState {
    /// Provider served by this upstream.
    pub provider: Provider,
    /// Token manager for this provider.
    pub token_manager: TokenManager,
    /// HTTP client for this provider.
    pub client: CodexClient,
}

impl AppState {
    /// Builds application state from the configured token manager, upstream client, and model list.
    #[must_use]
    pub fn new(
        token_manager: TokenManager,
        codex: CodexClient,
        api_key: Option<String>,
        models: ModelList,
    ) -> Self {
        Self::new_with_model_fallback(token_manager, codex, api_key, models, None)
    }

    /// Builds application state and optionally enables Anthropic model fallback.
    #[must_use]
    pub fn new_with_model_fallback(
        token_manager: TokenManager,
        codex: CodexClient,
        api_key: Option<String>,
        models: ModelList,
        model_fallback: Option<String>,
    ) -> Self {
        Self::new_multi_with_model_fallback(
            vec![UpstreamState {
                provider: codex.provider(),
                token_manager,
                client: codex,
            }],
            api_key,
            models,
            model_fallback,
        )
    }

    /// Builds application state from all configured upstream providers.
    ///
    /// # Panics
    ///
    /// Panics when called with an empty upstream list.
    #[must_use]
    pub fn new_multi_with_model_fallback(
        upstreams: Vec<UpstreamState>,
        api_key: Option<String>,
        models: ModelList,
        model_fallback: Option<String>,
    ) -> Self {
        let codex = upstreams
            .iter()
            .find(|upstream| upstream.provider == Provider::Codex)
            .or_else(|| upstreams.first())
            .expect("at least one upstream provider is required");
        let status = StatusClient::new(Client::new(), codex.client.base_url().to_owned());
        Self {
            upstreams: Arc::new(upstreams),
            status,
            api_key: api_key.map(Arc::from),
            models,
            model_fallback: model_fallback.map(Arc::from),
            responses: store::ResponseStore::default(),
            batches: store::BatchStore::default(),
        }
    }

    /// Rewrites known unsupported Anthropic model ids to the configured fallback.
    pub(crate) fn rewrite_model(&self, model: &mut String) {
        let Some(fallback) = self.model_fallback.as_deref() else {
            return;
        };

        let normalized = normalize_model(model);
        if self.supports_model(&normalized) || !looks_like_anthropic_model(&normalized) {
            return;
        }

        if !self.supports_model(fallback) {
            tracing::warn!(
                requested_model = %model,
                fallback_model = %fallback,
                "model fallback ignored"
            );
            return;
        }

        tracing::warn!(
            requested_model = %model,
            fallback_model = %fallback,
            "model fallback applied"
        );
        fallback.clone_into(model);
    }

    fn supports_model(&self, candidate: &str) -> bool {
        self.models
            .data
            .iter()
            .any(|model| normalize_model(&model.id) == candidate)
    }

    pub(crate) fn upstream_for_model(&self, model: &str) -> Option<&UpstreamState> {
        let provider = provider_for_model(model);
        self.upstreams
            .iter()
            .find(|upstream| upstream.provider == provider)
            .or_else(|| {
                (provider == Provider::Codex)
                    .then(|| self.upstreams.first())
                    .flatten()
            })
    }

    pub(crate) fn default_upstream(&self) -> Option<&UpstreamState> {
        self.upstreams.first()
    }
}

fn looks_like_anthropic_model(model: &str) -> bool {
    model.starts_with("claude-")
        || model.eq_ignore_ascii_case("sonnet")
        || model.eq_ignore_ascii_case("opus")
        || model.eq_ignore_ascii_case("haiku")
        || model.contains("sonnet")
        || model.contains("opus")
        || model.contains("haiku")
}

/// Binds the local listener and serves the Axum router until shutdown.
///
/// # Errors
///
/// Returns an error when binding the socket or serving the router fails.
pub async fn serve(addr: SocketAddr, state: AppState) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, router(state)).await?;
    Ok(())
}

/// Builds the HTTP router for all supported endpoints.
pub fn router(state: AppState) -> Router {
    // Keep route registration centralized so the server surface stays easy to audit.
    Router::new()
        .route("/health", get(handlers::health))
        .route("/v1/auth/refresh", post(handlers::manual_refresh))
        .route("/v1/status", get(handlers::status))
        .route("/v1/models", get(handlers::models))
        .route("/v1/responses", post(handlers::responses))
        .route("/v1/responses/compact", post(handlers::compact_response))
        .route(
            "/v1/responses/input_tokens",
            post(handlers::count_response_input_tokens),
        )
        .route("/v1/messages", post(handlers::messages))
        .route(
            "/v1/messages/count_tokens",
            post(handlers::count_message_tokens),
        )
        .route(
            "/v1/messages/batches",
            get(handlers::list_message_batches).post(handlers::create_message_batch),
        )
        .route(
            "/v1/messages/batches/{batch_id}",
            get(handlers::get_message_batch).delete(handlers::delete_message_batch),
        )
        .route(
            "/v1/messages/batches/{batch_id}/cancel",
            post(handlers::cancel_message_batch),
        )
        .route(
            "/v1/messages/batches/{batch_id}/results",
            get(handlers::message_batch_results),
        )
        .route("/v1/chat/completions", post(handlers::chat_completions))
        .route("/v1/images/generations", post(handlers::image_generations))
        .layer(middleware::from_fn(log_request_summary))
        .with_state(state)
}

async fn log_request_summary(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let response = next.run(request).await;
    tracing::debug!(
        %method,
        path = %path,
        status = response.status().as_u16(),
        "http_request"
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Error,
        config::{AuthStore, Credentials, Provider, now_unix},
        oauth::CodexOAuthClient,
        openai::response::ModelList,
        testsupport::TempDir,
    };
    use axum::{
        Json,
        body::to_bytes,
        extract::{Form, State},
        http::{HeaderMap, HeaderValue, StatusCode, header::HOST},
        response::IntoResponse,
        routing::{get, post},
    };
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use reqwest::Client;
    use serde_json::{Value, json};
    use std::{collections::HashMap, sync::Arc};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;
    use tokio::time::{Duration, sleep};

    fn jwt_with_account_id(account_id: &str) -> String {
        let encoded = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({
                "https://api.openai.com/auth": { "chatgpt_account_id": account_id }
            }))
            .unwrap(),
        );
        format!("header.{encoded}.sig")
    }

    async fn refresh_handler(Form(form): Form<HashMap<String, String>>) -> Json<Value> {
        assert_eq!(form.get("refresh_token").unwrap(), "old_refresh");
        Json(json!({
            "access_token": jwt_with_account_id("acc_refreshed"),
            "refresh_token": "new_refresh",
            "expires_in": 3600
        }))
    }

    async fn spawn_refresh_server() -> String {
        let app = Router::new().route("/token", post(refresh_handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        url
    }

    async fn account_handler() -> Json<Value> {
        Json(json!({
            "accounts": {
                "default": {
                    "account": {
                        "name": "Personal",
                        "structure": "personal"
                    },
                    "entitlement": {
                        "subscription_plan": "chatgptplus",
                        "has_active_subscription": true,
                        "expires_at": "2026-05-01T00:00:00Z"
                    }
                }
            }
        }))
    }

    async fn usage_handler() -> Json<Value> {
        Json(json!({
            "email": "test@example.com",
            "plan_type": "pro",
            "rate_limit": {
                "primary_window": {
                    "used_percent": 10,
                    "reset_at": "2026-04-27T12:00:00Z"
                },
                "secondary_window": {
                    "remaining_percent": 90,
                    "reset_at": "2026-05-01T00:00:00Z"
                }
            },
            "credits": { "balance": 1 }
        }))
    }

    async fn spawn_status_server() -> String {
        let app = Router::new()
            .route("/accounts/check/v4-2023-04-27", get(account_handler))
            .route("/wham/usage", get(usage_handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        url
    }

    async fn codex_complete_handler() -> impl axum::response::IntoResponse {
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"OK\"}]}],\"usage\":{\"input_tokens\":12,\"output_tokens\":5,\"total_tokens\":17}}}\n\n",
        )
    }

    async fn codex_tool_call_handler() -> impl axum::response::IntoResponse {
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"lookup\",\"arguments\":\"{\\\"q\\\":\\\"x\\\"}\"}],\"usage\":{\"input_tokens\":7,\"output_tokens\":3,\"total_tokens\":10}}}\n\n",
        )
    }

    async fn codex_image_stream_handler() -> impl axum::response::IntoResponse {
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "event: response.created\n",
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_upstream\",\"model\":\"gpt-5.5\",\"output\":[]}}\n\n",
                "event: response.completed\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_upstream\",\"model\":\"gpt-5.5\",\"output\":[{\"type\":\"image_generation_call\",\"id\":\"ig_1\",\"result\":\"YWJj\",\"output_format\":\"png\",\"revised_prompt\":\"refined prompt\"}],\"usage\":{\"input_tokens\":12,\"output_tokens\":5,\"total_tokens\":17}}}\n\n"
            ),
        )
    }

    async fn codex_tool_stream_handler() -> impl axum::response::IntoResponse {
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "event: response.output_item.added\n",
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"lookup\",\"arguments\":\"\"}}\n\n",
                "event: response.function_call_arguments.delta\n",
                "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"item_id\":\"fc_1\",\"delta\":\"{\\\"q\\\":\\\"x\\\"}\"}\n\n",
                "event: response.output_item.done\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"lookup\",\"arguments\":\"{\\\"q\\\":\\\"x\\\"}\"}}\n\n",
                "event: response.completed\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_tool_stream\",\"model\":\"gpt-5.5\",\"status\":\"completed\",\"output\":[{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"lookup\",\"arguments\":\"{\\\"q\\\":\\\"x\\\"}\"}],\"usage\":{\"input_tokens\":7,\"output_tokens\":3,\"total_tokens\":10}}}\n\n"
            ),
        )
    }

    async fn codex_reasoning_handler() -> impl axum::response::IntoResponse {
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_reasoning\",\"model\":\"gpt-5.5\",\"status\":\"completed\",\"output\":[{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"work\"}],\"encrypted_content\":\"sig\"},{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"OK\"}]}],\"usage\":{\"input_tokens\":12,\"output_tokens\":5,\"total_tokens\":17}}}\n\n",
        )
    }

    async fn codex_reasoning_stream_handler() -> impl axum::response::IntoResponse {
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "event: ping\n",
                "data: {\"type\":\"ping\"}\n\n",
                "event: response.reasoning_text.delta\n",
                "data: {\"type\":\"response.reasoning_text.delta\",\"output_index\":0,\"item_id\":\"rs_1\",\"content_index\":0,\"delta\":\"step\"}\n\n",
                "event: response.output_item.done\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"step\"}],\"encrypted_content\":\"sig\"}}\n\n",
                "event: response.output_text.delta\n",
                "data: {\"type\":\"response.output_text.delta\",\"output_index\":1,\"item_id\":\"msg_1\",\"content_index\":0,\"delta\":\"OK\"}\n\n",
                "event: response.output_text.done\n",
                "data: {\"type\":\"response.output_text.done\",\"output_index\":1,\"item_id\":\"msg_1\",\"content_index\":0,\"text\":\"OK\"}\n\n",
                "event: response.completed\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_reasoning_stream\",\"model\":\"gpt-5.5\",\"status\":\"completed\",\"output\":[{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"step\"}],\"encrypted_content\":\"sig\"},{\"type\":\"message\",\"id\":\"msg_1\",\"content\":[{\"type\":\"output_text\",\"text\":\"OK\"}]}],\"usage\":{\"input_tokens\":12,\"output_tokens\":5,\"total_tokens\":17}}}\n\n"
            ),
        )
    }

    async fn codex_cache_usage_handler() -> impl axum::response::IntoResponse {
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"OK\"}]}],\"usage\":{\"input_tokens\":12,\"cache_creation_input_tokens\":100,\"cache_read_input_tokens\":200,\"output_tokens\":5,\"total_tokens\":317,\"server_tool_use\":{\"web_search_requests\":1}}}}\n\n",
        )
    }

    async fn codex_cache_usage_stream_handler() -> impl axum::response::IntoResponse {
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "event: response.output_text.delta\n",
                "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"OK\"}\n\n",
                "event: response.output_text.done\n",
                "data: {\"type\":\"response.output_text.done\",\"output_index\":0,\"text\":\"OK\"}\n\n",
                "event: response.completed\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_cache\",\"model\":\"gpt-5.5\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"OK\"}]}],\"usage\":{\"input_tokens\":12,\"cache_creation_input_tokens\":100,\"cache_read_input_tokens\":200,\"output_tokens\":5,\"total_tokens\":317,\"server_tool_use\":{\"web_search_requests\":1}}}}\n\n"
            ),
        )
    }

    async fn codex_null_output_stream_handler() -> impl axum::response::IntoResponse {
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "event: response.output_text.delta\n",
                "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"item_id\":\"msg_1\",\"content_index\":0,\"delta\":\"OK\"}\n\n",
                "event: response.output_text.done\n",
                "data: {\"type\":\"response.output_text.done\",\"output_index\":0,\"item_id\":\"msg_1\",\"content_index\":0,\"text\":\"OK\"}\n\n",
                "event: response.completed\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_null_output\",\"model\":\"gpt-5.5\",\"status\":\"completed\",\"output\":null,\"usage\":{\"input_tokens\":12,\"output_tokens\":5,\"total_tokens\":17}}}\n\n"
            ),
        )
    }

    async fn codex_incomplete_handler() -> impl axum::response::IntoResponse {
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_incomplete\",\"model\":\"gpt-5.5\",\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"partial\"}]}],\"usage\":{\"input_tokens\":12,\"output_tokens\":5,\"total_tokens\":17}}}\n\n",
        )
    }

    async fn codex_incomplete_result_stream_handler() -> impl axum::response::IntoResponse {
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "event: response.output_text.delta\n",
                "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"item_id\":\"msg_1\",\"content_index\":0,\"delta\":\"OK\"}\n\n",
                "event: response.output_text.done\n",
                "data: {\"type\":\"response.output_text.done\",\"output_index\":0,\"item_id\":\"msg_1\",\"content_index\":0,\"text\":\"OK\"}\n\n",
                "event: response.incomplete\n",
                "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_incomplete_result\",\"model\":\"gpt-5.5\",\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"incomplete_result\"},\"output\":null,\"usage\":{\"input_tokens\":12,\"output_tokens\":5,\"total_tokens\":17}}}\n\n"
            ),
        )
    }

    async fn codex_bad_request_handler() -> impl axum::response::IntoResponse {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "detail": "The 'claude-opus-4-7' model is not supported when using Codex with a ChatGPT account."
            })),
        )
    }

    async fn spawn_codex_server(tool_call: bool) -> String {
        let app = if tool_call {
            Router::new().route("/codex/responses", post(codex_tool_call_handler))
        } else {
            Router::new().route("/codex/responses", post(codex_complete_handler))
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        url
    }

    async fn spawn_image_stream_codex_server() -> String {
        let app = Router::new().route("/codex/responses", post(codex_image_stream_handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        url
    }

    async fn spawn_tool_stream_codex_server() -> String {
        let app = Router::new().route("/codex/responses", post(codex_tool_stream_handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        url
    }

    async fn spawn_incomplete_codex_server() -> String {
        let app = Router::new().route("/codex/responses", post(codex_incomplete_handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        url
    }

    async fn spawn_incomplete_result_stream_codex_server() -> String {
        let app = Router::new().route(
            "/codex/responses",
            post(codex_incomplete_result_stream_handler),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        url
    }

    async fn spawn_reasoning_codex_server() -> String {
        let app = Router::new().route("/codex/responses", post(codex_reasoning_handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        url
    }

    async fn spawn_reasoning_stream_codex_server() -> String {
        let app = Router::new().route("/codex/responses", post(codex_reasoning_stream_handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        url
    }

    async fn spawn_bad_request_codex_server() -> String {
        let app = Router::new().route("/codex/responses", post(codex_bad_request_handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        url
    }

    async fn spawn_cache_usage_codex_server() -> String {
        let app = Router::new().route("/codex/responses", post(codex_cache_usage_handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        url
    }

    async fn spawn_cache_usage_stream_codex_server() -> String {
        let app = Router::new().route("/codex/responses", post(codex_cache_usage_stream_handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        url
    }

    async fn spawn_null_output_stream_codex_server() -> String {
        let app = Router::new().route("/codex/responses", post(codex_null_output_stream_handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        url
    }

    async fn delayed_codex_complete_handler() -> impl axum::response::IntoResponse {
        sleep(Duration::from_millis(100)).await;
        codex_complete_handler().await
    }

    async fn spawn_delayed_codex_server() -> String {
        let app = Router::new().route("/codex/responses", post(delayed_codex_complete_handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        url
    }

    async fn spawn_recording_codex_server(captured: Arc<Mutex<Vec<Value>>>) -> String {
        async fn recording_handler(
            State(captured): State<Arc<Mutex<Vec<Value>>>>,
            Json(body): Json<Value>,
        ) -> impl axum::response::IntoResponse {
            captured.lock().await.push(body);
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"OK\"}]}],\"usage\":{\"input_tokens\":12,\"output_tokens\":5,\"total_tokens\":17}}}\n\n",
            )
        }

        let app = Router::new()
            .route("/codex/responses", post(recording_handler))
            .with_state(captured);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        url
    }

    async fn spawn_strict_recording_codex_server(captured: Arc<Mutex<Vec<Value>>>) -> String {
        async fn strict_recording_handler(
            State(captured): State<Arc<Mutex<Vec<Value>>>>,
            Json(body): Json<Value>,
        ) -> impl axum::response::IntoResponse {
            if let Some(path) = find_forbidden_key_path(&body, "cache_control") {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "detail": format!("Unknown parameter: '{path}'.")
                    })),
                );
            }
            if let Some(path) = find_forbidden_key_path(&body, "max_output_tokens") {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "detail": format!("Unsupported parameter: {path}")
                    })),
                );
            }

            captured.lock().await.push(body);
            (
                StatusCode::OK,
                Json(json!({
                    "id": "resp_strict",
                    "model": "gpt-5.5",
                    "output": [{"type":"message","content":[{"type":"output_text","text":"OK"}]}],
                    "usage": {"input_tokens":12,"output_tokens":5,"total_tokens":17}
                })),
            )
        }

        let app = Router::new()
            .route("/codex/responses", post(strict_recording_handler))
            .with_state(captured);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        url
    }

    async fn spawn_stream_required_codex_server(captured: Arc<Mutex<Vec<Value>>>) -> String {
        async fn stream_required_handler(
            State(captured): State<Arc<Mutex<Vec<Value>>>>,
            Json(body): Json<Value>,
        ) -> impl axum::response::IntoResponse {
            captured.lock().await.push(body.clone());
            if body.get("stream").and_then(Value::as_bool) != Some(true) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "detail": "Stream must be set to true"
                    })),
                )
                    .into_response();
            }

            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_stream_required\",\"model\":\"gpt-5.5\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"OK\"}]}],\"usage\":{\"input_tokens\":12,\"output_tokens\":5,\"total_tokens\":17}}}\n\n",
            )
                .into_response()
        }

        let app = Router::new()
            .route("/codex/responses", post(stream_required_handler))
            .with_state(captured);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        url
    }

    fn assert_stream_events_in_order(stream: &str, events: &[&str]) {
        let mut cursor = 0;
        for event in events {
            let offset = stream[cursor..]
                .find(event)
                .unwrap_or_else(|| panic!("missing stream event {event} after byte {cursor}"));
            cursor += offset + event.len();
        }
    }

    fn find_forbidden_key_path(value: &Value, key: &str) -> Option<String> {
        match value {
            Value::Object(object) => {
                if object.contains_key(key) {
                    return Some(key.to_owned());
                }

                object.iter().find_map(|(name, nested)| {
                    find_forbidden_key_path(nested, key).map(|suffix| {
                        if suffix.starts_with('[') {
                            format!("{name}{suffix}")
                        } else {
                            format!("{name}.{suffix}")
                        }
                    })
                })
            }
            Value::Array(array) => array.iter().enumerate().find_map(|(index, nested)| {
                find_forbidden_key_path(nested, key).map(|suffix| {
                    if suffix.starts_with('[') {
                        format!("[{index}]{suffix}")
                    } else {
                        format!("[{index}].{suffix}")
                    }
                })
            }),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => None,
        }
    }

    fn test_state(store: AuthStore, token_url: String, api_key: Option<String>) -> AppState {
        let http = Client::new();
        AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), token_url),
            ),
            CodexClient::new(http, "http://codex.invalid"),
            api_key,
            ModelList::from_ids(["gpt-test"]),
        )
    }

    async fn wait_for_batch_to_finish(
        state: AppState,
        headers: HeaderMap,
        batch_id: String,
    ) -> Value {
        for _ in 0..50 {
            let response = handlers::get_message_batch(
                axum::extract::State(state.clone()),
                headers.clone(),
                axum::extract::Path(batch_id.clone()),
            )
            .await;
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let value = serde_json::from_slice::<Value>(&body).unwrap();
            if value["processing_status"] == "ended" {
                return value;
            }
            sleep(Duration::from_millis(20)).await;
        }

        panic!("batch did not finish in time");
    }

    #[test]
    fn authorizes_when_no_api_key_is_configured() {
        assert!(authorize(&HeaderMap::new(), None).is_ok());
    }

    #[test]
    fn authorizes_bearer_token() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret"),
        );

        assert!(authorize(&headers, Some("secret")).is_ok());
    }

    #[test]
    fn rejects_wrong_api_key() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("wrong"));

        assert!(matches!(
            authorize(&headers, Some("secret")),
            Err(Error::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn manual_refresh_refreshes_credentials_without_returning_tokens() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "old_access".into(),
                refresh_token: "old_refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let state = test_state(
            store.clone(),
            spawn_refresh_server().await,
            Some("secret".into()),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::manual_refresh(axum::extract::State(state), headers).await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice::<Value>(&body).unwrap();
        assert_eq!(value["providers"][0]["account_id"], "acc_refreshed");
        assert!(value["providers"][0].get("expires_at").is_some());
        assert!(value.get("access_token").is_none());
        assert!(value.get("refresh_token").is_none());

        let saved = store.load().unwrap().unwrap();
        assert_eq!(saved.refresh_token, "new_refresh");
        assert_eq!(saved.account_id, "acc_refreshed");
    }

    #[tokio::test]
    async fn manual_refresh_requires_api_key_when_configured() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        let state = test_state(store, spawn_refresh_server().await, Some("secret".into()));

        let response =
            handlers::manual_refresh(axum::extract::State(state), HeaderMap::new()).await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn status_returns_account_and_rate_limit_snapshot() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();

        let status_base_url = spawn_status_server().await;
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, status_base_url),
            Some("secret".into()),
            ModelList::from_ids(["gpt-test"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::status(axum::extract::State(state), headers).await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice::<Value>(&body).unwrap();
        assert_eq!(value["provider"], "codex");
        assert_eq!(value["account_id"], "acc_old");
        assert_eq!(value["account"]["plan"], "chatgptplus");
        assert!(value["account"]["subscription_expires_at_local"].is_string());
        assert!(value["account"]["subscription_remaining_seconds"].is_number());
        assert_eq!(value["rate_limits"][0]["name"], "5h");
        assert_eq!(value["rate_limits"][0]["remaining_percent"], 90.0);
        assert!(value["rate_limits"][0]["reset_at_local"].is_string());
        assert!(value["rate_limits"][0]["reset_in_seconds"].is_number());
        assert!(value["token"]["expires_at_local"].is_string());
        assert!(value["warnings"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn status_returns_authenticated_snapshot_for_unsupported_provider() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: Provider::Grok,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: String::new(),
            })
            .unwrap();

        let http = Client::new();
        let state = AppState::new_multi_with_model_fallback(
            vec![UpstreamState {
                provider: Provider::Grok,
                token_manager: TokenManager::new_for_provider(store, Provider::Grok, http.clone()),
                client: CodexClient::new_for_provider(http, Provider::Grok),
            }],
            Some("secret".into()),
            ModelList::from_ids(["grok-4.3"]),
            None,
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::status(axum::extract::State(state), headers).await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice::<Value>(&body).unwrap();
        assert_eq!(value["provider"], "grok");
        assert!(value["token"]["remaining_seconds"].is_number());
        assert!(value["account"].is_null());
        assert!(value["rate_limits"].as_array().unwrap().is_empty());
        assert_eq!(
            value["warnings"].as_array().unwrap(),
            &[
                json!("Grok account metadata support is not implemented"),
                json!("Grok rate-limit support is not implemented"),
            ]
        );
        assert!(value.get("expires_at").is_none());
    }

    #[tokio::test]
    async fn messages_returns_anthropic_message_response() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, spawn_codex_server(false).await),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::messages(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "max_tokens": 128,
                    "messages": [{"role": "user", "content": "hello"}]
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice::<Value>(&body).unwrap();
        assert_eq!(value["type"], "message");
        assert_eq!(value["role"], "assistant");
        assert_eq!(value["content"][0]["type"], "text");
        assert_eq!(value["content"][0]["text"], "OK");
        assert_eq!(value["stop_reason"], "end_turn");
        assert_eq!(value["usage"]["input_tokens"], 12);
    }

    #[tokio::test]
    async fn messages_returns_tool_use_blocks() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, spawn_codex_server(true).await),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::messages(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "max_tokens": 128,
                    "messages": [{"role": "user", "content": "hello"}]
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice::<Value>(&body).unwrap();
        assert_eq!(value["content"][0]["type"], "tool_use");
        assert_eq!(value["content"][0]["id"], "call_1");
        assert_eq!(value["content"][0]["name"], "lookup");
        assert_eq!(value["stop_reason"], "tool_use");
    }

    #[tokio::test]
    async fn messages_return_thinking_blocks_from_reasoning_output() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, spawn_reasoning_codex_server().await),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("extended-thinking-2025-05-14"),
        );

        let response = handlers::messages(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "max_tokens": 128,
                    "thinking": {"type": "enabled", "budget_tokens": 4096},
                    "messages": [{"role": "user", "content": "hello"}]
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice::<Value>(&body).unwrap();
        assert_eq!(value["content"][0]["type"], "thinking");
        assert_eq!(value["content"][0]["thinking"], "work");
        assert_eq!(value["content"][0]["signature"], "sig");
        assert_eq!(value["content"][1]["type"], "text");
    }

    #[tokio::test]
    async fn messages_preserve_cache_usage_fields() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, spawn_cache_usage_codex_server().await),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::messages(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "max_tokens": 128,
                    "messages": [{"role": "user", "content": "hello"}]
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice::<Value>(&body).unwrap();
        assert_eq!(value["usage"]["cache_creation_input_tokens"], 100);
        assert_eq!(value["usage"]["cache_read_input_tokens"], 200);
        assert_eq!(value["usage"]["server_tool_use"]["web_search_requests"], 1);
    }

    #[tokio::test]
    async fn count_message_tokens_returns_estimate() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        let state = test_state(store, spawn_refresh_server().await, Some("secret".into()));
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::count_message_tokens(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "messages": [{"role": "user", "content": "Reply with the single word OK"}]
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice::<Value>(&body).unwrap();
        assert!(value["input_tokens"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn count_message_tokens_include_documents_tools_and_thinking() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        let state = test_state(store, spawn_refresh_server().await, Some("secret".into()));
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("extended-thinking-2025-05-14"),
        );

        let response = handlers::count_message_tokens(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "thinking": {"type": "enabled", "budget_tokens": 2048},
                    "tools": [{"name": "lookup", "description": "fetch docs", "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}}}],
                    "messages": [{
                        "role": "user",
                        "content": [{"type": "document", "source": {"type": "text", "text": "document body"}}]
                    }]
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice::<Value>(&body).unwrap();
        assert!(value["input_tokens"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn responses_returns_response_object() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, spawn_codex_server(false).await),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::responses(
            axum::extract::State(state.clone()),
            headers.clone(),
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "input": "draw a cat",
                    "tools": [{"type":"image_generation","size":"1024x1024"}],
                    "tool_choice": {"type":"image_generation"}
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice::<Value>(&body).unwrap();
        assert_eq!(value["object"], "response");
        assert_eq!(value["status"], "completed");
        assert_eq!(value["output"][0]["type"], "message");
    }

    #[tokio::test]
    async fn responses_stream_image_generation_passthroughs_upstream_events() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, spawn_image_stream_codex_server().await),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::responses(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "stream": true,
                    "input": "draw a cat",
                    "tools": [{"type":"image_generation","size":"1024x1024"}],
                    "tool_choice": {"type":"image_generation"}
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("event: response.created"));
        assert!(text.contains("event: response.completed"));
        assert!(text.contains("\"type\":\"image_generation_call\""));
        assert!(text.contains("\"result\":\"YWJj\""));
    }

    #[tokio::test]
    async fn responses_stream_emits_text_output_item_lifecycle() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, spawn_cache_usage_stream_codex_server().await),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::responses(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "stream": true,
                    "input": "hello"
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert_stream_events_in_order(
            &text,
            &[
                "event: response.created",
                "event: response.output_item.added",
                "event: response.content_part.added",
                "event: response.output_text.delta",
                "event: response.output_text.done",
                "event: response.content_part.done",
                "event: response.output_item.done",
                "event: response.completed",
            ],
        );
        assert!(text.contains("\"type\":\"message\""));
        assert!(text.contains("\"role\":\"assistant\""));
        assert!(text.contains("\"status\":\"in_progress\""));
        assert!(text.contains("\"status\":\"completed\""));
        assert!(text.contains("\"text\":\"OK\""));
    }

    #[tokio::test]
    async fn responses_stream_tool_call_emits_function_call_lifecycle() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, spawn_codex_server(true).await),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::responses(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "stream": true,
                    "input": "look up x"
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert_stream_events_in_order(
            &text,
            &[
                "event: response.created",
                "event: response.output_item.added",
                "event: response.function_call_arguments.delta",
                "event: response.function_call_arguments.done",
                "event: response.output_item.done",
                "event: response.completed",
            ],
        );
        assert!(text.contains("\"type\":\"function_call\""));
        assert!(text.contains("\"name\":\"lookup\""));
        assert!(text.contains("\"arguments\":\"{\\\"q\\\":\\\"x\\\"}\""));
        assert!(text.contains("\"finish_reason\":\"tool_calls\""));
        assert!(!text.contains("event: response.content_part.added"));
        assert!(!text.contains("\"type\":\"message\""));
    }

    #[tokio::test]
    async fn responses_raw_stream_backfills_null_terminal_output() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, spawn_null_output_stream_codex_server().await),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::responses(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "stream": true,
                    "input": "hello",
                    "tools": [{"type":"image_generation","size":"1024x1024"}],
                    "tool_choice": {"type":"image_generation"}
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("event: response.completed"));
        assert!(!text.contains("\"output\":null"));
        assert!(text.contains("\"output\":["));
        assert!(text.contains("\"type\":\"message\""));
        assert!(text.contains("\"id\":\"msg_1\""));
        assert!(text.contains("\"text\":\"OK\""));
    }

    #[tokio::test]
    async fn responses_raw_stream_completes_incomplete_result_with_output() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, spawn_incomplete_result_stream_codex_server().await),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::responses(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "stream": true,
                    "input": "hello",
                    "tools": [{"type":"image_generation","size":"1024x1024"}],
                    "tool_choice": {"type":"image_generation"}
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("event: response.completed"));
        assert!(!text.contains("event: response.incomplete"));
        assert!(!text.contains("\"type\":\"response.incomplete\""));
        assert!(text.contains("\"status\":\"completed\""));
        assert!(!text.contains("\"incomplete_result\""));
        assert!(text.contains("\"text\":\"OK\""));
    }

    #[tokio::test]
    async fn responses_chat_stream_completes_incomplete_result_finish_reason() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, spawn_incomplete_result_stream_codex_server().await),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::responses(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "stream": true,
                    "input": "hello"
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("event: response.completed"));
        assert!(text.contains("\"finish_reason\":\"stop\""));
        assert!(!text.contains("\"incomplete_result\""));
        assert!(text.contains("\"text\":\"OK\""));
    }

    #[tokio::test]
    async fn responses_non_streaming_still_streams_upstream() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(
                http,
                spawn_stream_required_codex_server(captured.clone()).await,
            ),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::responses(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "stream": false,
                    "input": "hello"
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice::<Value>(&body).unwrap();
        assert_eq!(value["object"], "response");
        assert_eq!(value["status"], "completed");
        assert_eq!(value["output"][0]["type"], "message");
        let (captured_len, captured_stream) = {
            let captured = captured.lock().await;
            (captured.len(), captured[0]["stream"].clone())
        };
        assert_eq!(captured_len, 1);
        assert_eq!(captured_stream, true);
    }

    #[tokio::test]
    async fn messages_stream_image_generation_returns_anthropic_image_block_events() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, spawn_image_stream_codex_server().await),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::messages(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "stream": true,
                    "max_tokens": 256,
                    "messages": [{"role": "user", "content": "draw a cat"}],
                    "tools": [{"name":"image_generation"}]
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("event: message_start"));
        assert!(text.contains("event: content_block_start"));
        assert!(text.contains("\"type\":\"image\""));
        assert!(text.contains("\"media_type\":\"image/png\""));
        assert!(text.contains("\"data\":\"YWJj\""));
        assert!(text.contains("event: message_stop"));
    }

    #[tokio::test]
    async fn messages_stream_tool_use_returns_anthropic_tool_events() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, spawn_tool_stream_codex_server().await),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::messages(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "stream": true,
                    "max_tokens": 256,
                    "messages": [{"role": "user", "content": "look up x"}]
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("event: content_block_start"));
        assert!(text.contains("\"type\":\"tool_use\""));
        assert!(text.contains("\"type\":\"input_json_delta\""));
        assert!(text.contains("\"partial_json\":\"{\\\"q\\\":\\\"x\\\"}\""));
        assert!(text.contains("\"stop_reason\":\"tool_use\""));
        assert!(text.contains("event: message_stop"));
    }

    #[tokio::test]
    async fn messages_stream_reasoning_returns_thinking_events() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, spawn_reasoning_stream_codex_server().await),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("extended-thinking-2025-05-14"),
        );

        let response = handlers::messages(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "stream": true,
                    "max_tokens": 256,
                    "thinking": {"type": "enabled", "budget_tokens": 4096},
                    "messages": [{"role": "user", "content": "hello"}]
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("event: ping"));
        assert!(text.contains("\"type\":\"thinking\""));
        assert!(text.contains("\"type\":\"thinking_delta\""));
        assert_eq!(text.matches("\"thinking\":\"step\"").count(), 1);
        assert!(text.contains("\"type\":\"signature_delta\""));
        assert!(text.contains("event: message_stop"));
    }

    #[tokio::test]
    async fn messages_stream_preserve_cache_usage_fields() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, spawn_cache_usage_stream_codex_server().await),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::messages(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "stream": true,
                    "max_tokens": 256,
                    "messages": [{"role": "user", "content": "hello"}]
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("\"cache_creation_input_tokens\":100"));
        assert!(text.contains("\"cache_read_input_tokens\":200"));
        assert!(text.contains("\"web_search_requests\":1"));
    }

    #[tokio::test]
    async fn messages_preserve_upstream_client_errors() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, spawn_bad_request_codex_server().await),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::messages(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "claude-opus-4-7",
                    "max_tokens": 128,
                    "messages": [{"role": "user", "content": "hello"}]
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice::<Value>(&body).unwrap();
        assert_eq!(value["error"]["type"], "invalid_request_error");
        assert!(
            value["error"]["message"]
                .as_str()
                .unwrap()
                .contains("claude-opus-4-7")
        );
    }

    #[tokio::test]
    async fn messages_rewrite_known_anthropic_models_to_configured_fallback() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let http = Client::new();
        let state = AppState::new_with_model_fallback(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, spawn_recording_codex_server(captured.clone()).await),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
            Some("gpt-5.5".into()),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::messages(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "claude-sonnet-4-5",
                    "max_tokens": 128,
                    "messages": [{"role": "user", "content": "hello"}]
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let (captured_len, captured_model) = {
            let captured = captured.lock().await;
            (captured.len(), captured[0]["model"].clone())
        };
        assert_eq!(captured_len, 1);
        assert_eq!(captured_model, "gpt-5.5");
    }

    #[tokio::test]
    async fn messages_non_streaming_still_streams_upstream() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(
                http,
                spawn_stream_required_codex_server(captured.clone()).await,
            ),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::messages(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "stream": false,
                    "max_tokens": 128,
                    "messages": [{"role": "user", "content": "hello"}]
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice::<Value>(&body).unwrap();
        assert_eq!(value["type"], "message");
        assert_eq!(value["content"][0]["text"], "OK");
        let (captured_len, captured_stream) = {
            let captured = captured.lock().await;
            (captured.len(), captured[0]["stream"].clone())
        };
        assert_eq!(captured_len, 1);
        assert_eq!(captured_stream, true);
    }

    #[tokio::test]
    async fn responses_preserve_incomplete_status_and_reason() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, spawn_incomplete_codex_server().await),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::messages(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "max_tokens": 128,
                    "messages": [{"role": "user", "content": "hello"}]
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice::<Value>(&body).unwrap();
        assert_eq!(value["content"][0]["type"], "text");
        assert_eq!(value["content"][0]["text"], "partial");
        assert_eq!(value["stop_reason"], "max_tokens");
    }

    #[tokio::test]
    async fn models_return_anthropic_shape_when_requested() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        let state = test_state(store, spawn_refresh_server().await, Some("secret".into()));
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));

        let response = handlers::models(axum::extract::State(state), headers).await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice::<Value>(&body).unwrap();
        assert_eq!(value["data"][0]["type"], "model");
        assert!(value["data"][0]["display_name"].is_string());
    }

    #[tokio::test]
    async fn message_batches_can_be_retrieved_with_results() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, spawn_codex_server(false).await),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));
        headers.insert(HOST, HeaderValue::from_static("localhost:14550"));

        let response = handlers::create_message_batch(
            axum::extract::State(state.clone()),
            headers.clone(),
            Json(
                serde_json::from_value(json!({
                    "requests": [
                        {
                            "custom_id": "req_1",
                            "params": {
                                "model": "gpt-5.5",
                                "max_tokens": 32,
                                "messages": [{"role": "user", "content": "hello"}]
                            }
                        },
                        {
                            "custom_id": "req_2",
                            "params": {
                                "model": "gpt-5.5",
                                "max_tokens": 32,
                                "messages": [{"role": "user", "content": "again"}]
                            }
                        }
                    ]
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice::<Value>(&body).unwrap();
        assert_eq!(value["type"], "message_batch");
        assert_eq!(value["processing_status"], "in_progress");
        let batch_id = value["id"].as_str().unwrap().to_owned();

        let retrieved =
            wait_for_batch_to_finish(state.clone(), headers.clone(), batch_id.clone()).await;
        assert_eq!(retrieved["processing_status"], "ended");

        let results = handlers::message_batch_results(
            axum::extract::State(state),
            headers,
            axum::extract::Path(batch_id),
        )
        .await;
        assert_eq!(results.status(), StatusCode::OK);
        let body = to_bytes(results.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("\"custom_id\":\"req_1\""));
        assert!(body.contains("\"type\":\"succeeded\""));
    }

    #[tokio::test]
    async fn responses_compact_returns_message_history_plus_compaction_item() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        let state = test_state(store, spawn_refresh_server().await, Some("secret".into()));
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::compact_response(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "input": [
                        {
                            "type": "message",
                            "role": "developer",
                            "content": [{"type": "input_text", "text": "be terse"}]
                        },
                        {
                            "type": "message",
                            "role": "user",
                            "content": [{"type": "input_text", "text": "hello"}]
                        },
                        {
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": "hi"}]
                        }
                    ]
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice::<Value>(&body).unwrap();
        let output = value["output"].as_array().unwrap();
        assert_eq!(output.len(), 3);
        assert_eq!(output[0]["role"], "developer");
        assert_eq!(output[1]["role"], "user");
        assert_eq!(output[2]["type"], "compaction");
        assert!(output[2]["encrypted_content"].as_str().unwrap().len() > 8);
    }

    #[tokio::test]
    async fn message_batches_can_be_listed_and_deleted() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, spawn_delayed_codex_server().await),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));
        headers.insert(HOST, HeaderValue::from_static("localhost:14550"));

        let created = handlers::create_message_batch(
            axum::extract::State(state.clone()),
            headers.clone(),
            Json(
                serde_json::from_value(json!({
                    "requests": [{
                        "custom_id": "req_1",
                        "params": {
                            "model": "gpt-5.5",
                            "max_tokens": 32,
                            "messages": [{"role": "user", "content": "hello"}]
                        }
                    }]
                }))
                .unwrap(),
            ),
        )
        .await;
        let created_body = to_bytes(created.into_body(), usize::MAX).await.unwrap();
        let created_value = serde_json::from_slice::<Value>(&created_body).unwrap();
        let batch_id = created_value["id"].as_str().unwrap().to_owned();
        assert_eq!(created_value["processing_status"], "in_progress");

        let listed =
            handlers::list_message_batches(axum::extract::State(state.clone()), headers.clone())
                .await;
        assert_eq!(listed.status(), StatusCode::OK);
        let listed_body = to_bytes(listed.into_body(), usize::MAX).await.unwrap();
        let listed_value = serde_json::from_slice::<Value>(&listed_body).unwrap();
        assert_eq!(listed_value["data"][0]["id"], batch_id);

        let canceled = handlers::cancel_message_batch(
            axum::extract::State(state.clone()),
            headers.clone(),
            axum::extract::Path(batch_id.clone()),
        )
        .await;
        assert_eq!(canceled.status(), StatusCode::OK);
        let canceled_body = to_bytes(canceled.into_body(), usize::MAX).await.unwrap();
        let canceled_value = serde_json::from_slice::<Value>(&canceled_body).unwrap();
        assert_eq!(canceled_value["processing_status"], "canceling");

        let finished =
            wait_for_batch_to_finish(state.clone(), headers.clone(), batch_id.clone()).await;
        assert!(finished["request_counts"]["canceled"].as_u64().unwrap() >= 1);

        let results = handlers::message_batch_results(
            axum::extract::State(state.clone()),
            headers.clone(),
            axum::extract::Path(batch_id.clone()),
        )
        .await;
        assert_eq!(results.status(), StatusCode::OK);
        let results_body = to_bytes(results.into_body(), usize::MAX).await.unwrap();
        let results_text = String::from_utf8(results_body.to_vec()).unwrap();
        assert!(results_text.contains("\"type\":\"canceled\""));

        let deleted = handlers::delete_message_batch(
            axum::extract::State(state.clone()),
            headers.clone(),
            axum::extract::Path(batch_id.clone()),
        )
        .await;
        assert_eq!(deleted.status(), StatusCode::OK);

        let missing = handlers::get_message_batch(
            axum::extract::State(state),
            headers,
            axum::extract::Path(batch_id),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn responses_forward_compaction_summary_into_upstream_request() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(
                http,
                spawn_strict_recording_codex_server(captured.clone()).await,
            ),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let compacted = handlers::compact_response(
            axum::extract::State(state.clone()),
            headers.clone(),
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "input": [
                        {
                            "type": "message",
                            "role": "developer",
                            "content": [{"type": "input_text", "text": "be terse"}]
                        },
                        {
                            "type": "message",
                            "role": "user",
                            "content": [{"type": "input_text", "text": "hello"}]
                        }
                    ]
                }))
                .unwrap(),
            ),
        )
        .await;
        let compacted_body = to_bytes(compacted.into_body(), usize::MAX).await.unwrap();
        let compacted_value = serde_json::from_slice::<Value>(&compacted_body).unwrap();

        let response = handlers::responses(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "input": compacted_value["output"]
                }))
                .unwrap(),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let body = {
            let captured = captured.lock().await;
            captured.last().cloned().unwrap()
        };
        let instructions = body["instructions"].as_str().unwrap();
        assert!(instructions.contains("developer: be terse"));
        assert!(instructions.contains("user: hello"));
    }

    #[tokio::test]
    async fn messages_send_empty_instructions_when_system_is_absent() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, spawn_recording_codex_server(captured.clone()).await),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::messages(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "max_tokens": 128,
                    "messages": [{"role": "user", "content": "hello"}]
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = {
            let captured = captured.lock().await;
            captured.last().cloned().unwrap()
        };
        assert_eq!(body["instructions"], "");
    }

    #[tokio::test]
    async fn messages_accept_anthropic_beta_without_forwarding_internal_keys() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, spawn_recording_codex_server(captured.clone()).await),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("extended-thinking-2025-05-14"),
        );

        let response = handlers::messages(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "max_tokens": 128,
                    "messages": [{"role": "user", "content": "hello"}]
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = {
            let captured = captured.lock().await;
            captured.last().cloned().unwrap()
        };
        assert!(body.get("rotom_anthropic_beta").is_none());
    }

    #[tokio::test]
    async fn messages_ignore_unknown_claude_billing_header() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, spawn_recording_codex_server(captured.clone()).await),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        headers.insert(
            "x-anthropic-billing-header",
            HeaderValue::from_static("{\"cch\":\"bypass-third-party-cache\"}"),
        );

        let response = handlers::messages(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "max_tokens": 128,
                    "messages": [{"role": "user", "content": "hello"}]
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = {
            let captured = captured.lock().await;
            captured.last().cloned().unwrap()
        };
        assert!(body.get("x-anthropic-billing-header").is_none());
        assert!(body.get("cch").is_none());
        assert!(find_forbidden_key_path(&body, "x-anthropic-billing-header").is_none());
        assert!(find_forbidden_key_path(&body, "cch").is_none());
    }

    #[tokio::test]
    async fn messages_strip_claude_code_billing_block_from_system_prompt() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, spawn_recording_codex_server(captured.clone()).await),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::messages(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "max_tokens": 128,
                    "system": "x-anthropic-billing-header: cc_version=2.1.38; cc_entrypoint=cli; cch=4873d;\n\nbe terse",
                    "messages": [{"role": "user", "content": "hello"}]
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = {
            let captured = captured.lock().await;
            captured.last().cloned().unwrap()
        };
        assert_eq!(body["instructions"], "");
        assert_eq!(body["input"][0]["role"], "developer");
        assert_eq!(body["input"][0]["content"][0]["text"], "be terse");
        assert!(find_forbidden_key_path(&body, "x-anthropic-billing-header").is_none());
        assert!(find_forbidden_key_path(&body, "cch").is_none());
    }

    #[tokio::test]
    async fn messages_strip_cache_control_from_upstream_request() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(http, spawn_recording_codex_server(captured.clone()).await),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::messages(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "max_tokens": 128,
                    "system": [{"type": "text", "text": "be terse", "cache_control": {"type": "ephemeral"}}],
                    "tools": [{"name": "lookup", "input_schema": {"type": "object"}, "cache_control": {"type": "ephemeral"}}],
                    "messages": [{
                        "role": "user",
                        "content": [{"type": "input_text", "text": "hello", "cache_control": {"type": "ephemeral"}}]
                    }]
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = {
            let captured = captured.lock().await;
            captured.last().cloned().unwrap()
        };
        assert_eq!(body["instructions"], "");
        assert_eq!(body["input"][0]["role"], "developer");
        assert!(
            body["input"][0]["content"][0]
                .get("cache_control")
                .is_none()
        );
        assert!(
            body["input"][1]["content"][0]
                .get("cache_control")
                .is_none()
        );
        assert!(body["tools"][0].get("cache_control").is_none());
    }

    #[tokio::test]
    async fn responses_strip_cache_control_from_upstream_request() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                provider: crate::config::Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 600,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let http = Client::new();
        let state = AppState::new(
            TokenManager::new(
                store,
                CodexOAuthClient::new_with_token_url(http.clone(), spawn_refresh_server().await),
            ),
            CodexClient::new(
                http,
                spawn_strict_recording_codex_server(captured.clone()).await,
            ),
            Some("secret".into()),
            ModelList::from_ids(["gpt-5.5"]),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("secret"));

        let response = handlers::responses(
            axum::extract::State(state),
            headers,
            Json(
                serde_json::from_value(json!({
                    "model": "gpt-5.5",
                    "input": [{
                        "type": "message",
                        "role": "developer",
                        "content": [{
                            "type": "input_text",
                            "text": "be terse",
                            "cache_control": {"type": "ephemeral"}
                        }]
                    }],
                    "tools": [{
                        "type": "function",
                        "function": {
                            "name": "lookup",
                            "parameters": {"type": "object"}
                        },
                        "cache_control": {"type": "ephemeral"}
                    }]
                }))
                .unwrap(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = {
            let captured = captured.lock().await;
            captured.last().cloned().unwrap()
        };
        assert!(find_forbidden_key_path(&body, "cache_control").is_none());
    }
}
