//! HTTP client for account and rate-limit status endpoints.

use crate::{Error, Result, config::Credentials};
use reqwest::{
    Client,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT},
};
use serde_json::Value;

use super::{
    ACCOUNT_CHECK_VERSION,
    model::{AccountStatus, StatusSnapshot},
    parse::{backend_api_base_url, parse_account_status, parse_usage_status},
};

/// Fetches account and rate-limit state from `ChatGPT` backend endpoints.
#[derive(Clone)]
pub struct StatusClient {
    http: Client,
    base_url: String,
}

impl StatusClient {
    /// Creates a new status client targeting the provided `ChatGPT` backend base URL.
    pub fn new(http: Client, base_url: impl Into<String>) -> Self {
        Self {
            http,
            base_url: base_url.into(),
        }
    }

    /// Fetches account and usage information, preserving partial results as warnings.
    pub async fn fetch_status(&self, credentials: &Credentials) -> StatusSnapshot {
        let mut snapshot = StatusSnapshot {
            account: None,
            rate_limits: Vec::new(),
            credits_balance: None,
            warnings: Vec::new(),
        };

        match self.fetch_account(credentials).await {
            Ok(account) => snapshot.account = Some(account),
            Err(error) => snapshot
                .warnings
                .push(format!("account status unavailable: {error}")),
        }

        match self.fetch_usage(credentials).await {
            Ok(usage) => {
                snapshot.credits_balance = usage.credits_balance;
                snapshot.rate_limits = usage.rate_limits;
                merge_usage_account(snapshot.account.as_mut(), usage.account);
            }
            Err(error) => snapshot
                .warnings
                .push(format!("rate limits unavailable: {error}")),
        }

        snapshot
    }

    async fn fetch_account(&self, credentials: &Credentials) -> Result<AccountStatus> {
        let url = format!(
            "{}/accounts/check/{}",
            backend_api_base_url(&self.base_url),
            ACCOUNT_CHECK_VERSION
        );
        let value = self.get_json(&url, credentials).await?;
        parse_account_status(&value)
    }

    async fn fetch_usage(&self, credentials: &Credentials) -> Result<super::model::UsageStatus> {
        let url = format!("{}/wham/usage", backend_api_base_url(&self.base_url));
        let value = self.get_json(&url, credentials).await?;
        parse_usage_status(&value)
    }

    async fn get_json(&self, url: &str, credentials: &Credentials) -> Result<Value> {
        let response = self
            .http
            .get(url)
            .headers(status_headers(credentials)?)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(Error::upstream(format!(
                "status endpoint returned {status}: {text}"
            )));
        }

        Ok(response.json().await?)
    }
}

fn status_headers(credentials: &Credentials) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        header_value(&format!("Bearer {}", credentials.access_token))?,
    );
    headers.insert(
        HeaderName::from_static("chatgpt-account-id"),
        header_value(&credentials.account_id)?,
    );
    headers.insert(USER_AGENT, HeaderValue::from_static("pi (rust; rotom)"));
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(headers)
}

fn header_value(value: &str) -> Result<HeaderValue> {
    HeaderValue::from_str(value).map_err(|_| Error::config("invalid header value"))
}

fn merge_usage_account(account: Option<&mut AccountStatus>, usage: Option<AccountStatus>) {
    let (Some(account), Some(usage)) = (account, usage) else {
        return;
    };

    if account.email.is_none() {
        account.email = usage.email;
    }
    if account.plan.is_none() {
        account.plan = usage.plan;
    }
}
