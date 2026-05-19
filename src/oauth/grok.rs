use crate::{
    Error, Result,
    config::{Credentials, Provider, now_unix},
    oauth::pkce::Pkce,
};
use rand::{RngCore, SeedableRng, rngs::StdRng};
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;
use url::Url;

/// Default xAI API base URL used for Grok Responses requests.
pub const XAI_API_BASE_URL: &str = "https://api.x.ai/v1";
/// xAI OAuth discovery endpoint.
pub const XAI_OAUTH_DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
/// Public client identifier used by xAI's Grok CLI OAuth flow.
pub const XAI_OAUTH_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
/// OAuth scopes requested for Grok API access.
pub const XAI_OAUTH_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
/// Local redirect URI used by xAI OAuth.
pub const XAI_REDIRECT_URI: &str = "http://127.0.0.1:56121/callback";
const XAI_OAUTH_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const XAI_OAUTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Deserialize)]
struct DiscoveryResponse {
    authorization_endpoint: String,
    token_endpoint: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
}

/// OAuth client for exchanging and refreshing xAI Grok credentials.
#[derive(Clone)]
pub struct GrokOAuthClient {
    http: Client,
    discovery_url: String,
}

impl Default for GrokOAuthClient {
    fn default() -> Self {
        Self::new(default_http_client())
    }
}

impl GrokOAuthClient {
    /// Creates a Grok OAuth client backed by the provided HTTP client.
    #[must_use]
    pub fn new(http: Client) -> Self {
        Self {
            http,
            discovery_url: XAI_OAUTH_DISCOVERY_URL.to_owned(),
        }
    }

    /// Creates a browser-ready Grok OAuth authorization flow.
    ///
    /// # Errors
    ///
    /// Returns an error when OIDC discovery fails or the authorization URL
    /// cannot be constructed.
    pub async fn create_authorization_flow(&self) -> Result<super::AuthorizationFlow> {
        let discovery = self.discovery().await?;
        let mut rng = StdRng::from_os_rng();
        let Pkce {
            verifier,
            challenge,
        } = super::generate_pkce(&mut rng);
        let state = create_state(&mut rng);
        let mut authorize_url = Url::parse(&discovery.authorization_endpoint)?;
        authorize_url
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", XAI_OAUTH_CLIENT_ID)
            .append_pair("redirect_uri", XAI_REDIRECT_URI)
            .append_pair("scope", XAI_OAUTH_SCOPE)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("nonce", &create_state(&mut rng))
            .append_pair("plan", "generic")
            .append_pair("referrer", "codexia");

        Ok(super::AuthorizationFlow {
            verifier,
            state,
            authorize_url,
        })
    }

    /// Exchanges an authorization code plus PKCE verifier for Grok credentials.
    ///
    /// # Errors
    ///
    /// Returns an error when discovery, token exchange, or response parsing fails.
    pub async fn exchange_authorization_code(
        &self,
        code: &str,
        verifier: &str,
    ) -> Result<Credentials> {
        let discovery = self.discovery().await?;
        let challenge = super::code_challenge(verifier);
        let response = self
            .http
            .post(discovery.token_endpoint)
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", XAI_OAUTH_CLIENT_ID),
                ("code", code),
                ("redirect_uri", XAI_REDIRECT_URI),
                ("code_verifier", verifier),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
            ])
            .send()
            .await?;

        parse_token_response(response, "code exchange", None).await
    }

    /// Refreshes an existing Grok refresh token.
    ///
    /// # Errors
    ///
    /// Returns an error when discovery, refresh, or response parsing fails.
    pub async fn refresh_token(&self, refresh_token: &str) -> Result<Credentials> {
        let discovery = self.discovery().await?;
        let response = self
            .http
            .post(discovery.token_endpoint)
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", XAI_OAUTH_CLIENT_ID),
                ("refresh_token", refresh_token),
            ])
            .send()
            .await?;

        parse_token_response(response, "refresh", Some(refresh_token)).await
    }

    async fn discovery(&self) -> Result<DiscoveryResponse> {
        let response = self
            .http
            .get(&self.discovery_url)
            .send()
            .await
            .map_err(|error| {
                Error::oauth(format!(
                    "Grok OAuth discovery request to {} failed: {error}. Check DNS/proxy/VPN access to auth.x.ai.",
                    self.discovery_url
                ))
            })?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(Error::oauth(format!(
                "Grok OAuth discovery failed with status {status}: {text}"
            )));
        }
        let discovery = response.json::<DiscoveryResponse>().await?;
        validate_xai_endpoint(&discovery.authorization_endpoint)?;
        validate_xai_endpoint(&discovery.token_endpoint)?;
        Ok(discovery)
    }
}

fn default_http_client() -> Client {
    Client::builder()
        .connect_timeout(XAI_OAUTH_CONNECT_TIMEOUT)
        .timeout(XAI_OAUTH_REQUEST_TIMEOUT)
        .build()
        .expect("Grok OAuth HTTP client configuration is valid")
}

fn create_state(rng: &mut impl RngCore) -> String {
    let mut bytes = [0_u8; 16];
    rng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn validate_xai_endpoint(value: &str) -> Result<()> {
    let url = Url::parse(value)?;
    if url.scheme() != "https" {
        return Err(Error::oauth(
            "Grok OAuth discovery returned a non-HTTPS endpoint",
        ));
    }
    let host = url.host_str().unwrap_or_default();
    if host != "x.ai" && !host.ends_with(".x.ai") {
        return Err(Error::oauth(format!(
            "Grok OAuth discovery endpoint is not on x.ai: {host}"
        )));
    }
    Ok(())
}

async fn parse_token_response(
    response: reqwest::Response,
    operation: &str,
    previous_refresh_token: Option<&str>,
) -> Result<Credentials> {
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(Error::oauth(format!(
            "Grok OAuth {operation} failed with status {status}: {text}"
        )));
    }

    let token = response.json::<TokenResponse>().await?;
    if token.expires_in <= 0 {
        return Err(Error::oauth(
            "Grok OAuth token response has invalid expires_in",
        ));
    }
    let refresh_token = token
        .refresh_token
        .or_else(|| previous_refresh_token.map(str::to_owned))
        .ok_or_else(|| Error::oauth("Grok OAuth token response is missing refresh_token"))?;

    Ok(Credentials {
        provider: Provider::Grok,
        access_token: token.access_token,
        refresh_token,
        expires_at: now_unix().saturating_add(token.expires_in),
        account_id: String::new(),
    })
}
