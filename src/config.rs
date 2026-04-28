use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// Persisted OAuth credentials used to authenticate API requests.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Credentials {
    /// Bearer token used for authenticated API calls.
    pub access_token: String,
    /// Long-lived token used to mint a new access token.
    pub refresh_token: String,
    /// Access-token expiration timestamp, expressed as Unix seconds.
    pub expires_at: i64,
    /// Upstream account identifier associated with the token pair.
    pub account_id: String,
}

impl Credentials {
    /// Returns whether the credentials should be considered expired at `now_unix`.
    #[must_use]
    pub const fn is_expired_at(&self, now_unix: i64, skew_secs: i64) -> bool {
        self.expires_at.saturating_sub(skew_secs) <= now_unix
    }

    /// Returns whether the credentials are expired relative to the current system time.
    #[must_use]
    pub fn is_expired(&self, skew_secs: i64) -> bool {
        self.is_expired_at(now_unix(), skew_secs)
    }
}

/// Persisted runtime defaults used by `serve` and `daemon install`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AppConfig {
    /// Hostname or IP address the service should bind to.
    #[serde(default)]
    pub bind_host: Option<String>,
    /// TCP port the service should bind to.
    #[serde(default)]
    pub bind_port: Option<u16>,
    /// Override path for the persisted authentication file.
    #[serde(default)]
    pub auth_file: Option<PathBuf>,
    /// Static API key to expose from the local service, when configured.
    #[serde(default)]
    pub api_key: Option<String>,
}

/// Loads and saves persisted OAuth credentials from a single file.
#[derive(Debug, Clone)]
pub struct AuthStore {
    path: PathBuf,
}

impl AuthStore {
    /// Creates a store for credentials at `path`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Returns the default credential file path derived from environment variables.
    ///
    /// # Errors
    ///
    /// Returns an error when neither `CODEXIA_AUTH_FILE` nor a usable home
    /// directory environment variable is available.
    pub fn default_path() -> Result<PathBuf> {
        if let Ok(path) = env::var("CODEXIA_AUTH_FILE") {
            return Ok(PathBuf::from(path));
        }

        let home = env::var("CODEXIA_HOME")
            .or_else(|_| env::var("HOME"))
            .map_err(|_| Error::config("HOME is not set; pass --auth-file explicitly"))?;

        Ok(PathBuf::from(home).join(".codexia").join("auth.json"))
    }

    /// Creates a credential store that uses the default path resolution rules.
    ///
    /// # Errors
    ///
    /// Returns an error when the default credential path cannot be resolved.
    pub fn from_default_path() -> Result<Self> {
        Ok(Self::new(Self::default_path()?))
    }

    /// Returns the on-disk path used by this store.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads credentials from disk, or `None` when the file does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error when the file exists but cannot be read or decoded.
    pub fn load(&self) -> Result<Option<Credentials>> {
        match fs::read_to_string(&self.path) {
            Ok(raw) => Ok(Some(serde_json::from_str(&raw)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Persists credentials to disk using a private temporary file and atomic rename.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent directory cannot be created, the JSON
    /// cannot be serialized, or the file cannot be written atomically.
    pub fn save(&self, credentials: &Credentials) -> Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| Error::config("auth file path has no parent directory"))?;
        fs::create_dir_all(parent)?;

        let tmp = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(credentials)?;
        // Write to a sibling temp file first so a partial write never replaces the live secrets.
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
    /// Creates a store for application configuration at `path`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Returns the default application configuration path.
    ///
    /// # Errors
    ///
    /// Returns an error when the `codexia` home directory cannot be resolved.
    pub fn default_path() -> Result<PathBuf> {
        Ok(codexia_home()?.join("config.json"))
    }

    /// Creates a configuration store that uses the default path resolution rules.
    ///
    /// # Errors
    ///
    /// Returns an error when the default configuration path cannot be resolved.
    pub fn from_default_path() -> Result<Self> {
        Ok(Self::new(Self::default_path()?))
    }

    /// Returns the on-disk path used by this store.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads configuration from disk, or `None` when the file does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error when the file exists but cannot be read or decoded.
    pub fn load(&self) -> Result<Option<AppConfig>> {
        match fs::read_to_string(&self.path) {
            Ok(raw) => Ok(Some(serde_json::from_str(&raw)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Persists configuration to disk using a private temporary file and atomic rename.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent directory cannot be created, the JSON
    /// cannot be serialized, or the file cannot be written atomically.
    pub fn save(&self, config: &AppConfig) -> Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| Error::config("config file path has no parent directory"))?;
        fs::create_dir_all(parent)?;

        let tmp = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(config)?;
        // Use the same temp-file pattern as auth storage so readers never observe truncated JSON.
        write_secret_file(&tmp, &bytes)?;
        fs::rename(tmp, &self.path)?;
        Ok(())
    }

    /// Removes the persisted configuration file when it exists.
    ///
    /// # Errors
    ///
    /// Returns an error when removing an existing file fails for reasons other
    /// than it not being present.
    pub fn delete(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

/// Returns the current Unix timestamp in seconds.
#[must_use]
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
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
    use crate::testsupport::TempDir;

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
            api_key: Some("secret".into()),
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
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("missing.json"));

        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn saves_and_loads_credentials() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::new(dir.path().join("auth.json"));
        let credentials = sample_credentials();

        store.save(&credentials).unwrap();

        assert_eq!(store.load().unwrap(), Some(credentials));
    }

    #[test]
    fn missing_app_config_loads_as_none() {
        let dir = TempDir::new().unwrap();
        let store = AppConfigStore::new(dir.path().join("missing.json"));

        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn saves_and_loads_app_config() {
        let dir = TempDir::new().unwrap();
        let store = AppConfigStore::new(dir.path().join("config.json"));
        let config = sample_app_config();

        store.save(&config).unwrap();

        assert_eq!(store.load().unwrap(), Some(config));
    }

    #[test]
    fn deletes_app_config() {
        let dir = TempDir::new().unwrap();
        let store = AppConfigStore::new(dir.path().join("config.json"));
        let config = sample_app_config();

        store.save(&config).unwrap();
        store.delete().unwrap();

        assert_eq!(store.load().unwrap(), None);
    }
}
