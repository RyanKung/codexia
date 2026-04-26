use crate::{
    Error, Result,
    config::{AuthStore, Credentials},
    oauth::CodexOAuthClient,
};
use std::sync::Arc;
use tokio::sync::RwLock;

const REFRESH_SKEW_SECS: i64 = 60;

#[derive(Clone)]
pub struct TokenManager {
    store: AuthStore,
    oauth: CodexOAuthClient,
    cached: Arc<RwLock<Option<Credentials>>>,
}

impl TokenManager {
    pub fn new(store: AuthStore, oauth: CodexOAuthClient) -> Self {
        let cached = store.load().unwrap_or(None);
        Self {
            store,
            oauth,
            cached: Arc::new(RwLock::new(cached)),
        }
    }

    pub async fn credentials_snapshot(&self) -> Option<Credentials> {
        self.cached.read().await.clone()
    }

    pub async fn refresh(&self) -> Result<Credentials> {
        let mut guard = self.cached.write().await;
        let credentials = guard.clone().or(self.store.load()?).ok_or_else(|| {
            Error::config("not logged in; run `codexia login` before refreshing tokens")
        })?;

        let refreshed = self.refresh_credentials(&credentials).await?;
        self.store.save(&refreshed)?;
        *guard = Some(refreshed.clone());
        Ok(refreshed)
    }

    pub async fn credentials(&self) -> Result<Credentials> {
        if let Some(credentials) = self.cached.read().await.clone()
            && !credentials.is_expired(REFRESH_SKEW_SECS)
        {
            return Ok(credentials);
        }

        let mut guard = self.cached.write().await;
        let credentials = match guard.clone().or(self.store.load()?) {
            Some(credentials) => credentials,
            None => {
                return Err(Error::config(
                    "not logged in; run `codexia login` before serving requests",
                ));
            }
        };

        if !credentials.is_expired(REFRESH_SKEW_SECS) {
            *guard = Some(credentials.clone());
            return Ok(credentials);
        }

        let refreshed = self.refresh_credentials(&credentials).await?;
        self.store.save(&refreshed)?;
        *guard = Some(refreshed.clone());
        Ok(refreshed)
    }

    async fn refresh_credentials(&self, credentials: &Credentials) -> Result<Credentials> {
        self.oauth.refresh_token(&credentials.refresh_token).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::now_unix;
    use axum::{Form, Json, Router, routing::post};
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use reqwest::Client;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use tokio::net::TcpListener;

    fn jwt_with_payload(payload: Value) -> String {
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
        format!("header.{encoded}.sig")
    }

    fn jwt_with_account_id(account_id: &str) -> String {
        jwt_with_payload(json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": account_id }
        }))
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

    #[test]
    fn manager_loads_missing_store_without_panic() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        let manager = TokenManager::new(store, CodexOAuthClient::default());

        assert!(manager.cached.blocking_read().is_none());
    }

    #[tokio::test]
    async fn manager_returns_unexpired_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        let credentials = Credentials {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: now_unix() + 600,
            account_id: "acc".into(),
        };
        store.save(&credentials).unwrap();
        let manager = TokenManager::new(store, CodexOAuthClient::default());

        assert_eq!(manager.credentials().await.unwrap(), credentials);
    }

    #[tokio::test]
    async fn manager_refreshes_expired_credentials_and_saves_them() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        store
            .save(&Credentials {
                access_token: "old_access".into(),
                refresh_token: "old_refresh".into(),
                expires_at: now_unix() - 1,
                account_id: "acc_old".into(),
            })
            .unwrap();
        let oauth =
            CodexOAuthClient::new_with_token_url(Client::new(), spawn_refresh_server().await);
        let manager = TokenManager::new(store.clone(), oauth);

        let credentials = manager.credentials().await.unwrap();

        assert_eq!(credentials.refresh_token, "new_refresh");
        assert_eq!(credentials.account_id, "acc_refreshed");
        assert_eq!(store.load().unwrap(), Some(credentials));
    }

    #[tokio::test]
    async fn manager_refresh_forces_refresh_even_when_credentials_are_unexpired() {
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
        let oauth =
            CodexOAuthClient::new_with_token_url(Client::new(), spawn_refresh_server().await);
        let manager = TokenManager::new(store.clone(), oauth);

        let credentials = manager.refresh().await.unwrap();

        assert_eq!(credentials.refresh_token, "new_refresh");
        assert_eq!(credentials.account_id, "acc_refreshed");
        assert_eq!(manager.credentials_snapshot().await, Some(credentials));
    }
}
