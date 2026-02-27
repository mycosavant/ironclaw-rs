use std::time::Duration;

use crate::config::helpers::{parse_bool_env, parse_option_env, parse_optional_env};
use crate::error::ConfigError;
use crate::settings::Settings;

/// Agent behavior configuration.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub name: String,
    pub max_parallel_jobs: usize,
    pub job_timeout: Duration,
    pub stuck_threshold: Duration,
    pub repair_check_interval: Duration,
    pub max_repair_attempts: u32,
    /// Whether to use planning before tool execution.
    pub use_planning: bool,
    /// Session idle timeout. Sessions inactive longer than this are pruned.
    pub session_idle_timeout: Duration,
    /// Allow chat to use filesystem/shell tools directly (bypass sandbox).
    pub allow_local_tools: bool,
    /// Maximum daily LLM spend in cents (e.g. 10000 = $100). None = unlimited.
    pub max_cost_per_day_cents: Option<u64>,
    /// Maximum LLM/tool actions per hour. None = unlimited.
    pub max_actions_per_hour: Option<u64>,
    /// Maximum tool-call iterations per agentic loop invocation. Default 50.
    pub max_tool_iterations: usize,
    /// When true, skip tool approval checks entirely. For benchmarks/CI.
    pub auto_approve_tools: bool,
    /// When true, tool errors shown in SSE broadcasts and log events use a
    /// generic message instead of the raw error. The LLM still receives the
    /// full error for reasoning. Default: false.
    pub suppress_tool_errors: bool,
    /// Sliding-window size for SHA-256 cycle detection in agentic loops.
    /// Set to 0 to disable. Default: 8.
    pub cycle_window_size: usize,
    /// Inter-agent message bus capacity per job inbox. Default: 256.
    pub agent_bus_capacity: usize,
    /// Maximum child agents a single job can spawn. Default: 5.
    pub max_child_agents: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            name: "ironclaw".to_string(),
            max_parallel_jobs: 5,
            job_timeout: Duration::from_secs(1800),
            stuck_threshold: Duration::from_secs(300),
            repair_check_interval: Duration::from_secs(60),
            max_repair_attempts: 3,
            use_planning: false,
            session_idle_timeout: Duration::from_secs(3600),
            allow_local_tools: false,
            max_cost_per_day_cents: None,
            max_actions_per_hour: None,
            max_tool_iterations: 50,
            auto_approve_tools: false,
            suppress_tool_errors: false,
            cycle_window_size: 8,
            agent_bus_capacity: 256,
            max_child_agents: 5,
        }
    }
}

impl AgentConfig {
    pub(crate) fn resolve(settings: &Settings) -> Result<Self, ConfigError> {
        Ok(Self {
            name: parse_optional_env("AGENT_NAME", settings.agent.name.clone())?,
            max_parallel_jobs: parse_optional_env(
                "AGENT_MAX_PARALLEL_JOBS",
                settings.agent.max_parallel_jobs as usize,
            )?,
            job_timeout: Duration::from_secs(parse_optional_env(
                "AGENT_JOB_TIMEOUT_SECS",
                settings.agent.job_timeout_secs,
            )?),
            stuck_threshold: Duration::from_secs(parse_optional_env(
                "AGENT_STUCK_THRESHOLD_SECS",
                settings.agent.stuck_threshold_secs,
            )?),
            repair_check_interval: Duration::from_secs(parse_optional_env(
                "SELF_REPAIR_CHECK_INTERVAL_SECS",
                settings.agent.repair_check_interval_secs,
            )?),
            max_repair_attempts: parse_optional_env(
                "SELF_REPAIR_MAX_ATTEMPTS",
                settings.agent.max_repair_attempts,
            )?,
            use_planning: parse_bool_env("AGENT_USE_PLANNING", settings.agent.use_planning)?,
            session_idle_timeout: Duration::from_secs(parse_optional_env(
                "SESSION_IDLE_TIMEOUT_SECS",
                settings.agent.session_idle_timeout_secs,
            )?),
            allow_local_tools: parse_bool_env("ALLOW_LOCAL_TOOLS", false)?,
            max_cost_per_day_cents: parse_option_env("MAX_COST_PER_DAY_CENTS")?,
            max_actions_per_hour: parse_option_env("MAX_ACTIONS_PER_HOUR")?,
            max_tool_iterations: parse_optional_env(
                "AGENT_MAX_TOOL_ITERATIONS",
                settings.agent.max_tool_iterations,
            )?,
            auto_approve_tools: parse_bool_env(
                "AGENT_AUTO_APPROVE_TOOLS",
                settings.agent.auto_approve_tools,
            )?,
            suppress_tool_errors: parse_bool_env(
                "SUPPRESS_TOOL_ERRORS",
                settings.agent.suppress_tool_errors,
            )?,
            cycle_window_size: parse_optional_env(
                "AGENT_CYCLE_WINDOW_SIZE",
                settings.agent.cycle_window_size,
            )?,
            agent_bus_capacity: parse_optional_env(
                "AGENT_BUS_CAPACITY",
                settings.agent.agent_bus_capacity,
            )?,
            max_child_agents: parse_optional_env(
                "AGENT_MAX_CHILD_AGENTS",
                settings.agent.max_child_agents,
            )?,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::settings::Settings;

    use super::*;

    // These tests must run serially because they modify env vars.
    // Use `serial_test` or combine them into one test.
    #[test]
    fn test_defaults_and_env_overrides() {
        // Part 1: Ensure env vars are clean, then check defaults.
        unsafe {
            std::env::remove_var("AGENT_BUS_CAPACITY");
            std::env::remove_var("AGENT_MAX_CHILD_AGENTS");
        }
        let settings = Settings::default();
        let config = AgentConfig::resolve(&settings).unwrap();
        assert_eq!(config.agent_bus_capacity, 256);
        assert_eq!(config.max_child_agents, 5);

        // Part 2: Set env overrides and verify they take effect.
        unsafe {
            std::env::set_var("AGENT_BUS_CAPACITY", "512");
            std::env::set_var("AGENT_MAX_CHILD_AGENTS", "10");
        }
        let settings2 = Settings::default();
        let config2 = AgentConfig::resolve(&settings2).unwrap();
        assert_eq!(config2.agent_bus_capacity, 512);
        assert_eq!(config2.max_child_agents, 10);

        // Clean up env vars.
        unsafe {
            std::env::remove_var("AGENT_BUS_CAPACITY");
            std::env::remove_var("AGENT_MAX_CHILD_AGENTS");
        }
    }
}
