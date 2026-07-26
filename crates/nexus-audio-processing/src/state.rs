use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
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
#[non_exhaustive]
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

/// Errors resolving the per-user state and control locations.
#[derive(Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum DefaultPathError {
    /// No secure state directory could be derived from the environment.
    #[error("cannot determine a per-user state directory; set XDG_STATE_HOME or HOME")]
    StateDirectory,
    /// No secure runtime directory could be derived from the environment.
    #[error("cannot determine a per-user runtime directory; set XDG_RUNTIME_DIR")]
    RuntimeDirectory,
    /// An environment-provided directory was relative and therefore unsafe.
    #[error("{0} must be an absolute path")]
    RelativeDirectory(&'static str),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
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
    noise_suppression: bool,
    /// Effective echo-cancellation switch.
    echo_cancellation: bool,
    /// Whether the noise switch came from persisted state.
    noise_persisted: bool,
    /// Whether the echo switch came from persisted state.
    echo_persisted: bool,
}

impl StateSnapshot {
    #[cfg(feature = "pipewire-runtime")]
    pub(crate) const fn from_defaults(defaults: &ProcessingConfig) -> Self {
        Self {
            noise_suppression: defaults.noise_suppression,
            echo_cancellation: defaults.echo_cancellation,
            noise_persisted: false,
            echo_persisted: false,
        }
    }

    /// Returns the effective noise-suppression switch.
    pub const fn noise_suppression(&self) -> bool {
        self.noise_suppression
    }

    /// Returns the effective echo-cancellation switch.
    pub const fn echo_cancellation(&self) -> bool {
        self.echo_cancellation
    }

    /// Returns whether noise suppression was explicitly persisted.
    pub const fn noise_persisted(&self) -> bool {
        self.noise_persisted
    }

    /// Returns whether echo cancellation was explicitly persisted.
    pub const fn echo_persisted(&self) -> bool {
        self.echo_persisted
    }

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
        let bytes = serde_json::to_vec_pretty(state).map_err(|source| StateError::Write {
            path: self.path.clone(),
            source: io::Error::other(source),
        })?;
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
    _file: File,
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
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|source| StateError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        set_private_file(&file).map_err(|source| StateError::Write {
            path: path.to_path_buf(),
            source,
        })?;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(StateError::LockTimeout(path.to_path_buf()));
                }
                Err(TryLockError::Error(source)) => {
                    return Err(StateError::Write {
                        path: path.to_path_buf(),
                        source,
                    });
                }
            }
        }
    }
}

/// Returns the default per-user state path.
pub fn default_state_path() -> Result<PathBuf, DefaultPathError> {
    let directory = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .ok_or(DefaultPathError::StateDirectory)?;
    require_absolute(directory, "XDG_STATE_HOME or HOME")
        .map(|path| path.join("sonix/processing-v1.json"))
}

/// Returns the default per-user control socket path.
pub fn default_control_socket() -> Result<PathBuf, DefaultPathError> {
    let directory = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or(DefaultPathError::RuntimeDirectory)?;
    require_absolute(directory, "XDG_RUNTIME_DIR").map(|path| path.join("sonix/processing.sock"))
}

fn require_absolute(path: PathBuf, variable: &'static str) -> Result<PathBuf, DefaultPathError> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(DefaultPathError::RelativeDirectory(variable))
    }
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
        assert!(snapshot.noise_suppression());
        assert!(!snapshot.echo_cancellation());
        assert!(snapshot.noise_persisted());
        assert!(!snapshot.echo_persisted());
    }

    #[test]
    fn reset_restores_configured_defaults() {
        let (_directory, store) = store();
        let defaults = ProcessingConfig {
            echo_cancellation: true,
            ..Default::default()
        };
        store.set(Toggle::Echo, false, &defaults).unwrap();
        assert!(!store.load(&defaults).unwrap().echo_cancellation());
        assert!(store.reset(&defaults).unwrap().echo_cancellation());
    }

    #[test]
    fn rejects_newer_state_versions() {
        let (_directory, store) = store();
        fs::write(store.path(), br#"{"version":2}"#).unwrap();
        assert!(matches!(
            store.load(&ProcessingConfig::default()),
            Err(StateError::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn rejects_relative_default_directories() {
        assert_eq!(
            require_absolute(PathBuf::from("relative"), "XDG_RUNTIME_DIR"),
            Err(DefaultPathError::RelativeDirectory("XDG_RUNTIME_DIR"))
        );
    }

    #[test]
    fn rejects_corrupt_state() {
        let (_directory, store) = store();
        fs::write(store.path(), b"not-json").unwrap();
        assert!(matches!(
            store.load(&ProcessingConfig::default()),
            Err(StateError::Parse { .. })
        ));
    }
}
