use std::path::PathBuf;

use secrecy::SecretString;

use crate::config::helpers::{optional_env, parse_bool_env, parse_optional_env};
use crate::error::ConfigError;
use crate::settings::Settings;

/// Channel configurations.
#[derive(Debug, Clone)]
pub struct ChannelsConfig {
    pub cli: CliConfig,
    pub http: Option<HttpConfig>,
    pub gateway: Option<GatewayConfig>,
    /// Directory containing WASM channel modules (default: ~/.ironclaw/channels/).
    pub wasm_channels_dir: std::path::PathBuf,
    /// Whether WASM channels are enabled.
    pub wasm_channels_enabled: bool,
    /// Telegram owner user ID. When set, the bot only responds to this user.
    pub telegram_owner_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct CliConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub host: String,
    pub port: u16,
    pub webhook_secret: Option<SecretString>,
    pub user_id: String,
}

/// Gateway network mode controlling bind address and security posture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayNetworkMode {
    /// Loopback only (127.0.0.1). Default and safest mode.
    Loopback,
    /// Bind to all interfaces (0.0.0.0). Accessible from the local network.
    Lan,
    /// Bind to all interfaces with relaxed timeouts. For remote/internet access.
    /// **Must** be behind a reverse proxy with TLS in production.
    Remote,
}

impl GatewayNetworkMode {
    /// Parse from a string value (case-insensitive).
    pub fn from_str_value(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "loopback" | "local" => Some(Self::Loopback),
            "lan" => Some(Self::Lan),
            "remote" => Some(Self::Remote),
            _ => None,
        }
    }

    /// Returns the default bind host for this mode.
    pub fn default_host(&self) -> &'static str {
        match self {
            Self::Loopback => "127.0.0.1",
            Self::Lan | Self::Remote => "0.0.0.0",
        }
    }
}

/// Web gateway configuration.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub host: String,
    pub port: u16,
    /// Bearer token for authentication. Random hex generated at startup if unset.
    pub auth_token: Option<String>,
    pub user_id: String,
    /// Trusted-proxy auth mode: when set, the gateway trusts this header
    /// (e.g. `X-Forwarded-User`) and skips token auth. Only enable behind a
    /// reverse proxy that sets and validates this header.
    pub trusted_proxy_header: Option<String>,
    /// Network mode controlling bind address and security posture.
    pub network_mode: GatewayNetworkMode,
}

impl ChannelsConfig {
    pub(crate) fn resolve(settings: &Settings) -> Result<Self, ConfigError> {
        let http = if optional_env("HTTP_PORT")?.is_some() || optional_env("HTTP_HOST")?.is_some() {
            Some(HttpConfig {
                host: optional_env("HTTP_HOST")?.unwrap_or_else(|| "127.0.0.1".to_string()),
                port: parse_optional_env("HTTP_PORT", 8080)?,
                webhook_secret: optional_env("HTTP_WEBHOOK_SECRET")?.map(SecretString::from),
                user_id: optional_env("HTTP_USER_ID")?.unwrap_or_else(|| "http".to_string()),
            })
        } else {
            None
        };

        let gateway_enabled = parse_bool_env("GATEWAY_ENABLED", true)?;
        let gateway = if gateway_enabled {
            let network_mode = optional_env("GATEWAY_NETWORK_MODE")?
                .and_then(|s| GatewayNetworkMode::from_str_value(&s))
                .unwrap_or(GatewayNetworkMode::Loopback);

            // Explicit GATEWAY_HOST takes precedence; otherwise use the mode's default.
            let host = optional_env("GATEWAY_HOST")?
                .unwrap_or_else(|| network_mode.default_host().to_string());

            // Emit security warnings for non-loopback modes.
            match network_mode {
                GatewayNetworkMode::Lan => {
                    tracing::warn!(
                        "Gateway network mode is 'lan' — binding to {}. \
                         The gateway will be accessible from your local network. \
                         Ensure GATEWAY_AUTH_TOKEN is set to a strong value.",
                        host
                    );
                }
                GatewayNetworkMode::Remote => {
                    tracing::warn!(
                        "Gateway network mode is 'remote' — binding to {}. \
                         The gateway will be accessible from the internet. \
                         You MUST place it behind a reverse proxy with TLS. \
                         Ensure GATEWAY_AUTH_TOKEN is set to a strong, unique value.",
                        host
                    );
                }
                GatewayNetworkMode::Loopback => {}
            }

            Some(GatewayConfig {
                host,
                port: parse_optional_env("GATEWAY_PORT", 3000)?,
                auth_token: optional_env("GATEWAY_AUTH_TOKEN")?,
                user_id: optional_env("GATEWAY_USER_ID")?.unwrap_or_else(|| "default".to_string()),
                trusted_proxy_header: optional_env("GATEWAY_TRUSTED_PROXY_HEADER")?,
                network_mode,
            })
        } else {
            None
        };

        let cli_enabled = optional_env("CLI_ENABLED")?
            .map(|s| s.to_lowercase() != "false" && s != "0")
            .unwrap_or(true);

        Ok(Self {
            cli: CliConfig {
                enabled: cli_enabled,
            },
            http,
            gateway,
            wasm_channels_dir: optional_env("WASM_CHANNELS_DIR")?
                .map(PathBuf::from)
                .unwrap_or_else(default_channels_dir),
            wasm_channels_enabled: parse_bool_env("WASM_CHANNELS_ENABLED", true)?,
            telegram_owner_id: optional_env("TELEGRAM_OWNER_ID")?
                .map(|s| s.parse())
                .transpose()
                .map_err(|e: std::num::ParseIntError| ConfigError::InvalidValue {
                    key: "TELEGRAM_OWNER_ID".to_string(),
                    message: format!("must be an integer: {e}"),
                })?
                .or(settings.channels.telegram_owner_id),
        })
    }
}

/// Get the default channels directory (~/.ironclaw/channels/).
fn default_channels_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ironclaw")
        .join("channels")
}
