//! Test-only helpers shared across crate unit tests.

use std::{
    env, fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT_TEMP_DIR_ID: AtomicU64 = AtomicU64::new(0);

/// RAII temporary directory that removes itself when dropped.
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Creates a new unique temporary directory below the system temp root.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory cannot be created.
    pub fn new() -> io::Result<Self> {
        let mut path = env::temp_dir();
        path.push(format!(
            "codexia-test-{}-{}-{}",
            std::process::id(),
            unix_timestamp_nanos(),
            NEXT_TEMP_DIR_ID.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    /// Returns the directory path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[must_use]
fn unix_timestamp_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos())
}
