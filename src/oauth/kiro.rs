use crate::{
    Error, Result,
    config::{Credentials, Provider, now_unix},
};
use chrono::{DateTime, Utc};
use rand::{RngCore, SeedableRng, rngs::StdRng};
use reqwest::{Client, header::USER_AGENT};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    path::{Path, PathBuf},
};
use url::Url;

const DEFAULT_REGION: &str = "us-east-1";
const DEFAULT_DESKTOP_USER_AGENT_VERSION: &str = "0.7.45";
const KIRO_AUTH_PORTAL_URL: &str = "https://app.kiro.dev/signin";
const KIRO_AUTH_REDIRECT_URI: &str = "http://localhost:3128";
const KIRO_AUTH_REDIRECT_FROM: &str = "kirocli";
const KIRO_AUTH_CALLBACK_PATH: &str = "/oauth/callback";
const KIRO_SIGNIN_CALLBACK_PATH: &str = "/signin/callback";
const KIRO_CLI_TOKEN_KEYS: &[&str] = &[
    "kirocli:social:token",
    "kirocli:oidc:token",
    "kirocli:odic:token",
    "codewhisperer:oidc:token",
    "codewhisperer:odic:token",
];
const KIRO_CLI_DEVICE_KEYS: &[&str] = &[
    "kirocli:oidc:device-registration",
    "kirocli:odic:device-registration",
    "codewhisperer:oidc:device-registration",
    "codewhisperer:odic:device-registration",
];

/// Default Kiro IDE desktop token location relative to the user's home directory.
pub const KIRO_DESKTOP_TOKEN_RELATIVE_PATH: &str = ".aws/sso/cache/kiro-auth-token.json";
/// Default Kiro CLI `SQLite` credential store relative to the user's home directory.
pub const KIRO_CLI_DATABASE_RELATIVE_PATH: &str = ".local/share/kiro-cli/data.sqlite3";
/// macOS Kiro CLI `SQLite` credential store relative to the user's home directory.
pub const KIRO_CLI_MACOS_DATABASE_RELATIVE_PATH: &str =
    "Library/Application Support/kiro-cli/data.sqlite3";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum KiroRefreshSecret {
    Desktop {
        refresh_token: String,
        region: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_region: Option<String>,
        user_agent: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile_arn: Option<String>,
    },
    Sso {
        refresh_token: String,
        client_id: String,
        client_secret: String,
        sso_region: String,
        api_region: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile_arn: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
struct KiroTokenResponse {
    #[serde(rename = "accessToken", alias = "access_token")]
    access_token: String,
    #[serde(default, rename = "refreshToken", alias = "refresh_token")]
    refresh_token: Option<String>,
    #[serde(rename = "expiresIn", alias = "expires_in")]
    expires_in: i64,
    #[serde(default, rename = "profileArn", alias = "profile_arn")]
    profile_arn: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Kiro portal callback data needed to exchange a social login code.
pub struct KiroAuthorizationCallback {
    /// Authorization code returned by the Kiro portal.
    pub code: String,
    /// Optional state returned by the portal callback.
    pub state: Option<String>,
    /// Kiro portal login option, such as `google` or `github`.
    pub login_option: String,
    /// Callback path returned by the portal, usually `/oauth/callback`.
    pub path: String,
}

#[derive(Debug, Serialize)]
struct KiroCreateTokenRequest<'a> {
    code: &'a str,
    #[serde(rename = "code_verifier")]
    code_verifier: &'a str,
    #[serde(rename = "redirect_uri")]
    redirect_uri: String,
    #[serde(
        default,
        rename = "invitation_code",
        skip_serializing_if = "Option::is_none"
    )]
    invitation_code: Option<&'a str>,
}

#[derive(Clone)]
/// Client for refreshing credentials imported from local Kiro stores.
pub struct KiroOAuthClient {
    http: Client,
    auth_endpoint_override: Option<String>,
}

impl Default for KiroOAuthClient {
    fn default() -> Self {
        Self::new(Client::new())
    }
}

impl KiroOAuthClient {
    /// Creates a Kiro auth client backed by the provided HTTP client.
    #[must_use]
    pub const fn new(http: Client) -> Self {
        Self {
            http,
            auth_endpoint_override: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_auth_endpoint(http: Client, endpoint: impl Into<String>) -> Self {
        Self {
            http,
            auth_endpoint_override: Some(endpoint.into()),
        }
    }

    /// Creates a browser-ready Kiro portal authorization flow.
    ///
    /// # Errors
    ///
    /// Returns an error when the Kiro portal URL cannot be constructed.
    pub fn create_authorization_flow() -> Result<super::AuthorizationFlow> {
        let mut rng = StdRng::from_os_rng();
        let super::pkce::Pkce {
            verifier,
            challenge,
        } = super::generate_pkce(&mut rng);
        let state = create_state(&mut rng);
        let mut authorize_url = Url::parse(KIRO_AUTH_PORTAL_URL)?;
        authorize_url
            .query_pairs_mut()
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("redirect_uri", KIRO_AUTH_REDIRECT_URI)
            .append_pair("redirect_from", KIRO_AUTH_REDIRECT_FROM);

        Ok(super::AuthorizationFlow {
            verifier,
            state,
            authorize_url,
        })
    }

    /// Exchanges a Kiro portal callback for rotom-managed Kiro credentials.
    ///
    /// # Errors
    ///
    /// Returns an error when the callback uses an unsupported login option, the
    /// Kiro auth service rejects the code, or the response is malformed.
    pub async fn exchange_authorization_callback(
        &self,
        callback: &KiroAuthorizationCallback,
        verifier: &str,
    ) -> Result<Credentials> {
        validate_social_login_option(&callback.login_option)?;
        let url = self.desktop_auth_url(DEFAULT_REGION, "oauth/token")?;
        let user_agent = default_desktop_user_agent();
        let body = KiroCreateTokenRequest {
            code: &callback.code,
            code_verifier: verifier,
            redirect_uri: callback.token_exchange_redirect_uri(),
            invitation_code: None,
        };
        let response = self
            .http
            .post(url)
            .header(USER_AGENT, &user_agent)
            .json(&body)
            .send()
            .await?;
        let token = parse_kiro_token_response(response, "code exchange").await?;
        let refresh_token = token
            .refresh_token
            .clone()
            .ok_or_else(|| Error::oauth("Kiro token response is missing refreshToken"))?;
        let api_region = token
            .profile_arn
            .as_deref()
            .and_then(region_from_profile_arn)
            .unwrap_or(DEFAULT_REGION)
            .to_owned();
        let secret = KiroRefreshSecret::Desktop {
            refresh_token,
            region: DEFAULT_REGION.to_owned(),
            api_region: Some(api_region),
            user_agent,
            profile_arn: token.profile_arn.clone(),
        };
        credentials_from_response(token, &secret)
    }

    /// Imports credentials from the default Kiro CLI `SQLite` store.
    ///
    /// # Errors
    ///
    /// Returns an error when the default home path cannot be resolved, the
    /// `SQLite` database cannot be read, or required token fields are missing.
    pub fn import_cli_default() -> Result<Credentials> {
        Self::import_cli_database(&default_cli_database_path()?)
    }

    /// Imports credentials from the default Kiro IDE desktop token JSON file.
    ///
    /// # Errors
    ///
    /// Returns an error when the default home path cannot be resolved, the JSON
    /// file cannot be read, or required token fields are missing.
    pub fn import_desktop_default() -> Result<Credentials> {
        Self::import_desktop_file(&default_desktop_token_path()?)
    }

    /// Imports credentials from a Kiro CLI `SQLite` database.
    ///
    /// # Errors
    ///
    /// Returns an error when the database cannot be read or required token and
    /// device-registration fields are missing.
    pub fn import_cli_database(path: &Path) -> Result<Credentials> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|error| {
                Error::config(format!(
                    "failed to open Kiro CLI credential database {}: {error}",
                    path.display()
                ))
            })?;
        let token = first_auth_value(&connection, KIRO_CLI_TOKEN_KEYS)?.ok_or_else(|| {
            Error::config(format!(
                "no supported Kiro CLI token key found in {}",
                path.display()
            ))
        })?;
        let device = first_auth_value(&connection, KIRO_CLI_DEVICE_KEYS)?;
        credentials_from_cli_values(&token, device.as_deref())
    }

    /// Imports credentials from a Kiro IDE desktop token JSON file.
    ///
    /// # Errors
    ///
    /// Returns an error when the JSON file cannot be read or required token
    /// fields are missing.
    pub fn import_desktop_file(path: &Path) -> Result<Credentials> {
        let raw = fs::read_to_string(path).map_err(|error| {
            Error::config(format!(
                "failed to read Kiro desktop token file {}: {error}",
                path.display()
            ))
        })?;
        credentials_from_desktop_json(&raw)
    }

    /// Refreshes an imported Kiro refresh secret.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored secret is not a rotom Kiro secret, the
    /// refresh endpoint rejects the token, or the response is malformed.
    pub async fn refresh_token(&self, refresh_secret: &str) -> Result<Credentials> {
        let secret = decode_refresh_secret(refresh_secret)?;
        match secret {
            KiroRefreshSecret::Desktop {
                refresh_token,
                region,
                api_region,
                user_agent,
                profile_arn,
            } => {
                self.refresh_desktop(
                    &refresh_token,
                    &region,
                    api_region.as_deref(),
                    &user_agent,
                    profile_arn.as_deref(),
                )
                .await
            }
            KiroRefreshSecret::Sso {
                refresh_token,
                client_id,
                client_secret,
                sso_region,
                api_region,
                profile_arn,
            } => {
                self.refresh_sso(
                    &refresh_token,
                    &client_id,
                    &client_secret,
                    &sso_region,
                    &api_region,
                    profile_arn.as_deref(),
                )
                .await
            }
        }
    }

    async fn refresh_desktop(
        &self,
        refresh_token: &str,
        region: &str,
        api_region: Option<&str>,
        user_agent: &str,
        profile_arn: Option<&str>,
    ) -> Result<Credentials> {
        validate_region(region)?;
        let url = self.desktop_auth_url(region, "refreshToken")?;
        let response = self
            .http
            .post(url)
            .header(USER_AGENT, user_agent)
            .json(&serde_json::json!({ "refreshToken": refresh_token }))
            .send()
            .await?;
        let token = parse_kiro_token_response(response, "desktop refresh").await?;
        let next_refresh = token.refresh_token.as_deref().unwrap_or(refresh_token);
        let next_profile_arn = token
            .profile_arn
            .clone()
            .or_else(|| profile_arn.map(str::to_owned));
        let next_api_region = next_profile_arn
            .as_deref()
            .and_then(region_from_profile_arn)
            .map(str::to_owned)
            .or_else(|| api_region.map(str::to_owned));
        let secret = KiroRefreshSecret::Desktop {
            refresh_token: next_refresh.to_owned(),
            region: region.to_owned(),
            api_region: next_api_region,
            user_agent: user_agent.to_owned(),
            profile_arn: next_profile_arn,
        };
        credentials_from_response(token, &secret)
    }

    async fn refresh_sso(
        &self,
        refresh_token: &str,
        client_id: &str,
        client_secret: &str,
        sso_region: &str,
        api_region: &str,
        profile_arn: Option<&str>,
    ) -> Result<Credentials> {
        validate_region(sso_region)?;
        validate_region(api_region)?;
        let url = sso_token_url(sso_region)?;
        let response = self
            .http
            .post(url)
            .json(&sso_refresh_body(client_id, client_secret, refresh_token))
            .send()
            .await?;
        let token = parse_kiro_token_response(response, "SSO refresh").await?;
        let next_refresh = token.refresh_token.as_deref().unwrap_or(refresh_token);
        let next_profile_arn = token
            .profile_arn
            .clone()
            .or_else(|| profile_arn.map(str::to_owned));
        let next_api_region = next_profile_arn
            .as_deref()
            .and_then(region_from_profile_arn)
            .unwrap_or(api_region)
            .to_owned();
        let secret = KiroRefreshSecret::Sso {
            refresh_token: next_refresh.to_owned(),
            client_id: client_id.to_owned(),
            client_secret: client_secret.to_owned(),
            sso_region: sso_region.to_owned(),
            api_region: next_api_region,
            profile_arn: next_profile_arn,
        };
        credentials_from_response(token, &secret)
    }

    fn desktop_auth_url(&self, region: &str, path: &str) -> Result<String> {
        validate_region(region)?;
        let path = path.trim_start_matches('/');
        if let Some(endpoint) = &self.auth_endpoint_override {
            return Ok(format!("{}/{path}", endpoint.trim_end_matches('/')));
        }
        Ok(format!(
            "https://prod.{region}.auth.desktop.kiro.dev/{path}"
        ))
    }
}

/// Returns the default Kiro CLI `SQLite` database path.
///
/// # Errors
///
/// Returns an error when the user's home directory cannot be resolved.
pub fn default_cli_database_path() -> Result<PathBuf> {
    let candidates = default_cli_database_candidates()?;
    Ok(candidates
        .iter()
        .find(|path| path.exists())
        .cloned()
        .unwrap_or_else(|| candidates[0].clone()))
}

/// Returns the default Kiro IDE desktop token path.
///
/// # Errors
///
/// Returns an error when the user's home directory cannot be resolved.
pub fn default_desktop_token_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(KIRO_DESKTOP_TOKEN_RELATIVE_PATH))
}

/// Parses a Kiro portal callback URL or query string.
///
/// # Errors
///
/// Returns an error when the input does not contain a callback code and
/// `login_option`.
pub fn parse_kiro_authorization_callback(input: &str) -> Result<KiroAuthorizationCallback> {
    let value = input.trim();
    if value.is_empty() {
        return Err(Error::oauth("missing Kiro callback URL"));
    }

    if let Some(url) = parse_callback_url(value)? {
        return callback_from_parts(url.path(), url.query().unwrap_or_default());
    }

    if value.contains("code=") {
        let (path, query) = value
            .split_once('?')
            .map_or((KIRO_AUTH_CALLBACK_PATH, value), |(path, query)| {
                (normalize_callback_path(path), query)
            });
        return callback_from_parts(path, query);
    }

    Err(Error::oauth(
        "paste the full Kiro callback URL, including login_option and code",
    ))
}

impl KiroAuthorizationCallback {
    fn token_exchange_redirect_uri(&self) -> String {
        format!(
            "{KIRO_AUTH_REDIRECT_URI}{}?login_option={}",
            self.path, self.login_option
        )
    }
}

fn credentials_from_cli_values(token_raw: &str, device_raw: Option<&str>) -> Result<Credentials> {
    let token = parse_json_object(token_raw, "Kiro CLI token")?;
    let device = device_raw
        .map(|raw| parse_json_object(raw, "Kiro CLI device registration"))
        .transpose()?;
    let access_token = required_string(&token, &["accessToken", "access_token"])?;
    let refresh_token = required_string(&token, &["refreshToken", "refresh_token"])?;
    let expires_at = parse_expires_at(&token)?;
    let profile_arn = optional_string(&token, &["profileArn", "profile_arn"]);
    let Some(client_id) = optional_string(&token, &["clientId", "client_id"]).or_else(|| {
        device
            .as_ref()
            .and_then(|item| optional_string(item, &["clientId", "client_id"]))
    }) else {
        let region =
            optional_string(&token, &["region"]).unwrap_or_else(|| DEFAULT_REGION.to_owned());
        let api_region = profile_arn
            .as_deref()
            .and_then(region_from_profile_arn)
            .map(str::to_owned)
            .or_else(|| optional_string(&token, &["apiRegion", "api_region"]))
            .unwrap_or_else(|| region.clone());
        validate_region(&region)?;
        validate_region(&api_region)?;
        let user_agent = optional_string(&token, &["userAgent", "user_agent"])
            .unwrap_or_else(default_desktop_user_agent);
        let secret = KiroRefreshSecret::Desktop {
            refresh_token,
            region,
            api_region: Some(api_region),
            user_agent,
            profile_arn,
        };
        return credentials_from_import(access_token, expires_at, &secret);
    };
    let client_secret = optional_string(&token, &["clientSecret", "client_secret"])
        .or_else(|| {
            device
                .as_ref()
                .and_then(|item| optional_string(item, &["clientSecret", "client_secret"]))
        })
        .ok_or_else(|| Error::config("Kiro CLI credentials are missing client_secret"))?;
    let sso_region = optional_string(&token, &["ssoRegion", "sso_region", "region"])
        .or_else(|| {
            device
                .as_ref()
                .and_then(|item| optional_string(item, &["ssoRegion", "sso_region", "region"]))
        })
        .unwrap_or_else(|| DEFAULT_REGION.to_owned());
    let api_region = profile_arn
        .as_deref()
        .and_then(region_from_profile_arn)
        .map(str::to_owned)
        .or_else(|| optional_string(&token, &["apiRegion", "api_region"]))
        .unwrap_or_else(|| sso_region.clone());
    validate_region(&sso_region)?;
    validate_region(&api_region)?;
    let secret = KiroRefreshSecret::Sso {
        refresh_token,
        client_id,
        client_secret,
        sso_region,
        api_region,
        profile_arn,
    };
    credentials_from_import(access_token, expires_at, &secret)
}

fn credentials_from_desktop_json(raw: &str) -> Result<Credentials> {
    let token = parse_json_object(raw, "Kiro desktop token")?;
    let access_token = required_string(&token, &["accessToken", "access_token"])?;
    let refresh_token = required_string(&token, &["refreshToken", "refresh_token"])?;
    let expires_at = parse_expires_at(&token)?;
    let profile_arn = optional_string(&token, &["profileArn", "profile_arn"]);
    let region = optional_string(&token, &["region"]).unwrap_or_else(|| DEFAULT_REGION.to_owned());
    let api_region = profile_arn
        .as_deref()
        .and_then(region_from_profile_arn)
        .map(str::to_owned)
        .or_else(|| optional_string(&token, &["apiRegion", "api_region"]))
        .unwrap_or_else(|| region.clone());
    validate_region(&region)?;
    validate_region(&api_region)?;
    let user_agent = optional_string(&token, &["userAgent", "user_agent"])
        .unwrap_or_else(default_desktop_user_agent);
    let secret = KiroRefreshSecret::Desktop {
        refresh_token,
        region,
        api_region: Some(api_region),
        user_agent,
        profile_arn,
    };
    credentials_from_import(access_token, expires_at, &secret)
}

/// Returns the Kiro profile ARN stored in imported refresh metadata.
///
/// # Errors
///
/// Returns an error if the credentials were not imported by rotom's Kiro
/// adapter or do not contain the required profile ARN.
pub fn profile_arn_from_credentials(credentials: &Credentials) -> Result<String> {
    let secret = decode_refresh_secret(&credentials.refresh_token)?;
    match secret {
        KiroRefreshSecret::Desktop {
            profile_arn: Some(profile_arn),
            ..
        }
        | KiroRefreshSecret::Sso {
            profile_arn: Some(profile_arn),
            ..
        } => Ok(profile_arn),
        KiroRefreshSecret::Desktop { .. } | KiroRefreshSecret::Sso { .. } => Err(Error::config(
            "Kiro credentials are missing profileArn; run `rotom login --kiro` or `rotom kiro import` again",
        )),
    }
}

/// Returns the Kiro API region encoded in imported credentials.
///
/// # Errors
///
/// Returns an error when stored refresh metadata is malformed.
pub fn api_region_from_credentials(credentials: &Credentials) -> Result<String> {
    let secret = decode_refresh_secret(&credentials.refresh_token)?;
    match secret {
        KiroRefreshSecret::Desktop {
            region,
            api_region,
            profile_arn,
            ..
        } => Ok(api_region
            .or_else(|| {
                profile_arn
                    .as_deref()
                    .and_then(region_from_profile_arn)
                    .map(str::to_owned)
            })
            .unwrap_or(region)),
        KiroRefreshSecret::Sso { api_region, .. } => Ok(api_region),
    }
}

fn credentials_from_import(
    access_token: String,
    expires_at: i64,
    secret: &KiroRefreshSecret,
) -> Result<Credentials> {
    Ok(Credentials {
        provider: Provider::Kiro,
        access_token,
        refresh_token: encode_refresh_secret(secret)?,
        expires_at,
        account_id: account_label(secret),
    })
}

fn credentials_from_response(
    token: KiroTokenResponse,
    secret: &KiroRefreshSecret,
) -> Result<Credentials> {
    if token.expires_in <= 0 {
        return Err(Error::oauth(
            "Kiro token response has invalid expiresIn value",
        ));
    }
    Ok(Credentials {
        provider: Provider::Kiro,
        access_token: token.access_token,
        refresh_token: encode_refresh_secret(secret)?,
        expires_at: now_unix().saturating_add(token.expires_in),
        account_id: account_label(secret),
    })
}

async fn parse_kiro_token_response(
    response: reqwest::Response,
    operation: &str,
) -> Result<KiroTokenResponse> {
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(Error::oauth(format!(
            "Kiro {operation} failed with status {status}: {text}"
        )));
    }
    Ok(response.json::<KiroTokenResponse>().await?)
}

fn first_auth_value(connection: &Connection, keys: &[&str]) -> Result<Option<String>> {
    for key in keys {
        if let Some(value) = auth_value(connection, key)? {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

fn auth_value(connection: &Connection, key: &str) -> Result<Option<String>> {
    let mut statement = connection
        .prepare("SELECT value FROM auth_kv WHERE key = ?1 LIMIT 1")
        .map_err(|error| Error::config(format!("failed to read Kiro auth_kv table: {error}")))?;
    match statement.query_row([key], |row| row.get::<_, String>(0)) {
        Ok(value) => Ok(Some(value)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(Error::config(format!(
            "failed to read Kiro auth key {key}: {error}"
        ))),
    }
}

fn parse_json_object(raw: &str, label: &str) -> Result<Value> {
    let value = serde_json::from_str::<Value>(raw)
        .map_err(|error| Error::config(format!("{label} is not valid JSON: {error}")))?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(Error::config(format!("{label} is not a JSON object")))
    }
}

fn required_string(value: &Value, keys: &[&str]) -> Result<String> {
    optional_string(value, keys).ok_or_else(|| {
        Error::config(format!(
            "missing required field {}",
            keys.first().copied().unwrap_or("unknown")
        ))
    })
}

fn optional_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| value.get(*key))
        .find_map(|item| item.as_str().filter(|value| !value.is_empty()))
        .map(str::to_owned)
}

fn parse_expires_at(value: &Value) -> Result<i64> {
    let raw = value
        .get("expiresAt")
        .or_else(|| value.get("expires_at"))
        .or_else(|| value.get("expiration"))
        .ok_or_else(|| Error::config("missing required field expiresAt"))?;
    if let Some(seconds) = raw.as_i64() {
        return Ok(normalize_epoch(seconds));
    }
    if let Some(seconds) = raw.as_u64().and_then(|item| i64::try_from(item).ok()) {
        return Ok(normalize_epoch(seconds));
    }
    if let Some(text) = raw.as_str() {
        if let Ok(seconds) = text.parse::<i64>() {
            return Ok(normalize_epoch(seconds));
        }
        let parsed = DateTime::parse_from_rfc3339(text)
            .map_err(|error| Error::config(format!("invalid expiresAt timestamp: {error}")))?;
        return Ok(parsed.with_timezone(&Utc).timestamp());
    }
    Err(Error::config(
        "expiresAt must be a timestamp or RFC3339 string",
    ))
}

const fn normalize_epoch(value: i64) -> i64 {
    if value > 10_000_000_000 {
        value / 1000
    } else {
        value
    }
}

fn encode_refresh_secret(secret: &KiroRefreshSecret) -> Result<String> {
    Ok(serde_json::to_string(secret)?)
}

fn decode_refresh_secret(value: &str) -> Result<KiroRefreshSecret> {
    serde_json::from_str(value).map_err(|_| {
        Error::config(
            "Kiro credentials are missing refresh metadata; run `rotom login --kiro` or `rotom kiro import`",
        )
    })
}

fn account_label(secret: &KiroRefreshSecret) -> String {
    match secret {
        KiroRefreshSecret::Desktop {
            region,
            api_region,
            profile_arn,
            ..
        } => {
            let label_region = api_region
                .as_deref()
                .or_else(|| profile_arn.as_deref().and_then(region_from_profile_arn))
                .unwrap_or(region);
            format!("kiro-desktop:{label_region}")
        }
        KiroRefreshSecret::Sso {
            sso_region,
            api_region,
            ..
        } => format!("kiro-sso:{sso_region}->{api_region}"),
    }
}

fn create_state(rng: &mut impl RngCore) -> String {
    let mut bytes = [0_u8; 16];
    rng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn parse_callback_url(value: &str) -> Result<Option<Url>> {
    if value.contains("://") {
        return Url::parse(value).map(Some).map_err(Into::into);
    }
    if value.starts_with("localhost") || value.starts_with("127.0.0.1") {
        return Url::parse(&format!("http://{value}"))
            .map(Some)
            .map_err(Into::into);
    }
    Ok(None)
}

fn normalize_callback_path(path: &str) -> &str {
    let trimmed = path.trim();
    if trimmed.is_empty() || !trimmed.starts_with('/') {
        KIRO_AUTH_CALLBACK_PATH
    } else {
        trimmed
    }
}

fn callback_from_parts(path: &str, query: &str) -> Result<KiroAuthorizationCallback> {
    let path = normalize_callback_path(path);
    if path != KIRO_AUTH_CALLBACK_PATH && path != KIRO_SIGNIN_CALLBACK_PATH {
        return Err(Error::oauth(format!(
            "unsupported Kiro callback path: {path}"
        )));
    }
    let query = query.split_once('#').map_or(query, |(query, _)| query);
    let pairs = url::form_urlencoded::parse(query.as_bytes()).collect::<Vec<_>>();
    let code = pairs
        .iter()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| value.to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::oauth("missing Kiro authorization code"))?;
    let login_option = pairs
        .iter()
        .find(|(key, _)| key == "login_option")
        .map(|(_, value)| value.to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::oauth("missing Kiro login_option"))?;
    let state = pairs
        .iter()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.to_string())
        .filter(|value| !value.is_empty());

    Ok(KiroAuthorizationCallback {
        code,
        state,
        login_option,
        path: path.to_owned(),
    })
}

fn validate_social_login_option(login_option: &str) -> Result<()> {
    match login_option {
        "google" | "github" => Ok(()),
        other => Err(Error::oauth(format!(
            "Kiro login_option {other:?} is not supported by `rotom login --kiro`; use Google/GitHub social login or import official Kiro CLI/IDE credentials explicitly"
        ))),
    }
}

fn default_desktop_user_agent() -> String {
    format!(
        "KiroIDE-{DEFAULT_DESKTOP_USER_AGENT_VERSION}-{}",
        auth_machine_fingerprint()
    )
}

fn auth_machine_fingerprint() -> String {
    let seed = env::var("USER")
        .or_else(|_| env::var("USERNAME"))
        .or_else(|_| env::var("HOME"))
        .unwrap_or_else(|_| "rotom".to_owned());
    let digest = Sha256::digest(seed.as_bytes());
    hex::encode(&digest[..8])
}

fn region_from_profile_arn(profile_arn: &str) -> Option<&str> {
    let mut parts = profile_arn.split(':');
    (parts.next()? == "arn").then_some(())?;
    parts.next()?;
    parts.next()?;
    let region = parts.next()?;
    (!region.is_empty()).then_some(region)
}

fn validate_region(region: &str) -> Result<()> {
    let valid = !region.is_empty()
        && region
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(Error::config("Kiro region contains invalid characters"))
    }
}

fn sso_token_url(region: &str) -> Result<String> {
    validate_region(region)?;
    Ok(format!("https://oidc.{region}.amazonaws.com/token"))
}

fn sso_refresh_body(client_id: &str, client_secret: &str, refresh_token: &str) -> Value {
    serde_json::json!({
        "grantType": "refresh_token",
        "clientId": client_id,
        "clientSecret": client_secret,
        "refreshToken": refresh_token,
    })
}

fn home_dir() -> Result<PathBuf> {
    env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| Error::config("HOME is not set; pass a Kiro credential path explicitly"))
}

fn default_cli_database_candidates() -> Result<Vec<PathBuf>> {
    let home = home_dir()?;
    let mut candidates = Vec::new();
    if let Ok(xdg_data_home) = env::var("XDG_DATA_HOME") {
        candidates.push(PathBuf::from(xdg_data_home).join("kiro-cli/data.sqlite3"));
    }
    candidates.push(home.join(KIRO_CLI_MACOS_DATABASE_RELATIVE_PATH));
    candidates.push(home.join(KIRO_CLI_DATABASE_RELATIVE_PATH));
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::TempDir;
    use axum::{
        Json, Router,
        extract::State,
        http::{HeaderMap, header::USER_AGENT as HTTP_USER_AGENT},
        routing::post,
    };
    use reqwest::Client;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::{net::TcpListener, sync::Mutex};

    type CreateTokenCapture = Arc<Mutex<Option<(Value, Option<String>)>>>;

    async fn create_token_handler(
        State(capture): State<CreateTokenCapture>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        let user_agent = headers
            .get(HTTP_USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        *capture.lock().await = Some((body, user_agent));
        Json(json!({
            "accessToken": "new-access",
            "refreshToken": "new-refresh",
            "expiresIn": 3600,
            "profileArn": "arn:aws:codewhisperer:us-west-2:123:profile/test"
        }))
    }

    async fn spawn_create_token_server() -> (String, CreateTokenCapture) {
        let capture = Arc::new(Mutex::new(None));
        let app = Router::new()
            .route("/oauth/token", post(create_token_handler))
            .with_state(capture.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (base_url, capture)
    }

    #[test]
    fn builds_kiro_portal_authorization_url() {
        let flow = KiroOAuthClient::create_authorization_flow().unwrap();
        let pairs = flow
            .authorize_url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();

        assert_eq!(
            flow.authorize_url.as_str().split('?').next().unwrap(),
            KIRO_AUTH_PORTAL_URL
        );
        assert_eq!(pairs.get("redirect_uri").unwrap(), KIRO_AUTH_REDIRECT_URI);
        assert_eq!(pairs.get("redirect_from").unwrap(), KIRO_AUTH_REDIRECT_FROM);
        assert_eq!(pairs.get("code_challenge_method").unwrap(), "S256");
        assert_eq!(flow.state.len(), 32);
        assert_eq!(flow.verifier.len(), 43);
    }

    #[test]
    fn parses_kiro_portal_callback() {
        let callback = parse_kiro_authorization_callback(
            "http://localhost:3128/oauth/callback?login_option=google&code=abc&state=xyz",
        )
        .unwrap();

        assert_eq!(callback.code, "abc");
        assert_eq!(callback.state.as_deref(), Some("xyz"));
        assert_eq!(callback.login_option, "google");
        assert_eq!(callback.path, "/oauth/callback");
        assert_eq!(
            callback.token_exchange_redirect_uri(),
            "http://localhost:3128/oauth/callback?login_option=google"
        );
    }

    #[tokio::test]
    async fn exchange_social_callback_posts_official_json_shape() {
        let (base_url, capture) = spawn_create_token_server().await;
        let callback = parse_kiro_authorization_callback(
            "http://localhost:3128/oauth/callback?login_option=github&code=abc&state=xyz",
        )
        .unwrap();
        let client = KiroOAuthClient::new_with_auth_endpoint(Client::new(), base_url);

        let credentials = client
            .exchange_authorization_callback(&callback, "verifier")
            .await
            .unwrap();

        assert_eq!(credentials.provider, Provider::Kiro);
        assert_eq!(credentials.account_id, "kiro-desktop:us-west-2");
        assert!(!credentials.refresh_token.contains("new-access"));

        let (body, user_agent) = capture.lock().await.clone().unwrap();
        assert_eq!(body["code"], "abc");
        assert_eq!(body["code_verifier"], "verifier");
        assert_eq!(
            body["redirect_uri"],
            "http://localhost:3128/oauth/callback?login_option=github"
        );
        assert!(body.get("invitation_code").is_none());
        assert!(
            user_agent
                .as_deref()
                .is_some_and(|value| value.starts_with("KiroIDE-"))
        );
    }

    #[test]
    fn imports_desktop_json_without_exposing_secret_fields() {
        let credentials = credentials_from_desktop_json(
            r#"{
              "accessToken": "access",
              "refreshToken": "refresh",
              "expiresAt": "2026-05-29T00:00:00Z",
              "profileArn": "arn:aws:codewhisperer:us-west-2:123:profile/test",
              "userAgent": "KiroIDE-2.4.2-test"
            }"#,
        )
        .unwrap();

        assert_eq!(credentials.provider, Provider::Kiro);
        assert_eq!(credentials.account_id, "kiro-desktop:us-west-2");
        assert!(!credentials.refresh_token.contains("access"));
        assert!(credentials.refresh_token.contains("KiroIDE-2.4.2-test"));
    }

    #[test]
    fn imports_cli_sqlite_with_device_registration() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("data.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "CREATE TABLE auth_kv (key TEXT PRIMARY KEY, value TEXT)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO auth_kv (key, value) VALUES (?1, ?2)",
                [
                    "kirocli:social:token",
                    r#"{"accessToken":"access","refreshToken":"refresh","expiresAt":1893456000,"profileArn":"arn:aws:codewhisperer:eu-central-1:123:profile/test"}"#,
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO auth_kv (key, value) VALUES (?1, ?2)",
                [
                    "kirocli:oidc:device-registration",
                    r#"{"clientId":"client","clientSecret":"secret","region":"us-east-1"}"#,
                ],
            )
            .unwrap();

        let credentials = KiroOAuthClient::import_cli_database(&path).unwrap();

        assert_eq!(credentials.provider, Provider::Kiro);
        assert_eq!(credentials.account_id, "kiro-sso:us-east-1->eu-central-1");
        assert!(!credentials.refresh_token.contains("access"));
        assert!(credentials.refresh_token.contains("client"));
        assert!(credentials.refresh_token.contains("secret"));
    }

    #[test]
    fn imports_cli_social_token_without_device_registration() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("data.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "CREATE TABLE auth_kv (key TEXT PRIMARY KEY, value TEXT)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO auth_kv (key, value) VALUES (?1, ?2)",
                [
                    "kirocli:social:token",
                    r#"{"access_token":"access","refresh_token":"refresh","expires_at":1893456000,"profile_arn":"arn:aws:codewhisperer:ap-southeast-1:123:profile/test","provider":"google"}"#,
                ],
            )
            .unwrap();

        let credentials = KiroOAuthClient::import_cli_database(&path).unwrap();

        assert_eq!(credentials.provider, Provider::Kiro);
        assert_eq!(credentials.account_id, "kiro-desktop:ap-southeast-1");
        let secret = decode_refresh_secret(&credentials.refresh_token).unwrap();
        assert_eq!(account_label(&secret), "kiro-desktop:ap-southeast-1");
    }

    #[test]
    fn builds_sso_refresh_body_with_camel_case_json() {
        let body = sso_refresh_body("client", "secret", "old-refresh");

        assert_eq!(body["grantType"], "refresh_token");
        assert_eq!(body["clientId"], "client");
        assert_eq!(body["clientSecret"], "secret");
        assert_eq!(body["refreshToken"], "old-refresh");
        assert_eq!(
            sso_token_url("us-east-1").unwrap(),
            "https://oidc.us-east-1.amazonaws.com/token"
        );
    }

    #[test]
    fn parses_kiro_camel_case_token_response_shape() {
        let token: KiroTokenResponse = serde_json::from_value(serde_json::json!({
            "accessToken": "access",
            "refreshToken": "refresh",
            "expiresIn": 3600,
            "profileArn": "arn:aws:codewhisperer:us-east-1:123:profile/test"
        }))
        .unwrap();

        assert_eq!(token.access_token, "access");
        assert_eq!(token.refresh_token.as_deref(), Some("refresh"));
        assert_eq!(token.expires_in, 3600);
        assert_eq!(
            token.profile_arn.as_deref(),
            Some("arn:aws:codewhisperer:us-east-1:123:profile/test")
        );
    }

    #[test]
    fn rejects_region_injection() {
        assert!(validate_region("us-east-1").is_ok());
        assert!(validate_region("us-east-1.example.com").is_err());
    }
}
