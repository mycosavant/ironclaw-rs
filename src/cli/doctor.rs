//! `ironclaw doctor` — active health diagnostics.
//!
//! Probes external dependencies and validates configuration to surface
//! problems before they bite during normal operation. Each check reports
//! pass/warn/fail/skip with actionable guidance on failures.
//!
//! # Check Categories
//!
//! | Category        | Checks                                                    |
//! |-----------------|-----------------------------------------------------------|
//! | Core            | Session auth, database connectivity, workspace dir        |
//! | LLM             | API reachability, model configuration, embeds             |
//! | Sandbox         | Docker daemon, sandbox image, proxy port                  |
//! | Gateway         | Health endpoint, auth token                               |
//! | Extensions      | WASM tools, WASM channels, skills                        |
//! | Binaries        | docker, cloudflared, ngrok, tailscale                     |

use std::path::PathBuf;
use std::time::Duration;

// ── Entry point ──────────────────────────────────────────────────────────────

/// Run all diagnostic checks and print results.
pub async fn run_doctor_command() -> anyhow::Result<()> {
    println!("IronClaw Doctor");
    println!("===============\n");

    let mut passed = 0u32;
    let mut warned = 0u32;
    let mut failed = 0u32;

    // ── Core ──────────────────────────────────────────────────────────────────

    println!("── Core ─────────────────────────────────────────────────────────");
    check("Session / auth",    check_auth().await,            &mut passed, &mut warned, &mut failed);
    check("Database",          check_database().await,        &mut passed, &mut warned, &mut failed);
    check("Workspace dir",     check_workspace_dir(),         &mut passed, &mut warned, &mut failed);
    check("Secrets key",       check_secrets_key(),           &mut passed, &mut warned, &mut failed);

    // ── LLM ───────────────────────────────────────────────────────────────────

    println!("\n── LLM ──────────────────────────────────────────────────────────");
    check("LLM backend",       check_llm_backend(),           &mut passed, &mut warned, &mut failed);
    check("LLM API reachable", check_llm_reachable().await,   &mut passed, &mut warned, &mut failed);
    check("Embeddings",        check_embeddings(),            &mut passed, &mut warned, &mut failed);

    // ── Sandbox ───────────────────────────────────────────────────────────────

    println!("\n── Sandbox ──────────────────────────────────────────────────────");
    check("Docker daemon",     check_docker_daemon(),         &mut passed, &mut warned, &mut failed);
    check("Sandbox image",     check_sandbox_image(),         &mut passed, &mut warned, &mut failed);
    check("Sandbox enabled",   check_sandbox_config(),        &mut passed, &mut warned, &mut failed);

    // ── Gateway ───────────────────────────────────────────────────────────────

    println!("\n── Gateway ──────────────────────────────────────────────────────");
    check("Gateway health",    check_gateway_health().await,  &mut passed, &mut warned, &mut failed);
    check("Gateway auth",      check_gateway_auth(),          &mut passed, &mut warned, &mut failed);

    // ── Extensions ────────────────────────────────────────────────────────────

    println!("\n── Extensions ───────────────────────────────────────────────────");
    check("WASM tools",        check_wasm_tools(),            &mut passed, &mut warned, &mut failed);
    check("WASM channels",     check_wasm_channels(),         &mut passed, &mut warned, &mut failed);
    check("Skills",            check_skills(),                &mut passed, &mut warned, &mut failed);

    // ── Binaries ──────────────────────────────────────────────────────────────

    println!("\n── Optional binaries ────────────────────────────────────────────");
    check("docker",            check_binary("docker",        &["--version"]), &mut passed, &mut warned, &mut failed);
    check("cloudflared",       check_binary("cloudflared",   &["--version"]), &mut passed, &mut warned, &mut failed);
    check("ngrok",             check_binary("ngrok",         &["version"]),   &mut passed, &mut warned, &mut failed);
    check("tailscale",         check_binary("tailscale",     &["version"]),   &mut passed, &mut warned, &mut failed);

    // ── Summary ───────────────────────────────────────────────────────────────

    println!();
    println!("─────────────────────────────────────────────────────────────────");
    print!("  {} passed", passed);
    if warned > 0 { print!(", {} warning(s)", warned); }
    if failed > 0 { print!(", {} failed", failed); }
    println!();

    if failed > 0 {
        println!("\n  Items marked [FAIL] need attention before IronClaw will work correctly.");
    }
    if warned > 0 {
        println!("  Items marked [warn] indicate degraded functionality (optional features).");
    }
    if failed == 0 && warned == 0 {
        println!("\n  Everything looks good. Run `ironclaw` to start.");
    }

    Ok(())
}

// ── Check runner ─────────────────────────────────────────────────────────────

fn check(
    name: &str,
    result: CheckResult,
    passed: &mut u32,
    warned: &mut u32,
    failed: &mut u32,
) {
    match result {
        CheckResult::Pass(detail) => {
            *passed += 1;
            println!("  [pass] {:24}  {}", name, detail);
        }
        CheckResult::Warn(detail) => {
            *warned += 1;
            println!("  [warn] {:24}  {}", name, detail);
        }
        CheckResult::Fail(detail) => {
            *failed += 1;
            println!("  [FAIL] {:24}  {}", name, detail);
        }
        CheckResult::Skip(reason) => {
            println!("  [skip] {:24}  {}", name, reason);
        }
    }
}

enum CheckResult {
    Pass(String),
    Warn(String),
    Fail(String),
    Skip(String),
}

// ── Core checks ──────────────────────────────────────────────────────────────

async fn check_auth() -> CheckResult {
    // API key mode
    if std::env::var("NEARAI_API_KEY").is_ok() {
        return CheckResult::Pass("NEARAI_API_KEY set".into());
    }
    // Session token injected via env (hosting)
    if std::env::var("NEARAI_SESSION_TOKEN").is_ok() {
        return CheckResult::Pass("NEARAI_SESSION_TOKEN set".into());
    }
    // Session file on disk
    let session_path = crate::llm::session::default_session_path();
    if session_path.exists() {
        match std::fs::read_to_string(&session_path) {
            Ok(content) if content.trim().is_empty() => {
                CheckResult::Fail("session file is empty — run `ironclaw onboard`".into())
            }
            Ok(_) => CheckResult::Pass(format!("session file at {}", session_path.display())),
            Err(e) => CheckResult::Fail(format!("cannot read session file: {e}")),
        }
    } else {
        CheckResult::Fail(format!(
            "no credentials found — run `ironclaw onboard` (looked for {})",
            session_path.display()
        ))
    }
}

async fn check_database() -> CheckResult {
    let backend = std::env::var("DATABASE_BACKEND")
        .unwrap_or_else(|_| "postgres".into());

    match backend.as_str() {
        "libsql" | "turso" | "sqlite" => {
            let path = std::env::var("LIBSQL_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|_| crate::config::default_libsql_path());
            if path.exists() {
                CheckResult::Pass(format!("libSQL exists ({})", path.display()))
            } else {
                CheckResult::Warn(format!(
                    "libSQL file not found at {} (will be created on first run)",
                    path.display()
                ))
            }
        }
        _ => {
            if std::env::var("DATABASE_URL").is_err() {
                return CheckResult::Fail(
                    "DATABASE_URL not set — set it or use DATABASE_BACKEND=libsql".into(),
                );
            }
            match try_pg_connect().await {
                Ok(()) => CheckResult::Pass("PostgreSQL connected".into()),
                Err(e) => CheckResult::Fail(format!("PostgreSQL connection failed: {e}")),
            }
        }
    }
}

fn check_workspace_dir() -> CheckResult {
    let dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ironclaw");

    if dir.exists() {
        if dir.is_dir() {
            CheckResult::Pass(dir.display().to_string())
        } else {
            CheckResult::Fail(format!("{} exists but is not a directory", dir.display()))
        }
    } else {
        CheckResult::Warn(format!("{} will be created on first run", dir.display()))
    }
}

fn check_secrets_key() -> CheckResult {
    if std::env::var("SECRETS_MASTER_KEY").is_ok() {
        CheckResult::Pass("SECRETS_MASTER_KEY set".into())
    } else {
        // Keychain may be configured — we cannot probe it without triggering
        // macOS unlock dialogs, so warn instead of failing.
        CheckResult::Warn(
            "SECRETS_MASTER_KEY not set (keychain may be configured — run `ironclaw onboard`)".into(),
        )
    }
}

// ── LLM checks ───────────────────────────────────────────────────────────────

fn check_llm_backend() -> CheckResult {
    let backend = std::env::var("LLM_BACKEND").unwrap_or_else(|_| "nearai".into());
    let model = match backend.as_str() {
        "nearai" => std::env::var("NEARAI_MODEL")
            .unwrap_or_else(|_| "claude-3-5-sonnet-20241022".into()),
        "openai" | "openai_compatible" => {
            std::env::var("LLM_MODEL").unwrap_or_else(|_| "gpt-4".into())
        }
        "anthropic" => {
            std::env::var("LLM_MODEL")
                .unwrap_or_else(|_| "claude-3-5-sonnet-20241022".into())
        }
        "ollama" => std::env::var("LLM_MODEL").unwrap_or_else(|_| "llama3".into()),
        "tinfoil" => {
            std::env::var("TINFOIL_MODEL").unwrap_or_else(|_| "kimi-k2-5".into())
        }
        _ => std::env::var("LLM_MODEL").unwrap_or_else(|_| "(unknown)".into()),
    };
    CheckResult::Pass(format!("backend={backend}, model={model}"))
}

async fn check_llm_reachable() -> CheckResult {
    let backend = std::env::var("LLM_BACKEND").unwrap_or_else(|_| "nearai".into());

    let base_url: String = match backend.as_str() {
        "nearai" => {
            if std::env::var("NEARAI_API_KEY").is_ok() {
                std::env::var("NEARAI_BASE_URL")
                    .unwrap_or_else(|_| "https://cloud-api.near.ai".into())
            } else {
                std::env::var("NEARAI_BASE_URL")
                    .unwrap_or_else(|_| "https://private.near.ai".into())
            }
        }
        "openai" => "https://api.openai.com".into(),
        "anthropic" => "https://api.anthropic.com".into(),
        "ollama" => std::env::var("OLLAMA_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:11434".into()),
        "openai_compatible" => match std::env::var("LLM_BASE_URL") {
            Ok(url) => url,
            Err(_) => {
                return CheckResult::Fail("LLM_BASE_URL not set for openai_compatible".into())
            }
        },
        "tinfoil" => "https://inference.tinfoil.sh".into(),
        other => return CheckResult::Skip(format!("unknown backend '{other}'")),
    };

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
    {
        Ok(c) => c,
        Err(e) => return CheckResult::Fail(format!("HTTP client error: {e}")),
    };

    // Probe TCP reachability only — no auth, no real LLM call that incurs cost.
    // Any HTTP-level response (4xx, 5xx) means the host is reachable.
    match client.get(&base_url).send().await {
        Ok(_) => CheckResult::Pass(format!("{base_url} reachable")),
        Err(e) if e.is_connect() || e.is_timeout() => {
            CheckResult::Fail(format!("{base_url} unreachable: {e}"))
        }
        Err(_) => CheckResult::Pass(format!("{base_url} reachable")),
    }
}

fn check_embeddings() -> CheckResult {
    let has_openai = std::env::var("OPENAI_API_KEY").is_ok();
    let enabled = std::env::var("EMBEDDING_ENABLED")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);
    let provider = std::env::var("EMBEDDING_PROVIDER")
        .unwrap_or_else(|_| "openai".into());
    let model = std::env::var("EMBEDDING_MODEL")
        .unwrap_or_else(|_| "text-embedding-3-small".into());

    if has_openai || enabled {
        CheckResult::Pass(format!("provider={provider}, model={model}"))
    } else {
        CheckResult::Warn(
            "OPENAI_API_KEY not set — semantic memory search falls back to FTS-only mode".into(),
        )
    }
}

// ── Sandbox checks ────────────────────────────────────────────────────────────

fn check_docker_daemon() -> CheckResult {
    // `docker info` fails if the daemon is not running (unlike `docker --version`
    // which only checks the client binary).
    match std::process::Command::new("docker")
        .args(["info", "--format", "{{.ServerVersion}}"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
    {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            CheckResult::Pass(format!("Docker daemon running (server v{version})"))
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let hint = if stderr.contains("permission denied") || stderr.contains("connect") {
                " — daemon not running or permission denied"
            } else {
                ""
            };
            CheckResult::Warn(format!("Docker daemon not available{hint}; sandbox disabled"))
        }
        Err(_) => CheckResult::Skip("docker not found in PATH; sandbox disabled".into()),
    }
}

fn check_sandbox_image() -> CheckResult {
    let image = std::env::var("SANDBOX_IMAGE")
        .unwrap_or_else(|_| "ironclaw-worker:latest".into());

    match std::process::Command::new("docker")
        .args(["image", "inspect", &image, "--format", "{{.Id}}"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
    {
        Ok(output) if output.status.success() => {
            let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let short_id = if id.len() > 19 { &id[..19] } else { &id };
            CheckResult::Pass(format!("image '{image}' found ({short_id})"))
        }
        Ok(_) => CheckResult::Warn(format!(
            "image '{image}' not found — build: `docker build -f Dockerfile.worker .`"
        )),
        Err(_) => CheckResult::Skip("docker not in PATH — cannot check sandbox image".into()),
    }
}

fn check_sandbox_config() -> CheckResult {
    let enabled = std::env::var("SANDBOX_ENABLED")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(true);
    if enabled {
        let policy = std::env::var("SANDBOX_DEFAULT_POLICY")
            .unwrap_or_else(|_| "workspace_write".into());
        CheckResult::Pass(format!("enabled (policy: {policy})"))
    } else {
        CheckResult::Warn("SANDBOX_ENABLED=false — commands run directly on host".into())
    }
}

// ── Gateway checks ────────────────────────────────────────────────────────────

async fn check_gateway_health() -> CheckResult {
    let host = std::env::var("GATEWAY_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port: u16 = std::env::var("GATEWAY_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3001);
    let url = format!("http://{host}:{port}/api/health");

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(e) => return CheckResult::Fail(format!("HTTP client error: {e}")),
    };

    match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => {
            CheckResult::Pass(format!("up at http://{host}:{port}"))
        }
        Ok(r) => CheckResult::Warn(format!(
            "gateway returned HTTP {} — may be starting up",
            r.status()
        )),
        Err(_) => CheckResult::Warn(format!(
            "not reachable at http://{host}:{port} — start with `ironclaw run`"
        )),
    }
}

fn check_gateway_auth() -> CheckResult {
    let gateway_enabled = std::env::var("GATEWAY_ENABLED")
        .map(|v| v != "false" && v != "0")
        .unwrap_or(true);

    if !gateway_enabled {
        return CheckResult::Skip("gateway disabled (GATEWAY_ENABLED=false)".into());
    }

    match std::env::var("GATEWAY_AUTH_TOKEN") {
        Ok(token) if token.len() >= 16 => {
            CheckResult::Pass(format!("GATEWAY_AUTH_TOKEN set ({} chars)", token.len()))
        }
        Ok(token) if !token.is_empty() => CheckResult::Warn(format!(
            "token very short ({} chars) — use a longer random token",
            token.len()
        )),
        _ => CheckResult::Warn(
            "GATEWAY_AUTH_TOKEN not set — a random token is generated at startup (not stable across restarts)".into(),
        ),
    }
}

// ── Extension checks ──────────────────────────────────────────────────────────

fn check_wasm_tools() -> CheckResult {
    let tools_dir = ironclaw_subdir("tools");
    if !tools_dir.exists() {
        return CheckResult::Warn(format!(
            "tools directory not found at {} — install tools with `ironclaw tool install`",
            tools_dir.display()
        ));
    }
    let count = count_wasm_files(&tools_dir);
    if count == 0 {
        CheckResult::Pass(format!("directory exists, 0 tools installed ({})", tools_dir.display()))
    } else {
        CheckResult::Pass(format!("{count} tool(s) installed ({})", tools_dir.display()))
    }
}

fn check_wasm_channels() -> CheckResult {
    let channels_dir = std::env::var("WASM_CHANNELS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| ironclaw_subdir("channels"));

    if !channels_dir.exists() {
        return CheckResult::Warn(format!(
            "channels directory not found at {} — install channels with `ironclaw channels install`",
            channels_dir.display()
        ));
    }
    let count = count_wasm_files(&channels_dir);
    if count == 0 {
        CheckResult::Pass(format!("directory exists, 0 channels installed ({})", channels_dir.display()))
    } else {
        CheckResult::Pass(format!("{count} channel(s) installed ({})", channels_dir.display()))
    }
}

fn check_skills() -> CheckResult {
    let enabled = std::env::var("SKILLS_ENABLED")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(true);

    if !enabled {
        return CheckResult::Skip("skills disabled (SKILLS_ENABLED=false)".into());
    }

    let user_count = count_skill_files(&ironclaw_subdir("skills"));
    let installed_count = count_skill_files(&ironclaw_subdir("installed_skills"));

    CheckResult::Pass(format!(
        "{} skill(s): {} user-local, {} registry-installed",
        user_count + installed_count,
        user_count,
        installed_count
    ))
}

// ── Binary checks ─────────────────────────────────────────────────────────────

fn check_binary(name: &str, args: &[&str]) -> CheckResult {
    match std::process::Command::new(name)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
    {
        Ok(output) => {
            let raw = if !output.stdout.is_empty() {
                output.stdout.clone()
            } else {
                output.stderr.clone()
            };
            let first_line = String::from_utf8_lossy(&raw)
                .trim()
                .lines()
                .next()
                .unwrap_or("")
                .to_string();

            if output.status.success() {
                CheckResult::Pass(first_line)
            } else {
                CheckResult::Fail(format!("{name} exited with {}", output.status))
            }
        }
        Err(_) => CheckResult::Skip(format!("{name} not found in PATH")),
    }
}

// ── PostgreSQL helper ─────────────────────────────────────────────────────────

#[cfg(feature = "postgres")]
async fn try_pg_connect() -> Result<(), String> {
    let url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL not set".to_string())?;
    let config = deadpool_postgres::Config {
        url: Some(url),
        ..Default::default()
    };
    let pool = config
        .create_pool(
            Some(deadpool_postgres::Runtime::Tokio1),
            tokio_postgres::NoTls,
        )
        .map_err(|e| format!("pool error: {e}"))?;
    let client = tokio::time::timeout(Duration::from_secs(5), pool.get())
        .await
        .map_err(|_| "connection timeout (5s)".to_string())?
        .map_err(|e| e.to_string())?;
    client
        .execute("SELECT 1", &[])
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(not(feature = "postgres"))]
async fn try_pg_connect() -> Result<(), String> {
    Err("postgres feature not compiled in".into())
}

// ── Filesystem helpers ────────────────────────────────────────────────────────

fn ironclaw_subdir(name: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ironclaw")
        .join(name)
}

fn count_wasm_files(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|ext| ext == "wasm"))
                .count()
        })
        .unwrap_or(0)
}

fn count_skill_files(dir: &std::path::Path) -> usize {
    if !dir.exists() {
        return 0;
    }
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .filter(|e| e.path().join("SKILL.md").exists())
                .count()
        })
        .unwrap_or(0)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_binary_finds_sh() {
        match check_binary("sh", &["-c", "echo ok"]) {
            CheckResult::Pass(_) => {}
            other => panic!("expected Pass for sh, got: {}", format_result(&other)),
        }
    }

    #[test]
    fn check_binary_skips_nonexistent() {
        match check_binary("__ironclaw_nonexistent_binary__", &["--version"]) {
            CheckResult::Skip(_) => {}
            other => panic!(
                "expected Skip for nonexistent binary, got: {}",
                format_result(&other)
            ),
        }
    }

    #[test]
    fn check_workspace_dir_does_not_panic() {
        let _ = check_workspace_dir();
    }

    #[test]
    fn check_secrets_key_does_not_panic() {
        let _ = check_secrets_key();
    }

    #[test]
    fn check_wasm_tools_does_not_panic() {
        let _ = check_wasm_tools();
    }

    #[test]
    fn check_wasm_channels_does_not_panic() {
        let _ = check_wasm_channels();
    }

    #[test]
    fn check_skills_does_not_panic() {
        let _ = check_skills();
    }

    #[test]
    fn check_sandbox_config_does_not_panic() {
        let _ = check_sandbox_config();
    }

    #[test]
    fn check_llm_backend_does_not_panic() {
        let _ = check_llm_backend();
    }

    #[tokio::test]
    async fn check_auth_does_not_panic() {
        let _ = check_auth().await;
    }

    fn format_result(r: &CheckResult) -> String {
        match r {
            CheckResult::Pass(s) => format!("Pass({s})"),
            CheckResult::Warn(s) => format!("Warn({s})"),
            CheckResult::Fail(s) => format!("Fail({s})"),
            CheckResult::Skip(s) => format!("Skip({s})"),
        }
    }
}
