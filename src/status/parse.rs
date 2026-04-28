//! Parsing helpers for account and usage status payloads.

use crate::{Error, Result};
use serde_json::Value;

use super::model::{AccountStatus, RateLimitWindow, UsageStatus};

/// Parses account metadata from the account-check endpoint payload.
pub(super) fn parse_account_status(value: &Value) -> Result<AccountStatus> {
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

/// Parses normalized usage status from the rate-limit endpoint payload.
pub(super) fn parse_usage_status(value: &Value) -> Result<UsageStatus> {
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

pub(super) fn backend_api_base_url(base_url: &str) -> String {
    let normalized = base_url.trim_end_matches('/');
    normalized.strip_suffix("/codex/responses").map_or_else(
        || {
            normalized
                .strip_suffix("/codex")
                .map_or_else(|| normalized.to_owned(), ToOwned::to_owned)
        },
        ToOwned::to_owned,
    )
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
        .or_else(|| value.as_str().and_then(|item| item.parse::<f64>().ok()))
}
