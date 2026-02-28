//! Backend abstraction for sandbox execution.
//!
//! The [`SandboxBackend`] trait provides a unified interface for running commands
//! in isolated environments. IronClaw ships two implementations:
//!
//! - **Docker** (default): Runs commands in ephemeral Docker containers with
//!   network proxy, credential injection, and capability dropping.
//! - **stereOS** (opt-in, feature `stereos`): Runs commands in hardened NixOS
//!   VMs via QEMU, with full VM-boundary isolation.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use uuid::Uuid;

use crate::sandbox::config::{ResourceLimits, SandboxPolicy};
use crate::sandbox::container::ContainerOutput;
use crate::sandbox::error::Result;

/// Identifies which sandbox backend is in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SandboxBackendKind {
    /// Docker container isolation (default).
    Docker,
    /// stereOS VM isolation (requires QEMU).
    StereOs,
}

impl std::fmt::Display for SandboxBackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Docker => write!(f, "docker"),
            Self::StereOs => write!(f, "stereos"),
        }
    }
}

/// Abstraction over Docker containers and stereOS VMs.
///
/// Both backends support:
/// - Availability checking (is the runtime installed and responding?)
/// - Image/VM readiness (does the required image or VM artifact exist?)
/// - Ephemeral command execution (run a command, collect output, clean up)
/// - Persistent instance lifecycle (create, start, stop) for long-running jobs
#[async_trait]
pub trait SandboxBackend: Send + Sync {
    /// Which backend this is.
    fn kind(&self) -> SandboxBackendKind;

    /// Check if the backend runtime is available on this host.
    async fn is_available(&self) -> bool;

    /// Check if the required image or VM artifact exists.
    async fn image_exists(&self) -> bool;

    /// Pull or build the image (if auto-provisioning is enabled).
    async fn pull_image(&self) -> Result<()>;

    /// Execute a command in an ephemeral sandbox (create, run, destroy).
    ///
    /// The sandbox is created, the command is run, output is collected, and
    /// the sandbox is destroyed — all within this single call.
    async fn execute(
        &self,
        command: &str,
        working_dir: &Path,
        policy: SandboxPolicy,
        limits: &ResourceLimits,
        env: HashMap<String, String>,
    ) -> Result<ContainerOutput>;

    /// Create a persistent instance for a long-running job.
    ///
    /// Returns an opaque instance ID (Docker container ID, QEMU PID, etc.)
    /// that can be passed to [`start_instance`], [`stop_instance`], etc.
    async fn create_instance(
        &self,
        job_id: Uuid,
        entrypoint: Vec<String>,
        env: Vec<(String, String)>,
        binds: Vec<InstanceBind>,
        limits: &ResourceLimits,
    ) -> Result<String>;

    /// Start a previously created instance.
    async fn start_instance(&self, instance_id: &str) -> Result<()>;

    /// Stop and remove a persistent instance.
    ///
    /// Sends a graceful stop signal, waits briefly, then force-kills if needed.
    /// Releases all resources (ports, files, process handles).
    async fn stop_instance(&self, instance_id: &str) -> Result<()>;

    /// The host address that instances should use to reach the orchestrator.
    ///
    /// - Docker: `172.17.0.1` (Linux) or `host.docker.internal` (macOS/Windows)
    /// - stereOS: `10.0.2.2` (QEMU user-mode networking default gateway)
    fn orchestrator_host(&self) -> &str;
}

/// A bind mount specification for persistent instances.
#[derive(Debug, Clone)]
pub struct InstanceBind {
    /// Host-side path.
    pub host_path: String,
    /// Guest-side mount point.
    pub guest_path: String,
    /// Whether the mount is writable.
    pub writable: bool,
}

/// Grace period for stopping instances before force-killing.
pub const STOP_GRACE_PERIOD: Duration = Duration::from_secs(10);
