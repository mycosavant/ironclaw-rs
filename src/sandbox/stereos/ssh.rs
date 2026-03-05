//! SSH client wrapper for communicating with stereOS VMs.
//!
//! Uses `ssh` and `scp` subprocesses (not a Rust SSH library) because:
//! - No new crate dependencies
//! - Matches stereOS's design: SSH is the only external interface
//! - Inherits the host's SSH key agent and configuration

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::sandbox::error::{Result, SandboxError};

/// Output from an SSH command execution.
#[derive(Debug, Clone)]
pub struct SshOutput {
    /// Exit code from the remote command.
    pub exit_code: i32,
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
}

/// SSH client for a specific VM instance.
#[derive(Debug)]
pub struct SshClient {
    /// Host to connect to (typically "127.0.0.1" for forwarded ports).
    host: String,
    /// SSH port on the host.
    port: u16,
    /// Remote user (typically "agent" for stereOS).
    user: String,
    /// Path to the SSH private key.
    key_path: PathBuf,
}

impl SshClient {
    /// Create a new SSH client for a VM instance.
    pub fn new(host: &str, port: u16, user: &str, key_path: PathBuf) -> Self {
        Self {
            host: host.to_string(),
            port,
            user: user.to_string(),
            key_path,
        }
    }

    /// Execute a command on the remote VM.
    ///
    /// `cmd` is passed verbatim to the remote shell — callers are responsible
    /// for quoting (see [`shell_quote`](super::runner::shell_quote)).
    /// Output is captured via `String::from_utf8_lossy`, so non-UTF-8 bytes
    /// are replaced with U+FFFD.
    pub async fn exec(&self, cmd: &str, timeout: Duration) -> Result<SshOutput> {
        use tokio::process::Command;

        let mut command = Command::new("ssh");
        command.args(self.base_args());
        command.arg(format!("{}@{}", self.user, self.host));
        command.arg("--");
        command.arg(cmd);

        let output = tokio::time::timeout(timeout, command.output())
            .await
            .map_err(|_| SandboxError::Timeout(timeout))?
            .map_err(|e| SandboxError::ExecutionFailed {
                reason: format!("SSH exec failed: {}", e),
            })?;

        Ok(SshOutput {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }

    /// Copy a file to the remote VM.
    ///
    /// Reserved for future use (e.g. injecting configuration files or
    /// build artifacts into VMs before execution).
    #[allow(dead_code)]
    pub async fn scp_to(
        &self,
        local_path: &Path,
        remote_path: &str,
        timeout: Duration,
    ) -> Result<()> {
        use tokio::process::Command;

        let mut command = Command::new("scp");
        command.args(self.scp_base_args());
        command.arg(local_path.to_string_lossy().as_ref());
        command.arg(format!("{}@{}:{}", self.user, self.host, remote_path));

        let output = tokio::time::timeout(timeout, command.output())
            .await
            .map_err(|_| SandboxError::Timeout(timeout))?
            .map_err(|e| SandboxError::ExecutionFailed {
                reason: format!("SCP failed: {}", e),
            })?;

        if !output.status.success() {
            return Err(SandboxError::ExecutionFailed {
                reason: format!(
                    "SCP to {}:{} failed: {}",
                    self.host,
                    remote_path,
                    String::from_utf8_lossy(&output.stderr)
                ),
            });
        }

        Ok(())
    }

    /// Wait until SSH is ready on the remote VM.
    ///
    /// Polls with exponential backoff until a connection succeeds or the
    /// timeout is reached. The entire polling loop is bounded by the timeout
    /// so inner probes cannot extend the deadline.
    pub async fn wait_ready(&self, timeout: Duration) -> Result<()> {
        let host = self.host.clone();
        let port = self.port;

        tokio::time::timeout(timeout, async {
            let mut delay = Duration::from_millis(200);
            let max_delay = Duration::from_secs(2);
            // Use a short per-probe timeout so we don't block the entire budget on one attempt.
            let probe_timeout = Duration::from_secs(3);

            loop {
                if self.exec("true", probe_timeout).await.is_ok() {
                    return Ok(());
                }
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(max_delay);
            }
        })
        .await
        .map_err(|_| SandboxError::ExecutionFailed {
            reason: format!("SSH not ready after {:?} ({}:{})", timeout, host, port),
        })?
    }

    /// Common SSH arguments for ephemeral VM connections.
    ///
    /// Uses a 5-second `ConnectTimeout` which is suitable for local QEMU VMs
    /// where SSH is available within milliseconds of the port being forwarded.
    ///
    /// `StrictHostKeyChecking` is disabled intentionally: stereOS VMs are
    /// ephemeral (new key every boot via `snapshot=on`), so there is no stable
    /// host identity to verify.
    fn base_args(&self) -> Vec<String> {
        vec![
            "-o".to_string(),
            "StrictHostKeyChecking=no".to_string(),
            "-o".to_string(),
            "UserKnownHostsFile=/dev/null".to_string(),
            "-o".to_string(),
            "ConnectTimeout=5".to_string(),
            "-o".to_string(),
            "LogLevel=ERROR".to_string(),
            "-p".to_string(),
            self.port.to_string(),
            "-i".to_string(),
            self.key_path.to_string_lossy().to_string(),
        ]
    }

    /// Common SCP arguments.
    fn scp_base_args(&self) -> Vec<String> {
        vec![
            "-o".to_string(),
            "StrictHostKeyChecking=no".to_string(),
            "-o".to_string(),
            "UserKnownHostsFile=/dev/null".to_string(),
            "-o".to_string(),
            "ConnectTimeout=5".to_string(),
            "-o".to_string(),
            "LogLevel=ERROR".to_string(),
            "-P".to_string(), // SCP uses uppercase -P for port
            self.port.to_string(),
            "-i".to_string(),
            self.key_path.to_string_lossy().to_string(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use crate::sandbox::stereos::ssh::*;

    #[test]
    fn test_base_args_include_key_and_port() {
        let client = SshClient::new("127.0.0.1", 12345, "agent", PathBuf::from("/tmp/key"));
        let args = client.base_args();

        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"12345".to_string()));
        assert!(args.contains(&"-i".to_string()));
        assert!(args.contains(&"/tmp/key".to_string()));
        assert!(args.contains(&"StrictHostKeyChecking=no".to_string()));
    }

    #[test]
    fn test_scp_base_args_uses_uppercase_p() {
        let client = SshClient::new("127.0.0.1", 12345, "agent", PathBuf::from("/tmp/key"));
        let args = client.scp_base_args();

        // SCP uses -P (uppercase) for port
        assert!(args.contains(&"-P".to_string()));
        assert!(args.contains(&"12345".to_string()));
    }

    #[test]
    fn test_ssh_client_debug() {
        let client = SshClient::new("127.0.0.1", 22, "agent", PathBuf::from("/tmp/key"));
        let debug = format!("{:?}", client);
        assert!(debug.contains("SshClient"));
        assert!(debug.contains("127.0.0.1"));
        assert!(debug.contains("agent"));
    }
}
