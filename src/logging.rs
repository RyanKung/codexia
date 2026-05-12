use crate::{Error, Result};
use serde::Serialize;
use std::fmt;
use tracing::Level;
use tracing_subscriber::FmtSubscriber;

/// Supported runtime logging modes for the local server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogLevel {
    /// Emit warnings and errors.
    #[default]
    Info,
    /// Emit error logs plus one-line HTTP request summaries.
    Debug,
    /// Emit full request, upstream, and streaming trace details.
    Trace,
}

impl LogLevel {
    /// Returns the corresponding tracing verbosity level.
    #[must_use]
    pub const fn tracing_level(self) -> Level {
        match self {
            Self::Info => Level::WARN,
            Self::Debug => Level::DEBUG,
            Self::Trace => Level::TRACE,
        }
    }

    /// Maps a `-v` count to the corresponding logging mode.
    #[must_use]
    pub const fn from_verbosity(verbosity: u8) -> Self {
        match verbosity {
            0 => Self::Info,
            1 => Self::Debug,
            _ => Self::Trace,
        }
    }

    /// Returns the canonical `-v` count used to recreate this mode.
    #[must_use]
    pub const fn verbosity(self) -> u8 {
        match self {
            Self::Info => 0,
            Self::Debug => 1,
            Self::Trace => 2,
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Info => f.write_str("info"),
            Self::Debug => f.write_str("debug"),
            Self::Trace => f.write_str("trace"),
        }
    }
}

/// Installs a global tracing subscriber for the selected log mode.
///
/// # Errors
///
/// Returns an error when the tracing subscriber cannot be installed.
pub fn init(level: LogLevel) -> Result<()> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(level.tracing_level())
        .with_target(level == LogLevel::Trace)
        .with_thread_ids(level == LogLevel::Trace)
        .with_file(level == LogLevel::Trace)
        .with_line_number(level == LogLevel::Trace)
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|_| Error::config("failed to initialize logging subscriber"))
}

/// Emits a trace-level structured JSON payload when trace logging is enabled.
pub fn trace_json<T: Serialize>(event: &str, value: &T) {
    if !tracing::enabled!(Level::TRACE) {
        return;
    }
    match serde_json::to_string(value) {
        Ok(json) => tracing::trace!(event = event, body = %json),
        Err(error) => tracing::trace!(event = event, %error, "failed to serialize trace payload"),
    }
}

/// Emits a trace-level text payload when trace logging is enabled.
pub fn trace_text(event: &str, text: &str) {
    if tracing::enabled!(Level::TRACE) {
        tracing::trace!(event = event, body = %text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_verbosity_to_log_levels() {
        assert_eq!(LogLevel::from_verbosity(0), LogLevel::Info);
        assert_eq!(LogLevel::from_verbosity(1), LogLevel::Debug);
        assert_eq!(LogLevel::from_verbosity(2), LogLevel::Trace);
        assert_eq!(LogLevel::from_verbosity(3), LogLevel::Trace);
        assert_eq!(LogLevel::Trace.verbosity(), 2);
        assert_eq!(LogLevel::Info.tracing_level(), Level::WARN);
    }
}
