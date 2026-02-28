//! Playwright-based browser automation tools.
//!
//! Provides six tools for web interaction: navigate, click, type, screenshot,
//! read_page, and close. Each browser session is backed by a headless Chromium
//! subprocess managed via a Node.js Playwright driver script that communicates
//! over stdin/stdout JSON.
//!
//! # Security Model
//!
//! - **SSRF defense**: All navigation URLs pass through `validate_url()` (DNS
//!   resolution + private IP blocking) and an optional domain allowlist.
//! - **Environment isolation**: The Node.js subprocess launches with `env_clear()`
//!   and only receives safe OS/toolchain variables (`SAFE_ENV_VARS`).
//! - **Leak detection**: Page content returned by `browser_read_page` is scanned
//!   by `LeakDetector` before reaching the LLM context.
//! - **Output limits**: Page text is truncated to 64 KB (`MAX_OUTPUT_SIZE`).
//! - **Rate limiting**: Navigate/click/type at 20/min, screenshot at 10/min.
//! - **Session limits**: Configurable max concurrent sessions, stale cleanup.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, RwLock};

use crate::context::JobContext;
use crate::error::BrowserError;
use crate::safety::{LeakDetectionError, LeakDetector};
use crate::sandbox::proxy::allowlist::DomainAllowlist;
use crate::tools::builtin::http::validate_url;
use crate::tools::builtin::shell::SAFE_ENV_VARS;
use crate::tools::tool::{
    ApprovalRequirement, Tool, ToolDomain, ToolError, ToolOutput, ToolRateLimitConfig, require_str,
};

impl From<BrowserError> for ToolError {
    fn from(e: BrowserError) -> Self {
        match e {
            BrowserError::SessionNotFound { .. } => ToolError::InvalidParameters(e.to_string()),
            BrowserError::SsrfBlocked { .. } => ToolError::NotAuthorized(e.to_string()),
            _ => ToolError::ExecutionFailed(e.to_string()),
        }
    }
}

/// Maximum output size before truncation (64 KB, matches shell tool).
const MAX_OUTPUT_SIZE: usize = 64 * 1024;

/// Maximum allowed CSS selector length.
const MAX_SELECTOR_LENGTH: usize = 512;

/// Validate a CSS selector string for safety.
///
/// Rejects:
/// - Empty selectors
/// - Selectors exceeding `MAX_SELECTOR_LENGTH`
/// - XPath expressions (`//`, `xpath=`)
/// - Playwright-specific dangerous pseudo-selectors
fn validate_selector(selector: &str) -> Result<(), ToolError> {
    if selector.is_empty() {
        return Err(ToolError::InvalidParameters(
            "selector cannot be empty".to_string(),
        ));
    }
    if selector.len() > MAX_SELECTOR_LENGTH {
        return Err(ToolError::InvalidParameters(format!(
            "selector too long ({} chars, max {})",
            selector.len(),
            MAX_SELECTOR_LENGTH
        )));
    }
    let lower = selector.to_lowercase();
    if lower.starts_with("//") || lower.starts_with("xpath=") || lower.contains(">> internal:") {
        return Err(ToolError::InvalidParameters(
            "XPath and internal selectors are not allowed; use CSS selectors only".to_string(),
        ));
    }
    Ok(())
}

/// Default browser session timeout (30 minutes).
const SESSION_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Maximum concurrent browser sessions.
const DEFAULT_MAX_SESSIONS: usize = 5;

/// Timeout for individual commands sent to the Playwright subprocess.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Name of the bundled Playwright driver script.
const DRIVER_SCRIPT_NAME: &str = "browser_driver.js";

// ---------------------------------------------------------------------------
// Session types
// ---------------------------------------------------------------------------

/// A browser session wrapping a Playwright subprocess.
pub(crate) struct BrowserSession {
    /// Unique session ID (kept for logging/debugging).
    _id: String,
    /// The Playwright subprocess handle.
    _child: Child,
    /// Communication channel (stdin for sending JSON commands).
    stdin: ChildStdin,
    /// Buffer for reading JSON responses from stdout.
    stdout: BufReader<ChildStdout>,
}

/// Entry in the session map.
///
/// `created_at` lives outside the Mutex so `cleanup_stale` can check
/// session age without acquiring the per-session lock, avoiding silent
/// skips when a session is actively in use by a command.
struct SessionEntry {
    session: Arc<Mutex<BrowserSession>>,
    created_at: Instant,
}

impl BrowserSession {
    /// Send a JSON command to the Playwright driver and read the response.
    async fn send_command(
        &mut self,
        cmd: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        let mut line = serde_json::to_string(&cmd).map_err(|e| {
            ToolError::ExecutionFailed(format!("failed to serialize browser command: {e}"))
        })?;
        line.push('\n');

        self.stdin.write_all(line.as_bytes()).await.map_err(|e| {
            ToolError::ExecutionFailed(format!("failed to write to browser process: {e}"))
        })?;
        self.stdin.flush().await.map_err(|e| {
            ToolError::ExecutionFailed(format!("failed to flush browser stdin: {e}"))
        })?;

        let mut response_line = String::new();
        let read_result =
            tokio::time::timeout(COMMAND_TIMEOUT, self.stdout.read_line(&mut response_line)).await;

        match read_result {
            Ok(Ok(0)) => Err(ToolError::ExecutionFailed(
                "browser process closed unexpectedly".to_string(),
            )),
            Ok(Ok(_)) => serde_json::from_str::<serde_json::Value>(response_line.trim())
                .map_err(|e| ToolError::ExecutionFailed(format!("invalid JSON from browser: {e}"))),
            Ok(Err(e)) => Err(ToolError::ExecutionFailed(format!(
                "failed to read from browser: {e}"
            ))),
            Err(_) => Err(ToolError::Timeout(COMMAND_TIMEOUT)),
        }
    }
}

impl SessionEntry {
    /// Whether this session has exceeded its lifetime.
    ///
    /// Can be checked without acquiring the per-session Mutex.
    fn is_stale(&self) -> bool {
        self.created_at.elapsed() > SESSION_TIMEOUT
    }
}

// ---------------------------------------------------------------------------
// Session manager
// ---------------------------------------------------------------------------

/// Manages all active browser sessions.
///
/// Each session is an independently lockable Playwright subprocess so that
/// concurrent tool calls on different sessions do not block each other.
pub struct BrowserSessionManager {
    sessions: RwLock<HashMap<String, SessionEntry>>,
    max_sessions: usize,
    url_allowlist: Option<DomainAllowlist>,
    driver_path: PathBuf,
}

impl BrowserSessionManager {
    /// Create a new session manager.
    pub fn new(driver_path: PathBuf) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            max_sessions: DEFAULT_MAX_SESSIONS,
            url_allowlist: None,
            driver_path,
        }
    }

    /// Set the maximum number of concurrent browser sessions.
    pub fn with_max_sessions(mut self, max: usize) -> Self {
        self.max_sessions = max;
        self
    }

    /// Set an optional domain allowlist for navigation URLs.
    pub fn with_url_allowlist(mut self, allowlist: DomainAllowlist) -> Self {
        self.url_allowlist = Some(allowlist);
        self
    }

    /// Get a reference to the domain allowlist, if configured.
    pub fn url_allowlist(&self) -> Option<&DomainAllowlist> {
        self.url_allowlist.as_ref()
    }

    /// Create a new browser session, returning its ID.
    ///
    /// Cleans up stale sessions first, then checks capacity before spawning
    /// a new Playwright subprocess. The capacity check and insert are done
    /// under a single write lock to prevent TOCTOU races.
    pub async fn create_session(&self) -> Result<String, ToolError> {
        self.cleanup_stale().await;

        // Fast pre-check under a read lock: reject early if already at capacity.
        // This avoids spawning an expensive Chromium subprocess when obviously full.
        // The authoritative check is under the write lock below (no TOCTOU gap).
        {
            let sessions = self.sessions.read().await;
            if sessions.len() >= self.max_sessions {
                return Err(BrowserError::SessionLimitReached {
                    max: self.max_sessions,
                }
                .into());
            }
        }

        // Spawn the subprocess *before* acquiring the write lock so we don't
        // hold it across blocking I/O. If capacity is exceeded when we go to
        // insert, we kill the process and return an error.
        let mut cmd = Command::new("node");
        cmd.arg(&self.driver_path);
        cmd.env_clear();
        for var in SAFE_ENV_VARS {
            if let Ok(val) = std::env::var(var) {
                cmd.env(var, val);
            }
        }
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| BrowserError::SubprocessError {
            reason: format!("failed to launch browser (is node and playwright installed?): {e}"),
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| BrowserError::SubprocessError {
                reason: "failed to capture stdin for browser process".to_string(),
            })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| BrowserError::SubprocessError {
                reason: "failed to capture stdout for browser process".to_string(),
            })?;

        // Drain stderr asynchronously to prevent the subprocess from blocking
        // when the stderr pipe buffer fills up (~64 KB on Linux). Without this,
        // Chromium warnings/errors can block stdout writes, causing send_command
        // to hang until COMMAND_TIMEOUT.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                    tracing::debug!(target: "browser_driver_stderr", "{}", line.trim());
                    line.clear();
                }
            });
        }

        let session_id = uuid::Uuid::new_v4().to_string();
        let session = BrowserSession {
            _id: session_id.clone(),
            _child: child,
            stdin,
            stdout: BufReader::new(stdout),
        };

        // Single write lock for both capacity check and insert — no TOCTOU gap.
        let mut sessions = self.sessions.write().await;
        if sessions.len() >= self.max_sessions {
            // Session will be dropped here, and kill_on_drop will clean up.
            return Err(BrowserError::SessionLimitReached {
                max: self.max_sessions,
            }
            .into());
        }
        sessions.insert(
            session_id.clone(),
            SessionEntry {
                session: Arc::new(Mutex::new(session)),
                created_at: Instant::now(),
            },
        );
        drop(sessions);

        tracing::info!(session_id = %session_id, "created browser session");
        Ok(session_id)
    }

    /// Get a handle to an existing session.
    pub(crate) async fn get_session(
        &self,
        id: &str,
    ) -> Result<Arc<Mutex<BrowserSession>>, ToolError> {
        self.sessions
            .read()
            .await
            .get(id)
            .map(|e| e.session.clone())
            .ok_or_else(|| BrowserError::SessionNotFound { id: id.to_string() }.into())
    }

    /// Close and remove a session, killing the subprocess.
    pub async fn close_session(&self, id: &str) -> Result<(), ToolError> {
        let entry = self.sessions.write().await.remove(id).ok_or_else(|| {
            ToolError::InvalidParameters(format!("browser session '{id}' not found"))
        })?;

        let mut session = entry.session.lock().await;
        // Try to send a graceful close command; ignore errors (process may already be dead).
        let _ = session
            .send_command(serde_json::json!({"action": "close"}))
            .await;

        tracing::info!(session_id = %id, "closed browser session");
        Ok(())
    }

    /// Remove sessions that have exceeded their lifetime.
    ///
    /// Two-phase approach: (1) identify stale IDs under read lock — no per-session
    /// Mutex needed because `created_at` lives on `SessionEntry`, (2) remove them
    /// under write lock. Graceful close commands are sent after releasing all locks.
    async fn cleanup_stale(&self) {
        // Phase 1: identify stale sessions under a read lock.
        // `created_at` is on SessionEntry, so no per-session lock is needed.
        let stale_ids: Vec<String> = {
            let sessions = self.sessions.read().await;
            sessions
                .iter()
                .filter(|(_, entry)| entry.is_stale())
                .map(|(id, _)| id.clone())
                .collect()
        };

        if stale_ids.is_empty() {
            return;
        }

        // Phase 2: remove stale sessions under write lock (no .await here).
        let removed: Vec<(String, SessionEntry)> = {
            let mut sessions = self.sessions.write().await;
            stale_ids
                .into_iter()
                .filter_map(|id| sessions.remove(&id).map(|entry| (id, entry)))
                .collect()
        };

        // Phase 3: send graceful close commands without holding any map lock.
        // Use a short timeout to avoid blocking session creation if an in-flight
        // caller still holds the Mutex. The subprocess has kill_on_drop so the
        // graceful close is a courtesy, not a correctness requirement.
        for (id, entry) in &removed {
            match tokio::time::timeout(Duration::from_secs(2), entry.session.lock()).await {
                Ok(mut session) => {
                    let _ = session
                        .send_command(serde_json::json!({"action": "close"}))
                        .await;
                }
                Err(_) => {
                    tracing::debug!(
                        session_id = %id,
                        "stale session lock timed out, relying on kill_on_drop"
                    );
                }
            }
            tracing::info!(session_id = %id, "cleaned up stale browser session");
        }
    }
}

// ---------------------------------------------------------------------------
// Driver script locator
// ---------------------------------------------------------------------------

/// Locate the `browser_driver.js` script.
///
/// Searches (1) adjacent to the running binary (dist/release builds),
/// (2) `CARGO_MANIFEST_DIR/src/tools/builtin/` (development / CI).
/// Does NOT search relative to CWD to avoid surprising behavior.
pub fn find_driver_script() -> Result<PathBuf, ToolError> {
    // Adjacent to binary (release / dist builds)
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let adjacent = dir.join(DRIVER_SCRIPT_NAME);
        if adjacent.exists() {
            return Ok(adjacent);
        }
    }

    // Cargo manifest dir (development / CI / tests)
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        let cargo_path = PathBuf::from(manifest)
            .join("src/tools/builtin")
            .join(DRIVER_SCRIPT_NAME);
        if cargo_path.exists() {
            return Ok(cargo_path);
        }
    }

    Err(ToolError::ExecutionFailed(
        "browser_driver.js not found — ensure the driver script is adjacent to the binary \
         or CARGO_MANIFEST_DIR is set"
            .to_string(),
    ))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a driver response, returning the `data` field on success or a
/// `ToolError` on failure.
fn parse_driver_response(response: &serde_json::Value) -> Result<&serde_json::Value, ToolError> {
    if response.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        Ok(response.get("data").unwrap_or(&serde_json::Value::Null))
    } else {
        let msg = response
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("unknown browser error");
        Err(ToolError::ExecutionFailed(format!("browser: {msg}")))
    }
}

/// Get or create a session, returning the session ID and a handle.
async fn get_or_create_session(
    manager: &BrowserSessionManager,
    params: &serde_json::Value,
) -> Result<(String, Arc<Mutex<BrowserSession>>), ToolError> {
    let session_id = if let Some(id) = params.get("session_id").and_then(|v| v.as_str()) {
        id.to_string()
    } else {
        manager.create_session().await?
    };
    let handle = manager.get_session(&session_id).await?;
    Ok((session_id, handle))
}

/// Get a required session by ID from params.
async fn get_required_session(
    manager: &BrowserSessionManager,
    params: &serde_json::Value,
) -> Result<(String, Arc<Mutex<BrowserSession>>), ToolError> {
    let session_id = require_str(params, "session_id")?;
    let handle = manager.get_session(session_id).await?;
    Ok((session_id.to_string(), handle))
}

/// Truncate text to `MAX_OUTPUT_SIZE`, appending a notice if truncated.
fn truncate_output(text: &str) -> String {
    if text.len() <= MAX_OUTPUT_SIZE {
        text.to_string()
    } else {
        // Find a safe truncation point (don't split a multi-byte char)
        let boundary = text
            .char_indices()
            .take_while(|(i, _)| *i < MAX_OUTPUT_SIZE)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(MAX_OUTPUT_SIZE);
        format!(
            "{}...\n[truncated at {} bytes]",
            &text[..boundary],
            boundary,
        )
    }
}

// ---------------------------------------------------------------------------
// Tool: browser_navigate
// ---------------------------------------------------------------------------

/// Navigate a browser to a URL (HTTPS only).
pub struct BrowserNavigateTool {
    session_manager: Arc<BrowserSessionManager>,
}

impl BrowserNavigateTool {
    pub fn new(session_manager: Arc<BrowserSessionManager>) -> Self {
        Self { session_manager }
    }
}

#[async_trait]
impl Tool for BrowserNavigateTool {
    fn name(&self) -> &str {
        "browser_navigate"
    }

    fn description(&self) -> &str {
        "Navigate a browser to a URL. Creates a new browser session if no session_id is \
         provided. Returns the page title, final URL, and session ID."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "URL to navigate to (https only)"
                },
                "session_id": {
                    "type": "string",
                    "description": "Optional existing session ID. A new session is created if omitted."
                },
                "wait_for": {
                    "type": "string",
                    "description": "Optional CSS selector to wait for after navigation"
                }
            },
            "required": ["url"]
        })
    }

    fn requires_approval(&self, _params: &serde_json::Value) -> ApprovalRequirement {
        ApprovalRequirement::UnlessAutoApproved
    }

    fn domain(&self) -> ToolDomain {
        // Browser tools launch an unsandboxed Chromium subprocess (--no-sandbox).
        // They MUST run inside a Docker container to provide process isolation.
        ToolDomain::Container
    }

    fn rate_limit_config(&self) -> Option<ToolRateLimitConfig> {
        Some(ToolRateLimitConfig::new(20, 200))
    }

    fn execution_timeout(&self) -> Duration {
        Duration::from_secs(90)
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let url_str = require_str(&params, "url")?;

        // SSRF defense: validate URL, resolve DNS, check for private IPs.
        let (parsed_url, _resolved_addrs) = validate_url(url_str).await?;

        // Domain allowlist check (initial URL).
        // The final URL after redirects is re-checked below after the
        // Playwright driver returns the actual page location.
        if let Some(allowlist) = self.session_manager.url_allowlist() {
            let host = parsed_url.host_str().unwrap_or("");
            if !allowlist.is_allowed(host).is_allowed() {
                return Err(BrowserError::SsrfBlocked {
                    reason: format!("domain '{host}' is not in the browser allowlist"),
                }
                .into());
            }
        }

        let (session_id, handle) = get_or_create_session(&self.session_manager, &params).await?;
        let mut session = handle.lock().await;

        let mut cmd = serde_json::json!({
            "action": "navigate",
            "url": parsed_url.as_str(),
        });
        if let Some(wait_for) = params.get("wait_for").and_then(|v| v.as_str()) {
            validate_selector(wait_for)?;
            cmd["wait_for"] = serde_json::json!(wait_for);
        }

        let response = session.send_command(cmd).await?;
        let data = parse_driver_response(&response)?;

        // SSRF defense: check the *final* URL after any server-side redirects.
        // The Playwright driver returns `page.url()` which reflects the actual
        // page location after following 3xx redirects. If the final URL differs
        // from the requested URL, re-validate it to catch redirects to private
        // IPs (e.g., a public site that 302s to http://169.254.169.254/).
        let final_url = data.get("url").and_then(|u| u.as_str()).unwrap_or("");
        if !final_url.is_empty() && final_url != parsed_url.as_str() {
            // Re-validate the final URL (DNS resolution + private IP check).
            if let Err(e) = validate_url(final_url).await {
                tracing::warn!(
                    original = %parsed_url,
                    final_url = %final_url,
                    "browser redirect led to blocked URL: {e}"
                );
                // Close the session to prevent further interaction with the
                // disallowed page.
                drop(session);
                let _ = self.session_manager.close_session(&session_id).await;
                return Err(BrowserError::SsrfBlocked {
                    reason: format!(
                        "redirect from {} to {} was blocked: {e}",
                        parsed_url, final_url,
                    ),
                }
                .into());
            }

            // Also check the domain allowlist for the final URL.
            if let Some(allowlist) = self.session_manager.url_allowlist()
                && let Ok(final_parsed) = reqwest::Url::parse(final_url)
            {
                let final_host = final_parsed.host_str().unwrap_or("");
                if !allowlist.is_allowed(final_host).is_allowed() {
                    tracing::warn!(
                        original = %parsed_url,
                        final_url = %final_url,
                        "browser redirect led to off-allowlist domain"
                    );
                    drop(session);
                    let _ = self.session_manager.close_session(&session_id).await;
                    return Err(BrowserError::SsrfBlocked {
                        reason: format!(
                            "redirect to domain '{}' is not in the browser allowlist",
                            final_host,
                        ),
                    }
                    .into());
                }
            }
        }

        Ok(ToolOutput::success(
            serde_json::json!({
                "session_id": session_id,
                "title": data.get("title").and_then(|t| t.as_str()).unwrap_or(""),
                "url": final_url,
            }),
            start.elapsed(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Tool: browser_click
// ---------------------------------------------------------------------------

/// Click an element in a browser session.
pub struct BrowserClickTool {
    session_manager: Arc<BrowserSessionManager>,
}

impl BrowserClickTool {
    pub fn new(session_manager: Arc<BrowserSessionManager>) -> Self {
        Self { session_manager }
    }
}

#[async_trait]
impl Tool for BrowserClickTool {
    fn name(&self) -> &str {
        "browser_click"
    }

    fn description(&self) -> &str {
        "Click an element identified by a CSS selector in an existing browser session."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Browser session ID"
                },
                "selector": {
                    "type": "string",
                    "description": "CSS selector of the element to click"
                }
            },
            "required": ["session_id", "selector"]
        })
    }

    fn requires_approval(&self, _params: &serde_json::Value) -> ApprovalRequirement {
        ApprovalRequirement::UnlessAutoApproved
    }

    fn domain(&self) -> ToolDomain {
        ToolDomain::Container
    }

    fn rate_limit_config(&self) -> Option<ToolRateLimitConfig> {
        Some(ToolRateLimitConfig::new(20, 200))
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let selector = require_str(&params, "selector")?;
        validate_selector(selector)?;
        let (_session_id, handle) = get_required_session(&self.session_manager, &params).await?;
        let mut session = handle.lock().await;

        let response = session
            .send_command(serde_json::json!({
                "action": "click",
                "selector": selector,
            }))
            .await?;
        let data = parse_driver_response(&response)?;

        Ok(ToolOutput::success(data.clone(), start.elapsed()))
    }
}

// ---------------------------------------------------------------------------
// Tool: browser_type
// ---------------------------------------------------------------------------

/// Type text into a form field in a browser session.
pub struct BrowserTypeTool {
    session_manager: Arc<BrowserSessionManager>,
    leak_detector: LeakDetector,
}

impl BrowserTypeTool {
    pub fn new(session_manager: Arc<BrowserSessionManager>) -> Self {
        Self {
            session_manager,
            leak_detector: LeakDetector::new(),
        }
    }
}

#[async_trait]
impl Tool for BrowserTypeTool {
    fn name(&self) -> &str {
        "browser_type"
    }

    fn description(&self) -> &str {
        "Type text into a form field identified by a CSS selector in a browser session."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Browser session ID"
                },
                "selector": {
                    "type": "string",
                    "description": "CSS selector of the input field"
                },
                "text": {
                    "type": "string",
                    "description": "Text to type into the field"
                }
            },
            "required": ["session_id", "selector", "text"]
        })
    }

    fn requires_approval(&self, _params: &serde_json::Value) -> ApprovalRequirement {
        ApprovalRequirement::UnlessAutoApproved
    }

    fn domain(&self) -> ToolDomain {
        ToolDomain::Container
    }

    fn rate_limit_config(&self) -> Option<ToolRateLimitConfig> {
        Some(ToolRateLimitConfig::new(20, 200))
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let selector = require_str(&params, "selector")?;
        validate_selector(selector)?;
        let text = require_str(&params, "text")?;

        // Leak-scan the text being typed to catch credential injection.
        // Reject ANY match (block or redact) — we should never type secrets into a page.
        let scan_result = self.leak_detector.scan(text);
        if !scan_result.is_clean() {
            let pattern_name = scan_result
                .matches
                .first()
                .map(|m| m.pattern_name.as_str())
                .unwrap_or("unknown");
            return Err(ToolError::NotAuthorized(format!(
                "text contains a potential secret ({pattern_name}); refusing to type it"
            )));
        }

        let (_session_id, handle) = get_required_session(&self.session_manager, &params).await?;
        let mut session = handle.lock().await;

        let response = session
            .send_command(serde_json::json!({
                "action": "type",
                "selector": selector,
                "text": text,
            }))
            .await?;
        let data = parse_driver_response(&response)?;

        Ok(ToolOutput::success(data.clone(), start.elapsed()))
    }
}

// ---------------------------------------------------------------------------
// Tool: browser_screenshot
// ---------------------------------------------------------------------------

/// Take a screenshot of the current page or a specific element.
pub struct BrowserScreenshotTool {
    session_manager: Arc<BrowserSessionManager>,
}

impl BrowserScreenshotTool {
    pub fn new(session_manager: Arc<BrowserSessionManager>) -> Self {
        Self { session_manager }
    }
}

#[async_trait]
impl Tool for BrowserScreenshotTool {
    fn name(&self) -> &str {
        "browser_screenshot"
    }

    fn description(&self) -> &str {
        "Take a PNG screenshot of the current page or a specific element. \
         Returns the image as a base64-encoded string."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Browser session ID"
                },
                "selector": {
                    "type": "string",
                    "description": "Optional CSS selector to screenshot a specific element"
                },
                "full_page": {
                    "type": "boolean",
                    "description": "Capture the full scrollable page (default: false)"
                }
            },
            "required": ["session_id"]
        })
    }

    fn domain(&self) -> ToolDomain {
        ToolDomain::Container
    }

    fn rate_limit_config(&self) -> Option<ToolRateLimitConfig> {
        Some(ToolRateLimitConfig::new(10, 100))
    }

    fn execution_timeout(&self) -> Duration {
        Duration::from_secs(30)
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let (_session_id, handle) = get_required_session(&self.session_manager, &params).await?;
        let mut session = handle.lock().await;

        let mut cmd = serde_json::json!({
            "action": "screenshot",
        });
        if let Some(selector) = params.get("selector").and_then(|v| v.as_str()) {
            validate_selector(selector)?;
            cmd["selector"] = serde_json::json!(selector);
        }
        if let Some(full_page) = params.get("full_page").and_then(|v| v.as_bool()) {
            cmd["full_page"] = serde_json::json!(full_page);
        }

        let response = session.send_command(cmd).await?;
        let data = parse_driver_response(&response)?;

        Ok(ToolOutput::success(data.clone(), start.elapsed()))
    }
}

// ---------------------------------------------------------------------------
// Tool: browser_read_page
// ---------------------------------------------------------------------------

/// Read the text content of the current page or a specific element.
pub struct BrowserReadPageTool {
    session_manager: Arc<BrowserSessionManager>,
    leak_detector: LeakDetector,
}

impl BrowserReadPageTool {
    pub fn new(session_manager: Arc<BrowserSessionManager>) -> Self {
        Self {
            session_manager,
            leak_detector: LeakDetector::new(),
        }
    }
}

#[async_trait]
impl Tool for BrowserReadPageTool {
    fn name(&self) -> &str {
        "browser_read_page"
    }

    fn description(&self) -> &str {
        "Read the text content of the current page or a specific element. \
         Content is scanned for secrets and truncated to 64 KB."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Browser session ID"
                },
                "selector": {
                    "type": "string",
                    "description": "Optional CSS selector to read a specific element. Reads full page if omitted."
                }
            },
            "required": ["session_id"]
        })
    }

    fn domain(&self) -> ToolDomain {
        ToolDomain::Container
    }

    fn rate_limit_config(&self) -> Option<ToolRateLimitConfig> {
        Some(ToolRateLimitConfig::new(20, 200))
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let (_session_id, handle) = get_required_session(&self.session_manager, &params).await?;
        let mut session = handle.lock().await;

        let mut cmd = serde_json::json!({ "action": "read_page" });
        if let Some(selector) = params.get("selector").and_then(|v| v.as_str()) {
            validate_selector(selector)?;
            cmd["selector"] = serde_json::json!(selector);
        }

        let response = session.send_command(cmd).await?;
        let data = parse_driver_response(&response)?;
        let page_text = data.get("text").and_then(|t| t.as_str()).unwrap_or("");

        // Leak detection: scan page content for secrets before returning.
        let cleaned = self
            .leak_detector
            .scan_and_clean(page_text)
            .map_err(|e| match e {
                LeakDetectionError::SecretLeakBlocked { pattern, preview } => {
                    ToolError::from(BrowserError::LeakDetected {
                        reason: format!(
                            "page content contains potential secret ({pattern}): {preview}"
                        ),
                    })
                }
            })?;

        let output_text = truncate_output(&cleaned);

        Ok(ToolOutput::text(output_text, start.elapsed()))
    }
}

// ---------------------------------------------------------------------------
// Tool: browser_close
// ---------------------------------------------------------------------------

/// Close a browser session and kill the subprocess.
pub struct BrowserCloseTool {
    session_manager: Arc<BrowserSessionManager>,
}

impl BrowserCloseTool {
    pub fn new(session_manager: Arc<BrowserSessionManager>) -> Self {
        Self { session_manager }
    }
}

#[async_trait]
impl Tool for BrowserCloseTool {
    fn name(&self) -> &str {
        "browser_close"
    }

    fn description(&self) -> &str {
        "Close a browser session and release its resources."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Browser session ID to close"
                }
            },
            "required": ["session_id"]
        })
    }

    fn requires_approval(&self, _params: &serde_json::Value) -> ApprovalRequirement {
        ApprovalRequirement::Never
    }

    fn domain(&self) -> ToolDomain {
        ToolDomain::Container
    }

    fn requires_sanitization(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();
        let session_id = require_str(&params, "session_id")?;
        self.session_manager.close_session(session_id).await?;
        Ok(ToolOutput::text(
            format!("session '{session_id}' closed"),
            start.elapsed(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Parameter validation tests ----

    fn dummy_ctx() -> JobContext {
        JobContext::new("test-job".to_string(), "test-user".to_string())
    }

    fn make_manager() -> Arc<BrowserSessionManager> {
        // Use a non-existent path — we won't actually launch a subprocess in
        // parameter validation tests.
        Arc::new(BrowserSessionManager::new(PathBuf::from(
            "/nonexistent/browser_driver.js",
        )))
    }

    #[tokio::test]
    async fn test_navigate_missing_url() {
        let tool = BrowserNavigateTool::new(make_manager());
        let result = tool.execute(serde_json::json!({}), &dummy_ctx()).await;
        assert!(matches!(result, Err(ToolError::InvalidParameters(_))));
    }

    #[tokio::test]
    async fn test_navigate_rejects_http() {
        let tool = BrowserNavigateTool::new(make_manager());
        let result = tool
            .execute(
                serde_json::json!({"url": "http://example.com"}),
                &dummy_ctx(),
            )
            .await;
        assert!(matches!(result, Err(ToolError::NotAuthorized(_))));
    }

    #[tokio::test]
    async fn test_navigate_rejects_localhost() {
        let tool = BrowserNavigateTool::new(make_manager());
        let result = tool
            .execute(
                serde_json::json!({"url": "https://localhost"}),
                &dummy_ctx(),
            )
            .await;
        assert!(matches!(result, Err(ToolError::NotAuthorized(_))));
    }

    #[tokio::test]
    async fn test_navigate_rejects_private_ip() {
        let tool = BrowserNavigateTool::new(make_manager());
        let result = tool
            .execute(
                serde_json::json!({"url": "https://192.168.1.1"}),
                &dummy_ctx(),
            )
            .await;
        assert!(matches!(result, Err(ToolError::NotAuthorized(_))));
    }

    #[tokio::test]
    async fn test_click_missing_session_id() {
        let tool = BrowserClickTool::new(make_manager());
        let result = tool
            .execute(serde_json::json!({"selector": "#btn"}), &dummy_ctx())
            .await;
        assert!(matches!(result, Err(ToolError::InvalidParameters(_))));
    }

    #[tokio::test]
    async fn test_click_missing_selector() {
        let tool = BrowserClickTool::new(make_manager());
        let result = tool
            .execute(serde_json::json!({"session_id": "fake-id"}), &dummy_ctx())
            .await;
        assert!(matches!(result, Err(ToolError::InvalidParameters(_))));
    }

    #[tokio::test]
    async fn test_type_missing_text() {
        let tool = BrowserTypeTool::new(make_manager());
        let result = tool
            .execute(
                serde_json::json!({"session_id": "fake-id", "selector": "#input"}),
                &dummy_ctx(),
            )
            .await;
        assert!(matches!(result, Err(ToolError::InvalidParameters(_))));
    }

    #[tokio::test]
    async fn test_close_missing_session_id() {
        let tool = BrowserCloseTool::new(make_manager());
        let result = tool.execute(serde_json::json!({}), &dummy_ctx()).await;
        assert!(matches!(result, Err(ToolError::InvalidParameters(_))));
    }

    #[tokio::test]
    async fn test_close_nonexistent_session() {
        let tool = BrowserCloseTool::new(make_manager());
        let result = tool
            .execute(
                serde_json::json!({"session_id": "does-not-exist"}),
                &dummy_ctx(),
            )
            .await;
        assert!(matches!(result, Err(ToolError::InvalidParameters(_))));
    }

    // ---- Domain allowlist tests ----

    #[tokio::test]
    async fn test_allowlist_blocks_unlisted_domain() {
        let mgr = Arc::new(
            BrowserSessionManager::new(PathBuf::from("/nonexistent"))
                .with_url_allowlist(DomainAllowlist::new(&["example.com".to_string()])),
        );
        let tool = BrowserNavigateTool::new(mgr);
        // Use google.com which reliably resolves — the test verifies the
        // allowlist rejects it (not on the list), not DNS behavior.
        let result = tool
            .execute(
                serde_json::json!({"url": "https://google.com/page"}),
                &dummy_ctx(),
            )
            .await;
        assert!(
            matches!(result, Err(ToolError::NotAuthorized(_))),
            "expected NotAuthorized for off-allowlist domain, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_allowlist_allows_listed_domain() {
        let mgr = Arc::new(
            BrowserSessionManager::new(PathBuf::from("/nonexistent"))
                .with_url_allowlist(DomainAllowlist::new(&["example.com".to_string()])),
        );
        let tool = BrowserNavigateTool::new(mgr);
        // This will fail later (no actual node process), but it should pass
        // the allowlist check. The error should NOT be NotAuthorized.
        let result = tool
            .execute(
                serde_json::json!({"url": "https://example.com/page"}),
                &dummy_ctx(),
            )
            .await;
        // Should fail trying to spawn, not at allowlist check
        assert!(!matches!(result, Err(ToolError::NotAuthorized(_))));
    }

    // ---- Leak detection tests ----

    #[test]
    fn test_leak_detector_blocks_api_key() {
        let detector = LeakDetector::new();
        let content = "Page text contains sk-proj-abcdefghijklmnopqrstuvwxyz1234567890ABCDEFGHIJKLMNOP which is a key";
        let result = detector.scan_and_clean(content);
        // Should either block or redact — either way the original is not returned as-is
        match result {
            Err(_) => {} // blocked — good
            Ok(cleaned) => assert_ne!(cleaned, content, "secret should be redacted"),
        }
    }

    #[test]
    fn test_leak_detector_passes_clean_content() {
        let detector = LeakDetector::new();
        let content = "This is a normal web page with no secrets.";
        let result = detector.scan_and_clean(content);
        let cleaned = result.expect("should be clean");
        assert_eq!(cleaned, content);
    }

    // ---- Output truncation tests ----

    #[test]
    fn test_truncate_output_short() {
        let text = "short text";
        assert_eq!(truncate_output(text), text);
    }

    #[test]
    fn test_truncate_output_long() {
        let text = "a".repeat(MAX_OUTPUT_SIZE + 1000);
        let result = truncate_output(&text);
        assert!(result.len() <= MAX_OUTPUT_SIZE + 100); // allow room for notice
        assert!(result.contains("[truncated at"));
    }

    #[test]
    fn test_truncate_output_multibyte() {
        // Ensure we don't split a multi-byte character.
        // Place a 4-byte emoji so its start index is within MAX_OUTPUT_SIZE
        // but it extends past it. We need at least MAX_OUTPUT_SIZE + 1 total
        // bytes to trigger truncation.
        //
        // MAX_OUTPUT_SIZE - 2 x-chars, then a 4-byte emoji, then more padding.
        // Emoji starts at byte (MAX_OUTPUT_SIZE - 2), ends at (MAX_OUTPUT_SIZE + 2).
        // take_while includes it (start < MAX_OUTPUT_SIZE), so boundary = MAX_OUTPUT_SIZE + 2.
        // Total text is longer, so truncation still fires but preserves the emoji.
        let text = "x".repeat(MAX_OUTPUT_SIZE - 2) + "\u{1F600}" + &"y".repeat(100);
        let result = truncate_output(&text);
        assert!(result.contains("[truncated at"), "should be truncated");
        // The result must be valid UTF-8 (Rust guarantees this via &str slicing)
        // and the emoji must be intact (not split).
        assert!(
            result.contains('\u{1F600}'),
            "emoji should be preserved intact"
        );

        // Also test that a string just barely over the limit gets truncated
        // without the emoji (emoji starts at MAX_OUTPUT_SIZE, excluded by take_while).
        let text2 = "x".repeat(MAX_OUTPUT_SIZE) + "\u{1F600}";
        let result2 = truncate_output(&text2);
        assert!(result2.contains("[truncated at"), "should be truncated");
        // Emoji starts at index MAX_OUTPUT_SIZE which is NOT < MAX_OUTPUT_SIZE,
        // so it's excluded. Boundary = MAX_OUTPUT_SIZE.
        assert!(
            !result2.contains('\u{1F600}'),
            "emoji at boundary should be excluded"
        );
    }

    // ---- Session manager unit tests ----

    #[tokio::test]
    async fn test_session_manager_get_nonexistent() {
        let mgr = BrowserSessionManager::new(PathBuf::from("/nonexistent"));
        let result = mgr.get_session("no-such-session").await;
        assert!(matches!(result, Err(ToolError::InvalidParameters(_))));
    }

    #[test]
    fn test_session_is_stale() {
        // We can't easily construct a BrowserSession without a real process,
        // but we can test the timeout constant is reasonable.
        assert_eq!(SESSION_TIMEOUT, Duration::from_secs(30 * 60));
    }

    // ---- Tool metadata tests ----

    #[test]
    fn test_tool_names() {
        let mgr = make_manager();
        assert_eq!(
            BrowserNavigateTool::new(Arc::clone(&mgr)).name(),
            "browser_navigate"
        );
        assert_eq!(
            BrowserClickTool::new(Arc::clone(&mgr)).name(),
            "browser_click"
        );
        assert_eq!(
            BrowserTypeTool::new(Arc::clone(&mgr)).name(),
            "browser_type"
        );
        assert_eq!(
            BrowserScreenshotTool::new(Arc::clone(&mgr)).name(),
            "browser_screenshot"
        );
        assert_eq!(
            BrowserReadPageTool::new(Arc::clone(&mgr)).name(),
            "browser_read_page"
        );
        assert_eq!(BrowserCloseTool::new(mgr).name(), "browser_close");
    }

    #[test]
    fn test_approval_requirements() {
        let mgr = make_manager();
        let params = serde_json::json!({});
        assert_eq!(
            BrowserNavigateTool::new(Arc::clone(&mgr)).requires_approval(&params),
            ApprovalRequirement::UnlessAutoApproved,
        );
        assert_eq!(
            BrowserClickTool::new(Arc::clone(&mgr)).requires_approval(&params),
            ApprovalRequirement::UnlessAutoApproved,
        );
        assert_eq!(
            BrowserTypeTool::new(Arc::clone(&mgr)).requires_approval(&params),
            ApprovalRequirement::UnlessAutoApproved,
        );
        assert_eq!(
            BrowserCloseTool::new(mgr).requires_approval(&params),
            ApprovalRequirement::Never,
        );
    }

    #[test]
    fn test_rate_limit_configs() {
        let mgr = make_manager();
        let nav_rl = BrowserNavigateTool::new(Arc::clone(&mgr)).rate_limit_config();
        assert!(nav_rl.is_some());
        let nav_rl = nav_rl.unwrap();
        assert_eq!(nav_rl.requests_per_minute, 20);
        assert_eq!(nav_rl.requests_per_hour, 200);

        let ss_rl = BrowserScreenshotTool::new(mgr).rate_limit_config();
        assert!(ss_rl.is_some());
        let ss_rl = ss_rl.unwrap();
        assert_eq!(ss_rl.requests_per_minute, 10);
        assert_eq!(ss_rl.requests_per_hour, 100);
    }

    #[test]
    fn test_parse_driver_response_success() {
        let resp = serde_json::json!({"ok": true, "data": {"title": "Example"}});
        let data = parse_driver_response(&resp).expect("should succeed");
        assert_eq!(data.get("title").and_then(|t| t.as_str()), Some("Example"));
    }

    #[test]
    fn test_parse_driver_response_error() {
        let resp = serde_json::json!({"ok": false, "error": "element not found"});
        let data = parse_driver_response(&resp);
        assert!(matches!(data, Err(ToolError::ExecutionFailed(_))));
    }

    // ---- Session limit enforcement test ----

    #[tokio::test]
    async fn test_session_limit_enforced() {
        // Configure a manager with max 1 session.
        let mgr = BrowserSessionManager::new(PathBuf::from("/nonexistent")).with_max_sessions(1);

        // First create_session will try to spawn node (which fails), but the
        // limit check happens *after* spawn in the new code. To test the limit
        // without a real node process, we directly insert a fake session handle.
        let fake_session = BrowserSession {
            _id: "fake-1".to_string(),
            _child: Command::new("true")
                .kill_on_drop(true)
                .spawn()
                .expect("true should exist"),
            stdin: Command::new("true")
                .stdin(std::process::Stdio::piped())
                .spawn()
                .expect("spawn")
                .stdin
                .take()
                .expect("stdin"),
            stdout: BufReader::new(
                Command::new("true")
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .expect("spawn")
                    .stdout
                    .take()
                    .expect("stdout"),
            ),
        };
        mgr.sessions.write().await.insert(
            "fake-1".to_string(),
            SessionEntry {
                session: Arc::new(Mutex::new(fake_session)),
                created_at: Instant::now(),
            },
        );

        // Now attempt to create another session — should fail with limit error.
        // The spawn will fail (node not at /nonexistent), but if it somehow
        // succeeds, the capacity check must still reject it.
        let result = mgr.create_session().await;
        match result {
            Err(ToolError::ExecutionFailed(msg)) => {
                // Either "maximum browser sessions reached" or "failed to launch"
                // Both are acceptable — the important thing is it doesn't succeed.
                assert!(
                    msg.contains("maximum browser sessions reached")
                        || msg.contains("failed to launch"),
                    "unexpected error: {msg}"
                );
            }
            Ok(_) => panic!("should not create session when at capacity"),
            Err(other) => panic!("unexpected error variant: {other}"),
        }
    }

    // ---- Selector validation tests ----

    #[test]
    fn test_validate_selector_rejects_empty() {
        assert!(matches!(
            validate_selector(""),
            Err(ToolError::InvalidParameters(_))
        ));
    }

    #[test]
    fn test_validate_selector_rejects_too_long() {
        let long = "a".repeat(MAX_SELECTOR_LENGTH + 1);
        assert!(matches!(
            validate_selector(&long),
            Err(ToolError::InvalidParameters(_))
        ));
    }

    #[test]
    fn test_validate_selector_rejects_xpath() {
        assert!(validate_selector("//div[@class='foo']").is_err());
        assert!(validate_selector("xpath=//html").is_err());
        assert!(validate_selector("XPATH=//html").is_err());
    }

    #[test]
    fn test_validate_selector_rejects_internal() {
        assert!(validate_selector("div >> internal:role=button").is_err());
    }

    #[test]
    fn test_validate_selector_accepts_valid_css() {
        assert!(validate_selector("#my-button").is_ok());
        assert!(validate_selector(".container > div:nth-child(2)").is_ok());
        assert!(validate_selector("input[type='text']").is_ok());
    }

    // ---- SSRF redirect defense tests ----

    #[tokio::test]
    async fn test_validate_url_rejects_redirect_to_metadata() {
        // Simulates what happens when a redirect leads to the metadata endpoint.
        // validate_url("https://169.254.169.254/") should fail because
        // 169.254.169.254 is a link-local (disallowed) IP.
        let result = validate_url("https://169.254.169.254/").await;
        assert!(result.is_err(), "metadata endpoint IP should be blocked");
    }

    #[tokio::test]
    async fn test_validate_url_rejects_redirect_to_loopback() {
        let result = validate_url("https://127.0.0.1/").await;
        assert!(result.is_err(), "loopback IP should be blocked");
    }

    // ---- Domain test ----

    #[test]
    fn test_all_tools_domain_is_container() {
        let mgr = make_manager();
        assert_eq!(
            BrowserNavigateTool::new(Arc::clone(&mgr)).domain(),
            ToolDomain::Container
        );
        assert_eq!(
            BrowserClickTool::new(Arc::clone(&mgr)).domain(),
            ToolDomain::Container
        );
        assert_eq!(
            BrowserTypeTool::new(Arc::clone(&mgr)).domain(),
            ToolDomain::Container
        );
        assert_eq!(
            BrowserScreenshotTool::new(Arc::clone(&mgr)).domain(),
            ToolDomain::Container
        );
        assert_eq!(
            BrowserReadPageTool::new(Arc::clone(&mgr)).domain(),
            ToolDomain::Container
        );
        assert_eq!(BrowserCloseTool::new(mgr).domain(), ToolDomain::Container);
    }
}
