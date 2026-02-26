//! `ironclaw gateway` — control plane CLI for the web gateway.
//!
//! Provides subcommands for checking gateway status, streaming logs, and
//! requesting a graceful shutdown — all via the running gateway's HTTP API.
//!
//! # Subcommands
//!
//! | Subcommand          | Description                                        |
//! |---------------------|----------------------------------------------------|
//! | `gateway status`    | Show uptime, connections, cost summary             |
//! | `gateway logs`      | Stream real-time logs over SSE                     |
//! | `gateway stop`      | Request graceful shutdown via API                  |
//! | `gateway start`     | Check if running; print start instructions if not  |

use std::time::Duration;

use clap::Subcommand;

// ── CLI types ────────────────────────────────────────────────────────────────

/// Control the IronClaw web gateway.
#[derive(Subcommand, Debug, Clone)]
pub enum GatewayCommand {
    /// Show gateway status: uptime, active connections, and cost summary.
    Status {
        /// Gateway base URL (overrides GATEWAY_HOST + GATEWAY_PORT)
        #[arg(long)]
        gateway: Option<String>,

        /// Bearer auth token (overrides GATEWAY_AUTH_TOKEN)
        #[arg(long)]
        token: Option<String>,
    },

    /// Stream real-time logs from the running gateway.
    Logs {
        /// Gateway base URL (overrides GATEWAY_HOST + GATEWAY_PORT)
        #[arg(long)]
        gateway: Option<String>,

        /// Bearer auth token (overrides GATEWAY_AUTH_TOKEN)
        #[arg(long)]
        token: Option<String>,

        /// Minimum log level to display (TRACE, DEBUG, INFO, WARN, ERROR)
        #[arg(long, default_value = "INFO")]
        level: String,

        /// Keep streaming until Ctrl-C (default: reads up to 60 s then exits)
        #[arg(short, long)]
        follow: bool,
    },

    /// Request a graceful shutdown of the running gateway.
    Stop {
        /// Gateway base URL (overrides GATEWAY_HOST + GATEWAY_PORT)
        #[arg(long)]
        gateway: Option<String>,

        /// Bearer auth token (overrides GATEWAY_AUTH_TOKEN)
        #[arg(long)]
        token: Option<String>,

        /// Skip the confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Check whether the gateway is reachable; print start instructions if not.
    Start {
        /// Gateway base URL (overrides GATEWAY_HOST + GATEWAY_PORT)
        #[arg(long)]
        gateway: Option<String>,
    },
}

// ── Entry point ──────────────────────────────────────────────────────────────

pub async fn run_gateway_command(cmd: GatewayCommand) -> anyhow::Result<()> {
    let _ = dotenvy::dotenv(); // best-effort .env load

    match cmd {
        GatewayCommand::Status { gateway, token } => status(gateway, token).await,
        GatewayCommand::Logs {
            gateway,
            token,
            level,
            follow,
        } => logs(gateway, token, level, follow).await,
        GatewayCommand::Stop {
            gateway,
            token,
            yes,
        } => stop(gateway, token, yes).await,
        GatewayCommand::Start { gateway } => start(gateway).await,
    }
}

// ── Commands ─────────────────────────────────────────────────────────────────

async fn status(gateway: Option<String>, token: Option<String>) -> anyhow::Result<()> {
    let base_url = resolve_gateway_url(gateway)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    // Public health check first (no auth required)
    let health = client
        .get(format!("{}/api/health", base_url))
        .send()
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "Could not reach gateway at {}. Is IronClaw running?\n  {}",
                base_url,
                e
            )
        })?;

    if !health.status().is_success() {
        anyhow::bail!("Gateway health check failed (HTTP {})", health.status());
    }

    println!("Gateway: {}", base_url);
    println!("Health:  up");

    // Authenticated status (optional — skip gracefully if no token)
    let auth_token = resolve_token(token);
    if let Ok(ref tok) = auth_token {
        let resp = client
            .get(format!("{}/api/gateway/status", base_url))
            .bearer_auth(tok)
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => {
                let json: serde_json::Value = r.json().await?;
                print_gateway_status(&json);
            }
            Ok(r) => {
                eprintln!(
                    "Note: status endpoint returned {} (token may be wrong)",
                    r.status()
                );
            }
            Err(e) => {
                eprintln!("Note: could not fetch detailed status: {}", e);
            }
        }
    } else {
        println!("\n  (Set GATEWAY_AUTH_TOKEN or pass --token for detailed status)");
    }

    Ok(())
}

fn print_gateway_status(json: &serde_json::Value) {
    let uptime = json
        .get("uptime_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let total = json
        .get("total_connections")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let sse = json
        .get("sse_connections")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let ws = json
        .get("ws_connections")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    println!("Uptime:  {}", format_duration(uptime));
    println!(
        "Connections: {} total ({} SSE, {} WebSocket)",
        total, sse, ws
    );

    if let Some(cost) = json.get("daily_cost").and_then(|v| v.as_str()) {
        println!("Daily cost: ${}", cost);
    }
    if let Some(actions) = json.get("actions_this_hour").and_then(|v| v.as_u64()) {
        println!("Actions/hour: {}", actions);
    }
    if let Some(models) = json.get("model_usage").and_then(|v| v.as_array())
        && !models.is_empty()
    {
        println!("Model usage:");
        for m in models {
            let model = m.get("model").and_then(|v| v.as_str()).unwrap_or("?");
            let cost = m.get("cost").and_then(|v| v.as_str()).unwrap_or("0");
            let input = m.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let output = m.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            println!(
                "  {} — {}k in / {}k out / ${}",
                model,
                input / 1000,
                output / 1000,
                cost
            );
        }
    }
}

async fn logs(
    gateway: Option<String>,
    token: Option<String>,
    level: String,
    follow: bool,
) -> anyhow::Result<()> {
    let base_url = resolve_gateway_url(gateway)?;
    let auth_token = resolve_token(token)?;

    // Normalise level to uppercase for comparison
    let min_level = log_level_value(&level.to_uppercase());

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(if follow { 0 } else { 90 })) // 0 = no timeout when following
        .build()?;

    println!(
        "Streaming logs from {} (level ≥ {}) …",
        base_url,
        level.to_uppercase()
    );
    println!("Press Ctrl-C to stop.\n");

    let mut resp = client
        .get(format!("{}/api/logs/events", base_url))
        .bearer_auth(&auth_token)
        .send()
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "Could not reach gateway at {}. Is IronClaw running?\n  {}",
                base_url,
                e
            )
        })?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Gateway returned {}: {}", status, body);
    }

    // Stream SSE lines using chunk-by-chunk reading — no extra crates needed.
    // We accumulate partial bytes in a buffer and emit complete lines.

    // Deadline for non-follow mode
    let deadline = if follow {
        None
    } else {
        Some(tokio::time::Instant::now() + Duration::from_secs(60))
    };

    // SSE line accumulator.  We keep a byte vec and drain complete \n-terminated
    // lines from the front, avoiding the O(n²) re-allocation of slicing + to_string.
    // A hard 64 KiB cap on a single buffered line guards against a runaway gateway.
    const MAX_LINE_BYTES: usize = 64 * 1024;
    let mut buf: Vec<u8> = Vec::with_capacity(4096);

    loop {
        // Check deadline before reading
        if let Some(d) = deadline
            && tokio::time::Instant::now() >= d
        {
            println!("\n(60 s timeout reached — use --follow to keep streaming)");
            break;
        }

        let chunk = resp.chunk().await?;
        let Some(bytes) = chunk else { break };
        buf.extend_from_slice(&bytes);

        // Drain complete lines from the front of the buffer.
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            // Extract the line and remove trailing \r if present (CRLF).
            let end = if nl > 0 && buf[nl - 1] == b'\r' {
                nl - 1
            } else {
                nl
            };
            let line = String::from_utf8_lossy(&buf[..end]).into_owned();
            buf.drain(..nl + 1);

            if let Some(data) = line.strip_prefix("data: ") {
                if data == "[DONE]" {
                    return Ok(());
                }
                // Try to parse as JSON log event and apply level filter
                if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
                    let event_level = event
                        .get("level")
                        .and_then(|v| v.as_str())
                        .unwrap_or("INFO");
                    if log_level_value(event_level) >= min_level {
                        print_log_event(&event);
                    }
                } else {
                    // Print raw data for non-JSON events
                    println!("{}", data);
                }
            }
        }

        // Guard: if no newline arrived and buffer exceeds the cap, the gateway
        // sent a pathologically long line — skip it to avoid memory exhaustion.
        if buf.len() > MAX_LINE_BYTES {
            eprintln!("[warn] Oversized SSE line ({} bytes) — skipping", buf.len());
            buf.clear();
        }
    }

    Ok(())
}

fn print_log_event(event: &serde_json::Value) {
    let timestamp = event
        .get("timestamp")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let level = event
        .get("level")
        .and_then(|v| v.as_str())
        .unwrap_or("INFO");
    let message = event
        .get("message")
        .or_else(|| event.get("fields").and_then(|f| f.get("message")))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let target = event.get("target").and_then(|v| v.as_str()).unwrap_or("");

    // Colour-code level
    let level_display = match level {
        "ERROR" => "\x1b[31mERROR\x1b[0m",
        "WARN" => "\x1b[33m WARN\x1b[0m",
        "INFO" => "\x1b[32m INFO\x1b[0m",
        "DEBUG" => "\x1b[36mDEBUG\x1b[0m",
        "TRACE" => "\x1b[90mTRACE\x1b[0m",
        other => other,
    };

    let ts = if timestamp.len() >= 19 {
        &timestamp[11..19] // HH:MM:SS from ISO-8601
    } else {
        timestamp
    };

    if target.is_empty() {
        println!("{} {} {}", ts, level_display, message);
    } else {
        println!("{} {} [{}] {}", ts, level_display, target, message);
    }
}

async fn stop(gateway: Option<String>, token: Option<String>, yes: bool) -> anyhow::Result<()> {
    let base_url = resolve_gateway_url(gateway)?;
    let auth_token = resolve_token(token)?;

    if !yes {
        print!("Stop the IronClaw gateway at {}? [y/N]: ", base_url);
        use std::io::Write as _;
        std::io::stdout().flush()?;

        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;

        if !line.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    let resp = client
        .post(format!("{}/api/gateway/shutdown", base_url))
        .bearer_auth(&auth_token)
        .send()
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "Could not reach gateway at {}. Is IronClaw running?\n  {}",
                base_url,
                e
            )
        })?;

    if resp.status().is_success() {
        println!("Shutdown signal sent. Gateway is stopping gracefully.");
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Shutdown request failed (HTTP {}): {}", status, body);
    }

    Ok(())
}

async fn start(gateway: Option<String>) -> anyhow::Result<()> {
    let base_url = resolve_gateway_url(gateway)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    match client.get(format!("{}/api/health", base_url)).send().await {
        Ok(r) if r.status().is_success() => {
            println!("Gateway is already running at {}", base_url);
            println!("Use `ironclaw gateway status` for details.");
        }
        _ => {
            println!("Gateway is not reachable at {}", base_url);
            println!();
            println!("To start IronClaw:");
            println!("  ironclaw run           # foreground");
            println!("  ironclaw service start # background (systemd / launchd)");
            println!();
            println!("Environment:");
            println!("  GATEWAY_ENABLED=true   # enable the web gateway");
            println!("  GATEWAY_HOST=127.0.0.1");
            println!("  GATEWAY_PORT=3001");
        }
    }

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
        return Ok(t.trim().to_string());
    }
    std::env::var("GATEWAY_AUTH_TOKEN")
        .map(|t| t.trim().to_string())
        .map_err(|_| {
            anyhow::anyhow!("No auth token provided. Set GATEWAY_AUTH_TOKEN or pass --token.")
        })
}

/// Convert a log level string to a numeric priority (higher = more severe).
/// Accepts both upper and lower case; unknown levels default to INFO (2).
fn log_level_value(level: &str) -> u8 {
    match level.to_uppercase().as_str() {
        "TRACE" => 0,
        "DEBUG" => 1,
        "INFO" => 2,
        "WARN" | "WARNING" => 3,
        "ERROR" => 4,
        _ => 2,
    }
}

fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{}h {}m", h, m)
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_values() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(59), "59s");
        assert_eq!(format_duration(90), "1m 30s");
        assert_eq!(format_duration(3661), "1h 1m");
    }

    #[test]
    fn log_level_ordering() {
        assert!(log_level_value("TRACE") < log_level_value("DEBUG"));
        assert!(log_level_value("DEBUG") < log_level_value("INFO"));
        assert!(log_level_value("INFO") < log_level_value("WARN"));
        assert!(log_level_value("WARN") < log_level_value("ERROR"));
    }
}
