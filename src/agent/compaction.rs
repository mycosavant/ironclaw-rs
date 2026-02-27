//! Context compaction for preserving and summarizing conversation history.
//!
//! When the context window approaches its limit, compaction:
//! 1. Summarizes old turns
//! 2. Writes the summary to the workspace daily log
//! 3. Trims the context to keep only recent turns

use std::sync::Arc;

use chrono::Utc;

use crate::agent::context_monitor::{CompactionStrategy, ContextBreakdown};
use crate::agent::session::Thread;
use crate::error::Error;
use crate::llm::{ChatMessage, CompletionRequest, LlmProvider, Reasoning};
use crate::safety::SafetyLayer;
use crate::workspace::Workspace;

/// Prefix used to identify synthetic turns injected after compaction.
/// Used to skip these turns during subsequent summarization passes.
const COMPACTION_MARKER: &str =
    "[Context was compacted. Previous conversation was removed to free space.]";

/// Result of a compaction operation.
#[derive(Debug)]
pub struct CompactionResult {
    /// Number of turns removed.
    pub turns_removed: usize,
    /// Tokens before compaction.
    pub tokens_before: usize,
    /// Tokens after compaction.
    pub tokens_after: usize,
    /// Whether a summary was written to workspace.
    pub summary_written: bool,
    /// The generated summary (if any).
    pub summary: Option<String>,
}

/// Compacts conversation context to stay within limits.
pub struct ContextCompactor {
    llm: Arc<dyn LlmProvider>,
    safety: Arc<SafetyLayer>,
}

impl ContextCompactor {
    /// Create a new context compactor.
    pub fn new(llm: Arc<dyn LlmProvider>, safety: Arc<SafetyLayer>) -> Self {
        Self { llm, safety }
    }

    /// Compact a thread's context using the given strategy.
    ///
    /// If `identity_context` is provided and turns are removed, a synthetic
    /// context-reminder turn is prepended so the LLM retains awareness of
    /// workspace identity, recent summary, and active rules after compaction.
    pub async fn compact(
        &self,
        thread: &mut Thread,
        strategy: CompactionStrategy,
        workspace: Option<&Workspace>,
        identity_context: Option<&str>,
    ) -> Result<CompactionResult, Error> {
        let messages = thread.messages();
        let tokens_before = ContextBreakdown::analyze(&messages).total_tokens;

        let result = match strategy {
            CompactionStrategy::Summarize { keep_recent } => {
                self.compact_with_summary(thread, keep_recent, workspace)
                    .await?
            }
            CompactionStrategy::Truncate { keep_recent } => {
                self.compact_truncate(thread, keep_recent)
            }
            CompactionStrategy::MoveToWorkspace => {
                self.compact_to_workspace(thread, workspace).await?
            }
        };

        // Inject post-compaction context if turns were removed
        if result.turns_removed > 0 {
            self.inject_post_compaction_context(
                thread,
                result.summary.as_deref(),
                identity_context,
            );
        }

        let messages_after = thread.messages();
        let tokens_after = ContextBreakdown::analyze(&messages_after).total_tokens;

        Ok(CompactionResult {
            turns_removed: result.turns_removed,
            tokens_before,
            tokens_after,
            summary_written: result.summary_written,
            summary: result.summary,
        })
    }

    /// Inject a context-reminder turn at the start of the thread after compaction.
    ///
    /// This ensures the LLM retains awareness of:
    /// 1. The compaction summary (what was discussed before)
    /// 2. Workspace identity and rules (who the agent is, active constraints)
    fn inject_post_compaction_context(
        &self,
        thread: &mut Thread,
        summary: Option<&str>,
        identity_context: Option<&str>,
    ) {
        let has_summary = summary.is_some_and(|s| !s.is_empty());
        let has_identity = identity_context.is_some_and(|s| !s.is_empty());

        if !has_summary && !has_identity {
            return;
        }

        let mut parts = Vec::new();
        parts.push(COMPACTION_MARKER.to_string());

        if let Some(s) = summary
            && !s.is_empty()
        {
            parts.push(format!("## Conversation Summary\n\n{}", s));
        }

        if let Some(ctx) = identity_context
            && !ctx.is_empty()
        {
            parts.push(format!("## Workspace Context\n\n{}", ctx));
        }

        let reminder = parts.join("\n\n");

        // Prepend as a completed synthetic turn so the LLM sees it first
        let mut context_turn = crate::agent::session::Turn::new(0, &reminder);
        context_turn.complete(
            "Understood. I have the context from the previous conversation \
             and workspace identity. Continuing.",
        );

        // Insert at position 0 and renumber
        thread.turns.insert(0, context_turn);
        for (i, turn) in thread.turns.iter_mut().enumerate() {
            turn.turn_number = i;
        }
    }

    /// Compact by summarizing old turns.
    async fn compact_with_summary(
        &self,
        thread: &mut Thread,
        keep_recent: usize,
        workspace: Option<&Workspace>,
    ) -> Result<CompactionPartial, Error> {
        if thread.turns.len() <= keep_recent {
            return Ok(CompactionPartial::empty());
        }

        // Get turns to summarize
        let turns_to_remove = thread.turns.len() - keep_recent;
        let old_turns = &thread.turns[..turns_to_remove];

        // Build messages for summarization, skipping synthetic context turns
        // injected by previous compactions (they start with the compaction marker).
        let mut to_summarize = Vec::new();
        for turn in old_turns {
            if turn.user_input.starts_with(COMPACTION_MARKER) {
                continue;
            }
            to_summarize.push(ChatMessage::user(&turn.user_input));
            if let Some(ref response) = turn.response {
                to_summarize.push(ChatMessage::assistant(response));
            }
        }

        // Generate summary
        let summary = self.generate_summary(&to_summarize).await?;

        // Write to workspace if available, with workspace rules appended
        let summary_written = if let Some(ws) = workspace {
            // Load AGENTS.md as workspace rules for the summary audit trail
            let workspace_rules = match ws.read(crate::workspace::paths::AGENTS).await {
                Ok(doc) if !doc.content.is_empty() => Some(doc.content),
                _ => None,
            };
            match self
                .write_summary_to_workspace(ws, &summary, workspace_rules.as_deref())
                .await
            {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(
                        "Compaction summary write failed (turns will still be truncated): {}",
                        e
                    );
                    false
                }
            }
        } else {
            false
        };

        // Truncate thread
        thread.truncate_turns(keep_recent);

        Ok(CompactionPartial {
            turns_removed: turns_to_remove,
            summary_written,
            summary: Some(summary),
        })
    }

    /// Compact by simple truncation (no summary).
    fn compact_truncate(&self, thread: &mut Thread, keep_recent: usize) -> CompactionPartial {
        let turns_before = thread.turns.len();
        thread.truncate_turns(keep_recent);
        let turns_removed = turns_before - thread.turns.len();

        CompactionPartial {
            turns_removed,
            summary_written: false,
            summary: None,
        }
    }

    /// Move context to workspace without summarization.
    async fn compact_to_workspace(
        &self,
        thread: &mut Thread,
        workspace: Option<&Workspace>,
    ) -> Result<CompactionPartial, Error> {
        let Some(ws) = workspace else {
            // Fall back to truncation if no workspace
            return Ok(self.compact_truncate(thread, 5));
        };

        // Keep more turns when moving to workspace (we have a backup)
        let keep_recent = 10;
        if thread.turns.len() <= keep_recent {
            return Ok(CompactionPartial::empty());
        }

        let turns_to_remove = thread.turns.len() - keep_recent;
        let old_turns = &thread.turns[..turns_to_remove];

        // Format turns for storage
        let content = format_turns_for_storage(old_turns);

        // Write to workspace
        let written = match self.write_context_to_workspace(ws, &content).await {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(
                    "Compaction context write failed (turns will still be truncated): {}",
                    e
                );
                false
            }
        };

        // Truncate
        thread.truncate_turns(keep_recent);

        Ok(CompactionPartial {
            turns_removed: turns_to_remove,
            summary_written: written,
            summary: None,
        })
    }

    /// Generate a summary of messages using the LLM.
    async fn generate_summary(&self, messages: &[ChatMessage]) -> Result<String, Error> {
        let prompt = ChatMessage::system(
            r#"Summarize the following conversation concisely. Focus on:
- Key decisions made
- Important information exchanged
- Actions taken
- Outcomes achieved

Be brief but capture all important details. Use bullet points."#,
        );

        let mut request_messages = vec![prompt];

        // Add a user message with the conversation to summarize
        let formatted = messages
            .iter()
            .map(|m| {
                let role_str = match m.role {
                    crate::llm::Role::User => "User",
                    crate::llm::Role::Assistant => "Assistant",
                    crate::llm::Role::System => "System",
                    crate::llm::Role::Tool => {
                        return format!(
                            "Tool {}: {}",
                            m.name.as_deref().unwrap_or("unknown"),
                            m.content
                        );
                    }
                };
                format!("{}: {}", role_str, m.content)
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        request_messages.push(ChatMessage::user(format!(
            "Please summarize this conversation:\n\n{}",
            formatted
        )));

        let request = CompletionRequest::new(request_messages)
            .with_max_tokens(1024)
            .with_temperature(0.3);

        let reasoning = Reasoning::new(self.llm.clone(), self.safety.clone());
        let (text, _) = reasoning.complete(request).await?;
        Ok(text)
    }

    /// Write a summary to the workspace daily log.
    ///
    /// If `workspace_rules` is provided (typically from AGENTS.md), a
    /// "Active Workspace Rules" section is appended to the summary entry
    /// so the persisted record reflects any constraints that were in effect.
    async fn write_summary_to_workspace(
        &self,
        workspace: &Workspace,
        summary: &str,
        workspace_rules: Option<&str>,
    ) -> Result<(), Error> {
        let date = Utc::now().format("%Y-%m-%d");
        let mut entry = format!(
            "\n## Context Summary ({})\n\n{}\n",
            Utc::now().format("%H:%M UTC"),
            summary
        );

        if let Some(rules) = workspace_rules
            && !rules.is_empty()
        {
            entry.push_str(&format!("\n## Active Workspace Rules\n\n{}\n", rules));
        }

        workspace
            .append(&format!("daily/{}.md", date), &entry)
            .await?;
        Ok(())
    }

    /// Write full context to workspace for archival.
    async fn write_context_to_workspace(
        &self,
        workspace: &Workspace,
        content: &str,
    ) -> Result<(), Error> {
        let date = Utc::now().format("%Y-%m-%d");
        let entry = format!(
            "\n## Archived Context ({})\n\n{}\n",
            Utc::now().format("%H:%M UTC"),
            content
        );

        workspace
            .append(&format!("daily/{}.md", date), &entry)
            .await?;
        Ok(())
    }
}

/// Partial result during compaction (internal).
struct CompactionPartial {
    turns_removed: usize,
    summary_written: bool,
    summary: Option<String>,
}

impl CompactionPartial {
    fn empty() -> Self {
        Self {
            turns_removed: 0,
            summary_written: false,
            summary: None,
        }
    }
}

/// Format turns for storage in workspace.
fn format_turns_for_storage(turns: &[crate::agent::session::Turn]) -> String {
    turns
        .iter()
        .map(|turn| {
            let mut s = format!("**Turn {}**\n", turn.turn_number + 1);
            s.push_str(&format!("User: {}\n", turn.user_input));
            if let Some(ref response) = turn.response {
                s.push_str(&format!("Agent: {}\n", response));
            }
            if !turn.tool_calls.is_empty() {
                s.push_str("Tools: ");
                let tools: Vec<_> = turn.tool_calls.iter().map(|t| t.name.as_str()).collect();
                s.push_str(&tools.join(", "));
                s.push('\n');
            }
            s
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::session::Thread;
    use uuid::Uuid;

    #[test]
    fn test_format_turns() {
        let mut thread = Thread::new(Uuid::new_v4());
        thread.start_turn("Hello");
        thread.complete_turn("Hi there");
        thread.start_turn("How are you?");
        thread.complete_turn("I'm good!");

        let formatted = format_turns_for_storage(&thread.turns);
        assert!(formatted.contains("Turn 1"));
        assert!(formatted.contains("Hello"));
        assert!(formatted.contains("Turn 2"));
    }

    #[test]
    fn test_compaction_partial_empty() {
        let partial = CompactionPartial::empty();
        assert_eq!(partial.turns_removed, 0);
        assert!(!partial.summary_written);
    }

    #[test]
    fn test_inject_post_compaction_context_with_summary_and_identity() {
        use crate::config::SafetyConfig;
        use crate::safety::SafetyLayer;
        use crate::testing::StubLlm;

        let llm: Arc<dyn LlmProvider> = Arc::new(StubLlm::new("test"));
        let safety = Arc::new(SafetyLayer::new(&SafetyConfig {
            max_output_length: 100_000,
            injection_check_enabled: false,
            http_url_allowlist: None,
        }));
        let compactor = ContextCompactor::new(llm, safety);

        let mut thread = Thread::new(Uuid::new_v4());
        thread.start_turn("Current question");
        thread.complete_turn("Current answer");

        compactor.inject_post_compaction_context(
            &mut thread,
            Some("Summary of previous conversation"),
            Some("Agent instructions from AGENTS.md"),
        );

        // Should have 2 turns: synthetic context turn + original
        assert_eq!(thread.turns.len(), 2);
        assert!(thread.turns[0].user_input.contains("Context was compacted"));
        assert!(
            thread.turns[0]
                .user_input
                .contains("Summary of previous conversation")
        );
        assert!(
            thread.turns[0]
                .user_input
                .contains("Agent instructions from AGENTS.md")
        );
        assert!(thread.turns[0].response.is_some());
        // Original turn is now at index 1
        assert_eq!(thread.turns[1].user_input, "Current question");
        // Turn numbers should be renumbered
        assert_eq!(thread.turns[0].turn_number, 0);
        assert_eq!(thread.turns[1].turn_number, 1);
    }

    #[test]
    fn test_inject_post_compaction_context_with_summary_only() {
        use crate::config::SafetyConfig;
        use crate::safety::SafetyLayer;
        use crate::testing::StubLlm;

        let llm: Arc<dyn LlmProvider> = Arc::new(StubLlm::new("test"));
        let safety = Arc::new(SafetyLayer::new(&SafetyConfig {
            max_output_length: 100_000,
            injection_check_enabled: false,
            http_url_allowlist: None,
        }));
        let compactor = ContextCompactor::new(llm, safety);

        let mut thread = Thread::new(Uuid::new_v4());
        thread.start_turn("Q");
        thread.complete_turn("A");

        compactor.inject_post_compaction_context(&mut thread, Some("My summary"), None);

        assert_eq!(thread.turns.len(), 2);
        assert!(thread.turns[0].user_input.contains("My summary"));
        assert!(!thread.turns[0].user_input.contains("Workspace Context"));
    }

    #[test]
    fn test_inject_post_compaction_context_skips_when_empty() {
        use crate::config::SafetyConfig;
        use crate::safety::SafetyLayer;
        use crate::testing::StubLlm;

        let llm: Arc<dyn LlmProvider> = Arc::new(StubLlm::new("test"));
        let safety = Arc::new(SafetyLayer::new(&SafetyConfig {
            max_output_length: 100_000,
            injection_check_enabled: false,
            http_url_allowlist: None,
        }));
        let compactor = ContextCompactor::new(llm, safety);

        let mut thread = Thread::new(Uuid::new_v4());
        thread.start_turn("Q");
        thread.complete_turn("A");

        compactor.inject_post_compaction_context(&mut thread, None, None);

        // No injection — still 1 turn
        assert_eq!(thread.turns.len(), 1);
    }
}
