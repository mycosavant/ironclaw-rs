//! Background channel health monitor.
//!
//! Spawns a task that periodically calls `health_check_all()` on the
//! [`ChannelManager`] and tracks per-channel health state. On state
//! transitions (healthy → degraded → failed, or recovery) it logs and
//! optionally injects a system notification into the agent loop so the
//! operator can be made aware through the configured notification channel.
//!
//! # State machine
//!
//! ```text
//! Healthy ──(N failures)──▶ Degraded ──(threshold)──▶ Failed
//!    ▲──────────(success)──────┘◀───────────(success)────┘
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::channels::{ChannelManager, IncomingMessage};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the channel health watchdog.
#[derive(Debug, Clone)]
pub struct HealthMonitorConfig {
    /// How often to poll each channel's health endpoint.
    pub interval: Duration,
    /// Number of consecutive failures before a channel is considered degraded.
    pub degraded_threshold: u32,
    /// Number of consecutive failures before a channel is considered failed.
    pub failed_threshold: u32,
    /// `user_id` to use on injected system notification messages.
    /// Defaults to `"system"`.
    pub notify_user_id: String,
}

impl Default for HealthMonitorConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(60),
            degraded_threshold: 2,
            failed_threshold: 5,
            notify_user_id: "system".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Health state
// ---------------------------------------------------------------------------

/// Health status of a single channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelStatus {
    /// Not yet checked — initial state before the first poll completes.
    /// This prevents a spurious "recovered" log if the first check succeeds.
    Unknown,
    Healthy,
    Degraded,
    Failed,
}

impl ChannelStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
        }
    }
}

/// Running health record for a channel.
#[derive(Debug, Clone)]
struct ChannelHealth {
    status: ChannelStatus,
    consecutive_failures: u32,
    last_checked: DateTime<Utc>,
    /// When the status last transitioned.
    last_transition: DateTime<Utc>,
}

impl ChannelHealth {
    fn new() -> Self {
        let now = Utc::now();
        Self {
            status: ChannelStatus::Unknown,
            consecutive_failures: 0,
            last_checked: now,
            last_transition: now,
        }
    }
}

// ---------------------------------------------------------------------------
// Monitor
// ---------------------------------------------------------------------------

/// Watchdog that periodically checks channel health and emits notifications.
pub struct ChannelHealthMonitor {
    manager: Arc<ChannelManager>,
    config: HealthMonitorConfig,
}

impl ChannelHealthMonitor {
    /// Create a new monitor for the given `ChannelManager`.
    pub fn new(manager: Arc<ChannelManager>, config: HealthMonitorConfig) -> Self {
        Self { manager, config }
    }

    /// Spawn the watchdog as a background tokio task.
    ///
    /// The returned `JoinHandle` can be ignored — the task runs until the
    /// process exits.  Assign it to a variable if you want the ability to abort
    /// it explicitly.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(self.run())
    }

    async fn run(self) {
        let mut interval = tokio::time::interval(self.config.interval);
        // Skip the first (immediate) tick so we don't run before channels start.
        interval.tick().await;

        let mut state: HashMap<String, ChannelHealth> = HashMap::new();
        let inject = self.manager.inject_sender();

        loop {
            interval.tick().await;

            let results = self.manager.health_check_all().await;

            for (name, outcome) in &results {
                let health = state.entry(name.clone()).or_insert_with(ChannelHealth::new);
                health.last_checked = Utc::now();

                match outcome {
                    Ok(()) => {
                        // "Recovered" only makes sense after a confirmed Degraded/Failed
                        // state. Unknown (first check) and ongoing Healthy transitions
                        // must not produce a spurious recovery notification.
                        let recovered = matches!(
                            health.status,
                            ChannelStatus::Degraded | ChannelStatus::Failed
                        );
                        let prev_failures = health.consecutive_failures;
                        health.consecutive_failures = 0;

                        if recovered {
                            let prev = health.status.as_str();
                            health.status = ChannelStatus::Healthy;
                            health.last_transition = Utc::now();

                            tracing::info!(
                                channel = %name,
                                previous_status = prev,
                                failures_cleared = prev_failures,
                                "Channel recovered"
                            );

                            let msg = format!(
                                "[channel-monitor] Channel `{}` recovered (was {}, {} failure(s))",
                                name, prev, prev_failures
                            );
                            notify(&inject, &self.config.notify_user_id, msg).await;
                        }
                    }

                    Err(err) => {
                        health.consecutive_failures += 1;
                        let cf = health.consecutive_failures;

                        let new_status = if cf >= self.config.failed_threshold {
                            ChannelStatus::Failed
                        } else if cf >= self.config.degraded_threshold {
                            ChannelStatus::Degraded
                        } else {
                            ChannelStatus::Healthy
                        };

                        let transitioned = new_status != health.status;
                        if transitioned {
                            let prev = health.status.as_str();
                            health.status = new_status;
                            health.last_transition = Utc::now();

                            match new_status {
                                ChannelStatus::Degraded => {
                                    tracing::warn!(
                                        channel = %name,
                                        consecutive_failures = cf,
                                        error = %err,
                                        "Channel degraded"
                                    );
                                    let msg = format!(
                                        "[channel-monitor] Channel `{}` {} → degraded after {} failure(s): {}",
                                        name, prev, cf, err
                                    );
                                    notify(&inject, &self.config.notify_user_id, msg).await;
                                }
                                ChannelStatus::Failed => {
                                    tracing::error!(
                                        channel = %name,
                                        consecutive_failures = cf,
                                        error = %err,
                                        "Channel failed"
                                    );
                                    let msg = format!(
                                        "[channel-monitor] Channel `{}` FAILED after {} consecutive failure(s): {}",
                                        name, cf, err
                                    );
                                    notify(&inject, &self.config.notify_user_id, msg).await;
                                }
                                ChannelStatus::Healthy => {}
                                // Unknown is only an initial placeholder; it is never
                                // assigned as a `new_status` in the failure branch.
                                ChannelStatus::Unknown => {
                                    unreachable!("new_status cannot be Unknown")
                                }
                            }
                        } else {
                            // Same state — periodic log at lower severity.
                            tracing::debug!(
                                channel = %name,
                                status = health.status.as_str(),
                                consecutive_failures = cf,
                                error = %err,
                                "Channel health check failed (state unchanged)"
                            );
                        }
                    }
                }
            }

            // Prune entries for channels that no longer exist.
            state.retain(|name, _| results.contains_key(name));
        }
    }
}

/// Send a system notification into the agent loop via the injection channel.
async fn notify(tx: &tokio::sync::mpsc::Sender<IncomingMessage>, user_id: &str, content: String) {
    let msg = IncomingMessage::new("system", user_id, content);
    if let Err(e) = tx.send(msg).await {
        tracing::warn!("health_monitor: failed to inject notification: {}", e);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn config_defaults_are_sane() {
        let cfg = HealthMonitorConfig::default();
        assert!(cfg.degraded_threshold < cfg.failed_threshold);
        assert!(cfg.interval >= Duration::from_secs(1));
        assert_eq!(cfg.notify_user_id, "system");
    }

    #[test]
    fn channel_health_initial_state() {
        let h = ChannelHealth::new();
        // Initial state is Unknown, not Healthy, so the first successful check
        // does not emit a spurious "recovered" notification.
        assert_eq!(h.status, ChannelStatus::Unknown);
        assert_eq!(h.consecutive_failures, 0);
    }

    #[test]
    fn channel_status_as_str() {
        assert_eq!(ChannelStatus::Unknown.as_str(), "unknown");
        assert_eq!(ChannelStatus::Healthy.as_str(), "healthy");
        assert_eq!(ChannelStatus::Degraded.as_str(), "degraded");
        assert_eq!(ChannelStatus::Failed.as_str(), "failed");
    }

    /// Verify threshold logic: first failure is below degraded_threshold (2),
    /// second hits degraded, fifth hits failed.
    #[test]
    fn threshold_logic() {
        let cfg = HealthMonitorConfig::default(); // degraded=2, failed=5

        let failures_to_status = |cf: u32| {
            if cf >= cfg.failed_threshold {
                ChannelStatus::Failed
            } else if cf >= cfg.degraded_threshold {
                ChannelStatus::Degraded
            } else {
                ChannelStatus::Healthy
            }
        };

        assert_eq!(failures_to_status(1), ChannelStatus::Healthy);
        assert_eq!(failures_to_status(2), ChannelStatus::Degraded);
        assert_eq!(failures_to_status(4), ChannelStatus::Degraded);
        assert_eq!(failures_to_status(5), ChannelStatus::Failed);
        assert_eq!(failures_to_status(100), ChannelStatus::Failed);
    }

    #[test]
    fn notify_user_id_customizable() {
        let cfg = HealthMonitorConfig {
            notify_user_id: "admin".to_string(),
            ..Default::default()
        };
        assert_eq!(cfg.notify_user_id, "admin");
    }

    /// Counter that increments each call to verify async notify sends.
    #[tokio::test]
    async fn notify_sends_message() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<IncomingMessage>(8);
        let counter = Arc::new(AtomicU32::new(0));
        let counter2 = Arc::clone(&counter);

        tokio::spawn(async move {
            while rx.recv().await.is_some() {
                counter2.fetch_add(1, Ordering::Relaxed);
            }
        });

        notify(&tx, "system", "test notification".to_string()).await;
        notify(&tx, "system", "second notification".to_string()).await;

        // Give the spawned task a moment to process.
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(counter.load(Ordering::Relaxed), 2);
    }
}
