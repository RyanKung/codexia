//! Local API key authorization helpers for the HTTP server.

use crate::{Error, error::Result};
use axum::http::HeaderMap;

/// Validates the configured local API key against either `Authorization` or `x-api-key`.
///
/// # Errors
///
/// Returns [`Error::Unauthorized`](crate::Error::Unauthorized) when a local API
/// key is configured and neither header contains the expected secret.
pub fn authorize(headers: &HeaderMap, api_key: Option<&str>) -> Result<()> {
    let Some(expected) = api_key else {
        return Ok(());
    };

    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let x_api_key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok());

    if bearer == Some(expected) || x_api_key == Some(expected) {
        Ok(())
    } else {
        Err(Error::Unauthorized)
    }
}
