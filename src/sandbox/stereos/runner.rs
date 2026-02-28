//! stereOS VM sandbox backend implementation.
//!
//! Manages QEMU processes and communicates with VMs via SSH.
//! Each VM boots from an immutable base image (copy-on-write via `snapshot=on`),
//! ensuring a clean state for every execution.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::process::{Child, Command};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::sandbox::backend::{InstanceBind, SandboxBackend, SandboxBackendKind};
use crate::sandbox::config::{ResourceLimits, SandboxPolicy};
use crate::sandbox::container::ContainerOutput;
use crate::sandbox::error::{Result, SandboxError};
use crate::sandbox::stereos::ports::PortAllocator;
use crate::sandbox::stereos::ssh::SshClient;

/// Shell-quote a string using single quotes (POSIX-safe).
///
/// Strips NUL bytes before quoting since they cannot appear in POSIX shell strings.
fn shell_quote(s: &str) -> String {
    let cleaned = s.replace('\0', "");
    format!("'{}'", cleaned.replace('\'', "'\\''"))
}

/// Validate that an environment variable key is a safe POSIX identifier.
///
/// Rejects empty keys, keys containing NUL bytes, and keys that don't match
/// the pattern `[A-Za-z_][A-Za-z0-9_]*`.
fn validate_env_key(k: &str) -> Result<()> {
    if k.is_empty() {
        return Err(SandboxError::ExecutionFailed {
            reason: "empty environment variable key".to_string(),
        });
    }
    if k.contains('\0') {
        return Err(SandboxError::ExecutionFailed {
            reason: format!("environment variable key contains NUL byte: {:?}", k),
        });
    }
    let first = k.as_bytes()[0];
    let valid_first = first.is_ascii_alphabetic() || first == b'_';
    let valid_rest = k.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_');
    if !valid_first || !valid_rest {
        return Err(SandboxError::ExecutionFailed {
            reason: format!("invalid environment variable key: {:?}", k),
        });
    }
    Ok(())
}

/// Validate that an SSH key path exists and has secure permissions.
///
/// On Unix, checks that the key file is not readable by group or others
/// (mode `& 0o077 == 0`), matching OpenSSH's permission requirements.
fn validate_ssh_key_path(path: &Path) -> std::result::Result<(), String> {
    if !path.exists() {
        return Err(format!("SSH key not found: {}", path.display()));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path) {
            let mode = metadata.permissions().mode();
            if mode & 0o077 != 0 {
                return Err(format!(
                    "SSH key {} has insecure permissions {:o} (expected 0600 or stricter)",
                    path.display(),
                    mode & 0o777
                ));
            }
        }
    }

    Ok(())
}

/// Configuration for the stereOS runner.
#[derive(Debug, Clone)]
pub struct StereOsConfig {
    /// Path to the stereOS VM image (.qcow2 or raw).
    pub image_path: PathBuf,
    /// Path to `qemu-system-{arch}` binary (auto-detected if None).
    pub qemu_path: Option<PathBuf>,
    /// Path to the SSH private key for connecting to VMs.
    ///
    /// Production stereOS uses vsock-based key injection so the private key
    /// never touches the host filesystem. This path-based approach is for
    /// development images or pre-configured keys during integration testing.
    pub ssh_key_path: PathBuf,
    /// VM memory in megabytes.
    pub memory_mb: u64,
    /// Number of vCPUs.
    pub cpus: u32,
    /// First port for SSH port allocation.
    pub ssh_port_base: u16,
    /// Maximum concurrent VM instances.
    pub max_instances: usize,
    /// Timeout for VM boot (SSH readiness).
    pub boot_timeout: Duration,
    /// SSH user inside the VM (default: "agent").
    pub ssh_user: String,
    /// Host-side network proxy port (0 = disabled).
    ///
    /// When non-zero and the sandbox policy is sandboxed, `http_proxy` /
    /// `https_proxy` env vars are injected into VM commands pointing to
    /// `10.0.2.2:<proxy_port>` (the QEMU user-mode gateway).
    pub proxy_port: u16,
    /// Path to UEFI firmware (e.g. OVMF). When set, passes `-bios <path>`
    /// to QEMU for UEFI boot instead of legacy BIOS.
    pub uefi_firmware_path: Option<PathBuf>,
}

impl Default for StereOsConfig {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        Self {
            image_path: home.join(".ironclaw").join("stereos").join("stereos.qcow2"),
            qemu_path: None,
            ssh_key_path: home.join(".ironclaw").join("stereos").join("agent_key"),
            memory_mb: 2048,
            cpus: 2,
            ssh_port_base: 12200,
            max_instances: 5,
            boot_timeout: Duration::from_secs(10),
            ssh_user: "agent".to_string(),
            proxy_port: 0,
            uefi_firmware_path: None,
        }
    }
}

/// State of a running QEMU VM instance.
#[allow(dead_code)]
enum VmState {
    /// VM is running and SSH is ready.
    Running,
    /// VM has been stopped (tracked for diagnostic purposes).
    Stopped,
}

/// A tracked QEMU VM instance.
struct QemuInstance {
    /// Job ID this VM is running.
    _job_id: Uuid,
    /// QEMU child process.
    process: Child,
    /// Host-side SSH port forwarded to VM port 22.
    ssh_port: u16,
    /// Current VM state.
    _state: VmState,
    /// When the VM was created.
    _created_at: DateTime<Utc>,
}

/// stereOS VM sandbox backend.
///
/// Manages QEMU processes for running commands in hardened NixOS VMs.
/// Implements [`SandboxBackend`] so it can be used as a drop-in
/// replacement for Docker.
pub struct StereOsRunner {
    config: StereOsConfig,
    instances: Arc<RwLock<HashMap<String, QemuInstance>>>,
    ports: PortAllocator,
}

impl StereOsRunner {
    /// Create a new stereOS runner.
    pub fn new(config: StereOsConfig) -> Self {
        // Warn early if the SSH key is missing or has bad permissions.
        // Don't panic — the key may be provisioned later (e.g. by a setup script).
        if let Err(e) = validate_ssh_key_path(&config.ssh_key_path) {
            tracing::warn!("{}", e);
        }

        let ports = PortAllocator::new(config.ssh_port_base, config.max_instances as u16);
        Self {
            config,
            instances: Arc::new(RwLock::new(HashMap::new())),
            ports,
        }
    }

    /// Resolve the QEMU binary path.
    fn qemu_binary(&self) -> PathBuf {
        if let Some(ref path) = self.config.qemu_path {
            return path.clone();
        }

        // Auto-detect based on architecture
        let arch = if cfg!(target_arch = "x86_64") {
            "x86_64"
        } else if cfg!(target_arch = "aarch64") {
            "aarch64"
        } else {
            "x86_64"
        };

        PathBuf::from(format!("qemu-system-{arch}"))
    }

    /// Build the QEMU command for launching a VM.
    fn build_qemu_command(&self, ssh_port: u16, memory_mb: u64, cpus: u32) -> Command {
        let qemu = self.qemu_binary();
        let mut cmd = Command::new(qemu);

        cmd.args(["-m", &format!("{memory_mb}M"), "-smp", &cpus.to_string()]);

        // Enable KVM on Linux if /dev/kvm exists at runtime
        #[cfg(target_os = "linux")]
        if std::path::Path::new("/dev/kvm").exists() {
            cmd.arg("-enable-kvm");
        } else {
            tracing::warn!("KVM not available; falling back to software emulation");
        }

        // UEFI firmware (e.g. OVMF) if configured
        if let Some(ref fw) = self.config.uefi_firmware_path {
            cmd.args(["-bios", &fw.display().to_string()]);
        }

        // Drive: use snapshot=on for immutable base image
        cmd.args([
            "-drive",
            &format!(
                "file={},format=qcow2,if=virtio,snapshot=on",
                self.config.image_path.display()
            ),
        ]);

        // Networking: user-mode with SSH port forwarding
        cmd.args([
            "-netdev",
            &format!("user,id=net0,hostfwd=tcp::{ssh_port}-:22"),
            "-device",
            "virtio-net-pci,netdev=net0",
        ]);

        // TODO: Add vsock device (`-device vhost-vsock-pci,guest-cid=N`) for
        // host-guest communication without SSH (key injection, log streaming).

        // TODO: Add virtio-console device for structured output channel
        // (`-device virtio-serial -chardev socket,... -device virtconsole,...`).

        // No graphics, serial on stdio
        cmd.args(["-nographic", "-serial", "mon:stdio"]);

        // Suppress stdout/stderr from QEMU itself
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        cmd.stdin(std::process::Stdio::null());

        // Kill the child when the parent drops (Linux)
        cmd.kill_on_drop(true);

        cmd
    }

    /// Create an SSH client for a specific port.
    fn ssh_client(&self, port: u16) -> SshClient {
        SshClient::new(
            "127.0.0.1",
            port,
            &self.config.ssh_user,
            self.config.ssh_key_path.clone(),
        )
    }

    /// Return `export` statements for HTTP proxy env vars.
    ///
    /// When `proxy_port > 0` and the policy is sandboxed, returns four export
    /// statements pointing to the host-side proxy via the QEMU user-mode
    /// gateway (`10.0.2.2`). Returns an empty vec otherwise.
    fn proxy_env_exports(&self, policy: SandboxPolicy) -> Vec<String> {
        if self.config.proxy_port == 0 || !policy.is_sandboxed() {
            return Vec::new();
        }
        let proxy_url = format!("http://10.0.2.2:{}", self.config.proxy_port);
        vec![
            format!("export http_proxy={}", shell_quote(&proxy_url)),
            format!("export https_proxy={}", shell_quote(&proxy_url)),
            format!("export HTTP_PROXY={}", shell_quote(&proxy_url)),
            format!("export HTTPS_PROXY={}", shell_quote(&proxy_url)),
        ]
    }

    /// Spawn a QEMU VM, wait for SSH, return the SSH port.
    async fn spawn_vm(&self, memory_mb: u64, cpus: u32) -> Result<(Child, u16)> {
        let ssh_port = self.ports.allocate().await?;

        let mut cmd = self.build_qemu_command(ssh_port, memory_mb, cpus);

        let child = cmd.spawn().map_err(|e| SandboxError::ExecutionFailed {
            reason: format!(
                "failed to spawn QEMU ({}): {}",
                self.qemu_binary().display(),
                e
            ),
        })?;

        // Wait for SSH to become ready
        let ssh = self.ssh_client(ssh_port);
        if let Err(e) = ssh.wait_ready(self.config.boot_timeout).await {
            // Clean up on boot failure
            self.ports.release(ssh_port).await;
            return Err(SandboxError::ExecutionFailed {
                reason: format!("VM boot timeout: {}", e),
            });
        }

        Ok((child, ssh_port))
    }
}

#[async_trait]
impl SandboxBackend for StereOsRunner {
    fn kind(&self) -> SandboxBackendKind {
        SandboxBackendKind::StereOs
    }

    async fn is_available(&self) -> bool {
        // Check if QEMU binary exists and is executable
        let qemu = self.qemu_binary();
        tokio::process::Command::new(&qemu)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false)
    }

    async fn image_exists(&self) -> bool {
        tokio::fs::try_exists(&self.config.image_path)
            .await
            .unwrap_or(false)
    }

    async fn pull_image(&self) -> Result<()> {
        // stereOS images are built via Nix, not pulled.
        Err(SandboxError::Config {
            reason: format!(
                "stereOS image not found at {}. Build with: nix build .#stereos-ironclaw",
                self.config.image_path.display()
            ),
        })
    }

    async fn execute(
        &self,
        command: &str,
        working_dir: &Path,
        policy: SandboxPolicy,
        limits: &ResourceLimits,
        env: HashMap<String, String>,
    ) -> Result<ContainerOutput> {
        // M2: Check instance capacity before spawning
        if self.ports.allocated_count().await >= self.config.max_instances {
            return Err(SandboxError::CapacityExhausted {
                reason: format!(
                    "maximum stereOS instances reached ({})",
                    self.config.max_instances
                ),
            });
        }

        let start = std::time::Instant::now();

        tracing::info!(
            command,
            "stereOS: spawning ephemeral VM for command execution"
        );

        // Spawn a fresh VM
        let (mut child, ssh_port) = self
            .spawn_vm(limits.memory_bytes / (1024 * 1024), self.config.cpus)
            .await?;

        let ssh = self.ssh_client(ssh_port);

        // Build a single command that sets env vars, changes directory, and
        // runs the user command in the same shell session.
        let mut parts = Vec::new();

        // Inject proxy env vars for sandboxed policies
        parts.extend(self.proxy_env_exports(policy));

        // User-provided env vars (after proxy so user can override)
        for (k, v) in &env {
            validate_env_key(k)?;
            parts.push(format!("export {}={}", k, shell_quote(v)));
        }

        // Change to working directory if specified and non-root
        let wd = working_dir.to_string_lossy();
        if !wd.is_empty() && wd != "/" {
            parts.push(format!("cd {}", shell_quote(&wd)));
        }

        parts.push(command.to_string());
        let full_cmd = parts.join(" && ");

        // Execute the command
        let result = ssh.exec(&full_cmd, limits.timeout).await;

        // Clean up: kill the VM and release the port
        let _ = child.kill().await;
        self.ports.release(ssh_port).await;

        tracing::info!(ssh_port, "stereOS: ephemeral VM cleaned up");

        let duration = start.elapsed();

        match result {
            Ok(output) => {
                let mut stdout = output.stdout;
                let mut stderr = output.stderr;
                let mut truncated = false;

                let half_max = limits.max_output_bytes / 2;
                if stdout.len() > half_max {
                    let end = crate::util::floor_char_boundary(&stdout, half_max);
                    stdout.truncate(end);
                    truncated = true;
                }
                if stderr.len() > half_max {
                    let end = crate::util::floor_char_boundary(&stderr, half_max);
                    stderr.truncate(end);
                    truncated = true;
                }

                Ok(ContainerOutput {
                    exit_code: output.exit_code as i64,
                    stdout,
                    stderr,
                    duration,
                    truncated,
                })
            }
            Err(e) => Err(e),
        }
    }

    async fn create_instance(
        &self,
        job_id: Uuid,
        entrypoint: Vec<String>,
        env: Vec<(String, String)>,
        binds: Vec<InstanceBind>,
        limits: &ResourceLimits,
    ) -> Result<String> {
        if !binds.is_empty() {
            tracing::warn!(
                job_id = %job_id,
                count = binds.len(),
                "stereOS backend: bind mounts not supported, /workspace will not be populated"
            );
        }

        // Check instance limit under write lock to prevent TOCTOU races
        let mut instances = self.instances.write().await;
        if instances.len() >= self.config.max_instances {
            return Err(SandboxError::CapacityExhausted {
                reason: format!(
                    "maximum stereOS instances reached ({})",
                    self.config.max_instances
                ),
            });
        }

        // Spawn the VM
        let (child, ssh_port) = self
            .spawn_vm(limits.memory_bytes / (1024 * 1024), self.config.cpus)
            .await?;

        let ssh = self.ssh_client(ssh_port);

        // Build a single SSH command that exports env vars and launches the
        // entrypoint in the background. Using a single exec() call ensures
        // the exported variables are visible to the entrypoint process.
        let launch_result: Result<()> = async {
            // Validate env keys before building the command
            for (k, _) in &env {
                validate_env_key(k)?;
            }

            // Inject proxy env vars first, then user env vars (so user can
            // override). Persistent instances are always sandboxed — FullAccess
            // is handled by SandboxManager and never reaches the backend.
            let mut all_exports: Vec<String> =
                self.proxy_env_exports(SandboxPolicy::WorkspaceWrite);
            for (k, v) in &env {
                all_exports.push(format!("export {}={}", k, shell_quote(v)));
            }
            let env_prefix = all_exports.join(" && ");

            let cmd_str = entrypoint
                .iter()
                .map(|s| shell_quote(s))
                .collect::<Vec<_>>()
                .join(" ");

            let background_cmd = if env_prefix.is_empty() {
                format!("nohup {} > /tmp/worker.log 2>&1 &", cmd_str)
            } else {
                format!(
                    "{} && nohup {} > /tmp/worker.log 2>&1 &",
                    env_prefix, cmd_str
                )
            };

            ssh.exec(&background_cmd, Duration::from_secs(10))
                .await
                .map_err(|e| SandboxError::ExecutionFailed {
                    reason: format!("failed to start worker: {}", e),
                })?;

            Ok(())
        }
        .await;

        // On failure, clean up the port (QEMU process is killed via kill_on_drop)
        if let Err(e) = launch_result {
            // Drop child explicitly so kill_on_drop fires
            drop(child);
            self.ports.release(ssh_port).await;
            return Err(e);
        }

        // Generate instance ID and track the instance
        let instance_id = format!("stereos-{job_id}");
        tracing::info!(
            %instance_id,
            %job_id,
            ssh_port,
            "stereOS: persistent VM instance created"
        );

        instances.insert(
            instance_id.clone(),
            QemuInstance {
                _job_id: job_id,
                process: child,
                ssh_port,
                _state: VmState::Running,
                _created_at: Utc::now(),
            },
        );

        Ok(instance_id)
    }

    async fn start_instance(&self, instance_id: &str) -> Result<()> {
        // For stereOS, the VM is already started in create_instance.
        // This is a no-op (the worker binary was started via SSH).
        if !self.instances.read().await.contains_key(instance_id) {
            return Err(SandboxError::ExecutionFailed {
                reason: format!("instance {} not found", instance_id),
            });
        }
        Ok(())
    }

    async fn stop_instance(&self, instance_id: &str) -> Result<()> {
        let mut instances = self.instances.write().await;

        if let Some(mut instance) = instances.remove(instance_id) {
            // Force-kill the QEMU process (the agent user has no sudo,
            // so `sudo poweroff` would always fail; just kill directly).
            let _ = instance.process.kill().await;

            // Release the SSH port
            self.ports.release(instance.ssh_port).await;

            tracing::info!(instance_id, "stereOS VM stopped");
        }

        Ok(())
    }

    fn orchestrator_host(&self) -> &str {
        // QEMU user-mode networking default gateway
        "10.0.2.2"
    }
}

impl Drop for StereOsRunner {
    fn drop(&mut self) {
        // We can't do async cleanup in Drop, but kill_on_drop=true on the
        // QEMU processes ensures they'll be cleaned up when the Child handles
        // are dropped.
        let instances = self.instances.clone();
        if let Ok(guard) = instances.try_read()
            && !guard.is_empty()
        {
            tracing::warn!(
                "StereOsRunner dropped with {} active VMs (kill_on_drop will clean up)",
                guard.len()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::sandbox::stereos::runner::*;

    #[test]
    fn test_default_config() {
        let config = StereOsConfig::default();
        assert_eq!(config.memory_mb, 2048);
        assert_eq!(config.cpus, 2);
        assert_eq!(config.ssh_port_base, 12200);
        assert_eq!(config.max_instances, 5);
        assert_eq!(config.ssh_user, "agent");
    }

    #[test]
    fn test_qemu_binary_detection() {
        let runner = StereOsRunner::new(StereOsConfig::default());
        let binary = runner.qemu_binary();
        let binary_str = binary.to_string_lossy();
        assert!(
            binary_str.starts_with("qemu-system-"),
            "expected qemu-system-*, got: {}",
            binary_str
        );
    }

    #[test]
    fn test_orchestrator_host() {
        let runner = StereOsRunner::new(StereOsConfig::default());
        assert_eq!(runner.orchestrator_host(), "10.0.2.2");
    }

    #[tokio::test]
    async fn test_image_exists_with_missing_path() {
        let config = StereOsConfig {
            image_path: PathBuf::from("/nonexistent/stereos.qcow2"),
            ..Default::default()
        };
        let runner = StereOsRunner::new(config);
        assert!(!runner.image_exists().await);
    }

    #[test]
    fn test_shell_quote() {
        assert_eq!(shell_quote("hello"), "'hello'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
    }

    #[test]
    fn test_validate_env_key_valid() {
        assert!(validate_env_key("FOO").is_ok());
        assert!(validate_env_key("_BAR").is_ok());
        assert!(validate_env_key("MY_VAR_123").is_ok());
    }

    #[test]
    fn test_validate_env_key_invalid() {
        assert!(validate_env_key("").is_err());
        assert!(validate_env_key("123ABC").is_err());
        assert!(validate_env_key("FOO BAR").is_err());
        assert!(validate_env_key("X=$(rm -rf /)").is_err());
        assert!(validate_env_key("A;B").is_err());
    }

    #[test]
    fn test_shell_quote_nul_bytes() {
        // NUL bytes should be stripped before quoting
        assert_eq!(shell_quote("hel\0lo"), "'hello'");
        assert_eq!(shell_quote("\0"), "''");
        assert_eq!(shell_quote("a\0b\0c"), "'abc'");
    }

    #[test]
    fn test_validate_env_key_nul_byte() {
        assert!(validate_env_key("FOO\0BAR").is_err());
        assert!(validate_env_key("\0").is_err());
    }

    #[test]
    fn test_validate_ssh_key_path_missing() {
        let result = validate_ssh_key_path(Path::new("/nonexistent/key_file"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[cfg(unix)]
    #[test]
    fn test_validate_ssh_key_path_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join("ironclaw_test_ssh_perms");
        let _ = std::fs::create_dir_all(&dir);

        // Test with insecure permissions (0644)
        let insecure = dir.join("insecure_key");
        std::fs::write(&insecure, "fake-key").unwrap();
        std::fs::set_permissions(&insecure, std::fs::Permissions::from_mode(0o644)).unwrap();
        let result = validate_ssh_key_path(&insecure);
        assert!(result.is_err(), "should reject 0644 permissions");

        // Test with secure permissions (0600)
        let secure = dir.join("secure_key");
        std::fs::write(&secure, "fake-key").unwrap();
        std::fs::set_permissions(&secure, std::fs::Permissions::from_mode(0o600)).unwrap();
        let result = validate_ssh_key_path(&secure);
        assert!(result.is_ok(), "should accept 0600 permissions");

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_proxy_env_exports_sandboxed() {
        let config = StereOsConfig {
            proxy_port: 8080,
            ..Default::default()
        };
        let runner = StereOsRunner::new(config);
        let exports = runner.proxy_env_exports(SandboxPolicy::WorkspaceWrite);
        assert_eq!(exports.len(), 4);
        assert!(exports[0].contains("http_proxy"));
        assert!(exports[1].contains("https_proxy"));
        assert!(exports[2].contains("HTTP_PROXY"));
        assert!(exports[3].contains("HTTPS_PROXY"));
        // All should point to the QEMU gateway
        for export in &exports {
            assert!(
                export.contains("10.0.2.2:8080"),
                "expected proxy URL in: {export}"
            );
        }
    }

    #[test]
    fn test_proxy_env_exports_full_access() {
        let config = StereOsConfig {
            proxy_port: 8080,
            ..Default::default()
        };
        let runner = StereOsRunner::new(config);
        let exports = runner.proxy_env_exports(SandboxPolicy::FullAccess);
        assert!(
            exports.is_empty(),
            "FullAccess should not inject proxy vars"
        );
    }

    #[test]
    fn test_proxy_env_exports_no_proxy() {
        let config = StereOsConfig {
            proxy_port: 0,
            ..Default::default()
        };
        let runner = StereOsRunner::new(config);
        let exports = runner.proxy_env_exports(SandboxPolicy::WorkspaceWrite);
        assert!(
            exports.is_empty(),
            "proxy_port=0 should not inject proxy vars"
        );
    }

    #[test]
    fn test_default_config_new_fields() {
        let config = StereOsConfig::default();
        assert_eq!(config.proxy_port, 0);
        assert!(config.uefi_firmware_path.is_none());
    }
}
