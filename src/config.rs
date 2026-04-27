use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub account_id: String,
}

impl Credentials {
    pub fn is_expired_at(&self, now_unix: i64, skew_secs: i64) -> bool {
        self.expires_at.saturating_sub(skew_secs) <= now_unix
    }

    pub fn is_expired(&self, skew_secs: i64) -> bool {
        self.is_expired_at(now_unix(), skew_secs)
    }
}

/// Persisted runtime defaults used by `serve` and `daemon install`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub bind_host: Option<String>,
    #[serde(default)]
    pub bind_port: Option<u16>,
    #[serde(default)]
    pub auth_file: Option<PathBuf>,
    #[serde(default)]
    pub codex_base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub extra_models: Vec<String>,
    #[serde(default)]
    pub models_file: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct AuthStore {
    path: PathBuf,
}

impl AuthStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn default_path() -> Result<PathBuf> {
        if let Ok(path) = env::var("CODEXIA_AUTH_FILE") {
            return Ok(PathBuf::from(path));
        }

        let home = env::var("CODEXIA_HOME")
            .or_else(|_| env::var("HOME"))
            .map_err(|_| Error::config("HOME is not set; pass --auth-file explicitly"))?;

        Ok(PathBuf::from(home).join(".codexia").join("auth.json"))
    }

    pub fn from_default_path() -> Result<Self> {
        Ok(Self::new(Self::default_path()?))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Option<Credentials>> {
        match fs::read_to_string(&self.path) {
            Ok(raw) => Ok(Some(serde_json::from_str(&raw)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save(&self, credentials: &Credentials) -> Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| Error::config("auth file path has no parent directory"))?;
        fs::create_dir_all(parent)?;

        let tmp = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(credentials)?;
        write_secret_file(&tmp, &bytes)?;
        fs::rename(tmp, &self.path)?;
        Ok(())
    }
}

/// Loads and saves the persisted application configuration file.
#[derive(Debug, Clone)]
pub struct AppConfigStore {
    path: PathBuf,
}

impl AppConfigStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn default_path() -> Result<PathBuf> {
        Ok(codexia_home()?.join("config.json"))
    }

    pub fn from_default_path() -> Result<Self> {
        Ok(Self::new(Self::default_path()?))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Option<AppConfig>> {
        match fs::read_to_string(&self.path) {
            Ok(raw) => Ok(Some(serde_json::from_str(&raw)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save(&self, config: &AppConfig) -> Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| Error::config("config file path has no parent directory"))?;
        fs::create_dir_all(parent)?;

        let tmp = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(config)?;
        write_secret_file(&tmp, &bytes)?;
        fs::rename(tmp, &self.path)?;
        Ok(())
    }

    pub fn delete(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(unix)]
fn write_secret_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::{fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt};

    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_secret_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    fs::write(path, bytes)
}

fn codexia_home() -> Result<PathBuf> {
    if let Ok(path) = env::var("CODEXIA_HOME") {
        return Ok(PathBuf::from(path));
    }

    let home = env::var("HOME")
        .map_err(|_| Error::config("HOME is not set; pass --auth-file explicitly"))?;
    Ok(PathBuf::from(home).join(".codexia"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_credentials() -> Credentials {
        Credentials {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: 123,
            account_id: "acc_1".into(),
        }
    }

    fn sample_app_config() -> AppConfig {
        AppConfig {
            bind_host: Some("127.0.0.1".into()),
            bind_port: Some(14550),
            auth_file: Some(PathBuf::from("/tmp/auth.json")),
            codex_base_url: Some("https://chatgpt.com/backend-api".into()),
            api_key: Some("secret".into()),
            models: vec!["gpt-5.5".into()],
            extra_models: vec!["gpt-5.5-mini".into()],
            models_file: Some(PathBuf::from("/tmp/models.json")),
        }
    }

    #[test]
    fn detects_expiry_with_skew() {
        let credentials = Credentials {
            expires_at: 100,
            ..sample_credentials()
        };

        assert!(credentials.is_expired_at(95, 10));
        assert!(!credentials.is_expired_at(80, 10));
    }

    #[test]
    fn missing_auth_file_loads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::new(dir.path().join("missing.json"));

        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn saves_and_loads_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        let credentials = sample_credentials();

        store.save(&credentials).unwrap();

        assert_eq!(store.load().unwrap(), Some(credentials));
    }

    #[test]
    fn missing_app_config_loads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = AppConfigStore::new(dir.path().join("missing.json"));

        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn saves_and_loads_app_config() {
        let dir = tempfile::tempdir().unwrap();
        let store = AppConfigStore::new(dir.path().join("config.json"));
        let config = sample_app_config();

        store.save(&config).unwrap();

        assert_eq!(store.load().unwrap(), Some(config));
    }

    #[test]
    fn deletes_app_config() {
        let dir = tempfile::tempdir().unwrap();
        let store = AppConfigStore::new(dir.path().join("config.json"));
        let config = sample_app_config();

        store.save(&config).unwrap();
        store.delete().unwrap();

        assert_eq!(store.load().unwrap(), None);
    }
}
