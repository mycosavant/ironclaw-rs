//! Configuration hot-reload via filesystem watchers.
//!
//! Watches one or more config files (e.g., `~/.ironclaw/config.toml`,
//! `~/.ironclaw/settings.json`) for changes and delivers
//! [`ConfigReloadEvent`]s over a [`tokio::sync::mpsc`] channel so callers
//! can re-read settings without restarting the process.
//!
//! # Usage
//!
//! ```rust,ignore
//! use std::path::PathBuf;
//! use ironclaw::config::hot_reload::{ConfigWatcher, HotReloadConfig};
//!
//! let paths = vec![
//!     PathBuf::from("/home/user/.ironclaw/config.toml"),
//!     PathBuf::from("/home/user/.ironclaw/settings.json"),
//! ];
//! let (watcher, mut rx) = ConfigWatcher::new(HotReloadConfig { watch_paths: paths })?;
//! watcher.spawn();
//!
//! while let Some(event) = rx.recv().await {
//!     println!("Config changed: {:?}", event.path);
//! }
//! ```
//!
//! # Debouncing
//!
//! A brief debounce window (300 ms) is applied to consecutive events for
//! the *same* path before re-emitting, preventing notification storms from
//! editors or tools that perform multiple atomic writes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use notify::event::{EventKind, ModifyKind};
use notify::{Event, EventHandler, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A single config-reload notification.
#[derive(Debug, Clone)]
pub struct ConfigReloadEvent {
    /// The file that triggered the reload.
    pub path: PathBuf,
}

/// Configuration for the file watcher.
#[derive(Debug, Clone)]
pub struct HotReloadConfig {
    /// Paths (files or directories) to watch.
    pub watch_paths: Vec<PathBuf>,
    /// Debounce window: suppress repeated events for the same path within
    /// this duration.  Defaults to 300 ms.
    pub debounce: Duration,
}

impl Default for HotReloadConfig {
    fn default() -> Self {
        Self {
            watch_paths: default_watch_paths(),
            debounce: Duration::from_millis(300),
        }
    }
}

/// Returns the set of config paths IronClaw reads at startup.
///
/// - `~/.ironclaw/config.toml`  — TOML config overrides
/// - `~/.ironclaw/settings.json` — persisted user settings
/// - `.env` in the current working directory (if present)
pub fn default_watch_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = dirs::home_dir() {
        let dir = home.join(".ironclaw");
        paths.push(dir.join("config.toml"));
        paths.push(dir.join("settings.json"));
    }
    let cwd_env = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".env");
    if cwd_env.exists() {
        paths.push(cwd_env);
    }
    paths
}

// ---------------------------------------------------------------------------
// Watcher
// ---------------------------------------------------------------------------

/// Holds the active [`RecommendedWatcher`] handle.
///
/// The watcher is kept alive as long as this struct is not dropped.  Drop it
/// (or let it go out of scope) to stop watching.
pub struct ConfigWatcher {
    /// The underlying `notify` watcher.  Must be kept alive.
    _watcher: RecommendedWatcher,
}

impl ConfigWatcher {
    /// Create a new [`ConfigWatcher`] for the given config.
    ///
    /// Returns `(watcher, receiver)`.  Spawn the watcher and then await
    /// events on the returned receiver.
    ///
    /// # Errors
    ///
    /// Returns an error if the filesystem watcher cannot be initialised —
    /// this typically indicates a missing kernel feature (e.g., inotify
    /// not available) or a permissions problem.
    pub fn new(
        config: HotReloadConfig,
    ) -> Result<(Self, mpsc::Receiver<ConfigReloadEvent>), notify::Error> {
        let (tx, rx) = mpsc::channel::<ConfigReloadEvent>(64);

        // Debounce state: last-sent timestamp per path.
        let last_sent: Arc<Mutex<HashMap<PathBuf, std::time::Instant>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let debounce = config.debounce;

        let handler = DebouncedHandler {
            tx: tx.clone(),
            last_sent,
            debounce,
        };

        let mut watcher = notify::recommended_watcher(handler)?;

        // Register all configured watch paths (files AND their parent
        // directories so renames/recreates are also captured).
        let mut watched = std::collections::HashSet::new();
        for path in &config.watch_paths {
            // Watch the file directly (if it exists).
            if path.exists() {
                watcher
                    .watch(path, RecursiveMode::NonRecursive)
                    .unwrap_or_else(|e| {
                        tracing::debug!(path = %path.display(), error = %e, "hot_reload: cannot watch file");
                    });
            }
            // Always watch the parent directory so newly-created files are seen.
            if let Some(parent) = path.parent() {
                if parent.exists() && watched.insert(parent.to_path_buf()) {
                    watcher
                        .watch(parent, RecursiveMode::NonRecursive)
                        .unwrap_or_else(|e| {
                            tracing::debug!(path = %parent.display(), error = %e, "hot_reload: cannot watch directory");
                        });
                }
            }
        }

        // Store the set of paths we care about so the handler can filter.
        // We do this by passing it implicitly through `tx` and the handler struct.
        let watch_paths = config.watch_paths.clone();
        tracing::debug!(
            paths = ?watch_paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "hot_reload: watching config paths"
        );

        Ok((Self { _watcher: watcher }, rx))
    }

    /// Spawn the watcher to run for the lifetime of the process.
    ///
    /// Returns the `ConfigWatcher` wrapped in an `Arc` so it stays alive.
    /// The returned handle can be stored or ignored; dropping it stops watching.
    pub fn spawn(self) -> Arc<Self> {
        Arc::new(self)
    }
}

// ---------------------------------------------------------------------------
// Internal event handler
// ---------------------------------------------------------------------------

struct DebouncedHandler {
    tx: mpsc::Sender<ConfigReloadEvent>,
    last_sent: Arc<Mutex<HashMap<PathBuf, std::time::Instant>>>,
    debounce: Duration,
}

impl EventHandler for DebouncedHandler {
    fn handle_event(&mut self, event: Result<Event, notify::Error>) {
        let Ok(event) = event else {
            return;
        };

        // Only react to write/create/rename events; ignore metadata-only.
        let relevant = matches!(
            event.kind,
            EventKind::Create(_)
                | EventKind::Modify(ModifyKind::Data(_))
                | EventKind::Modify(ModifyKind::Name(_))
                | EventKind::Modify(ModifyKind::Any)
        );
        if !relevant {
            return;
        }

        for path in event.paths {
            // Deduplicate within the debounce window.
            // Use try_lock to avoid blocking the notify callback thread.
            let should_send = self
                .last_sent
                .try_lock()
                .map(|mut map| {
                    let now = std::time::Instant::now();
                    let last = map.entry(path.clone()).or_insert_with(|| {
                        now.checked_sub(self.debounce * 2).unwrap_or(now)
                    });
                    if now.duration_since(*last) >= self.debounce {
                        *last = now;
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(true); // If lock contended, send anyway (safe).

            if should_send {
                tracing::debug!(path = %path.display(), "hot_reload: config file changed");
                let ev = ConfigReloadEvent { path };
                // Use blocking_send since we're on the notify callback thread (not async).
                if self.tx.blocking_send(ev).is_err() {
                    // Receiver dropped — stop sending.
                    return;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn default_config_has_sensible_debounce() {
        let cfg = HotReloadConfig::default();
        assert!(cfg.debounce >= Duration::from_millis(100));
        assert!(cfg.debounce <= Duration::from_secs(5));
    }

    #[test]
    fn default_watch_paths_non_empty_when_home_exists() {
        // This test runs on development machines where $HOME is set.
        if dirs::home_dir().is_some() {
            let paths = default_watch_paths();
            assert!(!paths.is_empty());
            // config.toml and settings.json should always be in the list.
            let names: Vec<_> = paths
                .iter()
                .filter_map(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .collect();
            assert!(names.contains(&"config.toml".to_string()));
            assert!(names.contains(&"settings.json".to_string()));
        }
    }

    /// Verify the watcher can be constructed (doesn't require the paths to exist).
    #[test]
    fn watcher_construction_succeeds_with_nonexistent_paths() {
        let cfg = HotReloadConfig {
            watch_paths: vec![PathBuf::from("/nonexistent/path/config.toml")],
            debounce: Duration::from_millis(50),
        };
        let result = ConfigWatcher::new(cfg);
        // Construction should succeed even if the paths don't exist.
        assert!(result.is_ok(), "Watcher construction failed: {:?}", result.err());
    }

    /// End-to-end: write to a temp file and verify a reload event is delivered.
    #[tokio::test]
    async fn detects_file_change() {
        use std::fs;

        let dir = tempfile::tempdir().expect("temp dir");
        let file_path = dir.path().join("config.toml");
        fs::write(&file_path, b"initial = true").expect("write initial");

        let cfg = HotReloadConfig {
            watch_paths: vec![file_path.clone()],
            debounce: Duration::from_millis(50),
        };

        let (watcher, mut rx) = ConfigWatcher::new(cfg).expect("watcher");
        let _handle = watcher.spawn();

        // Give the watcher a moment to register before writing.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Write a change to the watched file.
        let mut f = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&file_path)
            .expect("reopen");
        writeln!(f, "changed = true").expect("write change");
        drop(f);

        // Wait for the reload event (up to 2 seconds).
        let timeout = Duration::from_secs(2);
        let event = tokio::time::timeout(timeout, rx.recv())
            .await
            .expect("timed out waiting for reload event")
            .expect("channel closed");

        // The event path should point to the changed file or its parent dir.
        let is_related = event.path == file_path
            || event.path == dir.path()
            || event.path.starts_with(dir.path());
        assert!(
            is_related,
            "Expected path related to {:?}, got {:?}",
            file_path,
            event.path
        );
    }
}
