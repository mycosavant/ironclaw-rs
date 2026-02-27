//! Web gateway channel for browser-based access to IronClaw.
//!
//! Provides a single-page web UI with:
//! - Chat with the agent (via REST + SSE)
//! - Workspace/memory browsing
//! - Job management
//!
//! ```text
//! Browser ─── POST /api/chat/send ──► Agent Loop
//!         ◄── GET  /api/chat/events ── SSE stream
//!         ─── GET  /api/chat/ws ─────► WebSocket (bidirectional)
//!         ─── GET  /api/memory/* ────► Workspace
//!         ─── GET  /api/jobs/* ──────► Database
//!         ◄── GET  / ───────────────── Static HTML/CSS/JS
//! ```

pub mod auth;
pub mod log_layer;
pub mod openai_compat;
pub mod pid_lock;
pub mod rbac;
pub mod server;
pub mod session_store;
pub mod sse;
pub mod types;
pub mod ws;

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::agent::SessionManager;
use crate::channels::{Channel, IncomingMessage, MessageStream, OutgoingResponse, StatusUpdate};
use crate::config::GatewayConfig;
use crate::db::Database;
use crate::error::ChannelError;
use crate::extensions::ExtensionManager;
use crate::orchestrator::job_manager::ContainerJobManager;
use crate::skills::catalog::SkillCatalog;
use crate::skills::registry::SkillRegistry;
use crate::tools::ToolRegistry;
use crate::workspace::Workspace;

use self::log_layer::{LogBroadcaster, LogLevelHandle};

use self::server::GatewayState;
use self::sse::SseManager;
use self::types::SseEvent;

/// Web gateway channel implementing the Channel trait.
pub struct GatewayChannel {
    config: GatewayConfig,
    state: Arc<GatewayState>,
    /// The actual auth token in use (generated or from config).
    auth_token: String,
    /// PID lock preventing multiple gateway instances.
    pid_lock: tokio::sync::Mutex<Option<pid_lock::PidLock>>,
}

impl GatewayChannel {
    /// Create a new gateway channel.
    ///
    /// If no auth token is configured, generates a random one and prints it.
    pub fn new(config: GatewayConfig) -> Self {
        let auth_token = config.auth_token.clone().unwrap_or_else(|| {
            use rand::Rng;
            let token: String = rand::thread_rng()
                .sample_iter(&rand::distributions::Alphanumeric)
                .take(32)
                .map(char::from)
                .collect();
            token
        });

        let state = Arc::new(GatewayState {
            msg_tx: tokio::sync::RwLock::new(None),
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
            user_id: config.user_id.clone(),
            shutdown_tx: tokio::sync::RwLock::new(None),
            ws_tracker: Some(Arc::new(ws::WsConnectionTracker::new())),
            llm_provider: None,
            skill_registry: None,
            skill_catalog: None,
            chat_rate_limiter: server::RateLimiter::new(30, 60),
            registry_entries: Vec::new(),
            cost_guard: None,
            startup_time: std::time::Instant::now(),
            sse_tickets: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            heartbeat_last_tick: None,
            routine_last_tick: None,
            repair_last_tick: None,
            session_store: session_store::new_session_store(),
            channel_health: None,
            trusted_proxy_header: config.trusted_proxy_header.clone(),
            roles: config.roles.clone(),
        });

        Self {
            config,
            state,
            auth_token,
            pid_lock: tokio::sync::Mutex::new(None),
        }
    }

    /// Helper to rebuild state, copying existing fields and applying a mutation.
    fn rebuild_state(&mut self, mutate: impl FnOnce(&mut GatewayState)) {
        let mut new_state = GatewayState {
            msg_tx: tokio::sync::RwLock::new(None),
            sse: SseManager::new(),
            workspace: self.state.workspace.clone(),
            session_manager: self.state.session_manager.clone(),
            log_broadcaster: self.state.log_broadcaster.clone(),
            log_level_handle: self.state.log_level_handle.clone(),
            extension_manager: self.state.extension_manager.clone(),
            tool_registry: self.state.tool_registry.clone(),
            store: self.state.store.clone(),
            job_manager: self.state.job_manager.clone(),
            prompt_queue: self.state.prompt_queue.clone(),
            user_id: self.state.user_id.clone(),
            shutdown_tx: tokio::sync::RwLock::new(None),
            ws_tracker: self.state.ws_tracker.clone(),
            llm_provider: self.state.llm_provider.clone(),
            skill_registry: self.state.skill_registry.clone(),
            skill_catalog: self.state.skill_catalog.clone(),
            chat_rate_limiter: server::RateLimiter::new(30, 60),
            registry_entries: self.state.registry_entries.clone(),
            cost_guard: self.state.cost_guard.clone(),
            startup_time: self.state.startup_time,
            sse_tickets: self.state.sse_tickets.clone(),
            heartbeat_last_tick: self.state.heartbeat_last_tick.clone(),
            routine_last_tick: self.state.routine_last_tick.clone(),
            repair_last_tick: self.state.repair_last_tick.clone(),
            channel_health: self.state.channel_health.clone(),
            session_store: self.state.session_store.clone(),
            trusted_proxy_header: self.state.trusted_proxy_header.clone(),
            roles: self.state.roles.clone(),
        };
        mutate(&mut new_state);
        self.state = Arc::new(new_state);
    }

    /// Inject the workspace reference for the memory API.
    pub fn with_workspace(mut self, workspace: Arc<Workspace>) -> Self {
        self.rebuild_state(|s| s.workspace = Some(workspace));
        self
    }

    /// Inject the session manager for thread/session info.
    pub fn with_session_manager(mut self, sm: Arc<SessionManager>) -> Self {
        self.rebuild_state(|s| s.session_manager = Some(sm));
        self
    }

    /// Inject the log broadcaster for the logs SSE endpoint.
    pub fn with_log_broadcaster(mut self, lb: Arc<LogBroadcaster>) -> Self {
        self.rebuild_state(|s| s.log_broadcaster = Some(lb));
        self
    }

    /// Inject the log level handle for runtime log level control.
    pub fn with_log_level_handle(mut self, h: Arc<LogLevelHandle>) -> Self {
        self.rebuild_state(|s| s.log_level_handle = Some(h));
        self
    }

    /// Inject the extension manager for the extensions API.
    pub fn with_extension_manager(mut self, em: Arc<ExtensionManager>) -> Self {
        self.rebuild_state(|s| s.extension_manager = Some(em));
        self
    }

    /// Inject the tool registry for the extensions API.
    pub fn with_tool_registry(mut self, tr: Arc<ToolRegistry>) -> Self {
        self.rebuild_state(|s| s.tool_registry = Some(tr));
        self
    }

    /// Inject the database store for sandbox job persistence.
    pub fn with_store(mut self, store: Arc<dyn Database>) -> Self {
        self.rebuild_state(|s| s.store = Some(store));
        self
    }

    /// Inject the container job manager for sandbox operations.
    pub fn with_job_manager(mut self, jm: Arc<ContainerJobManager>) -> Self {
        self.rebuild_state(|s| s.job_manager = Some(jm));
        self
    }

    /// Inject the prompt queue for Claude Code follow-up prompts.
    pub fn with_prompt_queue(
        mut self,
        pq: Arc<
            tokio::sync::Mutex<
                std::collections::HashMap<
                    uuid::Uuid,
                    std::collections::VecDeque<crate::orchestrator::api::PendingPrompt>,
                >,
            >,
        >,
    ) -> Self {
        self.rebuild_state(|s| s.prompt_queue = Some(pq));
        self
    }

    /// Inject the skill registry for skill management API.
    pub fn with_skill_registry(mut self, sr: Arc<std::sync::RwLock<SkillRegistry>>) -> Self {
        self.rebuild_state(|s| s.skill_registry = Some(sr));
        self
    }

    /// Inject the skill catalog for skill search API.
    pub fn with_skill_catalog(mut self, sc: Arc<SkillCatalog>) -> Self {
        self.rebuild_state(|s| s.skill_catalog = Some(sc));
        self
    }

    /// Inject the LLM provider for OpenAI-compatible API proxy.
    pub fn with_llm_provider(mut self, llm: Arc<dyn crate::llm::LlmProvider>) -> Self {
        self.rebuild_state(|s| s.llm_provider = Some(llm));
        self
    }

    /// Inject registry catalog entries for the available extensions API.
    pub fn with_registry_entries(mut self, entries: Vec<crate::extensions::RegistryEntry>) -> Self {
        self.rebuild_state(|s| s.registry_entries = entries);
        self
    }

    /// Inject the cost guard for token/cost tracking in the status popover.
    pub fn with_cost_guard(mut self, cg: Arc<crate::agent::cost_guard::CostGuard>) -> Self {
        self.rebuild_state(|s| s.cost_guard = Some(cg));
        self
    }

    /// Inject the heartbeat liveness tick atomic.
    pub fn with_heartbeat_tick(mut self, tick: Arc<std::sync::atomic::AtomicI64>) -> Self {
        self.rebuild_state(|s| s.heartbeat_last_tick = Some(tick));
        self
    }

    /// Inject the routine engine liveness tick atomic.
    pub fn with_routine_tick(mut self, tick: Arc<std::sync::atomic::AtomicI64>) -> Self {
        self.rebuild_state(|s| s.routine_last_tick = Some(tick));
        self
    }

    /// Inject the self-repair liveness tick atomic.
    pub fn with_repair_tick(mut self, tick: Arc<std::sync::atomic::AtomicI64>) -> Self {
        self.rebuild_state(|s| s.repair_last_tick = Some(tick));
        self
    }

    /// Inject the shared channel health state for the channels API.
    pub fn with_channel_health(mut self, ch: crate::channels::SharedChannelHealth) -> Self {
        self.rebuild_state(|s| s.channel_health = Some(ch));
        self
    }

    /// Get the auth token (for printing to console on startup).
    pub fn auth_token(&self) -> &str {
        &self.auth_token
    }

    /// Get a reference to the shared gateway state (for the agent to push SSE events).
    pub fn state(&self) -> &Arc<GatewayState> {
        &self.state
    }
}

#[async_trait]
impl Channel for GatewayChannel {
    fn name(&self) -> &str {
        "gateway"
    }

    async fn start(&self) -> Result<MessageStream, ChannelError> {
        // Acquire PID lock to prevent multiple gateway instances
        let lock = pid_lock::PidLock::acquire().map_err(|e| ChannelError::StartupFailed {
            name: "gateway".to_string(),
            reason: format!("PID lock: {}", e),
        })?;
        *self.pid_lock.lock().await = Some(lock);

        let (tx, rx) = mpsc::channel(256);
        *self.state.msg_tx.write().await = Some(tx);

        let addr: SocketAddr = format!("{}:{}", self.config.host, self.config.port)
            .parse()
            .map_err(|e| ChannelError::StartupFailed {
                name: "gateway".to_string(),
                reason: format!(
                    "Invalid address '{}:{}': {}",
                    self.config.host, self.config.port, e
                ),
            })?;

        // Warn if trusted-proxy is enabled on a non-loopback address
        if self.config.trusted_proxy_header.is_some() && !addr.ip().is_loopback() {
            tracing::warn!(
                header = ?self.config.trusted_proxy_header,
                bind = %addr,
                "Trusted-proxy auth is enabled on a non-loopback address. \
                 Any client can forge the header and bypass authentication. \
                 Ensure a reverse proxy strips and re-sets this header."
            );
        }

        server::start_server(addr, self.state.clone(), self.auth_token.clone()).await?;

        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    async fn respond(
        &self,
        msg: &IncomingMessage,
        response: OutgoingResponse,
    ) -> Result<(), ChannelError> {
        let thread_id = msg.thread_id.clone().unwrap_or_default();

        self.state.sse.broadcast(SseEvent::Response {
            content: response.content,
            thread_id,
        });

        Ok(())
    }

    async fn send_status(
        &self,
        status: StatusUpdate,
        metadata: &serde_json::Value,
    ) -> Result<(), ChannelError> {
        let thread_id = metadata
            .get("thread_id")
            .and_then(|v| v.as_str())
            .map(String::from);
        let event = match status {
            StatusUpdate::Thinking(msg) => SseEvent::Thinking {
                message: msg,
                thread_id: thread_id.clone(),
            },
            StatusUpdate::ToolStarted { name } => SseEvent::ToolStarted {
                name,
                thread_id: thread_id.clone(),
            },
            StatusUpdate::ToolCompleted { name, success } => SseEvent::ToolCompleted {
                name,
                success,
                thread_id: thread_id.clone(),
            },
            StatusUpdate::ToolResult { name, preview } => SseEvent::ToolResult {
                name,
                preview,
                thread_id: thread_id.clone(),
            },
            StatusUpdate::ToolProgress { name, chunk } => SseEvent::ToolProgress {
                name,
                chunk,
                thread_id: thread_id.clone(),
            },
            StatusUpdate::StreamChunk(content) => SseEvent::StreamChunk {
                content,
                thread_id: thread_id.clone(),
            },
            StatusUpdate::Status(msg) => SseEvent::Status {
                message: msg,
                thread_id: thread_id.clone(),
            },
            StatusUpdate::JobStarted {
                job_id,
                title,
                browse_url,
            } => SseEvent::JobStarted {
                job_id,
                title,
                browse_url,
            },
            StatusUpdate::ApprovalNeeded {
                request_id,
                tool_name,
                description,
                parameters,
            } => SseEvent::ApprovalNeeded {
                request_id,
                tool_name,
                description,
                parameters: serde_json::to_string_pretty(&parameters)
                    .unwrap_or_else(|_| parameters.to_string()),
                thread_id,
            },
            StatusUpdate::AuthRequired {
                extension_name,
                instructions,
                auth_url,
                setup_url,
            } => SseEvent::AuthRequired {
                extension_name,
                instructions,
                auth_url,
                setup_url,
            },
            StatusUpdate::AuthCompleted {
                extension_name,
                success,
                message,
            } => SseEvent::AuthCompleted {
                extension_name,
                success,
                message,
            },
        };

        self.state.sse.broadcast(event);
        Ok(())
    }

    async fn broadcast(
        &self,
        _user_id: &str,
        response: OutgoingResponse,
    ) -> Result<(), ChannelError> {
        self.state.sse.broadcast(SseEvent::Response {
            content: response.content,
            thread_id: String::new(),
        });
        Ok(())
    }

    async fn health_check(&self) -> Result<(), ChannelError> {
        if self.state.msg_tx.read().await.is_some() {
            Ok(())
        } else {
            Err(ChannelError::HealthCheckFailed {
                name: "gateway".to_string(),
            })
        }
    }

    async fn shutdown(&self) -> Result<(), ChannelError> {
        if let Some(tx) = self.state.shutdown_tx.write().await.take() {
            let _ = tx.send(());
        }
        *self.state.msg_tx.write().await = None;

        // Release PID lock (removes the PID file)
        self.pid_lock.lock().await.take();

        Ok(())
    }
}
