use crate::config::helpers::{parse_bool_env, parse_optional_env};
use crate::error::ConfigError;

/// Safety configuration.
#[derive(Debug, Clone)]
pub struct SafetyConfig {
    pub max_output_length: usize,
    pub injection_check_enabled: bool,
    /// Optional domain allowlist for the HTTP tool. When set, only requests to
    /// listed domains are permitted. Comma-separated in env var.
    pub http_url_allowlist: Option<Vec<String>>,
}

impl SafetyConfig {
    pub(crate) fn resolve() -> Result<Self, ConfigError> {
        let http_url_allowlist = std::env::var("HTTP_TOOL_URL_ALLOWLIST").ok().map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        });

        Ok(Self {
            max_output_length: parse_optional_env("SAFETY_MAX_OUTPUT_LENGTH", 100_000)?,
            injection_check_enabled: parse_bool_env("SAFETY_INJECTION_CHECK_ENABLED", true)?,
            http_url_allowlist,
        })
    }
}
