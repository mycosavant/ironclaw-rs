//! Prompt injection circuit breaker.
//!
//! Watches for repeated high-severity injection warnings produced by
//! [`crate::safety::Sanitizer`] during a single job's tool execution loop.
//! When the count of detected injection attempts reaches the configured
//! `threshold`, the breaker **trips** and signals the caller to abort
//! further tool execution for that job.
//!
//! # Design
//!
//! One `InjectionCircuitBreaker` instance is created **per job** (held on
//! [`crate::agent::worker::Worker`]).  Sharing is not necessary: each job
//! runs independently and tracks its own threat exposure.
//!
//! After every call to `sanitize_tool_output` that returns one or more
//! warnings whose severity is `High` or `Critical`, the caller invokes
//! [`InjectionCircuitBreaker::record_warnings`].  Before executing the *next*
//! tool call the caller checks [`InjectionCircuitBreaker::is_tripped`].  When
//! tripped, the job should be aborted and the user notified.
//!
//! ## Thread safety
//!
//! [`AtomicU32`](std::sync::atomic::AtomicU32) provides lock-free interior
//! mutability on `&self` references and makes `InjectionCircuitBreaker` both
//! `Send` and `Sync`.  This is necessary because `Worker` is owned by a single
//! tokio task, so `Send` is required, and `&self` method calls inside `async`
//! blocks require `Sync`.
//!
//! # Thresholds
//!
//! | Scenario                          | Recommended threshold |
//! |-----------------------------------|-----------------------|
//! | Production (default)              | 5                     |
//! | Strict (high-value workflows)     | 2                     |
//! | Permissive (trusted environments) | 10                    |
//!
//! The default is 5 — enough to avoid false-positive trips on noisy data
//! while catching adversarial patterns early.

use std::sync::atomic::{AtomicU32, Ordering};

use crate::safety::policy::Severity;
use crate::safety::sanitizer::InjectionWarning;

/// Prompt injection circuit breaker for a single job execution.
///
/// Create one instance per [`crate::agent::worker::Worker`] and call
/// [`record_warnings`](Self::record_warnings) after every sanitized tool
/// output.  Check [`is_tripped`](Self::is_tripped) before allowing further
/// tool execution.
///
/// Uses [`AtomicU32`] internally so `record_warnings` and `count` can be
/// called on a shared reference (`&self`), matching the call pattern in the
/// worker's `&self` async methods, while remaining `Send + Sync`.
pub struct InjectionCircuitBreaker {
    /// Number of injection detections that trips the breaker.
    threshold: u32,
    /// Running count of high- or critical-severity injection detections.
    count: AtomicU32,
}

impl InjectionCircuitBreaker {
    /// Create a breaker with the given threshold.
    ///
    /// # Panics
    ///
    /// Panics if `threshold` is 0 (a zero threshold would trip immediately on
    /// any output, making tool execution impossible).
    pub fn new(threshold: u32) -> Self {
        assert!(
            threshold > 0,
            "InjectionCircuitBreaker threshold must be > 0"
        );
        Self {
            threshold,
            count: AtomicU32::new(0),
        }
    }

    /// Create a breaker with the recommended default threshold of 5.
    pub fn default_threshold() -> Self {
        Self::new(5)
    }

    /// Record zero or more injection warnings from a single tool output pass.
    ///
    /// Only warnings whose severity is [`Severity::High`] or
    /// [`Severity::Critical`] increment the counter.  Lower-severity warnings
    /// are informational and do not count toward tripping.
    pub fn record_warnings(&self, warnings: &[InjectionWarning]) {
        let high_count = warnings
            .iter()
            .filter(|w| w.severity >= Severity::High)
            .count();
        self.count.fetch_add(high_count as u32, Ordering::Relaxed);
    }

    /// Returns `true` when the accumulated injection count has reached or
    /// exceeded the configured threshold.
    ///
    /// Callers should check this **before** executing the next tool call
    /// and abort the job if it returns `true`.
    pub fn is_tripped(&self) -> bool {
        self.count.load(Ordering::Relaxed) >= self.threshold
    }

    /// Current number of recorded high-severity injection detections.
    pub fn count(&self) -> u32 {
        self.count.load(Ordering::Relaxed)
    }

    /// Threshold at which the breaker trips.
    pub fn threshold(&self) -> u32 {
        self.threshold
    }

    /// Reset the breaker (used after a deliberate admin override or in tests).
    pub fn reset(&self) {
        self.count.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use super::*;
    use crate::safety::policy::Severity;
    use crate::safety::sanitizer::InjectionWarning;

    fn warning(severity: Severity) -> InjectionWarning {
        InjectionWarning {
            pattern: "test".to_string(),
            severity,
            location: Range { start: 0, end: 1 },
            description: "test warning".to_string(),
        }
    }

    #[test]
    fn test_not_tripped_initially() {
        let breaker = InjectionCircuitBreaker::default_threshold();
        assert!(!breaker.is_tripped());
        assert_eq!(breaker.count(), 0);
    }

    #[test]
    fn test_low_medium_warnings_do_not_count() {
        let breaker = InjectionCircuitBreaker::new(1);
        breaker.record_warnings(&[warning(Severity::Low), warning(Severity::Medium)]);
        assert!(!breaker.is_tripped(), "Low/Medium should not trip breaker");
        assert_eq!(breaker.count(), 0);
    }

    #[test]
    fn test_high_severity_counts() {
        let breaker = InjectionCircuitBreaker::new(3);
        breaker.record_warnings(&[warning(Severity::High)]);
        assert_eq!(breaker.count(), 1);
        assert!(!breaker.is_tripped());

        breaker.record_warnings(&[warning(Severity::High), warning(Severity::Critical)]);
        assert_eq!(breaker.count(), 3);
        assert!(breaker.is_tripped());
    }

    #[test]
    fn test_trips_at_threshold() {
        let breaker = InjectionCircuitBreaker::new(5);
        for _ in 0..4 {
            breaker.record_warnings(&[warning(Severity::Critical)]);
            assert!(!breaker.is_tripped());
        }
        breaker.record_warnings(&[warning(Severity::Critical)]);
        assert!(breaker.is_tripped());
        assert_eq!(breaker.count(), 5);
    }

    #[test]
    fn test_stays_tripped_after_threshold() {
        let breaker = InjectionCircuitBreaker::new(2);
        breaker.record_warnings(&[warning(Severity::High), warning(Severity::High)]);
        assert!(breaker.is_tripped());
        // Recording more warnings after tripping should keep it tripped
        breaker.record_warnings(&[warning(Severity::Low)]);
        assert!(breaker.is_tripped());
    }

    #[test]
    fn test_reset_clears_count() {
        let breaker = InjectionCircuitBreaker::new(2);
        breaker.record_warnings(&[warning(Severity::High), warning(Severity::High)]);
        assert!(breaker.is_tripped());
        breaker.reset();
        assert!(!breaker.is_tripped());
        assert_eq!(breaker.count(), 0);
    }

    #[test]
    fn test_threshold_accessor() {
        let breaker = InjectionCircuitBreaker::new(7);
        assert_eq!(breaker.threshold(), 7);
    }

    #[test]
    fn test_empty_warnings_do_not_change_count() {
        let breaker = InjectionCircuitBreaker::new(3);
        breaker.record_warnings(&[]);
        assert_eq!(breaker.count(), 0);
    }

    #[test]
    #[should_panic(expected = "threshold must be > 0")]
    fn test_zero_threshold_panics() {
        InjectionCircuitBreaker::new(0);
    }

    /// Mixed severity batch: only High and Critical count.
    #[test]
    fn test_mixed_severity_batch() {
        let breaker = InjectionCircuitBreaker::new(10);
        breaker.record_warnings(&[
            warning(Severity::Low),
            warning(Severity::High),
            warning(Severity::Medium),
            warning(Severity::Critical),
            warning(Severity::Low),
        ]);
        assert_eq!(breaker.count(), 2, "Only High+Critical should count");
    }
}
