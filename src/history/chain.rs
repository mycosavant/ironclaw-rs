//! Hash-chain verification for the action audit trail.
//!
//! Each `ActionRecord` carries a blake3 `content_hash` and a `prev_hash`
//! that links to the preceding action, forming a tamper-evident chain.

use uuid::Uuid;

use crate::context::ActionRecord;

/// Error returned when chain verification fails.
#[derive(Debug, thiserror::Error)]
pub enum ChainError {
    #[error("Action at sequence {sequence} has no content_hash")]
    MissingHash { sequence: u32 },
    #[error(
        "Action at sequence {sequence} has incorrect content_hash (expected {expected}, got {actual})"
    )]
    HashMismatch {
        sequence: u32,
        expected: String,
        actual: String,
    },
    #[error(
        "Action at sequence {sequence} has incorrect prev_hash (expected {expected}, got {actual})"
    )]
    LinkBroken {
        sequence: u32,
        expected: String,
        actual: String,
    },
}

/// Verify the hash chain for a job's action records.
///
/// Re-computes each action's content hash and validates the `prev_hash`
/// linkage. Returns `Ok(())` if the chain is intact, or the first error
/// found.
///
/// Actions with `None` hashes (pre-upgrade records) are silently skipped.
pub fn verify_chain(job_id: Uuid, actions: &[ActionRecord]) -> Result<(), ChainError> {
    let mut prev_hash: Option<&str> = None;

    for action in actions {
        let Some(ref stored_hash) = action.content_hash else {
            // Pre-upgrade action without hashes — skip verification.
            prev_hash = None;
            continue;
        };

        // Re-compute the expected content hash.
        let canonical = format!(
            "{}|{}|{}|{}|{}",
            job_id,
            action.sequence,
            action.tool_name,
            action.input,
            action.executed_at.to_rfc3339(),
        );
        let expected = format!("blake3:{}", blake3::hash(canonical.as_bytes()).to_hex());

        if *stored_hash != expected {
            return Err(ChainError::HashMismatch {
                sequence: action.sequence,
                expected,
                actual: stored_hash.clone(),
            });
        }

        // Verify the backward link.
        let stored_prev = action.prev_hash.as_deref().unwrap_or("");
        let expected_prev = prev_hash.unwrap_or("");
        if stored_prev != expected_prev {
            return Err(ChainError::LinkBroken {
                sequence: action.sequence,
                expected: expected_prev.to_string(),
                actual: stored_prev.to_string(),
            });
        }

        prev_hash = Some(stored_hash.as_str());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_action(seq: u32, tool: &str) -> ActionRecord {
        ActionRecord::new(seq, tool, serde_json::json!({"x": seq}))
    }

    #[test]
    fn verify_valid_chain() {
        let job_id = Uuid::new_v4();
        let mut actions = Vec::new();

        let mut a0 = make_action(0, "echo");
        a0.seal(job_id, None);
        actions.push(a0);

        let mut a1 = make_action(1, "shell");
        a1.seal(job_id, actions[0].content_hash.as_deref());
        actions.push(a1);

        let mut a2 = make_action(2, "echo");
        a2.seal(job_id, actions[1].content_hash.as_deref());
        actions.push(a2);

        assert!(verify_chain(job_id, &actions).is_ok());
    }

    #[test]
    fn detect_tampered_hash() {
        let job_id = Uuid::new_v4();
        let mut a0 = make_action(0, "echo");
        a0.seal(job_id, None);
        a0.content_hash = Some("blake3:0000".to_string());

        assert!(matches!(
            verify_chain(job_id, &[a0]),
            Err(ChainError::HashMismatch { sequence: 0, .. })
        ));
    }

    #[test]
    fn detect_broken_link() {
        let job_id = Uuid::new_v4();
        let mut a0 = make_action(0, "echo");
        a0.seal(job_id, None);

        let mut a1 = make_action(1, "shell");
        a1.seal(job_id, Some("blake3:bogus"));

        assert!(matches!(
            verify_chain(job_id, &[a0, a1]),
            Err(ChainError::LinkBroken { sequence: 1, .. })
        ));
    }

    #[test]
    fn genesis_action_has_no_prev_hash() {
        let job_id = Uuid::new_v4();
        let mut a0 = make_action(0, "echo");
        a0.seal(job_id, None);

        assert!(a0.prev_hash.is_none());
        assert!(verify_chain(job_id, &[a0]).is_ok());
    }

    #[test]
    fn seal_is_deterministic() {
        let job_id = Uuid::new_v4();
        let mut a = make_action(0, "echo");
        let mut b = a.clone();

        a.seal(job_id, None);
        b.seal(job_id, None);

        assert_eq!(a.content_hash, b.content_hash);
    }

    #[test]
    fn skip_pre_upgrade_actions() {
        let job_id = Uuid::new_v4();
        // Action without hashes (pre-upgrade)
        let old = make_action(0, "echo");
        assert!(old.content_hash.is_none());

        assert!(verify_chain(job_id, &[old]).is_ok());
    }
}
