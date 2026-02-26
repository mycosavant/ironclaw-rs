//! `ironclaw channels` — channel management CLI.
//!
//! Install, list, and remove WASM channel modules from the channels directory.
//! Channels are `.wasm` files (optionally with a paired `.capabilities.json`)
//! stored in `~/.ironclaw/channels/` (override with `WASM_CHANNELS_DIR`).
//!
//! # Subcommands
//!
//! | Subcommand          | Description                                           |
//! |---------------------|-------------------------------------------------------|
//! | `channels list`     | List installed WASM channels                          |
//! | `channels install`  | Install a channel from a local `.wasm` file           |
//! | `channels remove`   | Remove an installed channel                           |
//! | `channels status`   | Show active channel connections via gateway API       |

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Subcommand;
use tokio::fs;

// ── CLI types ────────────────────────────────────────────────────────────────

/// Manage WASM channel modules.
#[derive(Subcommand, Debug, Clone)]
pub enum ChannelsCommand {
    /// List all installed WASM channel modules.
    List {
        /// Channels directory (overrides WASM_CHANNELS_DIR)
        #[arg(long)]
        dir: Option<PathBuf>,

        /// Show extra details (size, capabilities)
        #[arg(short, long)]
        verbose: bool,
    },

    /// Install a WASM channel from a local file.
    Install {
        /// Path to the `.wasm` file (or containing directory built from source)
        path: PathBuf,

        /// Override the channel name (default: derived from filename)
        #[arg(short, long)]
        name: Option<String>,

        /// Path to a capabilities JSON file to pair with this channel
        #[arg(long)]
        caps: Option<PathBuf>,

        /// Channels directory (overrides WASM_CHANNELS_DIR)
        #[arg(long)]
        dir: Option<PathBuf>,

        /// Overwrite if the channel already exists
        #[arg(long)]
        force: bool,
    },

    /// Remove an installed channel.
    Remove {
        /// Channel name (without `.wasm` extension)
        name: String,

        /// Channels directory (overrides WASM_CHANNELS_DIR)
        #[arg(long)]
        dir: Option<PathBuf>,

        /// Skip confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Show active channel connection counts via the running gateway.
    Status {
        /// Gateway base URL (overrides GATEWAY_HOST + GATEWAY_PORT)
        #[arg(long)]
        gateway: Option<String>,

        /// Bearer auth token (overrides GATEWAY_AUTH_TOKEN)
        #[arg(long)]
        token: Option<String>,

        /// Also show installed-channel listing
        #[arg(short, long)]
        list: bool,
    },
}

// ── Entry point ──────────────────────────────────────────────────────────────

pub async fn run_channels_command(cmd: ChannelsCommand) -> anyhow::Result<()> {
    let _ = dotenvy::dotenv(); // best-effort .env load

    match cmd {
        ChannelsCommand::List { dir, verbose } => list_channels(dir, verbose).await,
        ChannelsCommand::Install {
            path,
            name,
            caps,
            dir,
            force,
        } => install_channel(path, name, caps, dir, force).await,
        ChannelsCommand::Remove { name, dir, yes } => remove_channel(name, dir, yes).await,
        ChannelsCommand::Status {
            gateway,
            token,
            list,
        } => channel_status(gateway, token, list).await,
    }
}

// ── Commands ─────────────────────────────────────────────────────────────────

async fn list_channels(dir: Option<PathBuf>, verbose: bool) -> anyhow::Result<()> {
    let channels_dir = resolve_channels_dir(dir);

    if !channels_dir.exists() {
        println!("No channels directory at {}", channels_dir.display());
        println!("Install a channel with: ironclaw channels install <path>");
        return Ok(());
    }

    let channels = collect_channels(&channels_dir).await?;

    if channels.is_empty() {
        println!("No channels installed in {}", channels_dir.display());
        println!();
        println!("Known bundled channels: telegram, slack, discord, whatsapp");
        println!("Build and install with: ironclaw channels install <path/to/channel.wasm>");
        return Ok(());
    }

    println!("Installed channels ({}/):", channels_dir.display());
    println!();

    for (name, path, has_caps, size) in &channels {
        if verbose {
            let wasm_bytes = fs::read(path).await.unwrap_or_default();
            let hash = sha256_short(&wasm_bytes);
            println!("  {} ({})", name, format_size(*size));
            println!("    Path: {}", path.display());
            println!("    Hash: {}", hash);
            println!("    Caps: {}", if *has_caps { "yes" } else { "no" });
            if *has_caps {
                let caps_path = path.with_extension("capabilities.json");
                if let Ok(content) = fs::read_to_string(&caps_path).await {
                    if let Ok(caps) = serde_json::from_str::<serde_json::Value>(&content) {
                        print_caps_summary(&caps);
                    }
                }
            }
            println!();
        } else {
            let caps_indicator = if *has_caps { "✓" } else { "✗" };
            println!(
                "  {} ({}, caps: {})",
                name,
                format_size(*size),
                caps_indicator
            );
        }
    }

    if !verbose {
        println!();
        println!(
            "  ({} channel(s) — use --verbose for details)",
            channels.len()
        );
    }

    Ok(())
}

async fn install_channel(
    path: PathBuf,
    name_override: Option<String>,
    caps_path: Option<PathBuf>,
    dir: Option<PathBuf>,
    force: bool,
) -> anyhow::Result<()> {
    let channels_dir = resolve_channels_dir(dir);

    // Resolve the source .wasm file — accept either a direct .wasm path or a
    // directory containing exactly one .wasm (e.g., the channel's build output).
    let (wasm_path, auto_caps_path) = resolve_wasm_source(&path).await?;

    // Determine channel name
    let channel_name = name_override.unwrap_or_else(|| {
        wasm_path
            .file_stem()
            .and_then(|s| s.to_str())
            // Strip common suffixes like `_channel` or `-channel` for cleaner names
            .map(|s| {
                s.trim_end_matches("_channel")
                    .trim_end_matches("-channel")
                    .to_string()
            })
            .unwrap_or_else(|| "channel".to_string())
    });

    // Validate the channel name
    if !is_valid_name(&channel_name) {
        anyhow::bail!(
            "Invalid channel name '{}'. Use only letters, numbers, hyphens, and underscores.",
            channel_name
        );
    }

    // Ensure target directory exists
    fs::create_dir_all(&channels_dir).await?;

    let target_wasm = channels_dir.join(format!("{}.wasm", channel_name));
    let target_caps = channels_dir.join(format!("{}.capabilities.json", channel_name));

    if target_wasm.exists() && !force {
        anyhow::bail!(
            "Channel '{}' already installed at {}. Use --force to overwrite.",
            channel_name,
            target_wasm.display()
        );
    }

    // Determine capabilities source: explicit --caps > adjacent file > auto-detected
    let caps_source = caps_path.or(auto_caps_path).or_else(|| {
        // Check adjacent to the wasm in the source dir
        let adj = wasm_path.with_extension("capabilities.json");
        adj.exists().then_some(adj)
    });

    // Copy WASM file
    println!("Installing '{}' → {}", channel_name, target_wasm.display());    // Validate the source is actually a WASM binary before copying.
    validate_wasm_magic(&wasm_path).await?;    fs::copy(&wasm_path, &target_wasm)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to copy WASM file: {}", e))?;

    // Copy capabilities if found
    if let Some(ref caps) = caps_source {
        validate_capabilities(caps).await?;
        fs::copy(caps, &target_caps).await?;
        println!("  Capabilities: {}", target_caps.display());
    } else {
        eprintln!("  Warning: No capabilities file found. Channel will have no permissions.");
        eprintln!("  Provide one with --caps <path> or place it adjacent to the .wasm.");
    }

    let wasm_bytes = fs::read(&target_wasm).await?;
    let hash = sha256_short(&wasm_bytes);

    println!();
    println!("Installed successfully:");
    println!("  Name: {}", channel_name);
    println!("  Size: {}", format_size(wasm_bytes.len() as u64));
    println!("  Hash: {}", hash);
    println!();
    println!("Restart IronClaw to activate the channel.");

    Ok(())
}

async fn remove_channel(name: String, dir: Option<PathBuf>, yes: bool) -> anyhow::Result<()> {
    let channels_dir = resolve_channels_dir(dir);
    let wasm_path = channels_dir.join(format!("{}.wasm", name));
    let caps_path = channels_dir.join(format!("{}.capabilities.json", name));

    if !wasm_path.exists() {
        anyhow::bail!("Channel '{}' not found in {}", name, channels_dir.display());
    }

    if !yes {
        print!("Remove channel '{}'? [y/N]: ", name);
        use std::io::Write as _;
        std::io::stdout().flush()?;

        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;

        if !line.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    fs::remove_file(&wasm_path).await?;
    println!("Removed {}", wasm_path.display());

    if caps_path.exists() {
        fs::remove_file(&caps_path).await?;
        println!("Removed {}", caps_path.display());
    }

    println!(
        "\nChannel '{}' removed. Restart IronClaw to take effect.",
        name
    );
    Ok(())
}

async fn channel_status(
    gateway: Option<String>,
    token: Option<String>,
    show_list: bool,
) -> anyhow::Result<()> {
    // ── Local disk inventory ──────────────────────────────────────────────────
    if show_list {
        let channels_dir = resolve_channels_dir(None);
        let channels = collect_channels(&channels_dir).await.unwrap_or_default();
        if channels.is_empty() {
            println!("Installed channels: none");
        } else {
            println!("Installed channels ({}):", channels.len());
            for (name, _, has_caps, size) in &channels {
                let c = if *has_caps { "✓" } else { "✗" };
                println!("  {} ({}, caps: {})", name, format_size(*size), c);
            }
        }
        println!();
    }

    // ── Gateway live status ───────────────────────────────────────────────────
    let base_url = resolve_gateway_url(gateway)?;
    let auth_token = resolve_gateway_token(token);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    // Health probe (public)
    match client.get(format!("{}/api/health", base_url)).send().await {
        Ok(r) if r.status().is_success() => {
            println!("Gateway: {} (up)", base_url);
        }
        _ => {
            println!("Gateway: {} (not reachable)", base_url);
            if !show_list {
                println!("Use `ironclaw gateway start` for setup instructions.");
            }
            return Ok(());
        }
    }

    // Authenticated status (channel connections)
    if let Some(ref tok) = auth_token {
        match client
            .get(format!("{}/api/gateway/status", base_url))
            .bearer_auth(tok)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                if let Ok(json) = r.json::<serde_json::Value>().await {
                    let sse = json
                        .get("sse_connections")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let ws = json
                        .get("ws_connections")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let uptime = json
                        .get("uptime_secs")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    println!(
                        "Active connections: {} SSE, {} WebSocket (uptime: {})",
                        sse,
                        ws,
                        format_duration(uptime)
                    );
                }
            }
            _ => {
                println!("(Set GATEWAY_AUTH_TOKEN for detailed connection counts)");
            }
        }
    } else {
        println!("(Set GATEWAY_AUTH_TOKEN for detailed connection counts)");
    }

    Ok(())
}

// ── Internal helpers ─────────────────────────────────────────────────────────

/// Resolve channels directory from CLI override or environment.
fn resolve_channels_dir(override_dir: Option<PathBuf>) -> PathBuf {
    if let Some(d) = override_dir {
        return d;
    }
    if let Ok(d) = std::env::var("WASM_CHANNELS_DIR") {
        return PathBuf::from(d);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ironclaw")
        .join("channels")
}

/// Resolve the WASM source from a path that may be a file or a directory.
/// Returns `(wasm_path, optional_caps_path)`.
async fn resolve_wasm_source(path: &Path) -> anyhow::Result<(PathBuf, Option<PathBuf>)> {
    if !path.exists() {
        anyhow::bail!("Path does not exist: {}", path.display());
    }

    if path.is_file() {
        // Direct .wasm file
        if path.extension().and_then(|e| e.to_str()) != Some("wasm") {
            anyhow::bail!("Expected a .wasm file, got: {}", path.display());
        }
        return Ok((path.to_path_buf(), None));
    }

    if path.is_dir() {
        // Look for .wasm in common build output locations
        let candidates = [
            // Dev build: target/wasm32-wasip2/release/*.wasm
            path.join("target/wasm32-wasip2/release"),
            // Flat layout
            path.to_path_buf(),
        ];

        for candidate_dir in &candidates {
            if !candidate_dir.exists() {
                continue;
            }
            if let Ok(mut entries) = fs::read_dir(candidate_dir).await {
                let mut wasm_files = Vec::new();
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let p = entry.path();
                    if p.extension().and_then(|e| e.to_str()) == Some("wasm") {
                        wasm_files.push(p);
                    }
                }
                if wasm_files.len() == 1 {
                    let wasm = wasm_files.remove(0);
                    // Look for a capabilities file adjacent to the discovered .wasm
                    // (using the wasm file's own stem, not the outer directory name).
                    let caps = wasm.with_extension("capabilities.json");
                    let caps_opt = caps.exists().then_some(caps);
                    return Ok((wasm, caps_opt));
                } else if wasm_files.len() > 1 {
                    anyhow::bail!(
                        "Multiple .wasm files found in {}. Use a direct path.",
                        candidate_dir.display()
                    );
                }
            }
        }

        anyhow::bail!(
            "No .wasm file found in {}. Build the channel first.",
            path.display()
        );
    }

    anyhow::bail!("Unexpected path type: {}", path.display())
}

/// Validate that a capabilities JSON is parseable and has the expected shape.
async fn validate_capabilities(path: &Path) -> anyhow::Result<()> {
    let content = fs::read_to_string(path).await.map_err(|e| {
        anyhow::anyhow!("Failed to read capabilities file {}: {}", path.display(), e)
    })?;
    let _: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("Invalid JSON in {}: {}", path.display(), e))?;
    Ok(())
}

/// Validate the first 4 bytes are the WebAssembly magic number (`\0asm`).
async fn validate_wasm_magic(path: &Path) -> anyhow::Result<()> {
    use tokio::io::AsyncReadExt;
    let mut f = tokio::fs::File::open(path)
        .await
        .map_err(|e| anyhow::anyhow!("Cannot open {}: {}", path.display(), e))?;
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic)
        .await
        .map_err(|_| anyhow::anyhow!("{} is too small to be a valid WASM file", path.display()))?;
    if magic != [0x00, 0x61, 0x73, 0x6D] {
        anyhow::bail!(
            "{} does not appear to be a WebAssembly binary (bad magic: {:02x?})",
            path.display(),
            magic
        );
    }
    Ok(())
}

/// Collect WASM files from `dir`, returning `(name, path, has_caps, size)`.
async fn collect_channels(dir: &Path) -> anyhow::Result<Vec<(String, PathBuf, bool, u64)>> {
    let mut channels = Vec::new();

    if !dir.exists() {
        return Ok(channels);
    }

    let mut entries = fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().map(|e| e == "wasm").unwrap_or(false) {
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();
            // Use async try_exists to avoid blocking the tokio thread.
            let caps_path = path.with_extension("capabilities.json");
            let has_caps = fs::try_exists(&caps_path).await.unwrap_or(false);
            let size = fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
            channels.push((name, path, has_caps, size));
        }
    }

    channels.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(channels)
}

/// Print a brief capabilities summary for the verbose listing.
fn print_caps_summary(caps: &serde_json::Value) {
    if let Some(http) = caps.get("http") {
        if let Some(allowlist) = http.get("allowlist").and_then(|v| v.as_array()) {
            let hosts: Vec<_> = allowlist
                .iter()
                .filter_map(|e| e.get("host").and_then(|h| h.as_str()))
                .collect();
            if !hosts.is_empty() {
                println!("    HTTP: {}", hosts.join(", "));
            }
        }
    }
    if let Some(secrets) = caps.get("secrets") {
        if let Some(names) = secrets.get("allowed_names").and_then(|v| v.as_array()) {
            if !names.is_empty() {
                println!("    Secrets: {}", names.len());
            }
        }
    }
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

fn sha256_short(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(bytes);
    hash.iter().take(8).map(|b| format!("{:02x}", b)).collect()
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn resolve_gateway_url(override_url: Option<String>) -> anyhow::Result<String> {
    if let Some(url) = override_url {
        return Ok(url.trim_end_matches('/').to_string());
    }
    let host = std::env::var("GATEWAY_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port: u16 = std::env::var("GATEWAY_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3001);
    Ok(format!("http://{}:{}", host, port))
}

fn resolve_gateway_token(override_token: Option<String>) -> Option<String> {
    override_token.or_else(|| std::env::var("GATEWAY_AUTH_TOKEN").ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_channel_names() {
        assert!(is_valid_name("telegram"));
        assert!(is_valid_name("my-channel"));
        assert!(is_valid_name("my_channel"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("my channel")); // space
        assert!(!is_valid_name("my/channel")); // slash
    }

    #[test]
    fn format_size_values() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1048576), "1.0 MB");
    }

    #[test]
    fn format_duration_values() {
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(90), "1m 30s");
        assert_eq!(format_duration(3661), "1h 1m");
    }
}
