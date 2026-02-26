//! Signal Messenger channel for IronClaw via signal-cli REST API.
//!
//! This WASM component implements the channel interface for receiving and
//! sending Signal messages through a self-hosted
//! [signal-cli REST API](https://github.com/bbernhard/signal-cli-rest-api)
//! instance.
//!
//! # Polling
//!
//! Signal does not support webhooks. The channel polls
//! `GET /v1/receive/{number}` on a configurable interval (default 30 s) via
//! `on_poll`.
//!
//! # Sending
//!
//! Responses are sent via `POST /v2/send` with `{ message, number, recipients }`.
//!
//! # Security
//!
//! - The API URL and phone number are read from workspace config written at
//!   startup; no secrets appear in WASM memory beyond what is necessary.
//! - Access control mirrors the Telegram/Slack pattern: `owner_id`, `dm_policy`
//!   (pairing | open | allowlist), and `allow_from` allowlist.

// Generate bindings from the WIT file
wit_bindgen::generate!({
    world: "sandboxed-channel",
    path: "../../wit/channel.wit",
});

use serde::{Deserialize, Serialize};

use exports::near::agent::channel::{
    AgentResponse, ChannelConfig, Guest, IncomingHttpRequest, OutgoingHttpResponse, PollConfig,
    StatusType, StatusUpdate,
};
use near::agent::channel_host::{self, EmittedMessage};

// ─────────────────────────────────────────────────────────────────────────────
// signal-cli REST API types
// ─────────────────────────────────────────────────────────────────────────────

/// Top-level envelope returned by `GET /v1/receive/{number}`.
#[derive(Debug, Deserialize)]
struct ReceiveEnvelope {
    envelope: Envelope,
}

/// Inner envelope from signal-cli.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
    source: Option<String>,
    source_number: Option<String>,
    source_name: Option<String>,
    timestamp: Option<u64>,
    data_message: Option<DataMessage>,
}

/// Payload for a regular chat message.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DataMessage {
    message: Option<String>,
    timestamp: Option<u64>,
    group_info: Option<GroupInfo>,
}

/// Group info present when the message was sent to a group.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GroupInfo {
    group_id: Option<String>,
}

/// Request body for `POST /v2/send`.
#[derive(Debug, Serialize)]
struct SendRequest<'a> {
    message: &'a str,
    number: &'a str,
    recipients: Vec<&'a str>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Message metadata
// ─────────────────────────────────────────────────────────────────────────────

/// Metadata persisted with every emitted message for response routing.
#[derive(Debug, Serialize, Deserialize)]
struct SignalMessageMetadata {
    /// Sender phone number (E.164).
    source: String,
    /// Display name of the sender, if available.
    source_name: Option<String>,
    /// Signal group ID if the message came from a group conversation.
    group_id: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Channel config
// ─────────────────────────────────────────────────────────────────────────────

/// Runtime configuration parsed from the capabilities `config` block.
#[derive(Debug, Deserialize)]
struct SignalConfig {
    /// Optional: restrict to a single authorized sender (phone number, E.164).
    #[serde(default)]
    owner_id: Option<String>,

    /// Access policy: `"pairing"` (default), `"open"`, or `"allowlist"`.
    #[serde(default)]
    dm_policy: Option<String>,

    /// Explicit list of allowed sender numbers (used when `dm_policy = "allowlist"`).
    #[serde(default)]
    allow_from: Option<Vec<String>>,

    /// Poll interval in milliseconds (default: 30 000).
    #[serde(default = "default_poll_interval_ms")]
    poll_interval_ms: u32,
}

fn default_poll_interval_ms() -> u32 {
    30_000
}

// ─────────────────────────────────────────────────────────────────────────────
// Workspace state keys
// ─────────────────────────────────────────────────────────────────────────────

const OWNER_ID_PATH: &str = "state/owner_id";
const DM_POLICY_PATH: &str = "state/dm_policy";
const ALLOW_FROM_PATH: &str = "state/allow_from";
const SIGNAL_NUMBER_PATH: &str = "state/signal_number";
const API_URL_PATH: &str = "state/api_url";

/// Channel name used by the pairing subsystem.
const CHANNEL_NAME: &str = "signal";

// ─────────────────────────────────────────────────────────────────────────────
// WIT guest implementation
// ─────────────────────────────────────────────────────────────────────────────

struct SignalChannel;

impl Guest for SignalChannel {
    // ─── on_start ────────────────────────────────────────────────────────────

    fn on_start(config_json: String) -> Result<ChannelConfig, String> {
        channel_host::log(channel_host::LogLevel::Info, "Signal channel starting");

        let config: SignalConfig = serde_json::from_str(&config_json)
            .map_err(|e| format!("Failed to parse Signal config: {}", e))?;

        // Read API URL and phone number from workspace config (set at install time)
        let signal_number = channel_host::workspace_read("config/signal_number")
            .filter(|s| !s.is_empty())
            .ok_or("signal_number not set in workspace config/signal_number")?;

        let api_url = channel_host::workspace_read("config/api_url")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "http://localhost:8080".to_string());

        // Persist runtime state for subsequent WASM callbacks
        let _ = channel_host::workspace_write(SIGNAL_NUMBER_PATH, &signal_number);
        let _ = channel_host::workspace_write(API_URL_PATH, &api_url);

        let owner_id = config.owner_id.as_deref().unwrap_or("");
        let _ = channel_host::workspace_write(OWNER_ID_PATH, owner_id);
        if !owner_id.is_empty() {
            channel_host::log(
                channel_host::LogLevel::Info,
                &format!("Owner restriction enabled: {}", owner_id),
            );
        }

        let dm_policy = config.dm_policy.as_deref().unwrap_or("pairing");
        let _ = channel_host::workspace_write(DM_POLICY_PATH, dm_policy);

        let allow_from_json =
            serde_json::to_string(&config.allow_from.unwrap_or_default())
                .unwrap_or_else(|_| "[]".to_string());
        let _ = channel_host::workspace_write(ALLOW_FROM_PATH, &allow_from_json);

        channel_host::log(
            channel_host::LogLevel::Info,
            &format!(
                "Signal channel ready: number={} api_url={} dm_policy={} poll={}ms",
                signal_number, api_url, dm_policy, config.poll_interval_ms,
            ),
        );

        Ok(ChannelConfig {
            display_name: "Signal".to_string(),
            http_endpoints: vec![],
            poll: Some(PollConfig {
                interval_ms: config.poll_interval_ms,
                enabled: true,
            }),
        })
    }

    // ─── on_http_request ─────────────────────────────────────────────────────

    fn on_http_request(_req: IncomingHttpRequest) -> OutgoingHttpResponse {
        // Signal uses polling only — no HTTP webhook endpoints are registered.
        OutgoingHttpResponse {
            status: 404,
            headers_json: r#"{"Content-Type":"text/plain"}"#.to_string(),
            body: b"Not Found".to_vec(),
        }
    }

    // ─── on_poll ─────────────────────────────────────────────────────────────

    fn on_poll() {
        let signal_number = match channel_host::workspace_read(SIGNAL_NUMBER_PATH) {
            Some(n) if !n.is_empty() => n,
            _ => {
                channel_host::log(
                    channel_host::LogLevel::Error,
                    "Signal number not in workspace state — skipping poll",
                );
                return;
            }
        };
        let api_url = channel_host::workspace_read(API_URL_PATH)
            .unwrap_or_else(|| "http://localhost:8080".to_string());

        let receive_url = format!("{}/v1/receive/{}", api_url, signal_number);
        let headers = serde_json::json!({"Accept": "application/json"});

        let response = channel_host::http_request(
            "GET",
            &receive_url,
            &headers.to_string(),
            None,
            None,
        );

        match response {
            Ok(resp) if resp.status == 200 => {
                let body_str = match std::str::from_utf8(&resp.body) {
                    Ok(s) => s,
                    Err(_) => {
                        channel_host::log(
                            channel_host::LogLevel::Error,
                            "Signal receive: non-UTF-8 response body",
                        );
                        return;
                    }
                };
                process_received_messages(body_str, &signal_number);
            }
            Ok(resp) => {
                channel_host::log(
                    channel_host::LogLevel::Warn,
                    &format!("Signal /v1/receive returned HTTP {}", resp.status),
                );
            }
            Err(e) => {
                channel_host::log(
                    channel_host::LogLevel::Warn,
                    &format!("Signal receive request failed: {}", e),
                );
            }
        }
    }

    // ─── on_respond ──────────────────────────────────────────────────────────

    fn on_respond(response: AgentResponse) -> Result<(), String> {
        let meta: SignalMessageMetadata = serde_json::from_str(&response.metadata_json)
            .map_err(|e| format!("Failed to parse Signal metadata: {}", e))?;

        let api_url = channel_host::workspace_read(API_URL_PATH)
            .unwrap_or_else(|| "http://localhost:8080".to_string());
        let signal_number = channel_host::workspace_read(SIGNAL_NUMBER_PATH)
            .unwrap_or_default();

        signal_send_message(&api_url, &signal_number, &meta.source, &response.content)
    }

    // ─── on_status ───────────────────────────────────────────────────────────

    fn on_status(update: StatusUpdate) {
        // Send a typing indicator when the agent starts thinking.
        if update.status != StatusType::Thinking {
            return;
        }

        let meta: SignalMessageMetadata = match serde_json::from_str(&update.metadata_json) {
            Ok(m) => m,
            Err(_) => return,
        };

        let api_url = channel_host::workspace_read(API_URL_PATH)
            .unwrap_or_else(|| "http://localhost:8080".to_string());
        let signal_number = channel_host::workspace_read(SIGNAL_NUMBER_PATH)
            .unwrap_or_default();

        // Best-effort typing indicator via `PUT /v1/typing/{account}`.
        // Older signal-cli versions may not support this; errors are silently
        // suppressed so they do not disrupt the agent turn.
        let typing_url = format!("{}/v1/typing/{}", api_url, signal_number);
        let typing_body = serde_json::json!({
            "recipient": meta.source,
            "action": "STARTED"
        });
        let headers = serde_json::json!({"Content-Type": "application/json"});

        let _ = channel_host::http_request(
            "PUT",
            &typing_url,
            &headers.to_string(),
            Some(typing_body.to_string().as_bytes()),
            Some(5_000),
        );
    }

    // ─── on_shutdown ─────────────────────────────────────────────────────────

    fn on_shutdown() {
        channel_host::log(channel_host::LogLevel::Info, "Signal channel shutting down");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Parse the receive response body and emit a message for each valid inbound
/// data message that passes access-control checks.
fn process_received_messages(body_str: &str, signal_number: &str) {
    let trimmed = body_str.trim();
    if trimmed.is_empty() || trimmed == "null" || trimmed == "[]" {
        return;
    }

    let envelopes: Vec<ReceiveEnvelope> = match serde_json::from_str(trimmed) {
        Ok(e) => e,
        Err(e) => {
            channel_host::log(
                channel_host::LogLevel::Error,
                &format!("Failed to parse Signal receive response: {}", e),
            );
            return;
        }
    };

    let owner_id = channel_host::workspace_read(OWNER_ID_PATH)
        .and_then(|s| if s.is_empty() { None } else { Some(s) });
    let dm_policy = channel_host::workspace_read(DM_POLICY_PATH)
        .unwrap_or_else(|| "pairing".to_string());
    let allow_from: Vec<String> = channel_host::workspace_read(ALLOW_FROM_PATH)
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    for ew in envelopes {
        let envelope = &ew.envelope;

        // Only process data messages with non-empty text
        let data_msg = match &envelope.data_message {
            Some(dm) => dm,
            None => continue,
        };
        let msg_text = match &data_msg.message {
            Some(t) if !t.trim().is_empty() => t.clone(),
            _ => continue,
        };

        // Resolve sender — prefer sourceNumber over source
        let sender = match envelope
            .source_number
            .as_deref()
            .or(envelope.source.as_deref())
        {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                channel_host::log(
                    channel_host::LogLevel::Warn,
                    "Signal envelope missing source — skipping",
                );
                continue;
            }
        };

        // Ignore our own messages
        if sender == signal_number {
            continue;
        }

        // ── Access control ────────────────────────────────────────────────

        // Hard owner lock: drop all non-owner messages
        if let Some(ref owner) = owner_id {
            if &sender != owner {
                channel_host::log(
                    channel_host::LogLevel::Warn,
                    &format!("Signal: message from non-owner {} rejected (owner lock)", sender),
                );
                continue;
            }
        }

        let group_id = data_msg
            .group_info
            .as_ref()
            .and_then(|g| g.group_id.clone());

        // Apply DM policy only to one-on-one messages (not groups)
        if group_id.is_none() && !check_sender_allowed(&sender, &dm_policy, &allow_from) {
            continue;
        }

        // ── Emit message ──────────────────────────────────────────────────

        let timestamp = data_msg.timestamp.or(envelope.timestamp).unwrap_or(0);

        let metadata = SignalMessageMetadata {
            source: sender.clone(),
            source_name: envelope.source_name.clone(),
            group_id: group_id.clone(),
        };
        let metadata_json = match serde_json::to_string(&metadata) {
            Ok(j) => j,
            Err(e) => {
                channel_host::log(
                    channel_host::LogLevel::Error,
                    &format!("Signal: failed to serialise metadata: {}", e),
                );
                continue;
            }
        };

        // thread_id encodes the conversation context for reply routing
        let conversation_id = group_id.as_deref().unwrap_or(&sender).to_string();
        let thread_id = Some(format!("{}/{}", CHANNEL_NAME, conversation_id));

        let display_name = match &envelope.source_name {
            Some(n) if !n.is_empty() => format!("{} ({})", n, sender),
            _ => sender.clone(),
        };

        let msg_id = format!(
            "signal-{}-{}",
            sender.chars().filter(|c| c.is_ascii_digit()).collect::<String>(),
            timestamp,
        );

        channel_host::emit_message(&EmittedMessage {
            user_id: sender.clone(),
            user_name: Some(display_name.clone()),
            content: msg_text,
            thread_id,
            metadata_json,
        });

        channel_host::log(
            channel_host::LogLevel::Debug,
            &format!("Signal: emitted message {} from {}", msg_id, display_name),
        );
    }
}

/// Check whether a sender is allowed to send messages according to the active
/// DM policy. Returns `true` if the message should be processed.
fn check_sender_allowed(sender: &str, dm_policy: &str, allow_from: &[String]) -> bool {
    match dm_policy {
        "open" => true,
        "allowlist" => {
            let ok = allow_from.contains(&"*".to_string())
                || allow_from.contains(&sender.to_string());
            if !ok {
                channel_host::log(
                    channel_host::LogLevel::Info,
                    &format!("Signal: DM from {} blocked (not in allowlist)", sender),
                );
            }
            ok
        }
        _ => {
            // "pairing" and anything unrecognised: check the pairing store
            let store_allowed = channel_host::pairing_read_allow_from(CHANNEL_NAME)
                .unwrap_or_default();

            let is_allowed = allow_from.contains(&"*".to_string())
                || allow_from.contains(&sender.to_string())
                || store_allowed.contains(&"*".to_string())
                || store_allowed.contains(&sender.to_string());

            if is_allowed {
                return true;
            }

            // Not yet approved — create a pairing request
            let meta = serde_json::json!({ "number": sender }).to_string();
            match channel_host::pairing_upsert_request(CHANNEL_NAME, sender, &meta) {
                Ok(result) => {
                    channel_host::log(
                        channel_host::LogLevel::Info,
                        &format!(
                            "Signal: pairing request created for {} (code={})",
                            sender, result.code,
                        ),
                    );
                    if result.created {
                        channel_host::log(
                            channel_host::LogLevel::Info,
                            &format!(
                                "Signal: approve with: ironclaw pairing approve signal {}",
                                result.code,
                            ),
                        );
                    }
                }
                Err(e) => {
                    channel_host::log(
                        channel_host::LogLevel::Warn,
                        &format!("Signal: pairing upsert failed for {}: {}", sender, e),
                    );
                }
            }
            false
        }
    }
}

/// Send a message via `POST /v2/send`.
fn signal_send_message(
    api_url: &str,
    signal_number: &str,
    recipient: &str,
    message: &str,
) -> Result<(), String> {
    let send_url = format!("{}/v2/send", api_url);
    let body = SendRequest {
        message,
        number: signal_number,
        recipients: vec![recipient],
    };
    let body_bytes =
        serde_json::to_vec(&body).map_err(|e| format!("Failed to serialise send body: {}", e))?;

    let headers = serde_json::json!({"Content-Type": "application/json"});

    let response = channel_host::http_request(
        "POST",
        &send_url,
        &headers.to_string(),
        Some(&body_bytes),
        None,
    )
    .map_err(|e| format!("HTTP request to signal-cli failed: {}", e))?;

    if response.status == 200 || response.status == 201 {
        channel_host::log(
            channel_host::LogLevel::Debug,
            &format!("Signal: message sent to {}", recipient),
        );
        Ok(())
    } else {
        let body_str = std::str::from_utf8(&response.body).unwrap_or("<binary>");
        Err(format!(
            "signal-cli POST /v2/send returned HTTP {}: {}",
            response.status, body_str,
        ))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Export
// ─────────────────────────────────────────────────────────────────────────────

export!(SignalChannel);
