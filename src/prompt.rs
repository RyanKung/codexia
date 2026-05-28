use rotom::{Error, Result, config::AppConfig, oauth::parse_authorization_input};
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};

pub fn config_string(
    config: Option<&AppConfig>,
    map: impl FnOnce(&AppConfig) -> Option<String>,
) -> Option<String> {
    config.and_then(map)
}

pub fn prompt_string(label: &str, default: &str) -> Result<String> {
    print!("{label} [{default}]: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let value = input.trim();
    Ok(if value.is_empty() {
        default.to_owned()
    } else {
        value.to_owned()
    })
}

pub fn prompt_optional_string(label: &str, default: Option<&str>) -> Result<Option<String>> {
    let suffix = default.map(|item| format!(" [{item}]")).unwrap_or_default();
    print!("{label}{suffix}: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let value = input.trim();
    if value.is_empty() {
        Ok(default.map(str::to_owned).filter(|item| !item.is_empty()))
    } else {
        Ok(Some(value.to_owned()))
    }
}

pub fn prompt_optional_path(label: &str, default: Option<&Path>) -> Result<Option<PathBuf>> {
    let suffix = default
        .map(|item| format!(" [{}]", item.display()))
        .unwrap_or_default();
    print!("{label}{suffix}: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let value = input.trim();
    if value.is_empty() {
        Ok(default.map(ToOwned::to_owned))
    } else {
        Ok(Some(PathBuf::from(value)))
    }
}

pub fn prompt_port(label: &str, default: u16) -> Result<u16> {
    let value = prompt_string(label, &default.to_string())?;
    value
        .parse::<u16>()
        .map_err(|_| Error::config(format!("invalid port: {value}")))
}

/// Reads the pasted OAuth callback URL or raw authorization code from stdin.
pub fn prompt_authorization_code(expected_state: &str) -> Result<String> {
    print!("Paste the full redirect URL or authorization code: ");
    std::io::stdout().flush()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let parsed = parse_authorization_input(&input);
    if parsed
        .state
        .as_deref()
        .is_some_and(|state| state != expected_state)
    {
        return Err(Error::oauth("state mismatch"));
    }

    parsed
        .code
        .ok_or_else(|| Error::oauth("missing authorization code"))
}
