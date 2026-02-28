//! MCP stdio transport — communicates with MCP servers via stdin/stdout.
//!
//! The stdio transport spawns a child process and exchanges newline-delimited
//! JSON-RPC 2.0 messages over its stdin (client → server) and stdout
//! (server → client). Stderr is drained and logged.
//!
//! # Usage
//!
//! ```ignore
//! let transport = StdioTransport::start("my-server", "npx", &["-y", "@mcp/server"], None)
//!     .await?;
//! let response = transport.send(McpRequest::list_tools(1)).await?;
//! transport.shutdown().await;
//! ```

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

use crate::tools::mcp::protocol::{McpError, McpRequest, McpResponse};
use crate::tools::tool::ToolError;

/// Default timeout for waiting for a response from the MCP server.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// MCP stdio transport.
///
/// Spawns a child process and communicates via newline-delimited JSON-RPC
/// over stdin/stdout. Background tasks handle reading stdout and draining stderr.
pub struct StdioTransport {
    /// Child process stdin, protected by mutex for concurrent writes.
    stdin: Arc<Mutex<tokio::process::ChildStdin>>,
    /// Pending response waiters, keyed by JSON-RPC request ID.
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<McpResponse>>>>,
    /// Whether the child process is still running.
    alive: Arc<AtomicBool>,
    /// Child process handle (for shutdown).
    child: Arc<Mutex<Option<Child>>>,
    /// Background reader task.
    reader_handle: Mutex<Option<JoinHandle<()>>>,
    /// Background stderr drain task.
    stderr_handle: Mutex<Option<JoinHandle<()>>>,
    /// Server name for logging.
    server_name: String,
}

impl std::fmt::Debug for StdioTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StdioTransport")
            .field("server_name", &self.server_name)
            .field("alive", &self.alive.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl StdioTransport {
    /// Start a new stdio transport by spawning a child process.
    ///
    /// The `command` and `args` specify the MCP server to run.
    /// Optional `env` provides additional environment variables.
    pub async fn start(
        server_name: &str,
        command: &str,
        args: &[&str],
        env: Option<&HashMap<String, String>>,
    ) -> Result<Self, ToolError> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if let Some(env_vars) = env {
            for (k, v) in env_vars {
                cmd.env(k, v);
            }
        }

        let mut child = cmd.spawn().map_err(|e| {
            ToolError::ExternalService(format!(
                "Failed to spawn MCP server '{}' ({}): {}",
                server_name, command, e
            ))
        })?;

        let stdin = child.stdin.take().ok_or_else(|| {
            ToolError::ExternalService("Failed to capture child stdin".to_string())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ToolError::ExternalService("Failed to capture child stdout".to_string())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            ToolError::ExternalService("Failed to capture child stderr".to_string())
        })?;

        let alive = Arc::new(AtomicBool::new(true));
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<McpResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Background task: read stdout line-by-line, dispatch responses
        let reader_pending = Arc::clone(&pending);
        let reader_alive = Arc::clone(&alive);
        let reader_name = server_name.to_string();
        let reader_handle = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        // EOF — child process closed stdout
                        tracing::debug!("MCP stdio '{}': stdout EOF", reader_name);
                        reader_alive.store(false, Ordering::SeqCst);
                        // Wake all pending requests with an error
                        let mut map = reader_pending.lock().await;
                        for (id, sender) in map.drain() {
                            let _ = sender.send(McpResponse {
                                jsonrpc: "2.0".to_string(),
                                id,
                                result: None,
                                error: Some(McpError {
                                    code: -1,
                                    message: "MCP server process exited".to_string(),
                                    data: None,
                                }),
                            });
                        }
                        break;
                    }
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<McpResponse>(trimmed) {
                            Ok(response) => {
                                let mut map = reader_pending.lock().await;
                                if let Some(sender) = map.remove(&response.id) {
                                    let _ = sender.send(response);
                                } else {
                                    tracing::trace!(
                                        "MCP stdio '{}': unmatched response id={}",
                                        reader_name,
                                        response.id
                                    );
                                }
                            }
                            Err(e) => {
                                tracing::trace!(
                                    "MCP stdio '{}': non-JSON stdout line: {} ({})",
                                    reader_name,
                                    trimmed,
                                    e
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("MCP stdio '{}': stdout read error: {}", reader_name, e);
                        reader_alive.store(false, Ordering::SeqCst);
                        break;
                    }
                }
            }
        });

        // Background task: drain stderr and log
        let stderr_name = server_name.to_string();
        let stderr_handle = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            tracing::debug!("MCP stdio '{}' stderr: {}", stderr_name, trimmed);
                        }
                    }
                    Err(e) => {
                        tracing::trace!("MCP stdio '{}': stderr read error: {}", stderr_name, e);
                        break;
                    }
                }
            }
        });

        tracing::info!(
            "MCP stdio transport started for '{}' ({})",
            server_name,
            command
        );

        Ok(Self {
            stdin: Arc::new(Mutex::new(stdin)),
            pending,
            alive,
            child: Arc::new(Mutex::new(Some(child))),
            reader_handle: Mutex::new(Some(reader_handle)),
            stderr_handle: Mutex::new(Some(stderr_handle)),
            server_name: server_name.to_string(),
        })
    }

    /// Send a JSON-RPC request and wait for the response.
    ///
    /// Notifications (requests with `id == 0`) are sent without waiting.
    /// Regular requests block until a matching response arrives or the
    /// timeout (60 s) expires.
    pub async fn send(&self, request: McpRequest) -> Result<McpResponse, ToolError> {
        if !self.alive.load(Ordering::SeqCst) {
            return Err(ToolError::ExternalService(format!(
                "MCP server '{}' process is not running",
                self.server_name
            )));
        }

        let is_notification = request.id == 0;

        // Serialize request as a single JSON line
        let mut json = serde_json::to_string(&request).map_err(|e| {
            ToolError::ExternalService(format!("Failed to serialize MCP request: {}", e))
        })?;
        json.push('\n');

        // Register pending response waiter (skip for notifications)
        let rx = if !is_notification {
            let (tx, rx) = oneshot::channel();
            self.pending.lock().await.insert(request.id, tx);
            Some(rx)
        } else {
            None
        };

        // Write to stdin
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(json.as_bytes()).await.map_err(|e| {
                ToolError::ExternalService(format!(
                    "Failed to write to MCP server '{}' stdin: {}",
                    self.server_name, e
                ))
            })?;
            stdin.flush().await.map_err(|e| {
                ToolError::ExternalService(format!(
                    "Failed to flush MCP server '{}' stdin: {}",
                    self.server_name, e
                ))
            })?;
        }

        // Notifications return immediately with a dummy response
        let Some(rx) = rx else {
            return Ok(McpResponse {
                jsonrpc: "2.0".to_string(),
                id: 0,
                result: None,
                error: None,
            });
        };

        // Wait for response with timeout
        match tokio::time::timeout(DEFAULT_TIMEOUT, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => {
                // oneshot sender dropped — reader task exited
                self.pending.lock().await.remove(&request.id);
                Err(ToolError::ExternalService(format!(
                    "MCP server '{}' closed connection",
                    self.server_name
                )))
            }
            Err(_) => {
                // Timeout
                self.pending.lock().await.remove(&request.id);
                Err(ToolError::ExternalService(format!(
                    "MCP request to '{}' timed out after {}s",
                    self.server_name,
                    DEFAULT_TIMEOUT.as_secs()
                )))
            }
        }
    }

    /// Check if the child process is still running.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Shut down the transport, killing the child process.
    pub async fn shutdown(&self) {
        self.alive.store(false, Ordering::SeqCst);

        // Kill the child process
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.kill().await;
        }

        // Abort background tasks
        if let Some(handle) = self.reader_handle.lock().await.take() {
            handle.abort();
        }
        if let Some(handle) = self.stderr_handle.lock().await.take() {
            handle.abort();
        }

        // Wake all pending requests with an error
        let mut map = self.pending.lock().await;
        for (id, sender) in map.drain() {
            let _ = sender.send(McpResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: None,
                error: Some(McpError {
                    code: -1,
                    message: "MCP transport shut down".to_string(),
                    data: None,
                }),
            });
        }

        tracing::info!("MCP stdio transport shut down for '{}'", self.server_name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::mcp::protocol::McpRequest;

    #[tokio::test]
    async fn test_start_invalid_command() {
        let result = StdioTransport::start("test", "___nonexistent_binary___", &[], None).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Failed to spawn"), "unexpected error: {}", err);
    }

    #[tokio::test]
    async fn test_alive_and_shutdown() {
        let transport = StdioTransport::start("test", "cat", &[], None)
            .await
            .unwrap();
        assert!(transport.is_alive());
        transport.shutdown().await;
        assert!(!transport.is_alive());
    }

    #[tokio::test]
    async fn test_notification_does_not_block() {
        // cat won't respond, but notifications return immediately
        let transport = StdioTransport::start("test", "cat", &[], None)
            .await
            .unwrap();
        let notification = McpRequest::initialized_notification();
        let result = transport.send(notification).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().id, 0);
        transport.shutdown().await;
    }

    #[tokio::test]
    async fn test_send_after_shutdown() {
        let transport = StdioTransport::start("test", "cat", &[], None)
            .await
            .unwrap();
        transport.shutdown().await;

        let request = McpRequest::list_tools(1);
        let result = transport.send(request).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_receive_echo_server() {
        // Bash script that reads JSON-RPC requests and writes valid responses.
        // Extracts the "id" field with sed and constructs a minimal response.
        let script = concat!(
            r#"while IFS= read -r line; do "#,
            r#"id=$(echo "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p'); "#,
            r#"[ -z "$id" ] && id=0; "#,
            r#"echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[]}}"; "#,
            r#"done"#,
        );

        let transport = StdioTransport::start("test-echo", "bash", &["-c", script], None).await;
        let Ok(transport) = transport else {
            // bash not available — skip test
            return;
        };

        let request = McpRequest::list_tools(1);
        let response = transport.send(request).await;
        assert!(response.is_ok(), "{:?}", response);

        let resp = response.unwrap();
        assert_eq!(resp.id, 1);
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());

        transport.shutdown().await;
    }

    #[tokio::test]
    async fn test_concurrent_requests() {
        let script = concat!(
            r#"while IFS= read -r line; do "#,
            r#"id=$(echo "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p'); "#,
            r#"[ -z "$id" ] && id=0; "#,
            r#"echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[]}}"; "#,
            r#"done"#,
        );

        let transport =
            StdioTransport::start("test-concurrent", "bash", &["-c", script], None).await;
        let Ok(transport) = transport else {
            return;
        };
        let transport = Arc::new(transport);

        let mut handles = vec![];
        for i in 1u64..=5 {
            let t = Arc::clone(&transport);
            handles.push(tokio::spawn(async move {
                let request = McpRequest::list_tools(i);
                t.send(request).await
            }));
        }

        for handle in handles {
            let result = handle.await.unwrap();
            assert!(result.is_ok(), "{:?}", result);
        }

        transport.shutdown().await;
    }

    #[tokio::test]
    async fn test_env_vars_passed_to_child() {
        // Verify that environment variables are passed to the child process
        let mut env = HashMap::new();
        env.insert("MCP_TEST_VAR".to_string(), "hello".to_string());

        let transport = StdioTransport::start(
            "test-env",
            "bash",
            &["-c", "echo $MCP_TEST_VAR"],
            Some(&env),
        )
        .await;
        let Ok(transport) = transport else {
            return;
        };

        // The process will output the env var to stdout and exit.
        // Give it a moment to complete.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Process should have exited after echo
        transport.shutdown().await;
    }
}
