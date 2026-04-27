//! Shared time formatting helpers for CLI and HTTP status output.

use crate::config::now_unix;
use chrono::{DateTime, Local, TimeZone};

/// Formats a positive or negative duration into a compact textual form.
pub fn format_duration(total_secs: i64) -> String {
    let total_secs = total_secs.max(0);
    let days = total_secs / 86_400;
    let hours = (total_secs % 86_400) / 3_600;
    let minutes = (total_secs % 3_600) / 60;
    let seconds = total_secs % 60;

    if days > 0 {
        format!("{days}d {hours:02}h {minutes:02}m {seconds:02}s")
    } else if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

/// Formats a Unix timestamp into local wall-clock time.
pub fn format_unix_timestamp_local(timestamp: i64) -> Option<String> {
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|datetime| datetime.format("%Y-%m-%d %H:%M:%S %:z").to_string())
}

/// Formats a status time that may arrive as either a Unix timestamp or RFC3339 string.
pub fn format_status_time_human(value: &str) -> String {
    parse_status_datetime(value)
        .map(format_datetime_with_remaining)
        .unwrap_or_else(|| value.to_owned())
}

/// Formats a status time into local wall-clock time without relative text.
pub fn format_status_time_local(value: &str) -> Option<String> {
    parse_status_datetime(value)
        .map(|datetime| datetime.format("%Y-%m-%d %H:%M:%S %:z").to_string())
}

/// Computes the signed distance in seconds between now and a status timestamp.
pub fn remaining_seconds_for_status_time(value: &str, now_unix: i64) -> Option<i64> {
    parse_status_datetime(value).map(|datetime| datetime.timestamp().saturating_sub(now_unix))
}

fn parse_status_datetime(value: &str) -> Option<DateTime<Local>> {
    if let Ok(timestamp) = value.parse::<i64>() {
        return Local.timestamp_opt(timestamp, 0).single();
    }

    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|datetime| datetime.with_timezone(&Local))
}

fn format_datetime_with_remaining(datetime: DateTime<Local>) -> String {
    let remaining = datetime.timestamp().saturating_sub(now_unix());
    format!(
        "{} ({})",
        datetime.format("%Y-%m-%d %H:%M:%S %:z"),
        if remaining >= 0 {
            format!("in {}", format_duration(remaining))
        } else {
            format!("{} ago", format_duration(-remaining))
        }
    )
}

#[cfg(test)]
mod tests {
    use super::{
        format_duration, format_status_time_human, format_status_time_local,
        format_unix_timestamp_local, remaining_seconds_for_status_time,
    };
    use chrono::{Local, TimeZone, Utc};

    #[test]
    fn formats_token_duration() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(59), "59s");
        assert_eq!(format_duration(60), "1m 00s");
        assert_eq!(format_duration(3_661), "1h 01m 01s");
        assert_eq!(format_duration(90_061), "1d 01h 01m 01s");
    }

    #[test]
    fn clamps_negative_duration_to_zero() {
        assert_eq!(format_duration(-1), "0s");
    }

    #[test]
    fn formats_unix_status_time() {
        let rendered = format_status_time_human("0");
        assert!(rendered.contains("1970-01-01"));
    }

    #[test]
    fn formats_rfc3339_status_time() {
        let datetime = Local.timestamp_opt(1_800_000_000, 0).single().unwrap();
        let rfc3339 = datetime.with_timezone(&Utc).to_rfc3339();
        let rendered = format_status_time_human(&rfc3339);
        let expected_prefix = datetime.format("%Y-%m-%d %H:%M:%S %:z").to_string();
        assert!(rendered.starts_with(&expected_prefix));
    }

    #[test]
    fn formats_local_timestamp_without_relative_text() {
        let rendered = format_unix_timestamp_local(0).unwrap();
        assert_eq!(rendered.len(), "1970-01-01 08:00:00 +08:00".len());
    }

    #[test]
    fn computes_remaining_seconds_for_status_time() {
        let now = 100;
        assert_eq!(remaining_seconds_for_status_time("125", now), Some(25));
    }

    #[test]
    fn formats_status_time_local() {
        let rendered = format_status_time_local("0").unwrap();
        assert!(rendered.contains("1970-01-01"));
    }
}
