//! Short-lived gateway session store for HVF-2.
//!
//! A single long-lived [`GATEWAY_AUTH_TOKEN`] is a hard target: leaked once
//! (log file, `ps aux`, crash dump), it provides indefinite access.  This
//! module provides per-session opaque tokens that:
//!
//! * Expire after 24 hours of inactivity (`SESSION_TTL_SECS`)
//! * Support at most [`MAX_GATEWAY_SESSIONS`] concurrent sessions
//! * Are revocable individually without rotating the master token
//!
//! # Flow
//!
//! ```text
//! POST /api/auth/session   (Bearer master-token)
//!     → { session_token, session_id, expires_in_secs }
//!
//! All subsequent requests:
//!     Authorization: Bearer <session_token>
//!     ─── accepted as long as session is valid and not expired ───
//!
//! DELETE /api/auth/session  (Bearer session-token)
//!     → 204 No Content   (session revoked)
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;
use uuid::Uuid;

use crate::channels::web::rbac::Role;

/// Inactivity TTL for gateway sessions (24 hours).
pub const SESSION_TTL_SECS: u64 = 86_400;

/// Maximum number of concurrent active gateway sessions.
pub const MAX_GATEWAY_SESSIONS: usize = 5;

/// Token length: 32 random bytes encoded as 64 lower-hex chars.
const TOKEN_BYTES: usize = 32;

/// One active gateway session.
#[derive(Debug, Clone)]
pub struct GatewaySession {
    /// Stable opaque session identifier (UUID).
    pub session_id: Uuid,
    /// When the session was created.
    pub created_at: Instant,
    /// When the session token was last accepted.
    pub last_used: Instant,
    /// RBAC role assigned to this session.
    pub role: Role,
}

/// Session store: maps opaque 64-char hex token → [`GatewaySession`].
///
/// The store is bounded at [`MAX_GATEWAY_SESSIONS`] entries; when the limit is
/// reached the oldest session (by `last_used`) is evicted before creating a
/// new one.
pub type SessionStore = Arc<Mutex<HashMap<String, GatewaySession>>>;

/// Create a new, empty session store.
pub fn new_session_store() -> SessionStore {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Generate a cryptographically random 64-char hex session token.
fn generate_token() -> String {
    use rand::RngCore as _;
    use std::fmt::Write as _;
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{:02x}", b);
        s
    })
}

/// Create a new session with the given role, evicting the oldest if at capacity.
///
/// Returns `(token, session_id)`.
pub async fn create_session(store: &SessionStore, role: Role) -> (String, Uuid) {
    let mut map = store.lock().await;

    // Evict expired sessions first.
    let now = Instant::now();
    map.retain(|_, s| now.duration_since(s.last_used).as_secs() < SESSION_TTL_SECS);

    // Evict oldest by last_used if still at capacity.
    if map.len() >= MAX_GATEWAY_SESSIONS
        && let Some(oldest_token) = map
            .iter()
            .min_by_key(|(_, s)| s.last_used)
            .map(|(t, _)| t.clone())
    {
        map.remove(&oldest_token);
        tracing::info!("Gateway session evicted (capacity limit)");
    }

    let token = generate_token();
    let session_id = Uuid::new_v4();
    map.insert(
        token.clone(),
        GatewaySession {
            session_id,
            created_at: now,
            last_used: now,
            role,
        },
    );

    tracing::info!(session_id = %session_id, %role, "Gateway session created");
    (token, session_id)
}

/// Validate a token and, if valid, refresh its `last_used` timestamp.
///
/// Returns `(session_id, role)` if the token is present and not expired.
pub async fn validate_and_touch(store: &SessionStore, token: &str) -> Option<(Uuid, Role)> {
    let mut map = store.lock().await;
    if let Some(session) = map.get_mut(token) {
        let elapsed = session.last_used.elapsed().as_secs();
        if elapsed < SESSION_TTL_SECS {
            session.last_used = Instant::now();
            return Some((session.session_id, session.role));
        }
        // Expired — remove it.
        let id = session.session_id;
        map.remove(token);
        tracing::info!(session_id = %id, elapsed_secs = elapsed, "Gateway session expired");
    }
    None
}

/// Revoke a session by its token, if the caller's role is high enough.
///
/// Returns `Ok(true)` if revoked, `Ok(false)` if the token was not found,
/// or `Err(target_role)` if the target session has a higher role than the caller.
pub async fn revoke_session(
    store: &SessionStore,
    token: &str,
    caller_role: Role,
) -> Result<bool, Role> {
    let mut map = store.lock().await;
    if let Some(session) = map.get(token) {
        if session.role > caller_role {
            return Err(session.role);
        }
        let session_id = session.session_id;
        let role = session.role;
        map.remove(token);
        tracing::info!(session_id = %session_id, %role, "Gateway session revoked");
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Revoke a session by its UUID, if the caller's role is high enough.
///
/// Returns `Ok(true)` if revoked, `Ok(false)` if the session ID was not found,
/// or `Err(target_role)` if the target session has a higher role than the caller.
pub async fn revoke_session_by_id(
    store: &SessionStore,
    session_id: Uuid,
    caller_role: Role,
) -> Result<bool, Role> {
    let mut map = store.lock().await;
    // Find the token key for this session ID.
    let token_key = map
        .iter()
        .find(|(_, s)| s.session_id == session_id)
        .map(|(t, _)| t.clone());

    if let Some(token) = token_key {
        let session = &map[&token];
        if session.role > caller_role {
            return Err(session.role);
        }
        let role = session.role;
        map.remove(&token);
        tracing::info!(session_id = %session_id, %role, "Gateway session revoked by ID");
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Return a snapshot of all active sessions (for diagnostics/listing).
pub async fn list_sessions(store: &SessionStore) -> Vec<(Uuid, Instant, Instant, Role)> {
    let map = store.lock().await;
    map.values()
        .map(|s| (s.session_id, s.created_at, s.last_used, s.role))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_create_and_validate_session() {
        let store = new_session_store();
        let (token, id) = create_session(&store, Role::User).await;
        assert_eq!(token.len(), 64);
        let result = validate_and_touch(&store, &token).await;
        assert_eq!(result, Some((id, Role::User)));
    }

    #[tokio::test]
    async fn test_revoke_session() {
        let store = new_session_store();
        let (token, _) = create_session(&store, Role::User).await;
        assert_eq!(revoke_session(&store, &token, Role::Owner).await, Ok(true));
        assert!(validate_and_touch(&store, &token).await.is_none());
        // Double-revoke is a no-op.
        assert_eq!(revoke_session(&store, &token, Role::Owner).await, Ok(false));
    }

    #[tokio::test]
    async fn test_revoke_session_blocked_by_higher_role() {
        let store = new_session_store();
        let (token, _) = create_session(&store, Role::Owner).await;
        // An Admin cannot revoke an Owner session.
        assert!(revoke_session(&store, &token, Role::Admin).await.is_err());
        // Session should still be valid.
        assert!(validate_and_touch(&store, &token).await.is_some());
        // Owner can revoke their own.
        assert_eq!(revoke_session(&store, &token, Role::Owner).await, Ok(true));
    }

    #[tokio::test]
    async fn test_unknown_token_returns_none() {
        let store = new_session_store();
        assert!(validate_and_touch(&store, "notarealtoken").await.is_none());
    }

    #[tokio::test]
    async fn test_capacity_evicts_oldest() {
        let store = new_session_store();
        let mut tokens = Vec::new();
        for _ in 0..MAX_GATEWAY_SESSIONS {
            let (t, _) = create_session(&store, Role::User).await;
            tokens.push(t);
        }
        assert_eq!(store.lock().await.len(), MAX_GATEWAY_SESSIONS);
        // Creating one more should evict the oldest.
        let (new_token, _) = create_session(&store, Role::Admin).await;
        let map = store.lock().await;
        assert_eq!(map.len(), MAX_GATEWAY_SESSIONS);
        assert!(map.contains_key(&new_token));
    }

    #[tokio::test]
    async fn test_session_preserves_role() {
        let store = new_session_store();
        let (token, _) = create_session(&store, Role::Viewer).await;
        let result = validate_and_touch(&store, &token).await;
        assert_eq!(result.map(|(_, r)| r), Some(Role::Viewer));
    }

    #[tokio::test]
    async fn test_revoke_session_by_id() {
        let store = new_session_store();
        let (token, session_id) = create_session(&store, Role::User).await;

        // Admin can revoke a User session by ID.
        assert_eq!(
            revoke_session_by_id(&store, session_id, Role::Admin).await,
            Ok(true)
        );
        // Token should no longer be valid.
        assert!(validate_and_touch(&store, &token).await.is_none());
    }

    #[tokio::test]
    async fn test_revoke_session_by_id_blocked() {
        let store = new_session_store();
        let (token, session_id) = create_session(&store, Role::Owner).await;

        // Admin cannot revoke an Owner session.
        assert!(
            revoke_session_by_id(&store, session_id, Role::Admin)
                .await
                .is_err()
        );
        // Session should still be valid.
        assert!(validate_and_touch(&store, &token).await.is_some());
    }

    #[tokio::test]
    async fn test_revoke_session_by_id_not_found() {
        let store = new_session_store();
        let fake_id = uuid::Uuid::new_v4();
        assert_eq!(
            revoke_session_by_id(&store, fake_id, Role::Owner).await,
            Ok(false)
        );
    }
}
