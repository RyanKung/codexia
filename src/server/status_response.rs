//! JSON response builders for `/v1/status`.

use crate::{
    config::Credentials,
    status::{AccountStatus, RateLimitWindow, StatusSnapshot},
    timefmt::{
        format_status_time_local, format_unix_timestamp_local, remaining_seconds_for_status_time,
    },
};
use serde_json::{Value, json};

/// Builds the structured JSON payload returned by the status endpoint.
pub fn build_status_response(credentials: &Credentials, snapshot: &StatusSnapshot) -> Value {
    let now = crate::config::now_unix();

    json!({
        "provider": credentials.provider,
        "account_id": credentials.account_id,
        "token": {
            "expires_at": credentials.expires_at,
            "remaining_seconds": credentials.expires_at.saturating_sub(now),
            "expires_at_local": format_unix_timestamp_local(credentials.expires_at),
        },
        "account": snapshot.account.as_ref().map(|account| account_status_json(account, now)),
        "credits_balance": snapshot.credits_balance,
        "rate_limits": snapshot.rate_limits.iter().map(|window| rate_limit_json(window, now)).collect::<Vec<_>>(),
        "warnings": snapshot.warnings,
    })
}

/// Builds the structured JSON payload for providers without account/rate-limit checks.
pub fn build_unsupported_provider_status_response(credentials: &Credentials) -> Value {
    let provider_name = credentials.provider.display_name();
    let snapshot = StatusSnapshot {
        account: None,
        rate_limits: Vec::new(),
        credits_balance: None,
        warnings: vec![
            format!("{provider_name} account metadata support is not implemented"),
            format!("{provider_name} rate-limit support is not implemented"),
        ],
    };
    build_status_response(credentials, &snapshot)
}

fn account_status_json(account: &AccountStatus, now_unix: i64) -> Value {
    json!({
        "name": account.name,
        "email": account.email,
        "structure": account.structure,
        "plan": account.plan,
        "has_active_subscription": account.has_active_subscription,
        "subscription_expires_at": account.subscription_expires_at,
        "subscription_expires_at_local": account.subscription_expires_at.as_deref().and_then(format_status_time_local),
        "subscription_remaining_seconds": account.subscription_expires_at.as_deref().and_then(|value| remaining_seconds_for_status_time(value, now_unix)),
    })
}

fn rate_limit_json(window: &RateLimitWindow, now_unix: i64) -> Value {
    json!({
        "name": window.name,
        "remaining_percent": window.remaining_percent,
        "reset_at": window.reset_at,
        "reset_at_local": window.reset_at.as_deref().and_then(format_status_time_local),
        "reset_in_seconds": window.reset_at.as_deref().and_then(|value| remaining_seconds_for_status_time(value, now_unix)),
    })
}
