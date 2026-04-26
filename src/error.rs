use axum::{Json, http::StatusCode, response::IntoResponse};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("URL error: {0}")]
    Url(#[from] url::ParseError),

    #[error("OAuth error: {0}")]
    OAuth(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("upstream error: {0}")]
    Upstream(String),

    #[error("unauthorized")]
    Unauthorized,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn oauth(message: impl Into<String>) -> Self {
        Self::OAuth(message.into())
    }

    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    pub fn upstream(message: impl Into<String>) -> Self {
        Self::Upstream(message.into())
    }

    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Config(_) | Self::OAuth(_) => StatusCode::BAD_REQUEST,
            Self::Upstream(_) | Self::Http(_) => StatusCode::BAD_GATEWAY,
            Self::Io(_) | Self::Json(_) | Self::Url(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> axum::response::Response {
        let status = self.status_code();
        let body = Json(json!({
            "error": {
                "message": self.to_string(),
                "type": match status {
                    StatusCode::UNAUTHORIZED => "authentication_error",
                    StatusCode::BAD_GATEWAY => "upstream_error",
                    _ => "invalid_request_error",
                }
            }
        }));
        (status, body).into_response()
    }
}
