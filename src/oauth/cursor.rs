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
use std::time::Duration;
use tokio::time::sleep;
use url::Url;

/// Default Cursor website URL used for browser login.
pub const CURSOR_WEBSITE_URL: &str = "https://cursor.com";
/// Default Cursor API base URL used by the official `cursor-agent` CLI.
pub const CURSOR_API_BASE_URL: &str = "https://api2.cursor.sh";

const CURSOR_LOGIN_PATH: &str = "loginDeepControl";
const CURSOR_AUTH_POLL_PATH: &str = "auth/poll";
const DEFAULT_MAX_POLL_ATTEMPTS: usize = 150;
const DEFAULT_INITIAL_POLL_DELAY: Duration = Duration::from_secs(1);
const DEFAULT_MAX_POLL_DELAY: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorPollResponse {
    access_token: String,
    refresh_token: String,
}

#[derive(Clone)]
/// OAuth-like browser login client for Cursor Agent credentials.
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
    /// Cursor Agent does not use a localhost callback for this flow. The CLI
    /// opens `cursor.com/loginDeepControl` and then polls the API with the UUID
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
    /// Cursor Agent's installed CLI does not expose or call a browser-token
    /// refresh endpoint. It re-exchanges a User API key when one is available,
    /// so browser-login credentials must currently be renewed by logging in
    /// again when the access token expires.
    pub fn refresh_token(&self, _refresh_token: &str) -> Result<Credentials> {
        Err(Error::oauth(
            "Cursor browser-login refresh is not implemented; run `rotom login --provider cursor` again when the token expires",
        ))
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
                        return Err(Error::oauth(format!(
                            "Cursor login polling request failed: {error}"
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

fn cursor_poll_url(api_base_url: &str, uuid: &str, verifier: &str) -> Result<Url> {
    let mut url = Url::parse(api_base_url)?.join(CURSOR_AUTH_POLL_PATH)?;
    url.query_pairs_mut()
        .append_pair("uuid", uuid)
        .append_pair("verifier", verifier);
    Ok(url)
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
    use axum::{Json, Router, extract::Query, routing::get};
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

    async fn spawn_poll_server() -> String {
        let app = Router::new().route("/auth/poll", get(poll_handler));
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
