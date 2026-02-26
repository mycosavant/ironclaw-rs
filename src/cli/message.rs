//! Message CLI commands.
//!
//! Sends messages to the running IronClaw instance through the web gateway's
//! HTTP API. Requires the gateway to be running and reachable.

use clap::Subcommand;

/// Send messages to the running IronClaw instance.
#[derive(Subcommand, Debug, Clone)]
pub enum MessageCommand {
    /// Send a chat message to the agent through the web gateway
    Send {
        /// Message text to send
        text: String,

        /// Gateway base URL (overrides GATEWAY_HOST + GATEWAY_PORT)
        #[arg(long)]
        gateway: Option<String>,

        /// Bearer auth token (overrides GATEWAY_AUTH_TOKEN)
        #[arg(long)]
        token: Option<String>,

        /// User ID / thread to send on behalf of (default: gateway default user)
        #[arg(long)]
        user: Option<String>,

        /// Wait for agent response and print it
        #[arg(short, long)]
        wait: bool,
    },
}

// ── Entry point ──────────────────────────────────────────────────────────────

pub async fn run_message_command(cmd: MessageCommand) -> anyhow::Result<()> {
    match cmd {
        MessageCommand::Send {
            text,
            gateway,
            token,
            user,
            wait,
        } => send(text, gateway, token, user, wait).await,
    }
}

// ── Commands ──────────────────────────────────────────────────────────────────

async fn send(
    text: String,
    gateway: Option<String>,
    token: Option<String>,
    user: Option<String>,
    wait: bool,
) -> anyhow::Result<()> {
    let _ = dotenvy::dotenv(); // best-effort .env load

    let base_url = resolve_gateway_url(gateway)?;
    let auth_token = resolve_token(token)?;
    let user_id = user.or_else(|| std::env::var("GATEWAY_USER_ID").ok())
        .unwrap_or_else(|| "default".to_string());

    let client = reqwest::Client::new();

    // POST /api/chat/send
    let body = serde_json::json!({
        "message": text,
        "user_id": user_id,
    });

    let response = client
        .post(format!("{}/api/chat/send", base_url))
        .bearer_auth(&auth_token)
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "Could not reach gateway at {}. Is IronClaw running?\n  {}",
                base_url,
                e
            )
        })?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Gateway returned {}: {}", status, body);
    }

    let resp_json: serde_json::Value = response.json().await?;
    let msg_id = resp_json.get("message_id")
        .and_then(|v| v.as_str())
        .unwrap_or("(unknown)");

    println!("Message sent (id: {})", msg_id);

    if wait {
        wait_for_response(&client, &base_url, &auth_token, msg_id).await?;
    }

    Ok(())
}

/// Poll the SSE event stream briefly to find the response to our message.
async fn wait_for_response(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    _msg_id: &str,
) -> anyhow::Result<()> {
    // Use the history endpoint to retrieve the latest assistant message.
    // We poll a few times with a short delay rather than full SSE parsing.
    use std::time::Duration;
    use tokio::time::sleep;

    println!("Waiting for response...");

    for attempt in 0..30u8 {
        sleep(Duration::from_millis(if attempt == 0 { 500 } else { 1000 })).await;

        let resp = client
            .get(format!("{}/api/chat/history", base_url))
            .bearer_auth(token)
            .query(&[("limit", "2")])
            .send()
            .await;

        let Ok(resp) = resp else { continue };
        if !resp.status().is_success() {
            continue;
        }

        let Ok(json) = resp.json::<serde_json::Value>().await else { continue };

        // The history response is {"messages": [...]}; latest assistant message
        if let Some(messages) = json.get("messages").and_then(|v| v.as_array()) {
            for msg in messages.iter().rev() {
                let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
                if role == "assistant" {
                    let content = msg
                        .get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    if !content.is_empty() {
                        println!("\nAgent: {}", content);
                        return Ok(());
                    }
                }
            }
        }
    }

    println!("\n(Timed out waiting for response — the agent may still be processing.)");
    Ok(())
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn resolve_gateway_url(override_url: Option<String>) -> anyhow::Result<String> {
    if let Some(url) = override_url {
        return Ok(url.trim_end_matches('/').to_string());
    }

    let host = std::env::var("GATEWAY_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port: u16 = std::env::var("GATEWAY_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3001);

    Ok(format!("http://{}:{}", host, port))
}

fn resolve_token(override_token: Option<String>) -> anyhow::Result<String> {
    if let Some(t) = override_token {
        return Ok(t);
    }

    std::env::var("GATEWAY_AUTH_TOKEN").map_err(|_| {
        anyhow::anyhow!(
            "No auth token provided. Set GATEWAY_AUTH_TOKEN or pass --token."
        )
    })
}
