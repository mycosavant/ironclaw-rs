use crate::config::helpers::{optional_env, parse_bool_env, parse_optional_env, parse_string_env};
use crate::error::ConfigError;

/// Docker sandbox configuration.
#[derive(Debug, Clone)]
pub struct SandboxModeConfig {
    /// Whether the Docker sandbox is enabled.
    pub enabled: bool,
    /// Sandbox backend: "docker" (default) or "stereos".
    pub backend: String,
    /// Sandbox policy: "readonly", "workspace_write", or "full_access".
    pub policy: String,
    /// Command timeout in seconds.
    pub timeout_secs: u64,
    /// Memory limit in megabytes.
    pub memory_limit_mb: u64,
    /// CPU shares (relative weight).
    pub cpu_shares: u32,
    /// Docker image for the sandbox.
    pub image: String,
    /// Whether to auto-pull the image if not found.
    pub auto_pull_image: bool,
    /// Additional domains to allow through the network proxy.
    pub extra_allowed_domains: Vec<String>,
    /// stereOS-specific configuration (populated when backend="stereos").
    #[cfg(feature = "stereos")]
    pub stereos: StereOsModeConfig,
}

/// stereOS VM-specific configuration.
#[cfg(feature = "stereos")]
#[derive(Debug, Clone)]
pub struct StereOsModeConfig {
    /// Path to the stereOS VM image (.qcow2 or raw).
    pub image_path: std::path::PathBuf,
    /// Path to `qemu-system-{arch}` binary (auto-detected if None).
    pub qemu_path: Option<std::path::PathBuf>,
    /// Path to the SSH private key for connecting to VMs.
    pub ssh_key_path: std::path::PathBuf,
    /// VM memory in megabytes.
    pub memory_mb: u64,
    /// Number of vCPUs.
    pub cpus: u32,
    /// First port for SSH port allocation.
    pub ssh_port_base: u16,
    /// Maximum concurrent VM instances.
    pub max_instances: usize,
    /// Timeout for VM boot in seconds.
    pub boot_timeout_secs: u64,
    /// Host-side network proxy port (0 = disabled).
    pub proxy_port: u16,
    /// Path to UEFI firmware (e.g. OVMF). None = legacy BIOS boot.
    pub uefi_firmware_path: Option<std::path::PathBuf>,
}

#[cfg(feature = "stereos")]
impl Default for StereOsModeConfig {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
        Self {
            image_path: home.join(".ironclaw").join("stereos").join("stereos.qcow2"),
            qemu_path: None,
            ssh_key_path: home.join(".ironclaw").join("stereos").join("agent_key"),
            memory_mb: 2048,
            cpus: 2,
            ssh_port_base: 12200,
            max_instances: 5,
            boot_timeout_secs: 10,
            proxy_port: 0,
            uefi_firmware_path: None,
        }
    }
}

#[cfg(feature = "stereos")]
impl StereOsModeConfig {
    /// Resolve stereOS configuration from environment variables.
    ///
    /// All variables are optional and fall back to [`Default`] values:
    ///
    /// | Variable                  | Type       | Default                                    |
    /// |---------------------------|------------|--------------------------------------------|
    /// | `STEREOS_IMAGE_PATH`      | `PathBuf`  | `~/.ironclaw/stereos/stereos.qcow2`        |
    /// | `STEREOS_QEMU_PATH`       | `PathBuf`  | auto-detect `qemu-system-{arch}`           |
    /// | `STEREOS_SSH_KEY`          | `PathBuf`  | `~/.ironclaw/stereos/agent_key`            |
    /// | `STEREOS_MEMORY_MB`       | `u64`      | `2048`                                     |
    /// | `STEREOS_CPUS`            | `u32`      | `2`                                        |
    /// | `STEREOS_SSH_PORT_BASE`   | `u16`      | `12200`                                    |
    /// | `STEREOS_MAX_INSTANCES`   | `usize`    | `5`                                        |
    /// | `STEREOS_BOOT_TIMEOUT`    | `u64` (s)  | `10`                                       |
    /// | `STEREOS_PROXY_PORT`      | `u16`      | `0` (disabled)                             |
    /// | `STEREOS_UEFI_FIRMWARE`   | `PathBuf`  | `None` (legacy BIOS)                       |
    pub(crate) fn resolve() -> Result<Self, ConfigError> {
        let defaults = Self::default();
        Ok(Self {
            image_path: optional_env("STEREOS_IMAGE_PATH")?
                .map(std::path::PathBuf::from)
                .unwrap_or(defaults.image_path),
            qemu_path: optional_env("STEREOS_QEMU_PATH")?.map(std::path::PathBuf::from),
            ssh_key_path: optional_env("STEREOS_SSH_KEY")?
                .map(std::path::PathBuf::from)
                .unwrap_or(defaults.ssh_key_path),
            memory_mb: parse_optional_env("STEREOS_MEMORY_MB", defaults.memory_mb)?,
            cpus: parse_optional_env("STEREOS_CPUS", defaults.cpus)?,
            ssh_port_base: parse_optional_env("STEREOS_SSH_PORT_BASE", defaults.ssh_port_base)?,
            max_instances: parse_optional_env("STEREOS_MAX_INSTANCES", defaults.max_instances)?,
            boot_timeout_secs: parse_optional_env(
                "STEREOS_BOOT_TIMEOUT",
                defaults.boot_timeout_secs,
            )?,
            proxy_port: parse_optional_env("STEREOS_PROXY_PORT", defaults.proxy_port)?,
            uefi_firmware_path: optional_env("STEREOS_UEFI_FIRMWARE")?
                .map(std::path::PathBuf::from),
        })
    }

    /// Convert to the runtime config used by the stereOS runner.
    ///
    /// The SSH user is fixed to `"agent"` — the standard user in stereOS VM
    /// images. This is not configurable because stereOS images always create
    /// this user with the correct shell and permissions.
    pub fn to_runner_config(&self) -> crate::sandbox::stereos::runner::StereOsConfig {
        crate::sandbox::stereos::runner::StereOsConfig {
            image_path: self.image_path.clone(),
            qemu_path: self.qemu_path.clone(),
            ssh_key_path: self.ssh_key_path.clone(),
            memory_mb: self.memory_mb,
            cpus: self.cpus,
            ssh_port_base: self.ssh_port_base,
            max_instances: self.max_instances,
            boot_timeout: std::time::Duration::from_secs(self.boot_timeout_secs),
            ssh_user: "agent".to_string(),
            proxy_port: self.proxy_port,
            uefi_firmware_path: self.uefi_firmware_path.clone(),
        }
    }
}

impl Default for SandboxModeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: "docker".to_string(),
            policy: "readonly".to_string(),
            timeout_secs: 120,
            memory_limit_mb: 2048,
            cpu_shares: 1024,
            image: "ironclaw-worker:latest".to_string(),
            auto_pull_image: true,
            extra_allowed_domains: Vec::new(),
            #[cfg(feature = "stereos")]
            stereos: StereOsModeConfig::default(),
        }
    }
}

impl SandboxModeConfig {
    pub(crate) fn resolve() -> Result<Self, ConfigError> {
        let extra_domains = optional_env("SANDBOX_EXTRA_DOMAINS")?
            .map(|s| s.split(',').map(|d| d.trim().to_string()).collect())
            .unwrap_or_default();

        Ok(Self {
            enabled: parse_bool_env("SANDBOX_ENABLED", true)?,
            backend: parse_string_env("SANDBOX_BACKEND", "docker")?,
            policy: parse_string_env("SANDBOX_POLICY", "readonly")?,
            timeout_secs: parse_optional_env("SANDBOX_TIMEOUT_SECS", 120)?,
            memory_limit_mb: parse_optional_env("SANDBOX_MEMORY_LIMIT_MB", 2048)?,
            cpu_shares: parse_optional_env("SANDBOX_CPU_SHARES", 1024)?,
            image: parse_string_env("SANDBOX_IMAGE", "ironclaw-worker:latest")?,
            auto_pull_image: parse_bool_env("SANDBOX_AUTO_PULL", true)?,
            extra_allowed_domains: extra_domains,
            #[cfg(feature = "stereos")]
            stereos: StereOsModeConfig::resolve()?,
        })
    }

    /// Convert to SandboxConfig for the sandbox module.
    pub fn to_sandbox_config(&self) -> crate::sandbox::SandboxConfig {
        use crate::sandbox::SandboxPolicy;
        use std::time::Duration;

        let policy = self.policy.parse().unwrap_or(SandboxPolicy::ReadOnly);

        let mut allowlist = crate::sandbox::default_allowlist();
        allowlist.extend(self.extra_allowed_domains.clone());

        crate::sandbox::SandboxConfig {
            enabled: self.enabled,
            policy,
            timeout: Duration::from_secs(self.timeout_secs),
            memory_limit_mb: self.memory_limit_mb,
            cpu_shares: self.cpu_shares,
            network_allowlist: allowlist,
            image: self.image.clone(),
            auto_pull_image: self.auto_pull_image,
            proxy_port: 0, // Auto-assign
        }
    }
}

/// Claude Code sandbox configuration.
#[derive(Debug, Clone)]
pub struct ClaudeCodeConfig {
    /// Whether Claude Code sandbox mode is available.
    pub enabled: bool,
    /// Host directory containing Claude auth config (not mounted into containers;
    /// auth is handled via ANTHROPIC_API_KEY env var instead).
    pub config_dir: std::path::PathBuf,
    /// Claude model to use (e.g. "sonnet", "opus").
    pub model: String,
    /// Maximum agentic turns before stopping.
    pub max_turns: u32,
    /// Memory limit in MB for Claude Code containers (heavier than workers).
    pub memory_limit_mb: u64,
    /// Allowed tool patterns for Claude Code permission settings.
    ///
    /// Written to `/workspace/.claude/settings.json` before spawning the CLI.
    /// Provides defense-in-depth: only explicitly listed tools are auto-approved.
    /// Any new/unknown tools would require interactive approval (which times out
    /// in the non-interactive container, failing safely).
    ///
    /// Patterns follow Claude Code syntax: `"Bash(*)"`, `"Read"`, `"Edit(*)"`, etc.
    pub allowed_tools: Vec<String>,
}

/// Default allowed tools for Claude Code inside containers.
///
/// These cover all standard Claude Code tools needed for autonomous operation.
/// The Docker container provides the primary security boundary; this allowlist
/// provides defense-in-depth by preventing any future unknown tools from being
/// silently auto-approved.
fn default_claude_code_allowed_tools() -> Vec<String> {
    [
        // File system -- glob patterns match Claude Code's settings.json format
        "Read(*)",
        "Write(*)",
        "Edit(*)",
        "Glob(*)",
        "Grep(*)",
        "NotebookEdit(*)",
        // Execution
        "Bash(*)",
        "Task(*)",
        // Network
        "WebFetch(*)",
        "WebSearch(*)",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

impl Default for ClaudeCodeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            config_dir: dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".claude"),
            model: "sonnet".to_string(),
            max_turns: 50,
            memory_limit_mb: 4096,
            allowed_tools: default_claude_code_allowed_tools(),
        }
    }
}

impl ClaudeCodeConfig {
    /// Load from environment variables only (used inside containers where
    /// there is no database or full config).
    pub fn from_env() -> Self {
        match Self::resolve() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Failed to resolve ClaudeCodeConfig: {e}, using defaults");
                Self::default()
            }
        }
    }

    /// Extract the OAuth access token from the host's credential store.
    ///
    /// On macOS: reads from Keychain (`Claude Code-credentials` service).
    /// On Linux: reads from `~/.claude/.credentials.json`.
    ///
    /// Returns the access token if found. The token typically expires in
    /// 8-12 hours, which is sufficient for any single container job.
    pub fn extract_oauth_token() -> Option<String> {
        // macOS: extract from Keychain
        if cfg!(target_os = "macos") {
            match std::process::Command::new("security")
                .args([
                    "find-generic-password",
                    "-s",
                    "Claude Code-credentials",
                    "-w",
                ])
                .output()
            {
                Ok(output) if output.status.success() => {
                    if let Ok(json) = String::from_utf8(output.stdout) {
                        return parse_oauth_access_token(json.trim());
                    }
                }
                Ok(_) => {
                    tracing::debug!("No Claude Code credentials in macOS Keychain");
                }
                Err(e) => {
                    tracing::debug!("Failed to query macOS Keychain: {e}");
                }
            }
        }

        // Linux / fallback: read from ~/.claude/.credentials.json
        if let Some(home) = dirs::home_dir() {
            let creds_path = home.join(".claude").join(".credentials.json");
            if let Ok(json) = std::fs::read_to_string(&creds_path) {
                return parse_oauth_access_token(&json);
            }
        }

        None
    }

    pub(crate) fn resolve() -> Result<Self, ConfigError> {
        let defaults = Self::default();
        Ok(Self {
            enabled: parse_bool_env("CLAUDE_CODE_ENABLED", defaults.enabled)?,
            config_dir: optional_env("CLAUDE_CONFIG_DIR")?
                .map(std::path::PathBuf::from)
                .unwrap_or(defaults.config_dir),
            model: parse_string_env("CLAUDE_CODE_MODEL", defaults.model)?,
            max_turns: parse_optional_env("CLAUDE_CODE_MAX_TURNS", defaults.max_turns)?,
            memory_limit_mb: parse_optional_env(
                "CLAUDE_CODE_MEMORY_LIMIT_MB",
                defaults.memory_limit_mb,
            )?,
            allowed_tools: optional_env("CLAUDE_CODE_ALLOWED_TOOLS")?
                .map(|s| {
                    s.split(',')
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .collect()
                })
                .unwrap_or(defaults.allowed_tools),
        })
    }
}

/// Parse the OAuth access token from a Claude Code credentials JSON blob.
///
/// Expected shape: `{"claudeAiOauth": {"accessToken": "sk-ant-oat01-..."}}`
fn parse_oauth_access_token(json: &str) -> Option<String> {
    let creds: serde_json::Value = serde_json::from_str(json).ok()?;
    creds["claudeAiOauth"]["accessToken"]
        .as_str()
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "stereos")]
    #[test]
    fn test_stereos_config_to_runner_includes_new_fields() {
        let config = StereOsModeConfig {
            proxy_port: 9090,
            uefi_firmware_path: Some(std::path::PathBuf::from("/usr/share/OVMF/OVMF_CODE.fd")),
            ..Default::default()
        };
        let runner_config = config.to_runner_config();
        assert_eq!(runner_config.proxy_port, 9090);
        assert_eq!(
            runner_config.uefi_firmware_path.as_deref(),
            Some(std::path::Path::new("/usr/share/OVMF/OVMF_CODE.fd"))
        );
    }

    #[cfg(feature = "stereos")]
    #[test]
    fn test_stereos_config_defaults_new_fields() {
        let config = StereOsModeConfig::default();
        assert_eq!(config.proxy_port, 0);
        assert!(config.uefi_firmware_path.is_none());
    }

    #[test]
    fn test_parse_oauth_access_token_valid() {
        let json = r#"{"claudeAiOauth": {"accessToken": "sk-ant-oat01-test"}}"#;
        let token = parse_oauth_access_token(json);
        assert_eq!(token.as_deref(), Some("sk-ant-oat01-test"));
    }

    #[test]
    fn test_parse_oauth_access_token_missing() {
        assert!(parse_oauth_access_token("{}").is_none());
        assert!(parse_oauth_access_token("invalid").is_none());
    }
}
