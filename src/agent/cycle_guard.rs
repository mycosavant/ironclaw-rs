//! Tool-call cycle detection for agentic loops.
//!
//! Maintains a sliding window of SHA-256 hashes over tool-call batches.
//! When a repeating pattern is detected the caller can force a text
//! response instead of continuing the tool loop.

use std::collections::VecDeque;

use sha2::{Digest, Sha256};

use crate::llm::ToolCall;

/// Detects repeating tool-call patterns in an agentic loop.
pub struct CycleGuard {
    window: VecDeque<[u8; 32]>,
    window_size: usize,
}

impl CycleGuard {
    /// Create a new guard with the given sliding-window capacity.
    pub fn new(window_size: usize) -> Self {
        Self {
            window: VecDeque::with_capacity(window_size),
            window_size: window_size.max(2),
        }
    }

    /// Returns the configured window size.
    pub fn window_size(&self) -> usize {
        self.window_size
    }

    /// Record a batch of tool calls and check for cycles.
    ///
    /// Returns `true` if a repeating pattern was detected.
    pub fn record_and_check(&mut self, tool_calls: &[ToolCall]) -> bool {
        if tool_calls.is_empty() {
            return false;
        }

        let hash = Self::hash_batch(tool_calls);

        // Evict oldest entry if at capacity.
        if self.window.len() >= self.window_size {
            self.window.pop_front();
        }
        self.window.push_back(hash);

        self.detect_cycle()
    }

    /// Record a batch of tool selections (name + args) and check for cycles.
    ///
    /// This is the `ToolSelection`-friendly variant used by the worker's
    /// `select_tools` path which doesn't produce `ToolCall` structs.
    pub fn record_and_check_selections(
        &mut self,
        selections: &[(&str, &serde_json::Value)],
    ) -> bool {
        if selections.is_empty() {
            return false;
        }

        let hash = Self::hash_pairs(selections);

        if self.window.len() >= self.window_size {
            self.window.pop_front();
        }
        self.window.push_back(hash);

        self.detect_cycle()
    }

    /// SHA-256 hash of a tool-call batch.
    ///
    /// Tool calls are sorted by name for determinism (the LLM may return
    /// the same set in a different order).
    fn hash_batch(tool_calls: &[ToolCall]) -> [u8; 32] {
        let mut sorted: Vec<&ToolCall> = tool_calls.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));

        let mut hasher = Sha256::new();
        for tc in &sorted {
            hasher.update(tc.name.as_bytes());
            hasher.update(b"\x00");
            hasher.update(tc.arguments.to_string().as_bytes());
            hasher.update(b"\x01");
        }
        hasher.finalize().into()
    }

    /// SHA-256 hash of `(name, args)` pairs from `ToolSelection`.
    fn hash_pairs(pairs: &[(&str, &serde_json::Value)]) -> [u8; 32] {
        let mut sorted: Vec<(&str, &serde_json::Value)> = pairs.to_vec();
        sorted.sort_by(|a, b| a.0.cmp(b.0));

        let mut hasher = Sha256::new();
        for (name, args) in &sorted {
            hasher.update(name.as_bytes());
            hasher.update(b"\x00");
            hasher.update(args.to_string().as_bytes());
            hasher.update(b"\x01");
        }
        hasher.finalize().into()
    }

    /// Check if the trailing hashes contain a repeating period.
    ///
    /// For each candidate period `p` (1..=window/2), check whether the
    /// last `p` hashes equal the `p` hashes immediately before them.
    fn detect_cycle(&self) -> bool {
        let len = self.window.len();
        if len < 2 {
            return false;
        }

        let max_period = len / 2;
        for p in 1..=max_period {
            let mut is_cycle = true;
            for i in 0..p {
                if self.window[len - 1 - i] != self.window[len - 1 - p - i] {
                    is_cycle = false;
                    break;
                }
            }
            if is_cycle {
                return true;
            }
        }

        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tool_call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: String::new(),
            name: name.to_string(),
            arguments: args,
        }
    }

    #[test]
    fn identical_batches_trigger_cycle() {
        let mut guard = CycleGuard::new(8);
        let batch = vec![make_tool_call("echo", serde_json::json!({"msg": "hi"}))];

        assert!(!guard.record_and_check(&batch));
        assert!(guard.record_and_check(&batch)); // second identical batch → cycle
    }

    #[test]
    fn varied_batches_no_false_positive() {
        let mut guard = CycleGuard::new(8);
        for i in 0..8 {
            let batch = vec![make_tool_call("echo", serde_json::json!({"n": i}))];
            assert!(!guard.record_and_check(&batch));
        }
    }

    #[test]
    fn two_step_cycle_detected() {
        let mut guard = CycleGuard::new(8);
        let a = vec![make_tool_call("read", serde_json::json!({}))];
        let b = vec![make_tool_call("write", serde_json::json!({}))];

        assert!(!guard.record_and_check(&a));
        assert!(!guard.record_and_check(&b));
        assert!(!guard.record_and_check(&a)); // A B A — not yet a full period-2 cycle
        assert!(guard.record_and_check(&b)); // A B A B — cycle detected (period 2)
    }

    #[test]
    fn empty_tool_calls_ignored() {
        let mut guard = CycleGuard::new(8);
        assert!(!guard.record_and_check(&[]));
        assert!(!guard.record_and_check(&[]));
    }

    #[test]
    fn window_eviction_clears_old_pattern() {
        let mut guard = CycleGuard::new(4);
        let a = vec![make_tool_call("a", serde_json::json!({}))];

        // Fill window with identical hashes
        assert!(!guard.record_and_check(&a));
        assert!(guard.record_and_check(&a));

        // Push enough different values to evict the old pattern
        for i in 0..4 {
            let b = vec![make_tool_call("b", serde_json::json!({"i": i}))];
            guard.record_and_check(&b);
        }

        // Original pattern should no longer be in window
        assert!(!guard.record_and_check(&a));
    }

    #[test]
    fn order_independent_hashing() {
        // Same tools in different order should produce the same hash
        let batch1 = vec![
            make_tool_call("alpha", serde_json::json!({"x": 1})),
            make_tool_call("beta", serde_json::json!({"y": 2})),
        ];
        let batch2 = vec![
            make_tool_call("beta", serde_json::json!({"y": 2})),
            make_tool_call("alpha", serde_json::json!({"x": 1})),
        ];

        assert_eq!(
            CycleGuard::hash_batch(&batch1),
            CycleGuard::hash_batch(&batch2)
        );
    }
}
