//! PID-based gateway lock to prevent multiple instances.
//!
//! Writes a PID file on gateway startup and removes it on shutdown.
//! Checks for stale locks (dead processes) and reclaims them.

use std::path::{Path, PathBuf};

/// A PID file lock for the gateway process.
///
/// When acquired, writes the current PID to `~/.ironclaw/gateway.pid`.
/// On drop or explicit release, removes the PID file.
pub struct PidLock {
    path: PathBuf,
}

impl PidLock {
    /// Default PID file location.
    fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".ironclaw")
            .join("gateway.pid")
    }

    /// Attempt to acquire the PID lock.
    ///
    /// Returns `Ok(PidLock)` if the lock was acquired, or an error if another
    /// gateway instance is running.
    ///
    /// Stale lock files (where the PID no longer corresponds to a running
    /// process) are automatically reclaimed.
    pub fn acquire() -> Result<Self, PidLockError> {
        let path = Self::default_path();
        Self::acquire_at(&path)
    }

    /// Acquire the lock at a specific path (for testing).
    pub fn acquire_at(path: &Path) -> Result<Self, PidLockError> {
        // Check for existing lock
        if path.exists() {
            match std::fs::read_to_string(path) {
                Ok(contents) => {
                    let pid_str = contents.trim();
                    if let Ok(pid) = pid_str.parse::<u32>() {
                        if is_process_running(pid) {
                            return Err(PidLockError::AlreadyRunning { pid });
                        }
                        // Stale lock — process is dead, reclaim it
                        tracing::info!(
                            pid = pid,
                            "Reclaiming stale gateway PID lock (process not running)"
                        );
                    }
                    // Invalid PID content or stale — remove and continue
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "Failed to read existing PID file, attempting to overwrite"
                    );
                }
            }
        }

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| PidLockError::Io {
                action: "create directory",
                source: e,
            })?;
        }

        // Write current PID
        let pid = std::process::id();
        std::fs::write(path, pid.to_string()).map_err(|e| PidLockError::Io {
            action: "write PID file",
            source: e,
        })?;

        tracing::debug!(pid = pid, path = %path.display(), "Gateway PID lock acquired");

        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    /// Release the PID lock, removing the PID file.
    pub fn release(self) {
        // Drop handles cleanup
        drop(self);
    }
}

impl Drop for PidLock {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            // Don't log if the file was already removed (e.g. manual cleanup)
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %e,
                    "Failed to remove gateway PID file"
                );
            }
        } else {
            tracing::debug!(path = %self.path.display(), "Gateway PID lock released");
        }
    }
}

/// Check whether a process with the given PID is running.
fn is_process_running(pid: u32) -> bool {
    // On Unix, sending signal 0 checks process existence without killing it.
    #[cfg(unix)]
    {
        // SAFETY: kill(pid, 0) is safe — it only checks if the process exists.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    // On non-Unix (Windows), use /proc or tasklist as fallback.
    #[cfg(not(unix))]
    {
        // Best-effort: check if /proc/PID exists (WSL, Linux)
        std::path::Path::new(&format!("/proc/{}", pid)).exists()
    }
}

/// Errors from PID lock operations.
#[derive(Debug)]
pub enum PidLockError {
    /// Another gateway instance is already running.
    AlreadyRunning { pid: u32 },
    /// I/O error during lock operations.
    Io {
        action: &'static str,
        source: std::io::Error,
    },
}

impl std::fmt::Display for PidLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning { pid } => {
                write!(
                    f,
                    "another gateway instance is already running (PID {})",
                    pid
                )
            }
            Self::Io { action, source } => {
                write!(f, "failed to {}: {}", action, source)
            }
        }
    }
}

impl std::error::Error for PidLockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_acquire_and_release() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("test.pid");

        let lock = PidLock::acquire_at(&pid_path).unwrap();
        assert!(pid_path.exists());

        let contents = std::fs::read_to_string(&pid_path).unwrap();
        assert_eq!(contents, std::process::id().to_string());

        drop(lock);
        assert!(!pid_path.exists());
    }

    #[test]
    fn test_stale_lock_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("test.pid");

        // Write a PID that doesn't correspond to a running process
        // Use PID 1_999_999 which is very unlikely to be running
        std::fs::write(&pid_path, "1999999").unwrap();

        // Should succeed because the process isn't running
        let lock = PidLock::acquire_at(&pid_path).unwrap();
        let contents = std::fs::read_to_string(&pid_path).unwrap();
        assert_eq!(contents, std::process::id().to_string());
        drop(lock);
    }

    #[test]
    fn test_active_lock_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("test.pid");

        // Write our own PID — it's definitely running
        std::fs::write(&pid_path, std::process::id().to_string()).unwrap();

        let result = PidLock::acquire_at(&pid_path);
        assert!(result.is_err());
        if let Err(PidLockError::AlreadyRunning { pid }) = result {
            assert_eq!(pid, std::process::id());
        } else {
            panic!("Expected AlreadyRunning error");
        }

        // Clean up
        std::fs::remove_file(&pid_path).unwrap();
    }

    #[test]
    fn test_nonexistent_parent_dir_created() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("nested").join("deep").join("test.pid");

        let lock = PidLock::acquire_at(&pid_path).unwrap();
        assert!(pid_path.exists());
        drop(lock);
    }
}
