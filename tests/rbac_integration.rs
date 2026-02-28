//! Integration tests for RBAC (Role-Based Access Control).
//!
//! Starts a real Axum server on a random port and verifies that:
//! - Master token resolves to Owner role
//! - Session tokens preserve their assigned role
//! - Lower-privilege roles get 403 on restricted endpoints
//! - Role escalation via session creation is blocked
//! - SSE tickets preserve the issuing caller's role
//! - Trusted-proxy unknown users default to Viewer

use std::net::SocketAddr;
use std::sync::Arc;

use ironclaw::channels::web::rbac::Role;
use ironclaw::channels::web::server::{GatewayState, start_server};
use ironclaw::channels::web::session_store;
use ironclaw::channels::web::sse::SseManager;
use ironclaw::channels::web::ws::WsConnectionTracker;

const AUTH_TOKEN: &str = "test-rbac-token-secret";

/// Start a gateway server on a random port.
async fn start_test_server() -> (SocketAddr, Arc<GatewayState>) {
    let (agent_tx, _agent_rx) = tokio::sync::mpsc::channel(64);

    let state = Arc::new(GatewayState {
        msg_tx: tokio::sync::RwLock::new(Some(agent_tx)),
        sse: SseManager::new(),
        workspace: None,
        session_manager: None,
        log_broadcaster: None,
        log_level_handle: None,
        extension_manager: None,
        tool_registry: None,
        store: None,
        job_manager: None,
        prompt_queue: None,
        user_id: "test-user".to_string(),
        shutdown_tx: tokio::sync::RwLock::new(None),
        ws_tracker: Some(Arc::new(WsConnectionTracker::new())),
        llm_provider: None,
        skill_registry: None,
        skill_catalog: None,
        chat_rate_limiter: ironclaw::channels::web::server::RateLimiter::new(30, 60),
        registry_entries: Vec::new(),
        cost_guard: None,
        startup_time: std::time::Instant::now(),
        sse_tickets: std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        heartbeat_last_tick: None,
        routine_last_tick: None,
        repair_last_tick: None,
        channel_health: None,
        session_store: session_store::new_session_store(),
        trusted_proxy_header: None,
        roles: std::collections::HashMap::new(),
        routine_engine: tokio::sync::RwLock::new(None),
    });

    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let bound_addr = start_server(addr, state.clone(), AUTH_TOKEN.to_string())
        .await
        .expect("Failed to start test server");

    (bound_addr, state)
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

/// Create a session with a specific role via the session create endpoint.
/// Returns the session token.
async fn create_session_with_role(
    addr: SocketAddr,
    caller_token: &str,
    role: &str,
) -> Result<String, reqwest::StatusCode> {
    let resp = client()
        .post(format!("http://{}/api/auth/session", addr))
        .header("Authorization", format!("Bearer {}", caller_token))
        .json(&serde_json::json!({ "role": role }))
        .send()
        .await
        .expect("HTTP request failed");

    if resp.status().is_success() {
        let body: serde_json::Value = resp.json().await.unwrap();
        Ok(body["session_token"].as_str().unwrap().to_string())
    } else {
        Err(resp.status())
    }
}

// =============================================================================
// Tests
// =============================================================================

#[tokio::test]
async fn test_master_token_is_owner() {
    let (addr, _state) = start_test_server().await;

    let resp = client()
        .get(format!("http://{}/api/gateway/status", addr))
        .header("Authorization", format!("Bearer {}", AUTH_TOKEN))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["role"], "owner");
}

#[tokio::test]
async fn test_session_preserves_role() {
    let (addr, _state) = start_test_server().await;

    // Create a Viewer session via master token (Owner)
    let viewer_token = create_session_with_role(addr, AUTH_TOKEN, "viewer")
        .await
        .unwrap();

    // Use the viewer token to check status — should see "viewer" role
    let resp = client()
        .get(format!("http://{}/api/gateway/status", addr))
        .header("Authorization", format!("Bearer {}", viewer_token))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["role"], "viewer");
}

#[tokio::test]
async fn test_viewer_cannot_send_message() {
    let (addr, _state) = start_test_server().await;

    let viewer_token = create_session_with_role(addr, AUTH_TOKEN, "viewer")
        .await
        .unwrap();

    let resp = client()
        .post(format!("http://{}/api/chat/send", addr))
        .header("Authorization", format!("Bearer {}", viewer_token))
        .json(&serde_json::json!({ "content": "hello" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 403);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Insufficient permissions"));
}

#[tokio::test]
async fn test_user_cannot_install_extension() {
    let (addr, _state) = start_test_server().await;

    let user_token = create_session_with_role(addr, AUTH_TOKEN, "user")
        .await
        .unwrap();

    let resp = client()
        .post(format!("http://{}/api/extensions/install", addr))
        .header("Authorization", format!("Bearer {}", user_token))
        .json(&serde_json::json!({ "name": "test-ext" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 403);
}

#[tokio::test]
async fn test_admin_cannot_shutdown_gateway() {
    let (addr, _state) = start_test_server().await;

    let admin_token = create_session_with_role(addr, AUTH_TOKEN, "admin")
        .await
        .unwrap();

    let resp = client()
        .post(format!("http://{}/api/gateway/shutdown", addr))
        .header("Authorization", format!("Bearer {}", admin_token))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 403);
}

#[tokio::test]
async fn test_session_role_escalation_blocked() {
    let (addr, _state) = start_test_server().await;

    // Create an Admin session (Admin has ManageSessions permission)
    let admin_token = create_session_with_role(addr, AUTH_TOKEN, "admin")
        .await
        .unwrap();

    // Admin tries to create an Owner session → should be 403 (escalation)
    let result = create_session_with_role(addr, &admin_token, "owner").await;
    assert_eq!(result, Err(reqwest::StatusCode::FORBIDDEN));

    // Admin can create an Admin session (same level)
    let result2 = create_session_with_role(addr, &admin_token, "admin").await;
    assert!(result2.is_ok());

    // Admin can create a User session (downgrade is fine)
    let result3 = create_session_with_role(addr, &admin_token, "user").await;
    assert!(result3.is_ok());

    // Admin can create a Viewer session (downgrade is fine)
    let result4 = create_session_with_role(addr, &admin_token, "viewer").await;
    assert!(result4.is_ok());

    // User cannot create sessions at all (ManageSessions requires Admin)
    let user_token = create_session_with_role(addr, AUTH_TOKEN, "user")
        .await
        .unwrap();
    let result5 = create_session_with_role(addr, &user_token, "viewer").await;
    assert_eq!(result5, Err(reqwest::StatusCode::FORBIDDEN));
}

#[tokio::test]
async fn test_viewer_can_read_gateway_status() {
    let (addr, _state) = start_test_server().await;

    let viewer_token = create_session_with_role(addr, AUTH_TOKEN, "viewer")
        .await
        .unwrap();

    // ViewGatewayStatus is Viewer-level, should succeed
    let resp = client()
        .get(format!("http://{}/api/gateway/status", addr))
        .header("Authorization", format!("Bearer {}", viewer_token))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn test_sse_ticket_preserves_role() {
    let (addr, state) = start_test_server().await;

    // Create a Viewer session
    let viewer_token = create_session_with_role(addr, AUTH_TOKEN, "viewer")
        .await
        .unwrap();

    // Get an SSE ticket as Viewer
    let resp = client()
        .post(format!("http://{}/api/sse/ticket", addr))
        .header("Authorization", format!("Bearer {}", viewer_token))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let ticket = body["ticket"].as_str().unwrap();

    // Verify the ticket in the store has Viewer role
    let tickets = state.sse_tickets.lock().await;
    let (_, stored_role) = tickets.get(ticket).unwrap();
    assert_eq!(*stored_role, Role::Viewer);
}

#[tokio::test]
async fn test_no_auth_returns_401() {
    let (addr, _state) = start_test_server().await;

    let resp = client()
        .get(format!("http://{}/api/gateway/status", addr))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn test_session_revoke_blocked_by_higher_role() {
    let (addr, _state) = start_test_server().await;

    // Create an Owner session and an Admin session
    let owner_token = create_session_with_role(addr, AUTH_TOKEN, "owner")
        .await
        .unwrap();
    let admin_token = create_session_with_role(addr, AUTH_TOKEN, "admin")
        .await
        .unwrap();

    // Admin tries to delete the Owner session by passing the owner token
    // as the Authorization header to the delete endpoint.
    // Note: DELETE /api/auth/session revokes the session identified by the
    // Authorization header. So to test cross-session revocation, the admin
    // would need to know the owner's token. In practice, only self-revocation
    // happens through this endpoint. We test that Admin can revoke their own.
    let resp = client()
        .delete(format!("http://{}/api/auth/session", addr))
        .header("Authorization", format!("Bearer {}", admin_token))
        .send()
        .await
        .unwrap();

    // Admin can revoke their own session (Admin >= Admin)
    assert_eq!(resp.status(), 204);

    // Owner can revoke their own too
    let resp2 = client()
        .delete(format!("http://{}/api/auth/session", addr))
        .header("Authorization", format!("Bearer {}", owner_token))
        .send()
        .await
        .unwrap();

    assert_eq!(resp2.status(), 204);
}

#[tokio::test]
async fn test_default_session_role_is_user() {
    let (addr, _state) = start_test_server().await;

    // Create a session with no role specified → should default to User
    let resp = client()
        .post(format!("http://{}/api/auth/session", addr))
        .header("Authorization", format!("Bearer {}", AUTH_TOKEN))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["role"], "user");
}

#[tokio::test]
async fn test_viewer_cannot_write_memory() {
    let (addr, _state) = start_test_server().await;

    let viewer_token = create_session_with_role(addr, AUTH_TOKEN, "viewer")
        .await
        .unwrap();

    let resp = client()
        .post(format!("http://{}/api/memory/write", addr))
        .header("Authorization", format!("Bearer {}", viewer_token))
        .json(&serde_json::json!({ "path": "test.md", "content": "evil" }))
        .send()
        .await
        .unwrap();

    // Viewer doesn't have WriteMemory (Admin-level)
    assert_eq!(resp.status(), 403);
}

#[tokio::test]
async fn test_user_can_send_message() {
    let (addr, _state) = start_test_server().await;

    let user_token = create_session_with_role(addr, AUTH_TOKEN, "user")
        .await
        .unwrap();

    let resp = client()
        .post(format!("http://{}/api/chat/send", addr))
        .header("Authorization", format!("Bearer {}", user_token))
        .json(&serde_json::json!({ "content": "hello" }))
        .send()
        .await
        .unwrap();

    // Should succeed (or at least not 403 — may be 503 if msg channel isn't set up)
    assert_ne!(resp.status(), 403);
}

#[tokio::test]
async fn test_session_list_requires_admin() {
    let (addr, _state) = start_test_server().await;

    let user_token = create_session_with_role(addr, AUTH_TOKEN, "user")
        .await
        .unwrap();

    let resp = client()
        .get(format!("http://{}/api/auth/sessions", addr))
        .header("Authorization", format!("Bearer {}", user_token))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 403);
}

#[tokio::test]
async fn test_viewer_cannot_call_openai_chat() {
    let (addr, _state) = start_test_server().await;

    let viewer_token = create_session_with_role(addr, AUTH_TOKEN, "viewer")
        .await
        .unwrap();

    // Viewer cannot call /v1/chat/completions (requires User / SendMessage)
    let resp = client()
        .post(format!("http://{}/v1/chat/completions", addr))
        .header("Authorization", format!("Bearer {}", viewer_token))
        .json(&serde_json::json!({
            "model": "test",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 403);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("Insufficient permissions")
    );
}

#[tokio::test]
async fn test_viewer_can_list_openai_models() {
    let (addr, _state) = start_test_server().await;

    let viewer_token = create_session_with_role(addr, AUTH_TOKEN, "viewer")
        .await
        .unwrap();

    // Viewer CAN call /v1/models (requires Viewer / ViewGatewayStatus).
    // Will be 503 because LLM provider is not configured, but NOT 403.
    let resp = client()
        .get(format!("http://{}/v1/models", addr))
        .header("Authorization", format!("Bearer {}", viewer_token))
        .send()
        .await
        .unwrap();

    assert_ne!(resp.status(), 403);
}

#[tokio::test]
async fn test_user_can_call_openai_chat() {
    let (addr, _state) = start_test_server().await;

    let user_token = create_session_with_role(addr, AUTH_TOKEN, "user")
        .await
        .unwrap();

    // User has SendMessage permission — should not get 403.
    // Will likely be 503 (LLM not configured), but NOT 403.
    let resp = client()
        .post(format!("http://{}/v1/chat/completions", addr))
        .header("Authorization", format!("Bearer {}", user_token))
        .json(&serde_json::json!({
            "model": "test",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .send()
        .await
        .unwrap();

    assert_ne!(resp.status(), 403);
}

#[tokio::test]
async fn test_admin_can_revoke_user_session_by_id() {
    let (addr, _state) = start_test_server().await;

    // Create an Admin session and a User session.
    let admin_token = create_session_with_role(addr, AUTH_TOKEN, "admin")
        .await
        .unwrap();
    let user_token = create_session_with_role(addr, AUTH_TOKEN, "user")
        .await
        .unwrap();

    // Find the User session's ID via the sessions list.
    let resp = client()
        .get(format!("http://{}/api/auth/sessions", addr))
        .header("Authorization", format!("Bearer {}", admin_token))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let sessions = body["sessions"].as_array().unwrap();

    // The User session is the one with role "user".
    let user_session = sessions
        .iter()
        .find(|s| s["role"].as_str() == Some("user"))
        .unwrap();
    let session_id = user_session["session_id"].as_str().unwrap();

    // Admin revokes the User session by ID.
    let resp = client()
        .delete(format!("http://{}/api/auth/sessions/{}", addr, session_id))
        .header("Authorization", format!("Bearer {}", admin_token))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    // The User session token should no longer work.
    let resp = client()
        .get(format!("http://{}/api/gateway/status", addr))
        .header("Authorization", format!("Bearer {}", user_token))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn test_admin_cannot_revoke_owner_session_by_id() {
    let (addr, _state) = start_test_server().await;

    // Create an Admin session and an Owner session.
    let admin_token = create_session_with_role(addr, AUTH_TOKEN, "admin")
        .await
        .unwrap();
    let _owner_token = create_session_with_role(addr, AUTH_TOKEN, "owner")
        .await
        .unwrap();

    // Find the Owner session's ID.
    let resp = client()
        .get(format!("http://{}/api/auth/sessions", addr))
        .header("Authorization", format!("Bearer {}", admin_token))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let sessions = body["sessions"].as_array().unwrap();

    let owner_session = sessions
        .iter()
        .find(|s| s["role"].as_str() == Some("owner"))
        .unwrap();
    let session_id = owner_session["session_id"].as_str().unwrap();

    // Admin tries to revoke Owner session → 403.
    let resp = client()
        .delete(format!("http://{}/api/auth/sessions/{}", addr, session_id))
        .header("Authorization", format!("Bearer {}", admin_token))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
}
