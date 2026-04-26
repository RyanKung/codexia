use crate::{
    Error,
    codex::client::CodexClient,
    error::Result,
    openai::{response::ModelList, types::ChatCompletionRequest},
    token::TokenManager,
};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures_util::{Stream, StreamExt, stream};
use serde_json::json;
use std::{convert::Infallible, net::SocketAddr, pin::Pin, sync::Arc};
use tokio::net::TcpListener;

#[derive(Clone)]
pub struct AppState {
    token_manager: TokenManager,
    codex: CodexClient,
    api_key: Option<Arc<str>>,
    models: ModelList,
}

impl AppState {
    pub fn new(
        token_manager: TokenManager,
        codex: CodexClient,
        api_key: Option<String>,
        models: ModelList,
    ) -> Self {
        Self {
            token_manager,
            codex,
            api_key: api_key.map(Arc::from),
            models,
        }
    }
}

pub async fn serve(addr: SocketAddr, state: AppState) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, router(state)).await?;
    Ok(())
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/auth/refresh", post(manual_refresh))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

async fn models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match authorize(&headers, state.api_key.as_deref()) {
        Ok(()) => Json(state.models).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn manual_refresh(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return error.into_response();
    }

    match state.token_manager.refresh().await {
        Ok(credentials) => Json(json!({
            "account_id": credentials.account_id,
            "expires_at": credentials.expires_at,
        }))
        .into_response(),
        Err(error) => error.into_response(),
    }
}

async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ChatCompletionRequest>,
) -> Response {
    if let Err(error) = authorize(&headers, state.api_key.as_deref()) {
        return error.into_response();
    }

    let credentials = match state.token_manager.credentials().await {
        Ok(credentials) => credentials,
        Err(error) => return error.into_response(),
    };

    if request.wants_stream() {
        match state.codex.stream_chat(request, &credentials).await {
            Ok(stream) => sse_response(stream).into_response(),
            Err(error) => error.into_response(),
        }
    } else {
        match state.codex.complete_chat(request, &credentials).await {
            Ok(response) => Json(response).into_response(),
            Err(error) => error.into_response(),
        }
    }
}

pub fn authorize(headers: &HeaderMap, api_key: Option<&str>) -> Result<()> {
    let Some(expected) = api_key else {
        return Ok(());
    };

    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let x_api_key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok());

    if bearer == Some(expected) || x_api_key == Some(expected) {
        Ok(())
    } else {
        Err(Error::Unauthorized)
    }
}

fn sse_response(
    stream: Pin<
        Box<dyn Stream<Item = Result<crate::openai::response::ChatCompletionChunk>> + Send>,
    >,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>>> {
    let mapped = stream.map(|item| {
        let event = match item {
            Ok(chunk) => Event::default().data(serde_json::to_string(&chunk).unwrap_or_default()),
            Err(error) => Event::default().data(
                json!({"error": {"message": error.to_string(), "type": "upstream_error"}})
                    .to_string(),
            ),
        };
        Ok(event)
    });

    let done = stream::once(async { Ok(Event::default().data("[DONE]")) });
    Sse::new(mapped.chain(done)).keep_alive(KeepAlive::default())
}

#[allow(dead_code)]
fn status_response(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": "invalid_request_error"
            }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{AuthStore, Credentials, now_unix},
        oauth::CodexOAuthClient,
        openai::response::ModelList,
    };
    use axum::{
        body::to_bytes,
        extract::Form,
        http::{HeaderValue, StatusCode},
    };
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use reqwest::Client;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use tokio::net::TcpListener;

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
        let dir = tempfile::tempdir().unwrap();
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

        let response = manual_refresh(State(state), headers).await;

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
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        let state = test_state(store, spawn_refresh_server().await, Some("secret".into()));

        let response = manual_refresh(State(state), HeaderMap::new()).await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
