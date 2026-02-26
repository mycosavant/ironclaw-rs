//! Memory tools for persistent workspace memory.
//!
//! These tools allow the agent to:
//! - Search past memories, decisions, and context
//! - Read and write files in the workspace
//!
//! # Usage
//!
//! The agent should use `memory_search` before answering questions about
//! prior work, decisions, dates, people, preferences, or todos.
//!
//! Use `memory_write` to persist important facts that should be remembered
//! across sessions.

use std::sync::Arc;

use async_trait::async_trait;

use crate::context::JobContext;
use crate::skills::SkillTrust;
use crate::tools::tool::{Tool, ToolError, ToolOutput, require_str};
use crate::workspace::{Workspace, paths};

/// Identity files that the LLM must not overwrite via tool calls.
/// These are loaded into the system prompt and could be used for prompt
/// injection if an attacker tricks the agent into overwriting them.
const PROTECTED_IDENTITY_FILES: &[&str] =
    &[paths::IDENTITY, paths::SOUL, paths::AGENTS, paths::USER];

/// Workspace path prefixes accessible to `Installed`-trust skills (read-only).
///
/// Paths outside these are invisible: installed skills cannot list, read, search,
/// or write them.  The `public/` prefix is reserved for content the owner
/// explicitly makes visible to third-party skills; `skills/` covers skill-owned
/// state so skills can manage their own data.
const INSTALLED_SKILL_READ_PREFIXES: &[&str] = &["skills/", "public/"];

/// Return the readable-prefix restriction for a given skill trust level.
///
/// `None` means *unrestricted* (trusted skill, full access).
/// `Some(prefixes)` means only paths that start with one of the listed prefixes
/// are readable; all other paths return `NotAuthorized`.
fn readable_prefixes_for_trust(trust: SkillTrust) -> Option<&'static [&'static str]> {
    match trust {
        SkillTrust::Trusted => None,
        SkillTrust::Installed => Some(INSTALLED_SKILL_READ_PREFIXES),
    }
}

/// Check whether `path` may be read by a skill with the given trust level.
///
/// Returns `Ok(())` if allowed, `Err(NotAuthorized)` otherwise.
///
/// # Path traversal prevention
///
/// Any path segment equal to `..` is rejected before the prefix check to prevent
/// traversal attacks such as `"skills/../secrets/key"` from bypassing the allowlist.
/// This check applies to all trust levels as a defence-in-depth measure.
fn check_read_path(path: &str, trust: SkillTrust) -> Result<(), ToolError> {
    // Reject path traversal unconditionally, for every trust level.
    if path.split('/').any(|segment| segment == "..") {
        return Err(ToolError::NotAuthorized(format!(
            "path '{}' contains '..' traversal segments which are not permitted",
            path,
        )));
    }

    let Some(prefixes) = readable_prefixes_for_trust(trust) else {
        return Ok(()); // Trusted — unrestricted (no prefix filter)
    };

    let normalized = path.trim_start_matches('/');
    if prefixes.iter().any(|p| normalized.starts_with(p)) {
        Ok(())
    } else {
        Err(ToolError::NotAuthorized(format!(
            "path '{}' is outside the memory prefixes accessible to installed skills \
             (allowed: {})",
            path,
            prefixes.join(", "),
        )))
    }
}

/// Filter tree entries at the workspace root to only those inside allowed prefixes.
///
/// Used when an installed skill calls `memory_tree` with an empty path parameter
/// so the root listing is restricted to `skills/` and `public/` subtrees.
fn filter_tree_entries_for_installed(entries: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    entries
        .into_iter()
        .filter(|entry| {
            // Each entry is either a String ("name/" or "name") or an Object
            // ({"name/": [children]}) from build_tree.  Match on the leading name.
            let name = match entry {
                serde_json::Value::String(s) => s.trim_end_matches('/').to_string(),
                serde_json::Value::Object(m) => m
                    .keys()
                    .next()
                    .map(|k| k.trim_end_matches('/').to_string())
                    .unwrap_or_default(),
                _ => String::new(),
            };
            INSTALLED_SKILL_READ_PREFIXES
                .iter()
                .any(|p| p.trim_end_matches('/') == name)
        })
        .collect()
}

/// Tool for searching workspace memory.
///
/// Performs hybrid search (FTS + semantic) across all memory documents.
/// The agent should call this tool before answering questions about
/// prior work, decisions, preferences, or any historical context.
pub struct MemorySearchTool {
    workspace: Arc<Workspace>,
}

impl MemorySearchTool {
    /// Create a new memory search tool.
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &str {
        "memory_search"
    }

    fn description(&self) -> &str {
        "Search past memories, decisions, and context. MUST be called before answering \
         questions about prior work, decisions, dates, people, preferences, or todos. \
         Returns relevant snippets with relevance scores."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query. Use natural language to describe what you're looking for."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results to return (default: 5, max: 20)",
                    "default": 5,
                    "minimum": 1,
                    "maximum": 20
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = std::time::Instant::now();

        let query = require_str(&params, "query")?;

        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .min(20) as usize;

        let results = self
            .workspace
            .search(query, limit)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("Search failed: {}", e)))?;

        // Filter results by document path when an installed skill is active.
        // Installed skills must not learn about the existence or content of paths
        // outside their allowed prefixes.
        let (results, filtered_count) = if let Some(trust) = ctx.active_skill_trust
            && let Some(prefixes) = readable_prefixes_for_trust(trust)
        {
            let before = results.len();
            let filtered: Vec<_> = results
                .into_iter()
                .filter(|r| {
                    let p = r.document_path.trim_start_matches('/');
                    prefixes.iter().any(|prefix| p.starts_with(prefix))
                })
                .collect();
            let removed = before - filtered.len();
            (filtered, removed)
        } else {
            (results, 0)
        };

        let output = serde_json::json!({
            "query": query,
            "results": results.iter().map(|r| serde_json::json!({
                "content": r.content,
                "score": r.score,
                "document_id": r.document_id.to_string(),
                "document_path": r.document_path,
                "is_hybrid_match": r.is_hybrid(),
            })).collect::<Vec<_>>(),
            "result_count": results.len(),
            "filtered_count": filtered_count,
        });

        Ok(ToolOutput::success(output, start.elapsed()))
    }

    fn requires_sanitization(&self) -> bool {
        false // Internal memory, trusted content
    }
}

/// Tool for writing to workspace memory.
///
/// Use this to persist important information that should be remembered
/// across sessions: decisions, preferences, facts, lessons learned.
pub struct MemoryWriteTool {
    workspace: Arc<Workspace>,
}

impl MemoryWriteTool {
    /// Create a new memory write tool.
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for MemoryWriteTool {
    fn name(&self) -> &str {
        "memory_write"
    }

    fn description(&self) -> &str {
        "Write to persistent memory (database-backed, NOT the local filesystem). \
         Use for important facts, decisions, preferences, or lessons learned that should \
         be remembered across sessions. Targets: 'memory' for curated long-term facts, \
         'daily_log' for timestamped session notes, 'heartbeat' for the periodic \
         checklist (HEARTBEAT.md), or provide a custom path for arbitrary file creation."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The content to write to memory. Be concise but include relevant context."
                },
                "target": {
                    "type": "string",
                    "description": "Where to write: 'memory' for MEMORY.md, 'daily_log' for today's log, 'heartbeat' for HEARTBEAT.md checklist, or a path like 'projects/alpha/notes.md'",
                    "default": "daily_log"
                },
                "append": {
                    "type": "boolean",
                    "description": "If true, append to existing content. If false, replace entirely.",
                    "default": true
                }
            },
            "required": ["content"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = std::time::Instant::now();

        // Installed skills cannot write workspace memory at all.
        if ctx.active_skill_trust == Some(SkillTrust::Installed) {
            return Err(ToolError::NotAuthorized(
                "installed skills cannot write to workspace memory".to_string(),
            ));
        }

        let content = require_str(&params, "content")?;

        if content.trim().is_empty() {
            return Err(ToolError::InvalidParameters(
                "content cannot be empty".to_string(),
            ));
        }

        let target = params
            .get("target")
            .and_then(|v| v.as_str())
            .unwrap_or("daily_log");

        // Reject writes to identity files that are loaded into the system prompt.
        // An attacker could use prompt injection to trick the agent into overwriting
        // these, poisoning future conversations.
        if PROTECTED_IDENTITY_FILES.contains(&target) {
            return Err(ToolError::NotAuthorized(format!(
                "writing to '{}' is not allowed (identity file protected from tool writes)",
                target,
            )));
        }

        let append = params
            .get("append")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let path = match target {
            "memory" => {
                if append {
                    self.workspace
                        .append_memory(content)
                        .await
                        .map_err(|e| ToolError::ExecutionFailed(format!("Write failed: {}", e)))?;
                } else {
                    self.workspace
                        .write(paths::MEMORY, content)
                        .await
                        .map_err(|e| ToolError::ExecutionFailed(format!("Write failed: {}", e)))?;
                }
                paths::MEMORY.to_string()
            }
            "daily_log" => {
                self.workspace
                    .append_daily_log(content)
                    .await
                    .map_err(|e| ToolError::ExecutionFailed(format!("Write failed: {}", e)))?;
                format!("daily/{}.md", chrono::Utc::now().format("%Y-%m-%d"))
            }
            "heartbeat" => {
                if append {
                    self.workspace
                        .append(paths::HEARTBEAT, content)
                        .await
                        .map_err(|e| ToolError::ExecutionFailed(format!("Write failed: {}", e)))?;
                } else {
                    self.workspace
                        .write(paths::HEARTBEAT, content)
                        .await
                        .map_err(|e| ToolError::ExecutionFailed(format!("Write failed: {}", e)))?;
                }
                paths::HEARTBEAT.to_string()
            }
            path => {
                // Protect identity files from LLM overwrites (prompt injection defense).
                // These files are injected into the system prompt, so poisoning them
                // would let an attacker rewrite the agent's core instructions.
                let normalized = path.trim_start_matches('/');
                if PROTECTED_IDENTITY_FILES
                    .iter()
                    .any(|p| normalized.eq_ignore_ascii_case(p))
                {
                    return Err(ToolError::NotAuthorized(format!(
                        "writing to '{}' is not allowed (identity file protected from tool access)",
                        path
                    )));
                }

                if append {
                    self.workspace
                        .append(path, content)
                        .await
                        .map_err(|e| ToolError::ExecutionFailed(format!("Write failed: {}", e)))?;
                } else {
                    self.workspace
                        .write(path, content)
                        .await
                        .map_err(|e| ToolError::ExecutionFailed(format!("Write failed: {}", e)))?;
                }
                path.to_string()
            }
        };

        let output = serde_json::json!({
            "status": "written",
            "path": path,
            "append": append,
            "content_length": content.len(),
        });

        Ok(ToolOutput::success(output, start.elapsed()))
    }

    fn requires_sanitization(&self) -> bool {
        false // Internal tool
    }

    fn rate_limit_config(&self) -> Option<crate::tools::tool::ToolRateLimitConfig> {
        Some(crate::tools::tool::ToolRateLimitConfig::new(20, 200))
    }
}

/// Tool for reading workspace files.
///
/// Use this to read the full content of any file in the workspace.
pub struct MemoryReadTool {
    workspace: Arc<Workspace>,
}

impl MemoryReadTool {
    /// Create a new memory read tool.
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for MemoryReadTool {
    fn name(&self) -> &str {
        "memory_read"
    }

    fn description(&self) -> &str {
        "Read a file from the workspace memory (database-backed storage). \
         Use this to read files shown by memory_tree. NOT for local filesystem files \
         (use read_file for those). Works with identity files, heartbeat checklist, \
         memory, daily logs, or any custom workspace path."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file (e.g., 'MEMORY.md', 'daily/2024-01-15.md', 'projects/alpha/notes.md')"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = std::time::Instant::now();

        let path = require_str(&params, "path")?;

        // Installed skills may only read paths within their allowed prefixes.
        if let Some(trust) = ctx.active_skill_trust {
            check_read_path(path, trust)?;
        }

        let doc = self
            .workspace
            .read(path)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("Read failed: {}", e)))?;

        let output = serde_json::json!({
            "path": doc.path,
            "content": doc.content,
            "word_count": doc.word_count(),
            "updated_at": doc.updated_at.to_rfc3339(),
        });

        Ok(ToolOutput::success(output, start.elapsed()))
    }

    fn requires_sanitization(&self) -> bool {
        false // Internal memory
    }
}

/// Tool for viewing workspace structure as a tree.
///
/// Returns a hierarchical view of files and directories with configurable depth.
pub struct MemoryTreeTool {
    workspace: Arc<Workspace>,
}

impl MemoryTreeTool {
    /// Create a new memory tree tool.
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self { workspace }
    }

    /// Recursively build tree structure.
    ///
    /// Returns a compact format where directories end with `/` and may have children.
    async fn build_tree(
        &self,
        path: &str,
        current_depth: usize,
        max_depth: usize,
    ) -> Result<Vec<serde_json::Value>, ToolError> {
        if current_depth > max_depth {
            return Ok(Vec::new());
        }

        let entries = self
            .workspace
            .list(path)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("Tree failed: {}", e)))?;

        let mut result = Vec::new();
        for entry in entries {
            // Directories end with `/`, files don't
            let display_path = if entry.is_directory {
                format!("{}/", entry.name())
            } else {
                entry.name().to_string()
            };

            if entry.is_directory && current_depth < max_depth {
                let children =
                    Box::pin(self.build_tree(&entry.path, current_depth + 1, max_depth)).await?;
                if children.is_empty() {
                    result.push(serde_json::Value::String(display_path));
                } else {
                    result.push(serde_json::json!({ display_path: children }));
                }
            } else {
                result.push(serde_json::Value::String(display_path));
            }
        }

        Ok(result)
    }
}

#[async_trait]
impl Tool for MemoryTreeTool {
    fn name(&self) -> &str {
        "memory_tree"
    }

    fn description(&self) -> &str {
        "View the workspace memory structure as a tree (database-backed storage). \
         Use memory_read to read files shown here, NOT read_file. \
         The workspace is separate from the local filesystem."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Root path to start from (empty string for workspace root)",
                    "default": ""
                },
                "depth": {
                    "type": "integer",
                    "description": "Maximum depth to traverse (1 = immediate children only)",
                    "default": 1,
                    "minimum": 1,
                    "maximum": 10
                }
            }
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = std::time::Instant::now();

        let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");

        let depth = params
            .get("depth")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .clamp(1, 10) as usize;

        // For installed skills, enforce prefix restrictions on the tree view.
        if let Some(trust) = ctx.active_skill_trust
            && readable_prefixes_for_trust(trust).is_some()
        {
            let normalized = path.trim_start_matches('/');
            if normalized.is_empty() {
                // Root listing: build the full tree but filter top-level entries
                // to only expose the allowed prefixes.
                let full_tree = self.build_tree("", 1, depth).await?;
                let restricted = filter_tree_entries_for_installed(full_tree);
                return Ok(ToolOutput::success(
                    serde_json::Value::Array(restricted),
                    start.elapsed(),
                ));
            }
            // Non-root: require path to be inside an allowed prefix.
            check_read_path(normalized, trust)?;
        }

        let tree = self.build_tree(path, 1, depth).await?;

        // Compact output: just the tree array
        Ok(ToolOutput::success(
            serde_json::Value::Array(tree),
            start.elapsed(),
        ))
    }

    fn requires_sanitization(&self) -> bool {
        false // Internal tool
    }
}

#[cfg(all(test, feature = "postgres"))]
mod tests {
    use super::*;

    fn make_test_workspace() -> Arc<Workspace> {
        Arc::new(Workspace::new(
            "test_user",
            deadpool_postgres::Pool::builder(deadpool_postgres::Manager::new(
                tokio_postgres::Config::new(),
                tokio_postgres::NoTls,
            ))
            .build()
            .unwrap(),
        ))
    }

    #[test]
    fn test_memory_search_schema() {
        let workspace = make_test_workspace();
        let tool = MemorySearchTool::new(workspace);

        assert_eq!(tool.name(), "memory_search");
        assert!(!tool.requires_sanitization());

        let schema = tool.parameters_schema();
        assert!(schema["properties"]["query"].is_object());
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&"query".into())
        );
    }

    #[test]
    fn test_memory_write_schema() {
        let workspace = make_test_workspace();
        let tool = MemoryWriteTool::new(workspace);

        assert_eq!(tool.name(), "memory_write");

        let schema = tool.parameters_schema();
        assert!(schema["properties"]["content"].is_object());
        assert!(schema["properties"]["target"].is_object());
        assert!(schema["properties"]["append"].is_object());
    }

    #[test]
    fn test_memory_read_schema() {
        let workspace = make_test_workspace();
        let tool = MemoryReadTool::new(workspace);

        assert_eq!(tool.name(), "memory_read");

        let schema = tool.parameters_schema();
        assert!(schema["properties"]["path"].is_object());
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&"path".into())
        );
    }

    #[test]
    fn test_memory_tree_schema() {
        let workspace = make_test_workspace();
        let tool = MemoryTreeTool::new(workspace);

        assert_eq!(tool.name(), "memory_tree");

        let schema = tool.parameters_schema();
        assert!(schema["properties"]["path"].is_object());
        assert!(schema["properties"]["depth"].is_object());
        assert_eq!(schema["properties"]["depth"]["default"], 1);
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    // ---------------------------------------------------------------------------
    // check_read_path
    // ---------------------------------------------------------------------------

    #[test]
    fn trusted_skill_reads_any_path() {
        assert!(check_read_path("context/passwords.md", SkillTrust::Trusted).is_ok());
        assert!(check_read_path("daily/2024-01-01.md", SkillTrust::Trusted).is_ok());
        assert!(check_read_path("MEMORY.md", SkillTrust::Trusted).is_ok());
    }

    #[test]
    fn installed_skill_reads_allowed_prefixes() {
        assert!(check_read_path("skills/my-skill/notes.md", SkillTrust::Installed).is_ok());
        assert!(check_read_path("public/faq.md", SkillTrust::Installed).is_ok());
        assert!(check_read_path("/skills/leading-slash.md", SkillTrust::Installed).is_ok());
    }

    #[test]
    fn installed_skill_denied_outside_allowed_prefixes() {
        let err = check_read_path("context/passwords.md", SkillTrust::Installed);
        assert!(matches!(err, Err(ToolError::NotAuthorized(_))));

        let err = check_read_path("MEMORY.md", SkillTrust::Installed);
        assert!(matches!(err, Err(ToolError::NotAuthorized(_))));

        let err = check_read_path("daily/2024-01-01.md", SkillTrust::Installed);
        assert!(matches!(err, Err(ToolError::NotAuthorized(_))));
    }

    // ---------------------------------------------------------------------------
    // Path traversal prevention
    // ---------------------------------------------------------------------------

    #[test]
    fn dotdot_traversal_blocked_for_installed_skill() {
        // "skills/../secrets/key" starts_with "skills/" but must be blocked
        let err = check_read_path("skills/../secrets/key", SkillTrust::Installed);
        assert!(
            matches!(err, Err(ToolError::NotAuthorized(ref msg)) if msg.contains("..")),
            "expected NotAuthorized with '..' mention, got {:?}",
            err
        );
    }

    #[test]
    fn dotdot_traversal_blocked_for_trusted_skill() {
        // ".." is rejected for ALL trust levels as defence-in-depth.
        let err = check_read_path("context/../../../etc/passwd", SkillTrust::Trusted);
        assert!(
            matches!(err, Err(ToolError::NotAuthorized(ref msg)) if msg.contains("..")),
            "Trusted skills must also be blocked from '..' traversal; got {:?}",
            err
        );
    }

    #[test]
    fn dotdot_in_middle_blocked_for_installed_skill() {
        // Various forms of traversal
        let cases = [
            "public/../secrets/key",
            "skills/my-skill/../../secrets",
            "../outside",
            "skills/foo/..",
        ];
        for path in cases {
            let err = check_read_path(path, SkillTrust::Installed);
            assert!(
                matches!(err, Err(ToolError::NotAuthorized(_))),
                "path '{}' should be blocked but was allowed",
                path
            );
        }
    }

    // ---------------------------------------------------------------------------
    // filter_tree_entries_for_installed
    // ---------------------------------------------------------------------------

    #[test]
    fn filter_tree_keeps_only_allowed_dirs() {
        let entries = vec![
            serde_json::Value::String("skills/".to_string()),
            serde_json::Value::String("public/".to_string()),
            serde_json::Value::String("context/".to_string()),
            serde_json::Value::String("daily/".to_string()),
            serde_json::json!({"skills/": ["my-skill/"]}),
            serde_json::json!({"context/": ["notes.md"]}),
        ];

        let filtered = filter_tree_entries_for_installed(entries);
        assert_eq!(filtered.len(), 3); // "skills/", "public/", {"skills/": [...]}

        let names: Vec<String> = filtered
            .iter()
            .map(|v| match v {
                serde_json::Value::String(s) => s.trim_end_matches('/').to_string(),
                serde_json::Value::Object(m) => m
                    .keys()
                    .next()
                    .map(|k| k.trim_end_matches('/').to_string())
                    .unwrap_or_default(),
                _ => String::new(),
            })
            .collect();

        assert!(names.contains(&"skills".to_string()));
        assert!(names.contains(&"public".to_string()));
        assert!(!names.contains(&"context".to_string()));
        assert!(!names.contains(&"daily".to_string()));
    }

    #[test]
    fn filter_tree_empty_input_returns_empty() {
        assert!(filter_tree_entries_for_installed(vec![]).is_empty());
    }

    // ---------------------------------------------------------------------------
    // readable_prefixes_for_trust
    // ---------------------------------------------------------------------------

    #[test]
    fn trusted_has_no_prefix_restriction() {
        assert!(readable_prefixes_for_trust(SkillTrust::Trusted).is_none());
    }

    #[test]
    fn installed_has_prefix_restriction() {
        let prefixes = readable_prefixes_for_trust(SkillTrust::Installed).unwrap();
        assert!(prefixes.contains(&"skills/"));
        assert!(prefixes.contains(&"public/"));
    }
}
