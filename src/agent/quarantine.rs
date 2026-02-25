//! Prompt injection circuit breaker with quarantine mode.
//!
//! After `QUARANTINE_THRESHOLD` consecutive turns that each produce at least one
//! High-or-Critical injection warning from tool output sanitization, the thread
//! enters quarantine:
//!
//! * All further tool calls are suppressed (the LLM sees only the canned notice).
//! * The user must type the exact string `/unquarantine` to lift the restriction.
//! * Quarantine also auto-expires after `QUARANTINE_DURATION_SECS` as a fallback.
//!
//! The exit path intentionally requires explicit human text input — the LLM cannot
//! trigger it via a tool call — preventing a malicious payload from self-escaping.

use std::time::{Duration, Instant};

use crate::safety::Severity;

/// Number of consecutive High+ turns required to enter quarantine.
pub const QUARANTINE_THRESHOLD: u32 = 3;

/// Auto-expiry of quarantine even without user confirmation (5 minutes).
const QUARANTINE_DURATION_SECS: u64 = 300;

/// The exact command that lifts quarantine (must come from a human turn).
pub const QUARANTINE_EXIT_COMMAND: &str = "/unquarantine";

/// Canned message shown every time the user sends input while quarantined.
pub const QUARANTINE_NOTICE: &str =
    "⚠️  QUARANTINE ACTIVE — Multiple high-severity prompt injection attempts \
     were detected in recent tool outputs. Tool calls are suspended to protect \
     your data.\n\n\
     When you are ready to resume normal operation, type: /unquarantine";

/// Per-thread prompt injection circuit breaker state.
///
/// This struct is intentionally **not** `Serialize`/`Deserialize`; it is
/// stored on [`Thread`](super::session::Thread) with `#[serde(skip)]` so
/// quarantine state resets cleanly on process restart.
#[derive(Debug, Clone)]
pub struct InjectionCounter {
    /// How many consecutive turns ended with at least one High+ warning.
    consecutive_high_severity: u32,
    /// `Some(until)` when quarantine is active; `None` otherwise.
    quarantine_until: Option<Instant>,
    /// Whether the current in-progress turn has seen a High+ warning yet.
    current_turn_had_high: bool,
}

impl Default for InjectionCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl InjectionCounter {
    /// Create a new, clean counter.
    pub fn new() -> Self {
        Self {
            consecutive_high_severity: 0,
            quarantine_until: None,
            current_turn_had_high: false,
        }
    }

    /// Call at the start of each new agentic turn to clear the per-turn flag.
    pub fn begin_turn(&mut self) {
        self.current_turn_had_high = false;
    }

    /// Call each time a tool output is sanitized.  Records whether this turn
    /// has seen a High-or-Critical severity warning.
    pub fn record_warning_severity(&mut self, severity: &Severity) {
        if *severity >= Severity::High {
            self.current_turn_had_high = true;
        }
    }

    /// Call at the end of a completed agentic turn (LLM produced a text response).
    ///
    /// Returns `true` if this call just triggered quarantine.
    pub fn end_turn(&mut self) -> bool {
        if self.current_turn_had_high {
            self.consecutive_high_severity += 1;
            if self.consecutive_high_severity >= QUARANTINE_THRESHOLD
                && self.quarantine_until.is_none()
            {
                self.quarantine_until = Some(
                    Instant::now() + Duration::from_secs(QUARANTINE_DURATION_SECS),
                );
                return true;
            }
        } else {
            // Safe turn: reset consecutive counter.
            self.consecutive_high_severity = 0;
        }
        false
    }

    /// Returns `true` if the thread is currently quarantined.
    pub fn is_quarantined(&self) -> bool {
        match self.quarantine_until {
            Some(until) => Instant::now() < until,
            None => false,
        }
    }

    /// Attempt to exit quarantine based on the user's literal input string.
    ///
    /// Returns `true` if quarantine was successfully lifted.  Only the exact
    /// command phrase lifts quarantine; any LLM-generated content cannot.
    pub fn try_exit(&mut self, input: &str) -> bool {
        if input.trim() == QUARANTINE_EXIT_COMMAND && self.quarantine_until.is_some() {
            self.quarantine_until = None;
            self.consecutive_high_severity = 0;
            true
        } else {
            false
        }
    }

    /// How many consecutive high-severity turns have been recorded (for logging).
    pub fn consecutive_count(&self) -> u32 {
        self.consecutive_high_severity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_quarantine_below_threshold() {
        let mut c = InjectionCounter::new();
        for _ in 0..QUARANTINE_THRESHOLD - 1 {
            c.begin_turn();
            c.record_warning_severity(&Severity::High);
            let triggered = c.end_turn();
            assert!(!triggered);
        }
        assert!(!c.is_quarantined());
    }

    #[test]
    fn test_quarantine_at_threshold() {
        let mut c = InjectionCounter::new();
        let mut triggered = false;
        for _ in 0..QUARANTINE_THRESHOLD {
            c.begin_turn();
            c.record_warning_severity(&Severity::Critical);
            triggered = c.end_turn();
        }
        assert!(triggered);
        assert!(c.is_quarantined());
    }

    #[test]
    fn test_safe_turn_resets_consecutive() {
        let mut c = InjectionCounter::new();
        for _ in 0..QUARANTINE_THRESHOLD - 1 {
            c.begin_turn();
            c.record_warning_severity(&Severity::High);
            c.end_turn();
        }
        // One safe turn resets the counter
        c.begin_turn();
        c.end_turn();
        assert_eq!(c.consecutive_count(), 0);
        assert!(!c.is_quarantined());
    }

    #[test]
    fn test_try_exit_lifts_quarantine() {
        let mut c = InjectionCounter::new();
        for _ in 0..QUARANTINE_THRESHOLD {
            c.begin_turn();
            c.record_warning_severity(&Severity::High);
            c.end_turn();
        }
        assert!(c.is_quarantined());

        // Wrong command does nothing
        assert!(!c.try_exit("wrong"));
        assert!(c.is_quarantined());

        // Correct command lifts it
        assert!(c.try_exit(QUARANTINE_EXIT_COMMAND));
        assert!(!c.is_quarantined());
        assert_eq!(c.consecutive_count(), 0);
    }

    #[test]
    fn test_medium_severity_does_not_count() {
        let mut c = InjectionCounter::new();
        for _ in 0..QUARANTINE_THRESHOLD + 5 {
            c.begin_turn();
            c.record_warning_severity(&Severity::Medium);
            c.end_turn();
        }
        assert!(!c.is_quarantined());
        assert_eq!(c.consecutive_count(), 0);
    }
}
