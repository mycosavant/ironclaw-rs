//! Workflow executor: runs workflow steps sequentially with support for
//! parallel fan-out, conditional branching, loops, and template substitution.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use crate::agent::scheduler::Scheduler;
use crate::agent::workflow::compiler::validate_workflow;
use crate::agent::workflow::types::{
    ConditionExpr, Workflow, WorkflowOutput, WorkflowRun, WorkflowRunStatus, WorkflowStep,
};
use crate::context::JobContext;
use crate::db::Database;
use crate::error::WorkflowError;
use crate::llm::{ChatMessage, CompletionRequest, LlmProvider};
use crate::safety::SafetyLayer;
use crate::tools::ToolRegistry;

/// Maximum nesting depth for condition evaluation to prevent stack overflow.
const MAX_CONDITION_DEPTH: usize = 32;
/// Maximum nesting depth for template value evaluation.
const MAX_TEMPLATE_VALUE_DEPTH: usize = 64;
/// Default workflow-level timeout (30 minutes).
const DEFAULT_WORKFLOW_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Maximum nesting depth for step execution to prevent stack overflow.
const MAX_STEP_EXECUTION_DEPTH: usize = 16;

/// Executes workflow definitions step by step.
pub struct WorkflowExecutor {
    store: Arc<dyn Database>,
    llm: Arc<dyn LlmProvider>,
    scheduler: Arc<Scheduler>,
    tools: Arc<ToolRegistry>,
    safety: Option<Arc<SafetyLayer>>,
    timeout: Duration,
}

impl WorkflowExecutor {
    pub fn new(
        store: Arc<dyn Database>,
        llm: Arc<dyn LlmProvider>,
        scheduler: Arc<Scheduler>,
        tools: Arc<ToolRegistry>,
    ) -> Self {
        Self {
            store,
            llm,
            scheduler,
            tools,
            safety: None,
            timeout: DEFAULT_WORKFLOW_TIMEOUT,
        }
    }

    /// Set the safety layer for tool output scanning.
    pub fn with_safety(mut self, safety: Arc<SafetyLayer>) -> Self {
        self.safety = Some(safety);
        self
    }

    /// Set a custom workflow-level timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Run a workflow to completion, updating the run record in the database.
    ///
    /// Wraps execution in a workflow-level timeout to prevent runaway runs.
    pub async fn run(
        &self,
        workflow: &Workflow,
        run: &mut WorkflowRun,
    ) -> Result<WorkflowOutput, WorkflowError> {
        let timeout = self.timeout;
        match tokio::time::timeout(timeout, self.run_inner(workflow, run)).await {
            Ok(result) => result,
            Err(_elapsed) => {
                run.status = WorkflowRunStatus::Failed;
                run.error = Some(format!(
                    "workflow execution timed out after {}s",
                    timeout.as_secs()
                ));
                run.completed_at = Some(Utc::now());
                let _ = self.store.update_workflow_run(run).await;
                Err(WorkflowError::Timeout {
                    timeout_secs: timeout.as_secs(),
                })
            }
        }
    }

    /// Inner run logic (without timeout wrapper).
    async fn run_inner(
        &self,
        workflow: &Workflow,
        run: &mut WorkflowRun,
    ) -> Result<WorkflowOutput, WorkflowError> {
        // Defensive: re-validate before executing (workflow may have been
        // modified since creation, or loaded from an older schema version).
        validate_workflow(workflow)?;

        // Seed outputs with input
        run.outputs.insert("input".to_string(), run.input.clone());

        for step in &workflow.steps {
            run.current_step = Some(step.id().to_string());
            // Persist progress
            if let Err(e) = self.store.update_workflow_run(run).await {
                tracing::warn!(workflow = %workflow.name, "failed to persist step progress: {e}");
            }

            match self
                .execute_step(step, &mut run.outputs, &run.user_id, 0)
                .await
            {
                Ok(()) => {}
                Err(e) => {
                    run.status = WorkflowRunStatus::Failed;
                    run.error = Some(e.to_string());
                    run.completed_at = Some(Utc::now());
                    let _ = self.store.update_workflow_run(run).await;
                    return Err(e);
                }
            }
        }

        run.status = WorkflowRunStatus::Completed;
        run.completed_at = Some(Utc::now());
        run.current_step = None;
        let _ = self.store.update_workflow_run(run).await;

        Ok(run.outputs.clone())
    }

    /// Dispatch a single step, adding its output to the outputs map.
    ///
    /// Uses `Pin<Box<...>>` to make the recursive future `Send`-able for
    /// parallel branch execution via `tokio::JoinSet`.
    fn execute_step<'a>(
        &'a self,
        step: &'a WorkflowStep,
        outputs: &'a mut WorkflowOutput,
        user_id: &'a str,
        depth: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), WorkflowError>> + Send + 'a>>
    {
        Box::pin(self.execute_step_inner(step, outputs, user_id, depth))
    }

    async fn execute_step_inner(
        &self,
        step: &WorkflowStep,
        outputs: &mut WorkflowOutput,
        user_id: &str,
        depth: usize,
    ) -> Result<(), WorkflowError> {
        if depth >= MAX_STEP_EXECUTION_DEPTH {
            return Err(WorkflowError::StepFailed {
                step_id: step.id().to_string(),
                reason: format!(
                    "step execution nesting exceeds maximum depth of {MAX_STEP_EXECUTION_DEPTH}"
                ),
            });
        }
        match step {
            WorkflowStep::Prompt {
                id,
                prompt,
                max_tokens,
            } => {
                let resolved = evaluate_template(prompt, outputs);
                let result = self.execute_prompt(&resolved, *max_tokens).await?;
                outputs.insert(id.clone(), serde_json::Value::String(result));
            }
            WorkflowStep::Tool {
                id,
                tool_name,
                params,
            } => {
                let resolved_params = evaluate_template_value(params, outputs);
                let result = self
                    .execute_tool(tool_name, resolved_params, user_id)
                    .await?;
                outputs.insert(id.clone(), result);
            }
            WorkflowStep::Parallel { id, branches } => {
                // NOTE: fail-fast semantics — if any branch errors, remaining branches
                // are aborted (JoinSet is dropped). Side effects from completed branches
                // (tool calls, dispatched jobs) are NOT rolled back. Callers should
                // design workflows to tolerate partial parallel execution.
                let mut set = tokio::task::JoinSet::new();
                let next_depth = depth + 1;
                for (branch_idx, branch) in branches.iter().enumerate() {
                    let branch = branch.clone();
                    let branch_outputs = outputs.clone();
                    let user_id = user_id.to_string();
                    let store = Arc::clone(&self.store);
                    let llm = Arc::clone(&self.llm);
                    let scheduler = Arc::clone(&self.scheduler);
                    let tools = Arc::clone(&self.tools);
                    let safety = self.safety.clone();

                    set.spawn(async move {
                        let mut executor = WorkflowExecutor::new(store, llm, scheduler, tools);
                        if let Some(s) = safety {
                            executor = executor.with_safety(s);
                        }
                        let mut local_outputs = branch_outputs;
                        for step in &branch {
                            executor
                                .execute_step(step, &mut local_outputs, &user_id, next_depth)
                                .await?;
                        }
                        Ok::<(usize, WorkflowOutput), WorkflowError>((branch_idx, local_outputs))
                    });
                }

                // Collect all branch results first, then merge in deterministic
                // (branch index) order so output is reproducible.
                let mut branch_results: Vec<(usize, WorkflowOutput)> = Vec::new();
                while let Some(result) = set.join_next().await {
                    let (idx, branch_outputs) = result
                        .map_err(|e| WorkflowError::StepFailed {
                            step_id: id.clone(),
                            reason: format!("branch join error: {e}"),
                        })?
                        .map_err(|e| WorkflowError::StepFailed {
                            step_id: id.clone(),
                            reason: e.to_string(),
                        })?;
                    branch_results.push((idx, branch_outputs));
                }
                branch_results.sort_by_key(|(idx, _)| *idx);

                // Merge in order: later branches (by definition index) win on conflict
                let pre_keys: std::collections::HashSet<String> = outputs.keys().cloned().collect();
                for (_idx, branch_outputs) in branch_results {
                    for (k, v) in branch_outputs {
                        // Only merge keys that are new from this branch (not pre-existing)
                        if !pre_keys.contains(&k) {
                            outputs.insert(k, v);
                        }
                    }
                }
                outputs.insert(id.clone(), serde_json::json!("parallel_complete"));
            }
            WorkflowStep::Condition {
                id,
                condition,
                then,
                otherwise,
            } => {
                let branch = if evaluate_condition(condition, outputs) {
                    then
                } else {
                    otherwise
                };
                for step in branch {
                    self.execute_step(step, outputs, user_id, depth + 1).await?;
                }
                outputs.insert(id.clone(), serde_json::json!("condition_evaluated"));
            }
            WorkflowStep::Loop {
                id,
                steps,
                exit_condition,
                max_iterations,
            } => {
                // Cycle detection relies on max_iterations rather than the SHA-256
                // guard because the workflow executor uses a different execution
                // path than the main agent loop.
                for iteration in 0..*max_iterations {
                    for step in steps {
                        self.execute_step(step, outputs, user_id, depth + 1).await?;
                    }
                    if evaluate_condition(exit_condition, outputs) {
                        outputs.insert(
                            id.clone(),
                            serde_json::json!({ "iterations": iteration + 1 }),
                        );
                        return Ok(());
                    }
                }
                return Err(WorkflowError::LoopExhausted {
                    step_id: id.clone(),
                    max_iterations: *max_iterations,
                });
            }
            WorkflowStep::Job {
                id,
                title,
                description,
                ..
            } => {
                let resolved_title = evaluate_template(title, outputs);
                let resolved_desc = evaluate_template(description, outputs);
                let job_id = self
                    .scheduler
                    .dispatch_job(user_id, &resolved_title, &resolved_desc, None)
                    .await
                    .map_err(|e| WorkflowError::StepFailed {
                        step_id: id.clone(),
                        reason: format!("dispatch failed: {e}"),
                    })?;
                outputs.insert(
                    id.clone(),
                    serde_json::json!({ "job_id": job_id.to_string() }),
                );
            }
        }
        Ok(())
    }

    /// Execute a single LLM prompt and return the response text.
    ///
    /// NOTE: Prompt results are stored in `workflow_runs.outputs` but are NOT
    /// individually recorded in the Merkle hash-chain audit trail. Only the
    /// top-level `workflow_run` tool invocation gets an `ActionRecord`. This
    /// is an accepted trade-off: per-step audit integration would require
    /// passing a `ConversationMemory` through the executor, and the
    /// `max_iterations` cap limits the blast radius.
    // TODO: Full per-step audit trail integration (see M10 in production review).
    async fn execute_prompt(&self, prompt: &str, max_tokens: u32) -> Result<String, WorkflowError> {
        let messages = vec![ChatMessage::user(prompt)];
        let request = CompletionRequest::new(messages)
            .with_max_tokens(max_tokens)
            .with_temperature(0.3);

        let response = self
            .llm
            .complete(request)
            .await
            .map_err(|e| WorkflowError::LlmFailed(e.to_string()))?;

        Ok(response.content)
    }

    /// Execute a tool by name with resolved parameters.
    ///
    /// NOTE: This routes directly through the `ToolRegistry`, bypassing the
    /// SHA-256 cycle guard used in the main agent loop (`Worker`). Repeated
    /// identical tool calls within a workflow loop rely on `max_iterations`
    /// for termination rather than cycle detection. Per-step audit records
    /// are not written here — only the top-level `workflow_run` invocation
    /// is recorded in the Merkle hash-chain. See M9/M10 in the production
    /// readiness audit for rationale.
    async fn execute_tool(
        &self,
        tool_name: &str,
        params: serde_json::Value,
        user_id: &str,
    ) -> Result<serde_json::Value, WorkflowError> {
        let tool = self
            .tools
            .get(tool_name)
            .await
            .ok_or_else(|| WorkflowError::ToolError(format!("tool '{tool_name}' not found")))?;

        // Build a synthetic JobContext for the tool call
        let job_ctx =
            JobContext::with_user(user_id, format!("workflow-tool-{tool_name}"), String::new());

        let output = tool
            .execute(params, &job_ctx)
            .await
            .map_err(|e| WorkflowError::ToolError(e.to_string()))?;

        // Run safety layer on tool output when configured and the tool requires it
        if let Some(ref safety) = self.safety
            && tool.requires_sanitization()
        {
            let output_str = match &output.result {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let sanitized = safety.sanitize_tool_output(tool_name, &output_str);
            if sanitized.was_modified {
                // Distinguish hard-blocked output (safety policy / leak detection)
                // from sanitized-but-allowed output (redaction, escaping).
                if sanitized.content.starts_with("[Output blocked") {
                    return Err(WorkflowError::SafetyBlocked {
                        step_id: tool_name.to_string(),
                        reason: sanitized.content.clone(),
                    });
                }
                tracing::warn!(
                    tool = tool_name,
                    warnings = sanitized.warnings.len(),
                    "SafetyLayer modified workflow tool output"
                );
                return Ok(serde_json::Value::String(sanitized.content));
            }
        }

        Ok(output.result)
    }
}

// ==================== Pure functions ====================

/// Substitute `{{key}}` and `{{key.subpath}}` references in a template string.
pub fn evaluate_template(template: &str, outputs: &WorkflowOutput) -> String {
    let mut result = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(start) = rest.find("{{") {
        result.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];
        if let Some(end) = after_open.find("}}") {
            let key = after_open[..end].trim();
            let value = resolve_key(key, outputs);
            result.push_str(&value);
            rest = &after_open[end + 2..];
        } else {
            // Unclosed template — copy literally
            result.push_str(&rest[start..]);
            rest = "";
        }
    }
    result.push_str(rest);
    result
}

/// Recursively substitute templates in a JSON value.
///
/// Depth-limited to prevent stack overflow on deeply-nested payloads.
pub fn evaluate_template_value(
    value: &serde_json::Value,
    outputs: &WorkflowOutput,
) -> serde_json::Value {
    evaluate_template_value_inner(value, outputs, 0)
}

fn evaluate_template_value_inner(
    value: &serde_json::Value,
    outputs: &WorkflowOutput,
    depth: usize,
) -> serde_json::Value {
    if depth >= MAX_TEMPLATE_VALUE_DEPTH {
        return value.clone();
    }
    match value {
        serde_json::Value::String(s) => {
            // If the whole string is a single template ref, return the raw JSON value
            if s.starts_with("{{") && s.ends_with("}}") && s.matches("{{").count() == 1 {
                let key = s[2..s.len() - 2].trim();
                if let Some(v) = resolve_key_value(key, outputs) {
                    return v;
                }
            }
            let resolved = evaluate_template(s, outputs);
            serde_json::Value::String(resolved)
        }
        serde_json::Value::Array(arr) => serde_json::Value::Array(
            arr.iter()
                .map(|v| evaluate_template_value_inner(v, outputs, depth + 1))
                .collect(),
        ),
        serde_json::Value::Object(map) => {
            let mut new_map = serde_json::Map::new();
            for (k, v) in map {
                new_map.insert(
                    k.clone(),
                    evaluate_template_value_inner(v, outputs, depth + 1),
                );
            }
            serde_json::Value::Object(new_map)
        }
        other => other.clone(),
    }
}

/// Evaluate a condition expression against the current outputs.
///
/// Depth-limited to prevent stack overflow from deeply-nested conditions.
pub fn evaluate_condition(condition: &ConditionExpr, outputs: &WorkflowOutput) -> bool {
    evaluate_condition_inner(condition, outputs, 0)
}

fn evaluate_condition_inner(
    condition: &ConditionExpr,
    outputs: &WorkflowOutput,
    depth: usize,
) -> bool {
    if depth >= MAX_CONDITION_DEPTH {
        tracing::warn!(
            "Condition evaluation exceeded max depth ({MAX_CONDITION_DEPTH}), returning false"
        );
        return false;
    }
    match condition {
        ConditionExpr::Equals { key, value } => {
            let resolved = resolve_key_value(key, outputs);
            resolved.as_ref() == Some(value)
        }
        ConditionExpr::NotEmpty { key } => {
            let resolved = resolve_key_value(key, outputs);
            match resolved {
                None => false,
                Some(serde_json::Value::Null) => false,
                Some(serde_json::Value::String(s)) => !s.is_empty(),
                Some(serde_json::Value::Array(a)) => !a.is_empty(),
                Some(serde_json::Value::Object(o)) => !o.is_empty(),
                Some(_) => true,
            }
        }
        ConditionExpr::Contains { key, substring } => {
            let val = resolve_key(key, outputs);
            val.contains(substring.as_str())
        }
        ConditionExpr::And { conditions } => conditions
            .iter()
            .all(|c| evaluate_condition_inner(c, outputs, depth + 1)),
        ConditionExpr::Or { conditions } => conditions
            .iter()
            .any(|c| evaluate_condition_inner(c, outputs, depth + 1)),
        ConditionExpr::Not { condition } => {
            !evaluate_condition_inner(condition, outputs, depth + 1)
        }
    }
}

/// Resolve a dotted key path (e.g. "step1.result") against outputs.
fn resolve_key(key: &str, outputs: &WorkflowOutput) -> String {
    match resolve_key_value(key, outputs) {
        Some(serde_json::Value::String(s)) => s,
        Some(v) => v.to_string(),
        None => String::new(),
    }
}

/// Resolve a dotted key path to a JSON value.
fn resolve_key_value(key: &str, outputs: &WorkflowOutput) -> Option<serde_json::Value> {
    let mut parts = key.splitn(2, '.');
    let top = parts.next()?;
    let value = outputs.get(top)?;

    match parts.next() {
        None => Some(value.clone()),
        Some(rest) => {
            // Walk into the JSON value
            let mut current = value;
            for part in rest.split('.') {
                current = current.get(part)?;
            }
            Some(current.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::workflow::executor::{
        evaluate_condition, evaluate_template, evaluate_template_value,
    };
    use crate::agent::workflow::types::{ConditionExpr, WorkflowOutput};

    #[test]
    fn test_evaluate_template_simple() {
        let mut outputs = WorkflowOutput::new();
        outputs.insert("input".into(), serde_json::json!("hello world"));
        outputs.insert("step1".into(), serde_json::json!("processed"));

        let result = evaluate_template("Input was: {{input}}, result: {{step1}}", &outputs);
        assert_eq!(result, "Input was: hello world, result: processed");
    }

    #[test]
    fn test_evaluate_template_dotted_key() {
        let mut outputs = WorkflowOutput::new();
        outputs.insert(
            "step1".into(),
            serde_json::json!({"name": "test", "count": 42}),
        );

        let result = evaluate_template("Name: {{step1.name}}, Count: {{step1.count}}", &outputs);
        assert_eq!(result, "Name: test, Count: 42");
    }

    #[test]
    fn test_evaluate_template_missing_key() {
        let outputs = WorkflowOutput::new();
        let result = evaluate_template("Missing: {{unknown}}", &outputs);
        assert_eq!(result, "Missing: ");
    }

    #[test]
    fn test_evaluate_template_unclosed() {
        let outputs = WorkflowOutput::new();
        let result = evaluate_template("Unclosed: {{start", &outputs);
        assert_eq!(result, "Unclosed: {{start");
    }

    #[test]
    fn test_evaluate_condition_equals() {
        let mut outputs = WorkflowOutput::new();
        outputs.insert("status".into(), serde_json::json!("done"));

        assert!(evaluate_condition(
            &ConditionExpr::Equals {
                key: "status".into(),
                value: serde_json::json!("done"),
            },
            &outputs,
        ));
        assert!(!evaluate_condition(
            &ConditionExpr::Equals {
                key: "status".into(),
                value: serde_json::json!("pending"),
            },
            &outputs,
        ));
    }

    #[test]
    fn test_evaluate_condition_not_empty() {
        let mut outputs = WorkflowOutput::new();
        outputs.insert("filled".into(), serde_json::json!("data"));
        outputs.insert("empty".into(), serde_json::json!(""));
        outputs.insert("null_val".into(), serde_json::Value::Null);

        assert!(evaluate_condition(
            &ConditionExpr::NotEmpty {
                key: "filled".into(),
            },
            &outputs,
        ));
        assert!(!evaluate_condition(
            &ConditionExpr::NotEmpty {
                key: "empty".into(),
            },
            &outputs,
        ));
        assert!(!evaluate_condition(
            &ConditionExpr::NotEmpty {
                key: "null_val".into(),
            },
            &outputs,
        ));
        assert!(!evaluate_condition(
            &ConditionExpr::NotEmpty {
                key: "missing".into(),
            },
            &outputs,
        ));
    }

    #[test]
    fn test_evaluate_condition_contains() {
        let mut outputs = WorkflowOutput::new();
        outputs.insert("text".into(), serde_json::json!("hello world"));

        assert!(evaluate_condition(
            &ConditionExpr::Contains {
                key: "text".into(),
                substring: "world".into(),
            },
            &outputs,
        ));
        assert!(!evaluate_condition(
            &ConditionExpr::Contains {
                key: "text".into(),
                substring: "universe".into(),
            },
            &outputs,
        ));
    }

    #[test]
    fn test_evaluate_condition_and_or_not() {
        let mut outputs = WorkflowOutput::new();
        outputs.insert("a".into(), serde_json::json!("yes"));
        outputs.insert("b".into(), serde_json::json!(""));

        assert!(evaluate_condition(
            &ConditionExpr::And {
                conditions: vec![
                    ConditionExpr::NotEmpty { key: "a".into() },
                    ConditionExpr::Not {
                        condition: Box::new(ConditionExpr::NotEmpty { key: "b".into() }),
                    },
                ],
            },
            &outputs,
        ));

        assert!(evaluate_condition(
            &ConditionExpr::Or {
                conditions: vec![
                    ConditionExpr::NotEmpty { key: "b".into() },
                    ConditionExpr::NotEmpty { key: "a".into() },
                ],
            },
            &outputs,
        ));
    }

    #[test]
    fn test_evaluate_template_value_passthrough() {
        let mut outputs = WorkflowOutput::new();
        outputs.insert("data".into(), serde_json::json!({"nested": true}));

        // Single template ref returns the JSON value directly
        let result = evaluate_template_value(&serde_json::json!("{{data}}"), &outputs);
        assert_eq!(result, serde_json::json!({"nested": true}));

        // Template within a string gets stringified
        let result = evaluate_template_value(&serde_json::json!("prefix: {{data}}"), &outputs);
        assert!(result.as_str().is_some_and(|s| s.starts_with("prefix:")));
    }

    #[test]
    fn test_evaluate_template_value_nested() {
        let mut outputs = WorkflowOutput::new();
        outputs.insert("name".into(), serde_json::json!("Alice"));

        let input = serde_json::json!({
            "greeting": "Hello {{name}}",
            "list": ["{{name}}", "Bob"]
        });
        let result = evaluate_template_value(&input, &outputs);

        assert_eq!(result["greeting"], "Hello Alice");
        assert_eq!(result["list"][0], "Alice");
        assert_eq!(result["list"][1], "Bob");
    }
}
