use crate::{Error, Result, config::Credentials};
use reqwest::{
    Client,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT},
};
use serde::Serialize;
use serde_json::Value;

const ACCOUNT_CHECK_VERSION: &str = "v4-2023-04-27";

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusSnapshot {
    pub account: Option<AccountStatus>,
    pub rate_limits: Vec<RateLimitWindow>,
    pub credits_balance: Option<f64>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccountStatus {
    pub name: Option<String>,
    pub email: Option<String>,
    pub structure: Option<String>,
    pub plan: Option<String>,
    pub has_active_subscription: Option<bool>,
    pub subscription_expires_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RateLimitWindow {
    pub name: String,
    pub remaining_percent: f64,
    pub reset_at: Option<String>,
}

#[derive(Clone)]
pub struct StatusClient {
    http: Client,
    base_url: String,
}

impl StatusClient {
    pub fn new(http: Client, base_url: impl Into<String>) -> Self {
        Self {
            http,
            base_url: base_url.into(),
        }
    }

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

    async fn fetch_usage(&self, credentials: &Credentials) -> Result<UsageStatus> {
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
    headers.insert(USER_AGENT, HeaderValue::from_static("pi (rust; codexia)"));
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(headers)
}

fn header_value(value: &str) -> Result<HeaderValue> {
    HeaderValue::from_str(value).map_err(|_| Error::config("invalid header value"))
}

fn backend_api_base_url(base_url: &str) -> String {
    let normalized = base_url.trim_end_matches('/');
    if let Some(value) = normalized.strip_suffix("/codex/responses") {
        value.to_owned()
    } else if let Some(value) = normalized.strip_suffix("/codex") {
        value.to_owned()
    } else {
        normalized.to_owned()
    }
}

fn parse_account_status(value: &Value) -> Result<AccountStatus> {
    let default = value
        .pointer("/accounts/default")
        .ok_or_else(|| Error::upstream("account response is missing /accounts/default"))?;

    let account = default.get("account");
    let entitlement = default.get("entitlement");

    Ok(AccountStatus {
        name: account
            .and_then(|item| item.get("name"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        email: default
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_owned),
        structure: account
            .and_then(|item| item.get("structure"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        plan: entitlement
            .and_then(|item| item.get("subscription_plan"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        has_active_subscription: entitlement
            .and_then(|item| item.get("has_active_subscription"))
            .and_then(Value::as_bool),
        subscription_expires_at: entitlement
            .and_then(|item| item.get("expires_at"))
            .and_then(string_like_value),
    })
}

#[derive(Debug, Clone, PartialEq)]
struct UsageStatus {
    account: Option<AccountStatus>,
    rate_limits: Vec<RateLimitWindow>,
    credits_balance: Option<f64>,
}

fn parse_usage_status(value: &Value) -> Result<UsageStatus> {
    let mut windows = Vec::new();

    if let Some(rate_limit) = value.get("rate_limit") {
        if let Some(window) = rate_limit
            .get("primary_window")
            .and_then(|item| parse_rate_limit_window("5h", item))
        {
            windows.push(window);
        }
        if let Some(window) = rate_limit
            .get("secondary_window")
            .and_then(|item| parse_rate_limit_window("weekly", item))
        {
            windows.push(window);
        }
    }

    if let Some(additional) = value
        .get("additional_rate_limits")
        .and_then(Value::as_array)
    {
        for item in additional {
            let name = item
                .get("limit_name")
                .and_then(Value::as_str)
                .unwrap_or("additional");
            if let Some(rate_limit) = item.get("rate_limit") {
                if let Some(window) = rate_limit
                    .get("primary_window")
                    .and_then(|entry| parse_rate_limit_window(&format!("{name} 5h"), entry))
                {
                    windows.push(window);
                }
                if let Some(window) = rate_limit
                    .get("secondary_window")
                    .and_then(|entry| parse_rate_limit_window(&format!("{name} weekly"), entry))
                {
                    windows.push(window);
                }
            }
        }
    }

    if windows.is_empty() {
        return Err(Error::upstream(
            "rate-limit response did not include recognizable windows",
        ));
    }

    Ok(UsageStatus {
        account: Some(AccountStatus {
            name: None,
            email: value
                .get("email")
                .and_then(Value::as_str)
                .map(str::to_owned),
            structure: None,
            plan: value
                .get("plan_type")
                .and_then(Value::as_str)
                .map(str::to_owned),
            has_active_subscription: None,
            subscription_expires_at: None,
        }),
        rate_limits: windows,
        credits_balance: value
            .pointer("/credits/balance")
            .and_then(number_like_value),
    })
}

fn parse_rate_limit_window(name: &str, value: &Value) -> Option<RateLimitWindow> {
    let remaining_percent = value
        .get("remaining_percent")
        .and_then(Value::as_f64)
        .or_else(|| {
            value
                .get("used_percent")
                .and_then(Value::as_f64)
                .map(|used| (100.0 - used).max(0.0))
        })?;

    Some(RateLimitWindow {
        name: name.to_owned(),
        remaining_percent,
        reset_at: value.get("reset_at").and_then(string_like_value),
    })
}

fn string_like_value(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_i64().map(|item| item.to_string()))
        .or_else(|| value.as_u64().map(|item| item.to_string()))
        .or_else(|| value.as_f64().map(|item| item.to_string()))
}

fn number_like_value(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_i64().map(|item| item as f64))
        .or_else(|| value.as_u64().map(|item| item as f64))
        .or_else(|| value.as_str().and_then(|item| item.parse::<f64>().ok()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::get};
    use serde_json::json;
    use tokio::net::TcpListener;

    fn sample_credentials() -> Credentials {
        Credentials {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: 1,
            account_id: "acc_1".into(),
        }
    }

    #[test]
    fn strips_codex_suffix_from_base_url() {
        assert_eq!(
            backend_api_base_url("https://chatgpt.com/backend-api/codex/responses"),
            "https://chatgpt.com/backend-api"
        );
        assert_eq!(
            backend_api_base_url("https://chatgpt.com/backend-api/codex"),
            "https://chatgpt.com/backend-api"
        );
        assert_eq!(
            backend_api_base_url("https://chatgpt.com/backend-api"),
            "https://chatgpt.com/backend-api"
        );
    }

    #[test]
    fn parses_account_status() {
        let account = parse_account_status(&json!({
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
        .unwrap();

        assert_eq!(account.plan.as_deref(), Some("chatgptplus"));
        assert_eq!(account.structure.as_deref(), Some("personal"));
        assert_eq!(account.email, None);
        assert_eq!(
            account.subscription_expires_at.as_deref(),
            Some("2026-05-01T00:00:00Z")
        );
    }

    #[test]
    fn parses_usage_status() {
        let usage = parse_usage_status(&json!({
            "email": "test@example.com",
            "plan_type": "pro",
            "rate_limit": {
                "primary_window": {
                    "used_percent": 12,
                    "reset_at": "2026-04-27T12:00:00Z"
                },
                "secondary_window": {
                    "remaining_percent": 94,
                    "reset_at": "2026-05-01T00:00:00Z"
                }
            },
            "credits": { "balance": "7.5" }
        }))
        .unwrap();

        assert_eq!(usage.account.unwrap().plan.as_deref(), Some("pro"));
        assert_eq!(usage.credits_balance, Some(7.5));
        assert_eq!(usage.rate_limits.len(), 2);
        assert_eq!(usage.rate_limits[0].remaining_percent, 88.0);
        assert_eq!(usage.rate_limits[1].remaining_percent, 94.0);
    }

    #[tokio::test]
    async fn fetches_partial_status_with_warnings() {
        async fn account_handler() -> Json<Value> {
            Json(json!({
                "accounts": {
                    "default": {
                        "account": { "structure": "personal" },
                        "entitlement": {
                            "subscription_plan": "chatgptplus",
                            "has_active_subscription": true
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
                    "primary_window": { "used_percent": 10 },
                    "secondary_window": { "remaining_percent": 90 }
                },
                "credits": { "balance": 1 }
            }))
        }

        let app = Router::new()
            .route(
                &format!("/accounts/check/{ACCOUNT_CHECK_VERSION}"),
                get(account_handler),
            )
            .route("/wham/usage", get(usage_handler));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = StatusClient::new(Client::new(), base_url);
        let snapshot = client.fetch_status(&sample_credentials()).await;

        assert!(snapshot.warnings.is_empty());
        assert_eq!(
            snapshot.account.unwrap().plan.as_deref(),
            Some("chatgptplus")
        );
        assert_eq!(snapshot.rate_limits.len(), 2);
        assert_eq!(snapshot.credits_balance, Some(1.0));
    }
}
