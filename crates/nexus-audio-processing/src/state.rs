use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::{ProcessingConfig, SCHEMA_VERSION};

/// A feature that can be changed independently through the control API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    /// Noise suppression.
    Noise,
    /// Echo cancellation.
    Echo,
}

/// Errors returned by persistent processor state.
#[derive(Debug, Error)]
pub enum StateError {
    /// The state file could not be read.
    #[error("cannot read state file {path}: {source}")]
    Read {
        /// State path.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// The state file could not be decoded.
    #[error("cannot parse state file {path}: {source}")]
    Parse {
        /// State path.
        path: PathBuf,
        /// Underlying JSON error.
        source: serde_json::Error,
    },
    /// The state schema is newer than this crate understands.
    #[error("unsupported state schema version {0}")]
    UnsupportedVersion(u32),
    /// The state file could not be written atomically.
    #[error("cannot write state file {path}: {source}")]
    Write {
        /// State path.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// The lock could not be acquired before the bounded wait expired.
    #[error("timed out waiting for state lock {0}")]
    LockTimeout(PathBuf),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
struct PersistedState {
    version: u32,
    noise_suppression: Option<bool>,
    echo_cancellation: Option<bool>,
}

/// The effective state after merging defaults with persisted overrides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct StateSnapshot {
    /// Effective noise-suppression switch.
    pub noise_suppression: bool,
    /// Effective echo-cancellation switch.
    pub echo_cancellation: bool,
    /// Whether the noise switch came from persisted state.
    pub noise_persisted: bool,
    /// Whether the echo switch came from persisted state.
    pub echo_persisted: bool,
}

impl StateSnapshot {
    /// Returns the effective feature configuration.
    pub const fn processing(&self, defaults: &ProcessingConfig) -> ProcessingConfig {
        ProcessingConfig {
            noise_suppression: self.noise_suppression,
            echo_cancellation: self.echo_cancellation,
            noise_level: defaults.noise_level,
            echo_delay: defaults.echo_delay,
        }
    }
}

/// Atomic, versioned state storage for independent feature toggles.
#[derive(Debug, Clone)]
pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    /// Creates a state store at an explicit path.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Returns the configured state path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads effective state without changing the file.
    pub fn load(&self, defaults: &ProcessingConfig) -> Result<StateSnapshot, StateError> {
        let persisted = self.read_persisted()?;
        Ok(snapshot(defaults, persisted))
    }

    /// Sets one toggle and atomically persists it.
    pub fn set(
        &self,
        toggle: Toggle,
        enabled: bool,
        defaults: &ProcessingConfig,
    ) -> Result<StateSnapshot, StateError> {
        let _lock = LockFile::acquire(&self.lock_path())?;
        let mut persisted = self.read_persisted_unlocked()?;
        match toggle {
            Toggle::Noise => persisted.noise_suppression = Some(enabled),
            Toggle::Echo => persisted.echo_cancellation = Some(enabled),
        }
        persisted.version = SCHEMA_VERSION;
        self.write_persisted(&persisted)?;
        Ok(snapshot(defaults, persisted))
    }

    /// Clears both persisted overrides and restores the configured defaults.
    pub fn reset(&self, defaults: &ProcessingConfig) -> Result<StateSnapshot, StateError> {
        let _lock = LockFile::acquire(&self.lock_path())?;
        let persisted = PersistedState {
            version: SCHEMA_VERSION,
            ..Default::default()
        };
        self.write_persisted(&persisted)?;
        Ok(snapshot(defaults, persisted))
    }

    fn lock_path(&self) -> PathBuf {
        self.path.with_extension("json.lock")
    }

    fn read_persisted(&self) -> Result<PersistedState, StateError> {
        self.read_persisted_unlocked()
    }

    fn read_persisted_unlocked(&self) -> Result<PersistedState, StateError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PersistedState {
                    version: SCHEMA_VERSION,
                    ..Default::default()
                });
            }
            Err(source) => {
                return Err(StateError::Read {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        let state: PersistedState =
            serde_json::from_slice(&bytes).map_err(|source| StateError::Parse {
                path: self.path.clone(),
                source,
            })?;
        if state.version > SCHEMA_VERSION {
            return Err(StateError::UnsupportedVersion(state.version));
        }
        Ok(state)
    }

    fn write_persisted(&self, state: &PersistedState) -> Result<(), StateError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|source| StateError::Write {
                path: self.path.clone(),
                source,
            })?;
            set_private_directory(parent).map_err(|source| StateError::Write {
                path: self.path.clone(),
                source,
            })?;
        }
        let bytes = serde_json::to_vec_pretty(state).expect("persisted state is serializable");
        let temporary = self.path.with_file_name(format!(
            ".{}.tmp-{}",
            file_name(&self.path),
            std::process::id()
        ));
        let result = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)?;
            set_private_file(&file)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, &self.path)?;
            set_private_path(&self.path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(|source| StateError::Write {
            path: self.path.clone(),
            source,
        })
    }
}

fn snapshot(defaults: &ProcessingConfig, persisted: PersistedState) -> StateSnapshot {
    StateSnapshot {
        noise_suppression: persisted
            .noise_suppression
            .unwrap_or(defaults.noise_suppression),
        echo_cancellation: persisted
            .echo_cancellation
            .unwrap_or(defaults.echo_cancellation),
        noise_persisted: persisted.noise_suppression.is_some(),
        echo_persisted: persisted.echo_cancellation.is_some(),
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("processing-state.json")
        .to_string()
}

#[cfg(unix)]
fn set_private_file(file: &File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_file(_file: &File) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn set_private_path(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

struct LockFile {
    path: PathBuf,
}

impl LockFile {
    fn acquire(path: &Path) -> Result<Self, StateError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| StateError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        }
        let deadline = Instant::now() + Duration::from_millis(250);
        loop {
            match OpenOptions::new().create_new(true).write(true).open(path) {
                Ok(file) => {
                    set_private_file(&file).map_err(|source| StateError::Write {
                        path: path.to_path_buf(),
                        source,
                    })?;
                    return Ok(Self {
                        path: path.to_path_buf(),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if Instant::now() >= deadline {
                        return Err(StateError::LockTimeout(path.to_path_buf()));
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(source) => {
                    return Err(StateError::Write {
                        path: path.to_path_buf(),
                        source,
                    });
                }
            }
        }
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Returns the default per-user state path.
pub fn default_state_path() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("goxlr-nexus/processing-v1.json")
}

/// Returns the default per-user control socket path.
pub fn default_control_socket() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("goxlr-nexus/processing.sock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProcessingConfig;

    fn store() -> (tempfile::TempDir, StateStore) {
        let directory = tempfile::tempdir().unwrap();
        let store = StateStore::new(directory.path().join("processing.json"));
        (directory, store)
    }

    #[test]
    fn independent_toggle_overrides_survive_reload() {
        let (_directory, store) = store();
        let defaults = ProcessingConfig::default();
        store.set(Toggle::Noise, true, &defaults).unwrap();
        let snapshot = store.load(&defaults).unwrap();
        assert!(snapshot.noise_suppression);
        assert!(!snapshot.echo_cancellation);
        assert!(snapshot.noise_persisted);
        assert!(!snapshot.echo_persisted);
    }

    #[test]
    fn reset_restores_configured_defaults() {
        let (_directory, store) = store();
        let defaults = ProcessingConfig {
            echo_cancellation: true,
            ..Default::default()
        };
        store.set(Toggle::Echo, false, &defaults).unwrap();
        assert!(!store.load(&defaults).unwrap().echo_cancellation);
        assert!(store.reset(&defaults).unwrap().echo_cancellation);
    }
}
