//! Public and internal data structures for status inspection.

use serde::Serialize;

/// Aggregated account, rate-limit, and warning information returned by the status client.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusSnapshot {
    pub account: Option<AccountStatus>,
    pub rate_limits: Vec<RateLimitWindow>,
    pub credits_balance: Option<f64>,
    pub warnings: Vec<String>,
}

/// Account-level metadata derived from account and usage endpoints.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccountStatus {
    pub name: Option<String>,
    pub email: Option<String>,
    pub structure: Option<String>,
    pub plan: Option<String>,
    pub has_active_subscription: Option<bool>,
    pub subscription_expires_at: Option<String>,
}

/// A single rate-limit window, such as the main 5-hour bucket or a weekly bucket.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RateLimitWindow {
    pub name: String,
    pub remaining_percent: f64,
    pub reset_at: Option<String>,
}

/// Internal normalized usage payload built from `/wham/usage`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UsageStatus {
    pub(crate) account: Option<AccountStatus>,
    pub(crate) rate_limits: Vec<RateLimitWindow>,
    pub(crate) credits_balance: Option<f64>,
}
