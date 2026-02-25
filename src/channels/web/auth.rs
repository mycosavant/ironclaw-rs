//! Bearer token authentication middleware for the web gateway.

use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use subtle::ConstantTimeEq;

use crate::channels::web::server::{SSE_TICKET_TTL_SECS, SseTicketStore};

/// Shared auth state injected via axum middleware state.
#[derive(Clone)]
pub struct AuthState {
    pub token: String,
    /// One-time SSE tickets shared with GatewayState.
    pub sse_tickets: SseTicketStore,
}

/// Auth middleware that validates bearer token from header or query param.
///
/// SSE connections can't set headers from `EventSource`, so we also accept
/// `?token=xxx` or `?ticket=xxx` as query parameters.
/// `?ticket=xxx` accepts a one-time ticket issued by `POST /api/sse/ticket`
/// (bearer-auth required to obtain a ticket).  The ticket is consumed on use
/// and expires after `SSE_TICKET_TTL_SECS` seconds.
pub async fn auth_middleware(
    State(auth): State<AuthState>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    // Try Authorization header first (constant-time comparison)
    if let Some(auth_header) = headers.get("authorization")
        && let Ok(value) = auth_header.to_str()
        && let Some(token) = value.strip_prefix("Bearer ")
        && bool::from(token.as_bytes().ct_eq(auth.token.as_bytes()))
    {
        return next.run(request).await;
    }

    if let Some(query) = request.uri().query() {
        for pair in query.split('&') {
            // Fall back to query parameter for SSE EventSource (constant-time comparison).
            // Percent-decode the raw value before comparing so tokens containing '+', '=',
            // or other special characters work correctly when the browser encodes the URL.
            if let Some(raw) = pair.strip_prefix("token=") {
                let token = urlencoding::decode(raw).unwrap_or(std::borrow::Cow::Borrowed(raw));
                if bool::from(token.as_bytes().ct_eq(auth.token.as_bytes())) {
                    return next.run(request).await;
                }
            }

            // One-time SSE ticket: consumed on first use, expires after TTL.
            if let Some(ticket_raw) = pair.strip_prefix("ticket=") {
                let ticket = urlencoding::decode(ticket_raw)
                    .unwrap_or(std::borrow::Cow::Borrowed(ticket_raw));
                let mut map = auth.sse_tickets.lock().await;
                if let Some(created_at) = map.get(ticket.as_ref()).copied() {
                    if created_at.elapsed().as_secs() < SSE_TICKET_TTL_SECS {
                        // Consume the ticket (one-time use) and allow the request.
                        map.remove(ticket.as_ref());
                        drop(map);
                        return next.run(request).await;
                    }
                    // Expired — remove it and fall through to reject.
                    map.remove(ticket.as_ref());
                }
            }
        }
    }

    (StatusCode::UNAUTHORIZED, "Invalid or missing auth token").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auth_state_clone() {
        let state = AuthState {
            token: "test-token".to_string(),
            sse_tickets: std::sync::Arc::new(tokio::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
        };
        let cloned = state.clone();
        assert_eq!(cloned.token, "test-token");
    }
}
