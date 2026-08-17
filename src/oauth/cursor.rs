use crate::{
    Error, Result,
    config::{Credentials, Provider},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{RngCore, SeedableRng, rngs::StdRng};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{error::Error as StdError, time::Duration};
use tokio::time::sleep;
use url::Url;

/// Default Cursor website URL used for browser login.
pub const CURSOR_WEBSITE_URL: &str = "https://cursor.com";
/// Default Cursor API base URL used by browser login and `AgentService` calls.
pub const CURSOR_API_BASE_URL: &str = "https://api2.cursor.sh";

const CURSOR_LOGIN_PATH: &str = "loginDeepControl";
const CURSOR_AUTH_POLL_PATH: &str = "auth/poll";
const CURSOR_TOKEN_PATH: &str = "oauth/token";
const DEFAULT_MAX_POLL_ATTEMPTS: usize = 150;
const DEFAULT_INITIAL_POLL_DELAY: Duration = Duration::from_secs(1);
const DEFAULT_MAX_POLL_DELAY: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorPollResponse {
    access_token: String,
    refresh_token: String,
}

#[derive(Debug, Deserialize)]
struct CursorRefreshResponse {
    access_token: String,
    #[serde(default, rename = "shouldLogout")]
    should_logout: bool,
}

#[derive(Clone)]
/// OAuth-like browser login client for Cursor credentials.
pub struct CursorOAuthClient {
    http: Client,
    website_url: String,
    api_base_url: String,
    max_poll_attempts: usize,
    initial_poll_delay: Duration,
    max_poll_delay: Duration,
}

impl Default for CursorOAuthClient {
    fn default() -> Self {
        Self::new(Client::new())
    }
}

impl CursorOAuthClient {
    /// Creates a Cursor login client backed by the provided HTTP client.
    #[must_use]
    pub fn new(http: Client) -> Self {
        Self {
            http,
            website_url: CURSOR_WEBSITE_URL.to_owned(),
            api_base_url: CURSOR_API_BASE_URL.to_owned(),
            max_poll_attempts: DEFAULT_MAX_POLL_ATTEMPTS,
            initial_poll_delay: DEFAULT_INITIAL_POLL_DELAY,
            max_poll_delay: DEFAULT_MAX_POLL_DELAY,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_endpoints(
        http: Client,
        website_url: impl Into<String>,
        api_base_url: impl Into<String>,
        max_poll_attempts: usize,
    ) -> Self {
        Self {
            http,
            website_url: website_url.into(),
            api_base_url: api_base_url.into(),
            max_poll_attempts,
            initial_poll_delay: Duration::from_millis(1),
            max_poll_delay: Duration::from_millis(1),
        }
    }

    /// Creates a browser-ready Cursor login flow.
    ///
    /// Cursor does not use a localhost callback for this flow. The browser
    /// opens `cursor.com/loginDeepControl`, and rotom polls the API with the UUID
    /// plus verifier until the browser approval completes.
    ///
    /// # Errors
    ///
    /// Returns an error when the login URL cannot be constructed.
    pub fn create_authorization_flow(&self) -> Result<super::AuthorizationFlow> {
        let mut rng = StdRng::from_os_rng();
        let verifier = base64url_random(&mut rng, 32);
        let challenge = cursor_code_challenge(&verifier);
        let uuid = random_uuid_v4(&mut rng);
        let mut authorize_url = Url::parse(&self.website_url)?.join(CURSOR_LOGIN_PATH)?;
        authorize_url
            .query_pairs_mut()
            .append_pair("challenge", &challenge)
            .append_pair("uuid", &uuid)
            .append_pair("mode", "login")
            .append_pair("redirectTarget", "cli");

        Ok(super::AuthorizationFlow {
            verifier,
            state: uuid,
            authorize_url,
        })
    }

    /// Waits for the user to complete the browser login and returns credentials.
    ///
    /// # Errors
    ///
    /// Returns an error when polling fails repeatedly, times out, or returns a
    /// token without a usable JWT expiry.
    pub async fn wait_for_browser_login(
        &self,
        flow: &super::AuthorizationFlow,
    ) -> Result<Credentials> {
        let token = self.poll_for_token(&flow.state, &flow.verifier).await?;
        credentials_from_poll_response(token)
    }

    /// Refreshes Cursor credentials.
    ///
    /// # Errors
    ///
    /// Returns an error when Cursor rejects the refresh token, requests logout,
    /// or returns an access token without a usable JWT expiry.
    pub async fn refresh_token(&self, refresh_token: &str) -> Result<Credentials> {
        let token_url = Url::parse(&self.api_base_url)?.join(CURSOR_TOKEN_PATH)?;
        let response = self
            .http
            .post(token_url.clone())
            .header("content-type", "application/json")
            .json(&serde_json::json!({
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
            }))
            .send()
            .await
            .map_err(|error| {
                Error::oauth(cursor_request_error_message(
                    "token refresh",
                    &token_url,
                    &error,
                ))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(Error::oauth(format!(
                "Cursor token refresh failed with status {status}: {text}"
            )));
        }

        let token = response.json::<CursorRefreshResponse>().await?;
        credentials_from_refresh_response(token, refresh_token)
    }

    async fn poll_for_token(&self, uuid: &str, verifier: &str) -> Result<CursorPollResponse> {
        let poll_url = cursor_poll_url(&self.api_base_url, uuid, verifier)?;
        let mut transient_failures = 0_u8;
        for attempt in 0..self.max_poll_attempts {
            let response = self
                .http
                .get(poll_url.clone())
                .header("content-type", "application/json")
                .send()
                .await;

            match response {
                Ok(response) if response.status() == reqwest::StatusCode::NOT_FOUND => {
                    self.sleep_before_next_poll(attempt).await;
                }
                Ok(response) if response.status().is_success() => {
                    let token = response.json::<CursorPollResponse>().await?;
                    return Ok(token);
                }
                Ok(response) => {
                    transient_failures = transient_failures.saturating_add(1);
                    if transient_failures >= 3 {
                        let status = response.status();
                        let text = response.text().await.unwrap_or_default();
                        return Err(Error::oauth(format!(
                            "Cursor login polling failed with status {status}: {text}"
                        )));
                    }
                    self.sleep_before_next_poll(attempt).await;
                }
                Err(error) => {
                    transient_failures = transient_failures.saturating_add(1);
                    if transient_failures >= 3 {
                        return Err(Error::oauth(cursor_request_error_message(
                            "login polling",
                            &poll_url,
                            &error,
                        )));
                    }
                    self.sleep_before_next_poll(attempt).await;
                }
            }
        }

        Err(Error::oauth(
            "Cursor login timed out before browser approval completed",
        ))
    }

    async fn sleep_before_next_poll(&self, attempt: usize) {
        let multiplier = 1.2_f64.powi(i32::try_from(attempt).unwrap_or(i32::MAX));
        let delay = self
            .initial_poll_delay
            .mul_f64(multiplier)
            .min(self.max_poll_delay);
        sleep(delay).await;
    }
}

fn credentials_from_poll_response(token: CursorPollResponse) -> Result<Credentials> {
    let payload = decode_jwt_payload(&token.access_token)?;
    let expires_at = payload
        .get("exp")
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .ok_or_else(|| Error::oauth("Cursor access token is missing JWT exp"))?;
    let account_id = cursor_account_id(&payload).unwrap_or_default();

    Ok(Credentials {
        provider: Provider::Cursor,
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at,
        account_id,
    })
}

fn credentials_from_refresh_response(
    token: CursorRefreshResponse,
    refresh_token: &str,
) -> Result<Credentials> {
    if token.should_logout {
        return Err(Error::oauth(
            "Cursor token refresh requested logout; run `rotom login --cursor` again",
        ));
    }

    let payload = decode_jwt_payload(&token.access_token)?;
    let expires_at = payload
        .get("exp")
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .ok_or_else(|| Error::oauth("Cursor access token is missing JWT exp"))?;
    let account_id = cursor_account_id(&payload).unwrap_or_default();

    Ok(Credentials {
        provider: Provider::Cursor,
        access_token: token.access_token,
        refresh_token: refresh_token.to_owned(),
        expires_at,
        account_id,
    })
}

fn cursor_poll_url(api_base_url: &str, uuid: &str, verifier: &str) -> Result<Url> {
    let mut url = Url::parse(api_base_url)?.join(CURSOR_AUTH_POLL_PATH)?;
    url.query_pairs_mut()
        .append_pair("uuid", uuid)
        .append_pair("verifier", verifier);
    Ok(url)
}

fn cursor_request_error_message(operation: &str, url: &Url, error: &reqwest::Error) -> String {
    let mut message = format!("Cursor {operation} request to {url} failed: {error}");
    let mut source = error.source();
    let mut sources = Vec::new();
    while let Some(error) = source {
        let text = error.to_string();
        if !text.is_empty() && !sources.iter().any(|item| item == &text) {
            sources.push(text);
        }
        source = error.source();
    }
    if !sources.is_empty() {
        message.push_str("; source: ");
        message.push_str(&sources.join(": "));
    }
    if error.is_connect() || error.is_timeout() {
        let host = url.host_str().unwrap_or("Cursor API");
        message.push_str(&format!(". Check DNS/proxy/VPN access to {host}."));
    }
    message
}

fn base64url_random(rng: &mut impl RngCore, len: usize) -> String {
    let mut bytes = vec![0_u8; len];
    rng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn cursor_code_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn random_uuid_v4(rng: &mut impl RngCore) -> String {
    let mut bytes = [0_u8; 16];
    rng.fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{}-{}-{}-{}-{}",
        hex::encode(&bytes[0..4]),
        hex::encode(&bytes[4..6]),
        hex::encode(&bytes[6..8]),
        hex::encode(&bytes[8..10]),
        hex::encode(&bytes[10..16])
    )
}

fn decode_jwt_payload(token: &str) -> Result<Value> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| Error::oauth("invalid Cursor JWT access token"))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| Error::oauth("invalid Cursor JWT payload encoding"))?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn cursor_account_id(payload: &Value) -> Option<String> {
    ["authId", "sub", "email"]
        .into_iter()
        .find_map(|key| payload.get(key).and_then(Value::as_str))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::Query,
        routing::{get, post},
    };
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use tokio::net::TcpListener;

    fn jwt_with_payload(payload: &Value) -> String {
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap());
        format!("header.{encoded}.sig")
    }

    async fn poll_handler(Query(query): Query<HashMap<String, String>>) -> Json<Value> {
        assert_eq!(query.get("uuid").map(String::as_str), Some("uuid-test"));
        assert_eq!(
            query.get("verifier").map(String::as_str),
            Some("verifier-test")
        );
        Json(json!({
            "accessToken": jwt_with_payload(&json!({
                "exp": 1_893_456_000_i64,
                "sub": "cursor-user"
            })),
            "refreshToken": "refresh-token"
        }))
    }

    async fn refresh_handler(Json(body): Json<Value>) -> Json<Value> {
        assert_eq!(
            body.get("grant_type").and_then(Value::as_str),
            Some("refresh_token")
        );
        assert_eq!(
            body.get("refresh_token").and_then(Value::as_str),
            Some("old-refresh")
        );
        Json(json!({
            "access_token": jwt_with_payload(&json!({
                "exp": 1_893_456_001_i64,
                "sub": "cursor-refreshed"
            })),
            "id_token": jwt_with_payload(&json!({
                "exp": 1_893_456_001_i64,
                "sub": "cursor-refreshed"
            })),
            "shouldLogout": false
        }))
    }

    async fn spawn_poll_server() -> String {
        let app = Router::new().route("/auth/poll", get(poll_handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        url
    }

    async fn spawn_refresh_server() -> String {
        let app = Router::new().route("/oauth/token", post(refresh_handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        url
    }

    #[test]
    fn builds_cursor_login_url() {
        let client = CursorOAuthClient::new(Client::new());
        let flow = client.create_authorization_flow().unwrap();
        let pairs = flow
            .authorize_url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();

        assert_eq!(
            flow.authorize_url.as_str().split('?').next().unwrap(),
            "https://cursor.com/loginDeepControl"
        );
        assert_eq!(
            pairs.get("mode").map(std::borrow::Cow::as_ref),
            Some("login")
        );
        assert_eq!(
            pairs.get("redirectTarget").map(std::borrow::Cow::as_ref),
            Some("cli")
        );
        assert_eq!(
            pairs.get("uuid").map(std::borrow::Cow::as_ref),
            Some(flow.state.as_str())
        );
        assert_eq!(flow.state.len(), 36);
        assert_eq!(flow.verifier.len(), 43);
        assert_eq!(pairs.get("challenge").map(|value| value.len()), Some(43));
    }

    #[tokio::test]
    async fn polls_cursor_login_token() {
        let base_url = spawn_poll_server().await;
        let client =
            CursorOAuthClient::new_with_endpoints(Client::new(), "https://cursor.com", base_url, 1);
        let flow = super::super::AuthorizationFlow {
            verifier: "verifier-test".to_owned(),
            state: "uuid-test".to_owned(),
            authorize_url: Url::parse("https://cursor.com/loginDeepControl").unwrap(),
        };

        let credentials = client.wait_for_browser_login(&flow).await.unwrap();

        assert_eq!(credentials.provider, Provider::Cursor);
        assert_eq!(credentials.refresh_token, "refresh-token");
        assert_eq!(credentials.expires_at, 1_893_456_000);
        assert_eq!(credentials.account_id, "cursor-user");
    }

    #[tokio::test]
    async fn cursor_login_poll_error_mentions_network_diagnostics() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let client =
            CursorOAuthClient::new_with_endpoints(Client::new(), "https://cursor.com", base_url, 3);
        let flow = super::super::AuthorizationFlow {
            verifier: "verifier-test".to_owned(),
            state: "uuid-test".to_owned(),
            authorize_url: Url::parse("https://cursor.com/loginDeepControl").unwrap(),
        };

        let error = client.wait_for_browser_login(&flow).await.unwrap_err();
        let message = error.to_string();

        assert!(message.contains("OAuth error: Cursor login polling request to"));
        assert!(message.contains("Check DNS/proxy/VPN access to 127.0.0.1."));
    }

    #[tokio::test]
    async fn refreshes_cursor_browser_token() {
        let base_url = spawn_refresh_server().await;
        let client =
            CursorOAuthClient::new_with_endpoints(Client::new(), "https://cursor.com", base_url, 1);

        let credentials = client.refresh_token("old-refresh").await.unwrap();

        assert_eq!(credentials.provider, Provider::Cursor);
        assert_eq!(credentials.refresh_token, "old-refresh");
        assert_eq!(credentials.expires_at, 1_893_456_001);
        assert_eq!(credentials.account_id, "cursor-refreshed");
    }

    #[test]
    fn rejects_cursor_token_without_exp() {
        let token = CursorPollResponse {
            access_token: jwt_with_payload(&json!({ "sub": "cursor-user" })),
            refresh_token: "refresh-token".to_owned(),
        };

        let error = credentials_from_poll_response(token).unwrap_err();

        assert_eq!(
            error.to_string(),
            "OAuth error: Cursor access token is missing JWT exp"
        );
    }
}
