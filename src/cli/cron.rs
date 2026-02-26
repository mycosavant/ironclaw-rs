//! Cron / routines CLI commands.
//!
//! Allows inspection and management of persistent routines (scheduled jobs,
//! event triggers, webhooks, and manual tasks) without running the full agent.

use std::sync::Arc;

use chrono::Utc;
use clap::Subcommand;
use uuid::Uuid;

use crate::agent::routine::{
    NotifyConfig, Routine, RoutineAction, RoutineGuardrails, Trigger,
};
use crate::db::Database;

/// Manage scheduled routines (cron jobs, event triggers, webhooks).
#[derive(Subcommand, Debug, Clone)]
pub enum CronCommand {
    /// List all routines
    List {
        /// Show only enabled routines
        #[arg(long)]
        enabled: bool,

        /// Filter by trigger type: cron, event, webhook, manual
        #[arg(long)]
        kind: Option<String>,
    },

    /// Show details and recent runs for a routine
    Show {
        /// Routine name or UUID
        name_or_id: String,

        /// Number of recent runs to show
        #[arg(short, long, default_value = "5")]
        runs: i64,
    },

    /// Create a new cron-scheduled routine
    Add {
        /// Routine name (must be unique)
        #[arg(short, long)]
        name: String,

        /// Cron expression or friendly string (e.g. "0 9 * * MON-FRI")
        #[arg(short, long)]
        schedule: String,

        /// Prompt to execute when the routine fires
        #[arg(short, long)]
        prompt: String,

        /// Optional description
        #[arg(short, long, default_value = "")]
        description: String,

        /// Enable the routine immediately (default: disabled)
        #[arg(long)]
        enable: bool,
    },

    /// Edit an existing routine's schedule, prompt, or description
    Edit {
        /// Routine name or UUID
        name_or_id: String,

        /// New cron schedule expression
        #[arg(long)]
        schedule: Option<String>,

        /// New prompt text
        #[arg(long)]
        prompt: Option<String>,

        /// New description
        #[arg(long)]
        description: Option<String>,

        /// Enable the routine
        #[arg(long, conflicts_with = "disable")]
        enable: bool,

        /// Disable the routine
        #[arg(long, conflicts_with = "enable")]
        disable: bool,
    },

    /// Delete a routine permanently
    Remove {
        /// Routine name or UUID
        name_or_id: String,

        /// Skip confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },

    /// Print the webhook URL for a webhook-type routine
    Webhook {
        /// Routine name or UUID
        name_or_id: String,

        /// Gateway base URL (default: http://localhost:3001)
        #[arg(long, default_value = "http://localhost:3001")]
        gateway: String,
    },
}

// ── Entry point ──────────────────────────────────────────────────────────────

/// Run a cron command against the database (works with any backend).
pub async fn run_cron_command_with_db(
    cmd: CronCommand,
    db: Arc<dyn Database>,
) -> anyhow::Result<()> {
    // Use a fixed user_id for CLI sessions (same as gateway default).
    let user_id = std::env::var("GATEWAY_USER_ID").unwrap_or_else(|_| "default".to_string());

    match cmd {
        CronCommand::List { enabled, kind } => list(&db, &user_id, enabled, kind).await,
        CronCommand::Show { name_or_id, runs } => show(&db, &user_id, &name_or_id, runs).await,
        CronCommand::Add {
            name,
            schedule,
            prompt,
            description,
            enable,
        } => add(&db, &user_id, name, schedule, prompt, description, enable).await,
        CronCommand::Edit {
            name_or_id,
            schedule,
            prompt,
            description,
            enable,
            disable,
        } => edit(&db, &user_id, &name_or_id, schedule, prompt, description, enable, disable).await,
        CronCommand::Remove { name_or_id, yes } => remove(&db, &user_id, &name_or_id, yes).await,
        CronCommand::Webhook { name_or_id, gateway } => {
            webhook_url(&db, &user_id, &name_or_id, &gateway).await
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Resolve a name-or-UUID string to a `Routine`.
async fn resolve(db: &Arc<dyn Database>, user_id: &str, name_or_id: &str) -> anyhow::Result<Routine> {
    if let Ok(id) = Uuid::parse_str(name_or_id) {
        db.get_routine(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("No routine found with id {}", id))
    } else {
        db.get_routine_by_name(user_id, name_or_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("No routine named '{}'", name_or_id))
    }
}

fn trigger_label(t: &Trigger) -> String {
    match t {
        Trigger::Cron { schedule } => format!("cron ({})", schedule),
        Trigger::Event { channel, pattern } => {
            let ch = channel.as_deref().unwrap_or("*");
            format!("event [{}] /{}/", ch, pattern)
        }
        Trigger::Webhook { path, .. } => {
            let p = path.as_deref().unwrap_or("<id>");
            format!("webhook (/hooks/routine/{})", p)
        }
        Trigger::Manual => "manual".to_string(),
    }
}

fn action_label(a: &RoutineAction) -> String {
    match a {
        RoutineAction::Lightweight { prompt, .. } => {
            let preview = if prompt.len() > 60 { &prompt[..60] } else { prompt };
            format!("lightweight: \"{}{}\"", preview, if prompt.len() > 60 { "…" } else { "" })
        }
        RoutineAction::FullJob { title, .. } => format!("full_job: \"{}\"", title),
    }
}

// ── Commands ─────────────────────────────────────────────────────────────────

async fn list(
    db: &Arc<dyn Database>,
    user_id: &str,
    only_enabled: bool,
    kind: Option<String>,
) -> anyhow::Result<()> {
    let mut routines = db.list_routines(user_id).await?;

    if only_enabled {
        routines.retain(|r| r.enabled);
    }
    if let Some(ref k) = kind {
        routines.retain(|r| r.trigger.type_tag() == k.as_str());
    }

    if routines.is_empty() {
        println!("No routines found.");
        return Ok(());
    }

    println!(
        "{:<36}  {:<20}  {:<8}  {:<10}  LAST RUN",
        "ID", "NAME", "ENABLED", "TYPE"
    );
    println!("{}", "-".repeat(95));

    for r in &routines {
        let last_run = r
            .last_run_at
            .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "never".to_string());
        println!(
            "{:<36}  {:<20}  {:<8}  {:<10}  {}",
            r.id,
            truncate(&r.name, 20),
            if r.enabled { "yes" } else { "no" },
            r.trigger.type_tag(),
            last_run,
        );
    }

    println!("\n{} routine(s)", routines.len());
    Ok(())
}

async fn show(
    db: &Arc<dyn Database>,
    user_id: &str,
    name_or_id: &str,
    run_limit: i64,
) -> anyhow::Result<()> {
    let r = resolve(db, user_id, name_or_id).await?;

    println!("Routine: {}", r.name);
    println!("  ID:          {}", r.id);
    println!("  Description: {}", if r.description.is_empty() { "(none)" } else { &r.description });
    println!("  Enabled:     {}", r.enabled);
    println!("  Trigger:     {}", trigger_label(&r.trigger));
    println!("  Action:      {}", action_label(&r.action));
    println!("  Run count:   {}", r.run_count);
    println!(
        "  Last run:    {}",
        r.last_run_at
            .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_else(|| "never".to_string())
    );
    println!(
        "  Next fire:   {}",
        r.next_fire_at
            .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_else(|| "not scheduled".to_string())
    );
    println!(
        "  Created:     {}",
        r.created_at.format("%Y-%m-%d %H:%M:%S UTC")
    );

    // Recent runs
    let runs = db.list_routine_runs(r.id, run_limit).await?;
    if runs.is_empty() {
        println!("\nNo runs recorded yet.");
    } else {
        println!("\nRecent runs ({}):", runs.len());
        println!(
            "  {:<36}  {:<8}  {:<10}  STARTED",
            "RUN ID", "STATUS", "TYPE"
        );
        println!("  {}", "-".repeat(75));
        for run in &runs {
            println!(
                "  {:<36}  {:<8}  {:<10}  {}",
                run.id,
                run.status,
                run.trigger_type,
                run.started_at.format("%Y-%m-%d %H:%M"),
            );
        }
    }

    Ok(())
}

async fn add(
    db: &Arc<dyn Database>,
    user_id: &str,
    name: String,
    schedule: String,
    prompt: String,
    description: String,
    enable: bool,
) -> anyhow::Result<()> {
    // Check for name collision.
    if db.get_routine_by_name(user_id, &name).await?.is_some() {
        anyhow::bail!("A routine named '{}' already exists. Use `cron edit` to modify it.", name);
    }

    let now = Utc::now();
    let routine = Routine {
        id: Uuid::new_v4(),
        name: name.clone(),
        description,
        user_id: user_id.to_string(),
        enabled: enable,
        trigger: Trigger::Cron { schedule: schedule.clone() },
        action: RoutineAction::Lightweight {
            prompt,
            context_paths: vec![],
            max_tokens: 4096,
        },
        guardrails: RoutineGuardrails::default(),
        notify: NotifyConfig::default(),
        last_run_at: None,
        next_fire_at: None,
        run_count: 0,
        consecutive_failures: 0,
        state: serde_json::Value::Object(Default::default()),
        created_at: now,
        updated_at: now,
    };

    db.create_routine(&routine).await?;

    println!("Created routine '{}'  ({})", name, routine.id);
    println!("  Schedule: {}", schedule);
    println!("  Enabled:  {}", enable);
    if !enable {
        println!("  Tip: pass --enable to start it now, or use `cron edit {} --enable`", name);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn edit(
    db: &Arc<dyn Database>,
    user_id: &str,
    name_or_id: &str,
    schedule: Option<String>,
    prompt: Option<String>,
    description: Option<String>,
    enable: bool,
    disable: bool,
) -> anyhow::Result<()> {
    if schedule.is_none() && prompt.is_none() && description.is_none() && !enable && !disable {
        anyhow::bail!("Nothing to change. Pass at least one of --schedule, --prompt, --description, --enable, --disable.");
    }

    let mut routine = resolve(db, user_id, name_or_id).await?;

    if let Some(s) = schedule {
        match &routine.trigger {
            Trigger::Cron { .. } => {
                routine.trigger = Trigger::Cron { schedule: s };
            }
            _ => anyhow::bail!(
                "Routine '{}' has a '{}' trigger, not cron. Cannot set --schedule.",
                routine.name,
                routine.trigger.type_tag()
            ),
        }
    }

    if let Some(p) = prompt {
        match &routine.action {
            RoutineAction::Lightweight {
                context_paths,
                max_tokens,
                ..
            } => {
                routine.action = RoutineAction::Lightweight {
                    prompt: p,
                    context_paths: context_paths.clone(),
                    max_tokens: *max_tokens,
                };
            }
            RoutineAction::FullJob { title, max_iterations, .. } => {
                routine.action = RoutineAction::FullJob {
                    title: title.clone(),
                    description: p,
                    max_iterations: *max_iterations,
                };
            }
        }
    }

    if let Some(d) = description {
        routine.description = d;
    }

    if enable {
        routine.enabled = true;
    }
    if disable {
        routine.enabled = false;
    }

    routine.updated_at = Utc::now();
    db.update_routine(&routine).await?;

    println!("Updated routine '{}'.", routine.name);
    Ok(())
}

async fn remove(
    db: &Arc<dyn Database>,
    user_id: &str,
    name_or_id: &str,
    yes: bool,
) -> anyhow::Result<()> {
    let routine = resolve(db, user_id, name_or_id).await?;

    if !yes {
        // Prompt the user.
        print!(
            "Delete routine '{}' ({})? [y/N] ",
            routine.name, routine.id
        );
        use std::io::Write;
        std::io::stdout().flush()?;

        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if !line.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    let deleted = db.delete_routine(routine.id).await?;
    if deleted {
        println!("Deleted routine '{}'.", routine.name);
    } else {
        println!("Routine not found (may have been deleted already).");
    }
    Ok(())
}

async fn webhook_url(
    db: &Arc<dyn Database>,
    user_id: &str,
    name_or_id: &str,
    gateway: &str,
) -> anyhow::Result<()> {
    let routine = resolve(db, user_id, name_or_id).await?;

    match &routine.trigger {
        Trigger::Webhook { path, secret } => {
            let path_segment = path
                .as_deref()
                .map(|p| p.to_string())
                .unwrap_or_else(|| routine.id.to_string());
            let url = format!("{}/hooks/routine/{}", gateway.trim_end_matches('/'), path_segment);
            println!("Webhook URL: {}", url);
            if secret.is_some() {
                println!("Secret:      (configured — send as X-Webhook-Secret header)");
            } else {
                println!("Secret:      (none — any POST will trigger it)");
            }
        }
        _ => {
            anyhow::bail!(
                "Routine '{}' is a '{}' routine, not a webhook routine.",
                routine.name,
                routine.trigger.type_tag()
            );
        }
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}
