//! CLI subcommands for listing and exporting conversation sessions.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use clap::{Subcommand, ValueEnum};
use uuid::Uuid;

use crate::db::Database;
use crate::error::DatabaseError;

// ── Sessions subcommand ─────────────────────────────────────────────────────

/// Manage conversation sessions.
#[derive(Subcommand, Debug, Clone)]
pub enum SessionsCommand {
    /// List recent conversation sessions.
    List {
        /// Filter by channel name (e.g. "gateway", "repl", "http").
        #[arg(long, default_value = "gateway")]
        channel: String,

        /// Filter by user ID.
        #[arg(long, default_value = "default")]
        user: String,

        /// Maximum number of sessions to show.
        #[arg(short = 'n', long, default_value = "20")]
        limit: i64,
    },

    /// Export a conversation session to stdout.
    Export {
        /// Conversation ID to export.
        #[arg()]
        id: Uuid,

        /// Output format.
        #[arg(long, default_value = "markdown")]
        format: ExportFormat,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ExportFormat {
    /// Markdown transcript with headings per message.
    Markdown,
    /// JSON array of messages.
    Json,
}

/// Run the sessions CLI subcommand.
pub async fn run_sessions_command(
    cmd: SessionsCommand,
    db: Arc<dyn Database>,
) -> anyhow::Result<()> {
    match cmd {
        SessionsCommand::List {
            channel,
            user,
            limit,
        } => list_sessions(&db, &user, &channel, limit).await,
        SessionsCommand::Export { id, format } => export_session(&db, id, format).await,
    }
}

async fn list_sessions(
    db: &Arc<dyn Database>,
    user_id: &str,
    channel: &str,
    limit: i64,
) -> anyhow::Result<()> {
    let summaries = db
        .list_conversations_with_preview(user_id, channel, limit)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to list conversations: {}", e))?;

    if summaries.is_empty() {
        println!(
            "No conversations found for user '{}' on channel '{}'.",
            user_id, channel
        );
        return Ok(());
    }

    // Print a compact table.
    println!("{:<38} {:>5}  {:<20}  Title", "ID", "Msgs", "Last Active");
    println!("{}", "-".repeat(90));

    for s in &summaries {
        let title = s
            .title
            .as_deref()
            .unwrap_or("(untitled)")
            .chars()
            .take(40)
            .collect::<String>();
        let ago = format_relative_time(s.last_activity);
        println!(
            "{:<38} {:>5}  {:<20}  {}",
            s.id, s.message_count, ago, title
        );
    }

    println!("\n{} conversation(s) shown.", summaries.len());
    Ok(())
}

async fn export_session(
    db: &Arc<dyn Database>,
    conversation_id: Uuid,
    format: ExportFormat,
) -> anyhow::Result<()> {
    let messages = db
        .list_conversation_messages(conversation_id)
        .await
        .map_err(|e: DatabaseError| anyhow::anyhow!("Failed to load messages: {}", e))?;

    if messages.is_empty() {
        eprintln!("No messages found for conversation {}.", conversation_id);
        return Ok(());
    }

    match format {
        ExportFormat::Markdown => {
            println!("# Conversation {}\n", conversation_id);
            for msg in &messages {
                let role_label = match msg.role.as_str() {
                    "user" => "User",
                    "assistant" => "Assistant",
                    "system" => "System",
                    other => other,
                };
                println!(
                    "## {} — {}\n\n{}\n",
                    role_label,
                    msg.created_at.format("%Y-%m-%d %H:%M:%S UTC"),
                    msg.content
                );
            }
        }
        ExportFormat::Json => {
            let json_messages: Vec<serde_json::Value> = messages
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "id": m.id.to_string(),
                        "role": m.role,
                        "content": m.content,
                        "created_at": m.created_at.to_rfc3339(),
                    })
                })
                .collect();

            let output = serde_json::json!({
                "conversation_id": conversation_id.to_string(),
                "message_count": messages.len(),
                "messages": json_messages,
            });

            println!(
                "{}",
                serde_json::to_string_pretty(&output)
                    .map_err(|e| anyhow::anyhow!("JSON serialization failed: {}", e))?
            );
        }
    }

    Ok(())
}

/// Format a UTC timestamp as a human-friendly relative time string.
fn format_relative_time(dt: DateTime<Utc>) -> String {
    let now = Utc::now();
    let delta = now.signed_duration_since(dt);

    if delta.num_seconds() < 60 {
        "just now".to_string()
    } else if delta.num_minutes() < 60 {
        format!("{}m ago", delta.num_minutes())
    } else if delta.num_hours() < 24 {
        format!("{}h ago", delta.num_hours())
    } else if delta.num_days() < 30 {
        format!("{}d ago", delta.num_days())
    } else {
        dt.format("%Y-%m-%d").to_string()
    }
}
