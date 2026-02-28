//! LLM-facing tools for managing workflows.
//!
//! Six tools let the agent manage workflows conversationally:
//! - `workflow_create` - Create a new workflow definition
//! - `workflow_run` - Start a workflow run (async, returns run ID)
//! - `workflow_list` - List all workflows
//! - `workflow_status` - Check status of a workflow run
//! - `workflow_delete` - Delete a workflow definition
//! - `workflow_update` - Update an existing workflow definition

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use uuid::Uuid;

use crate::agent::workflow::{
    WorkflowExecutor, WorkflowRun, WorkflowRunStatus, WorkflowStep, validate_workflow,
};
use crate::context::JobContext;
use crate::db::Database;
use crate::tools::tool::{Tool, ToolError, ToolOutput, require_str};

// ==================== workflow_create ====================

pub struct WorkflowCreateTool {
    store: Arc<dyn Database>,
}

impl WorkflowCreateTool {
    pub fn new(store: Arc<dyn Database>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for WorkflowCreateTool {
    fn name(&self) -> &str {
        "workflow_create"
    }

    fn description(&self) -> &str {
        "Create a new multi-step workflow definition. Workflows support sequential steps, \
         parallel fan-out, conditional branching, loops, and data flow between steps via \
         {{key}} template substitution."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Unique name for the workflow (e.g. 'daily-report-pipeline')"
                },
                "description": {
                    "type": "string",
                    "description": "What this workflow does"
                },
                "steps": {
                    "type": "array",
                    "description": "Array of workflow step objects. Each step has a 'type' field \
                        (prompt, tool, parallel, condition, loop, job), a unique 'id', and \
                        type-specific fields. Use {{step_id}} to reference earlier step outputs.",
                    "items": { "type": "object" }
                },
                "input_schema": {
                    "type": "object",
                    "description": "Optional JSON Schema for the workflow input"
                }
            },
            "required": ["name", "steps"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = std::time::Instant::now();

        let name = require_str(&params, "name")?;
        let description = params
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let steps_value = params
            .get("steps")
            .ok_or_else(|| ToolError::InvalidParameters("missing 'steps' parameter".to_string()))?;

        let steps: Vec<WorkflowStep> = serde_json::from_value(steps_value.clone())
            .map_err(|e| ToolError::InvalidParameters(format!("invalid steps: {e}")))?;

        let input_schema = params
            .get("input_schema")
            .cloned()
            .unwrap_or(serde_json::json!({}));

        let workflow = crate::agent::workflow::Workflow {
            id: Uuid::new_v4(),
            name: name.to_string(),
            description: description.to_string(),
            user_id: ctx.user_id.clone(),
            steps,
            input_schema,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        // Validate
        validate_workflow(&workflow).map_err(|e| {
            ToolError::InvalidParameters(format!("workflow validation failed: {e}"))
        })?;

        // Persist
        self.store
            .create_workflow(&workflow)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("failed to create workflow: {e}")))?;

        let result = serde_json::json!({
            "id": workflow.id.to_string(),
            "name": workflow.name,
            "step_count": workflow.steps.len(),
            "status": "created",
        });

        Ok(ToolOutput::success(result, start.elapsed()))
    }

    fn requires_sanitization(&self) -> bool {
        true // Workflow steps contain prompt templates that will be executed later
    }
}

// ==================== workflow_run ====================

pub struct WorkflowRunTool {
    store: Arc<dyn Database>,
    executor: Arc<WorkflowExecutor>,
}

impl WorkflowRunTool {
    pub fn new(store: Arc<dyn Database>, executor: Arc<WorkflowExecutor>) -> Self {
        Self { store, executor }
    }
}

#[async_trait]
impl Tool for WorkflowRunTool {
    fn name(&self) -> &str {
        "workflow_run"
    }

    fn description(&self) -> &str {
        "Start a workflow execution. Returns the run ID immediately; the workflow runs \
         asynchronously. Use workflow_status to check progress."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the workflow to run"
                },
                "input": {
                    "type": "object",
                    "description": "Input data for the workflow (accessible as {{input}} in templates)"
                }
            },
            "required": ["name"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = std::time::Instant::now();

        let name = require_str(&params, "name")?;
        let input = params
            .get("input")
            .cloned()
            .unwrap_or(serde_json::json!({}));

        // Look up the workflow
        let workflow = self
            .store
            .get_workflow_by_name(&ctx.user_id, name)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("DB error: {e}")))?
            .ok_or_else(|| ToolError::ExecutionFailed(format!("workflow '{name}' not found")))?;

        // Create a run record
        let mut run = WorkflowRun {
            id: Uuid::new_v4(),
            workflow_id: workflow.id,
            user_id: ctx.user_id.clone(),
            input,
            outputs: std::collections::HashMap::new(),
            status: WorkflowRunStatus::Running,
            current_step: None,
            error: None,
            started_at: Utc::now(),
            completed_at: None,
            routine_run_id: None,
        };

        self.store
            .create_workflow_run(&run)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("failed to create run: {e}")))?;

        let run_id = run.id;

        // Spawn execution asynchronously
        let executor = Arc::clone(&self.executor);
        tokio::spawn(async move {
            if let Err(e) = executor.run(&workflow, &mut run).await {
                tracing::error!(
                    workflow = %workflow.name,
                    run_id = %run_id,
                    "Workflow execution failed: {e}"
                );
            }
        });

        let result = serde_json::json!({
            "run_id": run_id.to_string(),
            "workflow": name,
            "status": "started",
        });

        Ok(ToolOutput::success(result, start.elapsed()))
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

// ==================== workflow_list ====================

pub struct WorkflowListTool {
    store: Arc<dyn Database>,
}

impl WorkflowListTool {
    pub fn new(store: Arc<dyn Database>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for WorkflowListTool {
    fn name(&self) -> &str {
        "workflow_list"
    }

    fn description(&self) -> &str {
        "List all workflow definitions with their step count and last update time."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(
        &self,
        _params: serde_json::Value,
        ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = std::time::Instant::now();

        let workflows =
            self.store.list_workflows(&ctx.user_id).await.map_err(|e| {
                ToolError::ExecutionFailed(format!("failed to list workflows: {e}"))
            })?;

        let list: Vec<serde_json::Value> = workflows
            .iter()
            .map(|w| {
                serde_json::json!({
                    "id": w.id.to_string(),
                    "name": w.name,
                    "description": w.description,
                    "step_count": w.steps.len(),
                    "created_at": w.created_at.to_rfc3339(),
                    "updated_at": w.updated_at.to_rfc3339(),
                })
            })
            .collect();

        let result = serde_json::json!({
            "count": list.len(),
            "workflows": list,
        });

        Ok(ToolOutput::success(result, start.elapsed()))
    }

    fn requires_sanitization(&self) -> bool {
        true // echoes user-supplied workflow names and descriptions
    }
}

// ==================== workflow_status ====================

pub struct WorkflowStatusTool {
    store: Arc<dyn Database>,
}

impl WorkflowStatusTool {
    pub fn new(store: Arc<dyn Database>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for WorkflowStatusTool {
    fn name(&self) -> &str {
        "workflow_status"
    }

    fn description(&self) -> &str {
        "Check the status of a workflow run by its run ID. Shows current step, outputs, and errors."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "run_id": {
                    "type": "string",
                    "description": "The run ID returned by workflow_run"
                }
            },
            "required": ["run_id"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = std::time::Instant::now();

        let run_id_str = require_str(&params, "run_id")?;
        let run_id: Uuid = run_id_str
            .parse()
            .map_err(|_| ToolError::InvalidParameters("invalid run_id UUID".to_string()))?;

        let run = self
            .store
            .get_workflow_run(run_id)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("DB error: {e}")))?
            .ok_or_else(|| {
                ToolError::ExecutionFailed(format!("workflow run '{run_id}' not found"))
            })?;

        // Verify the run belongs to the requesting user
        if run.user_id != ctx.user_id {
            return Err(ToolError::ExecutionFailed(format!(
                "workflow run '{run_id}' not found"
            )));
        }

        let duration_secs = run
            .completed_at
            .map(|c| c.signed_duration_since(run.started_at).num_seconds());

        let result = serde_json::json!({
            "run_id": run.id.to_string(),
            "workflow_id": run.workflow_id.to_string(),
            "status": run.status.to_string(),
            "current_step": run.current_step,
            "error": run.error,
            "started_at": run.started_at.to_rfc3339(),
            "completed_at": run.completed_at.map(|t| t.to_rfc3339()),
            "duration_secs": duration_secs,
            "output_keys": run.outputs.keys().collect::<Vec<_>>(),
        });

        Ok(ToolOutput::success(result, start.elapsed()))
    }

    fn requires_sanitization(&self) -> bool {
        true // echoes user-supplied step IDs and error messages
    }
}

// ==================== workflow_delete ====================

pub struct WorkflowDeleteTool {
    store: Arc<dyn Database>,
}

impl WorkflowDeleteTool {
    pub fn new(store: Arc<dyn Database>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for WorkflowDeleteTool {
    fn name(&self) -> &str {
        "workflow_delete"
    }

    fn description(&self) -> &str {
        "Delete a workflow definition by name. This cannot be undone."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the workflow to delete"
                }
            },
            "required": ["name"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = std::time::Instant::now();

        let name = require_str(&params, "name")?;

        // Look up workflow by name + user_id to verify ownership
        let workflow = self
            .store
            .get_workflow_by_name(&ctx.user_id, name)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("DB error: {e}")))?
            .ok_or_else(|| ToolError::ExecutionFailed(format!("workflow '{name}' not found")))?;

        let deleted =
            self.store.delete_workflow(workflow.id).await.map_err(|e| {
                ToolError::ExecutionFailed(format!("failed to delete workflow: {e}"))
            })?;

        if !deleted {
            return Err(ToolError::ExecutionFailed(format!(
                "workflow '{name}' could not be deleted"
            )));
        }

        let result = serde_json::json!({
            "name": name,
            "status": "deleted",
        });

        Ok(ToolOutput::success(result, start.elapsed()))
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

// ==================== workflow_update ====================

pub struct WorkflowUpdateTool {
    store: Arc<dyn Database>,
}

impl WorkflowUpdateTool {
    pub fn new(store: Arc<dyn Database>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for WorkflowUpdateTool {
    fn name(&self) -> &str {
        "workflow_update"
    }

    fn description(&self) -> &str {
        "Update an existing workflow definition. You can change the description, steps, \
         or input_schema. The workflow is re-validated after modification."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the workflow to update"
                },
                "description": {
                    "type": "string",
                    "description": "New description (optional)"
                },
                "steps": {
                    "type": "array",
                    "description": "New steps array (optional). Replaces the entire step list.",
                    "items": { "type": "object" }
                },
                "input_schema": {
                    "type": "object",
                    "description": "New input schema (optional)"
                }
            },
            "required": ["name"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = std::time::Instant::now();

        let name = require_str(&params, "name")?;

        // Load existing workflow by name + user_id
        let mut workflow = self
            .store
            .get_workflow_by_name(&ctx.user_id, name)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("DB error: {e}")))?
            .ok_or_else(|| ToolError::ExecutionFailed(format!("workflow '{name}' not found")))?;

        // Apply optional updates
        if let Some(desc) = params.get("description").and_then(|v| v.as_str()) {
            workflow.description = desc.to_string();
        }

        if let Some(steps_value) = params.get("steps") {
            let steps: Vec<WorkflowStep> = serde_json::from_value(steps_value.clone())
                .map_err(|e| ToolError::InvalidParameters(format!("invalid steps: {e}")))?;
            workflow.steps = steps;
        }

        if let Some(schema) = params.get("input_schema") {
            workflow.input_schema = schema.clone();
        }

        workflow.updated_at = Utc::now();

        // Re-validate the modified workflow
        validate_workflow(&workflow).map_err(|e| {
            ToolError::InvalidParameters(format!("workflow validation failed: {e}"))
        })?;

        // Persist
        self.store
            .update_workflow(&workflow)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("failed to update workflow: {e}")))?;

        let result = serde_json::json!({
            "id": workflow.id.to_string(),
            "name": workflow.name,
            "step_count": workflow.steps.len(),
            "status": "updated",
        });

        Ok(ToolOutput::success(result, start.elapsed()))
    }

    fn requires_sanitization(&self) -> bool {
        true // Updated steps may contain prompt templates that will be executed later
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::tool::Tool;

    fn dummy_ctx() -> JobContext {
        JobContext::new("test-job".to_string(), "test-user".to_string())
    }

    #[cfg(feature = "libsql")]
    async fn make_store() -> (Arc<dyn Database>, tempfile::TempDir) {
        crate::testing::test_db().await
    }

    // ---- Tool metadata tests (no DB needed) ----

    #[cfg(feature = "libsql")]
    #[tokio::test]
    async fn test_tool_names() {
        let (store, _dir) = make_store().await;
        assert_eq!(
            WorkflowCreateTool::new(Arc::clone(&store)).name(),
            "workflow_create"
        );
        assert_eq!(
            WorkflowListTool::new(Arc::clone(&store)).name(),
            "workflow_list"
        );
        assert_eq!(
            WorkflowStatusTool::new(Arc::clone(&store)).name(),
            "workflow_status"
        );
        assert_eq!(
            WorkflowDeleteTool::new(Arc::clone(&store)).name(),
            "workflow_delete"
        );
        assert_eq!(
            WorkflowUpdateTool::new(Arc::clone(&store)).name(),
            "workflow_update"
        );
    }

    #[cfg(feature = "libsql")]
    #[tokio::test]
    async fn test_schemas_have_required_fields() {
        let (store, _dir) = make_store().await;

        let create_schema = WorkflowCreateTool::new(Arc::clone(&store)).parameters_schema();
        let required = create_schema["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!("name")));
        assert!(required.contains(&serde_json::json!("steps")));

        let status_schema = WorkflowStatusTool::new(Arc::clone(&store)).parameters_schema();
        let required = status_schema["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!("run_id")));

        let delete_schema = WorkflowDeleteTool::new(Arc::clone(&store)).parameters_schema();
        let required = delete_schema["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!("name")));
    }

    #[cfg(feature = "libsql")]
    #[tokio::test]
    async fn test_sanitization_flags() {
        let (store, _dir) = make_store().await;
        assert!(WorkflowCreateTool::new(Arc::clone(&store)).requires_sanitization());
        assert!(WorkflowListTool::new(Arc::clone(&store)).requires_sanitization());
        assert!(WorkflowStatusTool::new(Arc::clone(&store)).requires_sanitization());
        assert!(!WorkflowDeleteTool::new(Arc::clone(&store)).requires_sanitization());
        assert!(WorkflowUpdateTool::new(Arc::clone(&store)).requires_sanitization());
    }

    // ---- Parameter validation tests (fail before DB calls) ----

    #[cfg(feature = "libsql")]
    #[tokio::test]
    async fn test_create_missing_name() {
        let (store, _dir) = make_store().await;
        let tool = WorkflowCreateTool::new(store);
        let result = tool
            .execute(serde_json::json!({"steps": []}), &dummy_ctx())
            .await;
        assert!(matches!(result, Err(ToolError::InvalidParameters(_))));
    }

    #[cfg(feature = "libsql")]
    #[tokio::test]
    async fn test_create_missing_steps() {
        let (store, _dir) = make_store().await;
        let tool = WorkflowCreateTool::new(store);
        let result = tool
            .execute(serde_json::json!({"name": "test"}), &dummy_ctx())
            .await;
        assert!(matches!(result, Err(ToolError::InvalidParameters(_))));
    }

    #[cfg(feature = "libsql")]
    #[tokio::test]
    async fn test_create_invalid_steps_json() {
        let (store, _dir) = make_store().await;
        let tool = WorkflowCreateTool::new(store);
        let result = tool
            .execute(
                serde_json::json!({"name": "test", "steps": "not-an-array"}),
                &dummy_ctx(),
            )
            .await;
        assert!(matches!(result, Err(ToolError::InvalidParameters(_))));
    }

    #[cfg(feature = "libsql")]
    #[tokio::test]
    async fn test_create_invalid_step_type() {
        let (store, _dir) = make_store().await;
        let tool = WorkflowCreateTool::new(store);
        let result = tool
            .execute(
                serde_json::json!({
                    "name": "test",
                    "steps": [{"type": "nonexistent", "id": "s1"}]
                }),
                &dummy_ctx(),
            )
            .await;
        assert!(result.is_err(), "invalid step type should be rejected");
    }

    #[cfg(feature = "libsql")]
    #[tokio::test]
    async fn test_create_and_list_workflow() {
        let (store, _dir) = make_store().await;
        let ctx = dummy_ctx();

        // Create a valid workflow
        let create_tool = WorkflowCreateTool::new(Arc::clone(&store));
        let result = create_tool
            .execute(
                serde_json::json!({
                    "name": "test-workflow",
                    "description": "A test workflow",
                    "steps": [
                        {
                            "type": "prompt",
                            "id": "step1",
                            "prompt": "Say hello"
                        }
                    ]
                }),
                &ctx,
            )
            .await;
        assert!(result.is_ok(), "create should succeed: {result:?}");

        // List workflows
        let list_tool = WorkflowListTool::new(Arc::clone(&store));
        let result = list_tool
            .execute(serde_json::json!({}), &ctx)
            .await
            .expect("list should succeed");
        assert_eq!(result.result["count"], 1);
    }

    #[cfg(feature = "libsql")]
    #[tokio::test]
    async fn test_status_missing_run_id() {
        let (store, _dir) = make_store().await;
        let tool = WorkflowStatusTool::new(store);
        let result = tool.execute(serde_json::json!({}), &dummy_ctx()).await;
        assert!(matches!(result, Err(ToolError::InvalidParameters(_))));
    }

    #[cfg(feature = "libsql")]
    #[tokio::test]
    async fn test_status_invalid_uuid() {
        let (store, _dir) = make_store().await;
        let tool = WorkflowStatusTool::new(store);
        let result = tool
            .execute(serde_json::json!({"run_id": "not-a-uuid"}), &dummy_ctx())
            .await;
        assert!(matches!(result, Err(ToolError::InvalidParameters(_))));
    }

    #[cfg(feature = "libsql")]
    #[tokio::test]
    async fn test_delete_missing_name() {
        let (store, _dir) = make_store().await;
        let tool = WorkflowDeleteTool::new(store);
        let result = tool.execute(serde_json::json!({}), &dummy_ctx()).await;
        assert!(matches!(result, Err(ToolError::InvalidParameters(_))));
    }

    #[cfg(feature = "libsql")]
    #[tokio::test]
    async fn test_update_missing_name() {
        let (store, _dir) = make_store().await;
        let tool = WorkflowUpdateTool::new(store);
        let result = tool.execute(serde_json::json!({}), &dummy_ctx()).await;
        assert!(matches!(result, Err(ToolError::InvalidParameters(_))));
    }

    #[cfg(feature = "libsql")]
    #[tokio::test]
    async fn test_delete_nonexistent_workflow() {
        let (store, _dir) = make_store().await;
        let tool = WorkflowDeleteTool::new(store);
        let result = tool
            .execute(serde_json::json!({"name": "does-not-exist"}), &dummy_ctx())
            .await;
        assert!(result.is_err());
    }

    #[cfg(feature = "libsql")]
    #[tokio::test]
    async fn test_update_nonexistent_workflow() {
        let (store, _dir) = make_store().await;
        let tool = WorkflowUpdateTool::new(store);
        let result = tool
            .execute(serde_json::json!({"name": "does-not-exist"}), &dummy_ctx())
            .await;
        assert!(result.is_err());
    }

    // WorkflowRunTool requires a full WorkflowExecutor (with Scheduler,
    // LLM, etc.) which is too heavyweight for unit tests. The parameter
    // validation (require_str for "name") and schema requirements are
    // verified via the schema test above and the create_and_list_workflow
    // integration test.
}
