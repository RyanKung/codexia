//! Public and internal data structures for status inspection.

use serde::Serialize;

/// Aggregated account, rate-limit, and warning information returned by the status client.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusSnapshot {
    /// Account metadata when the upstream account endpoint returned it.
    pub account: Option<AccountStatus>,
    /// Normalized rate-limit windows reported by the usage endpoint.
    pub rate_limits: Vec<RateLimitWindow>,
    /// Remaining prepaid or promotional credits, if available.
    pub credits_balance: Option<f64>,
    /// Non-fatal warnings collected while building the snapshot.
    pub warnings: Vec<String>,
}

/// Account-level metadata derived from account and usage endpoints.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccountStatus {
    /// Human-readable account name, if the service reported one.
    pub name: Option<String>,
    /// Primary account email address, if available.
    pub email: Option<String>,
    /// Organization or billing structure label, if available.
    pub structure: Option<String>,
    /// Current subscription or plan name, if available.
    pub plan: Option<String>,
    /// Whether the account currently has an active subscription.
    pub has_active_subscription: Option<bool>,
    /// Subscription expiration time in the upstream string format.
    pub subscription_expires_at: Option<String>,
}

/// A single rate-limit window, such as the main 5-hour bucket or a weekly bucket.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RateLimitWindow {
    /// Stable display name for the rate-limit bucket.
    pub name: String,
    /// Remaining quota as a percentage in the inclusive range `0.0..=100.0`.
    pub remaining_percent: f64,
    /// Reset time in the upstream string format, when provided.
    pub reset_at: Option<String>,
}

/// Internal normalized usage payload built from `/wham/usage`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UsageStatus {
    pub(crate) account: Option<AccountStatus>,
    pub(crate) rate_limits: Vec<RateLimitWindow>,
    pub(crate) credits_balance: Option<f64>,
}
