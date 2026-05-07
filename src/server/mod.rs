//! HTTP server state, router wiring, and endpoint tests.

use crate::{
    codex::client::CodexClient, error::Result, openai::response::ModelList, status::StatusClient,
    token::TokenManager,
};
use axum::{
    Router,
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
    token_manager: TokenManager,
    codex: CodexClient,
    status: StatusClient,
    api_key: Option<Arc<str>>,
    models: ModelList,
    responses: store::ResponseStore,
    batches: store::BatchStore,
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
        let status = StatusClient::new(Client::new(), codex.base_url().to_owned());
        Self {
            token_manager,
            status,
            codex,
            api_key: api_key.map(Arc::from),
            models,
            responses: store::ResponseStore::default(),
            batches: store::BatchStore::default(),
        }
    }
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
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Error,
        config::{AuthStore, Credentials, now_unix},
        oauth::CodexOAuthClient,
        openai::response::ModelList,
        testsupport::TempDir,
    };
    use axum::{
        Json,
        body::to_bytes,
        extract::{Form, State},
        http::{HeaderMap, HeaderValue, StatusCode, header::HOST},
        routing::{get, post},
    };
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use reqwest::Client;
    use serde_json::{Value, json};
    use std::{collections::HashMap, sync::Arc};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

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
        assert_eq!(value["account_id"], "acc_refreshed");
        assert!(value.get("expires_at").is_some());
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
    async fn messages_returns_anthropic_message_response() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
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
    async fn responses_returns_response_object() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
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
                    "instructions": "Be terse.",
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

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice::<Value>(&body).unwrap();
        assert_eq!(value["type"], "message_batch");
        assert_eq!(value["processing_status"], "ended");
        let batch_id = value["id"].as_str().unwrap().to_owned();

        let retrieved = handlers::get_message_batch(
            axum::extract::State(state.clone()),
            headers.clone(),
            axum::extract::Path(batch_id.clone()),
        )
        .await;
        assert_eq!(retrieved.status(), StatusCode::OK);

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

        let deleted = handlers::delete_message_batch(
            axum::extract::State(state.clone()),
            headers.clone(),
            axum::extract::Path(batch_id.clone()),
        )
        .await;
        assert_eq!(deleted.status(), StatusCode::OK);
        let deleted_body = to_bytes(deleted.into_body(), usize::MAX).await.unwrap();
        let deleted_value = serde_json::from_slice::<Value>(&deleted_body).unwrap();
        assert_eq!(deleted_value["id"], batch_id);
        assert_eq!(deleted_value["type"], "message_batch_deleted");

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
}
