//! Workflow-related WorkflowStore implementation for LibSqlBackend.

use std::collections::HashMap;

use async_trait::async_trait;
use libsql::params;
use uuid::Uuid;

use crate::agent::workflow::{Workflow, WorkflowRun, WorkflowRunStatus, WorkflowStep};
use crate::db::WorkflowStore;
use crate::db::libsql::{
    LibSqlBackend, fmt_opt_ts, fmt_ts, get_json, get_opt_text, get_opt_ts, get_text, get_ts,
};
use crate::error::DatabaseError;

/// Column list for workflows table.
const WORKFLOW_COLUMNS: &str = "\
    id, name, description, user_id, steps, input_schema, created_at, updated_at";

/// Column list for workflow_runs table.
const WORKFLOW_RUN_COLUMNS: &str = "\
    id, workflow_id, user_id, input, outputs, status, current_step, error, \
    started_at, completed_at, routine_run_id, created_at";

fn row_to_workflow(row: &libsql::Row) -> Result<Workflow, DatabaseError> {
    let steps_json = get_text(row, 4);
    let steps: Vec<WorkflowStep> = serde_json::from_str(&steps_json)
        .map_err(|e| DatabaseError::Serialization(format!("workflow steps: {e}")))?;

    let id_str = get_text(row, 0);
    let id: Uuid = id_str
        .parse()
        .map_err(|e| DatabaseError::Serialization(format!("workflow id '{id_str}': {e}")))?;

    Ok(Workflow {
        id,
        name: get_text(row, 1),
        description: get_text(row, 2),
        user_id: get_text(row, 3),
        steps,
        input_schema: get_json(row, 5),
        created_at: get_ts(row, 6),
        updated_at: get_ts(row, 7),
    })
}

fn row_to_workflow_run(row: &libsql::Row) -> Result<WorkflowRun, DatabaseError> {
    let status_str = get_text(row, 5);
    let status: WorkflowRunStatus = status_str
        .parse()
        .map_err(|e: String| DatabaseError::Serialization(e))?;

    let outputs_json = get_text(row, 4);
    let outputs: HashMap<String, serde_json::Value> = serde_json::from_str(&outputs_json)
        .map_err(|e| DatabaseError::Serialization(format!("workflow run outputs: {e}")))?;

    let id_str = get_text(row, 0);
    let id: Uuid = id_str
        .parse()
        .map_err(|e| DatabaseError::Serialization(format!("workflow run id '{id_str}': {e}")))?;

    let wf_id_str = get_text(row, 1);
    let workflow_id: Uuid = wf_id_str.parse().map_err(|e| {
        DatabaseError::Serialization(format!("workflow run workflow_id '{wf_id_str}': {e}"))
    })?;

    Ok(WorkflowRun {
        id,
        workflow_id,
        user_id: get_text(row, 2),
        input: get_json(row, 3),
        outputs,
        status,
        current_step: get_opt_text(row, 6),
        error: get_opt_text(row, 7),
        started_at: get_ts(row, 8),
        completed_at: get_opt_ts(row, 9),
        routine_run_id: get_opt_text(row, 10).and_then(|s| s.parse().ok()),
    })
}

#[async_trait]
impl WorkflowStore for LibSqlBackend {
    async fn create_workflow(&self, workflow: &Workflow) -> Result<(), DatabaseError> {
        let conn = self.connect().await?;
        let steps_json = serde_json::to_string(&workflow.steps)
            .map_err(|e| DatabaseError::Serialization(e.to_string()))?;

        conn.execute(
            r#"
            INSERT INTO workflows (id, name, description, user_id, steps, input_schema, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
            params![
                workflow.id.to_string(),
                workflow.name.as_str(),
                workflow.description.as_str(),
                workflow.user_id.as_str(),
                steps_json,
                workflow.input_schema.to_string(),
                fmt_ts(&workflow.created_at),
                fmt_ts(&workflow.updated_at),
            ],
        )
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?;
        Ok(())
    }

    async fn get_workflow(&self, id: Uuid) -> Result<Option<Workflow>, DatabaseError> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                &format!("SELECT {WORKFLOW_COLUMNS} FROM workflows WHERE id = ?1"),
                params![id.to_string()],
            )
            .await
            .map_err(|e| DatabaseError::Query(e.to_string()))?;

        match rows
            .next()
            .await
            .map_err(|e| DatabaseError::Query(e.to_string()))?
        {
            Some(row) => Ok(Some(row_to_workflow(&row)?)),
            None => Ok(None),
        }
    }

    async fn get_workflow_by_name(
        &self,
        user_id: &str,
        name: &str,
    ) -> Result<Option<Workflow>, DatabaseError> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                &format!(
                    "SELECT {WORKFLOW_COLUMNS} FROM workflows WHERE user_id = ?1 AND name = ?2"
                ),
                params![user_id, name],
            )
            .await
            .map_err(|e| DatabaseError::Query(e.to_string()))?;

        match rows
            .next()
            .await
            .map_err(|e| DatabaseError::Query(e.to_string()))?
        {
            Some(row) => Ok(Some(row_to_workflow(&row)?)),
            None => Ok(None),
        }
    }

    async fn list_workflows(&self, user_id: &str) -> Result<Vec<Workflow>, DatabaseError> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                &format!(
                    "SELECT {WORKFLOW_COLUMNS} FROM workflows WHERE user_id = ?1 ORDER BY name"
                ),
                params![user_id],
            )
            .await
            .map_err(|e| DatabaseError::Query(e.to_string()))?;

        let mut workflows = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| DatabaseError::Query(e.to_string()))?
        {
            workflows.push(row_to_workflow(&row)?);
        }
        Ok(workflows)
    }

    async fn update_workflow(&self, workflow: &Workflow) -> Result<(), DatabaseError> {
        let conn = self.connect().await?;
        let steps_json = serde_json::to_string(&workflow.steps)
            .map_err(|e| DatabaseError::Serialization(e.to_string()))?;

        // Use datetime('now') for consistency with PostgreSQL's now()
        conn.execute(
            r#"
            UPDATE workflows SET
                name = ?2, description = ?3, steps = ?4,
                input_schema = ?5, updated_at = datetime('now')
            WHERE id = ?1
            "#,
            params![
                workflow.id.to_string(),
                workflow.name.as_str(),
                workflow.description.as_str(),
                steps_json,
                workflow.input_schema.to_string(),
            ],
        )
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?;
        Ok(())
    }

    async fn delete_workflow(&self, id: Uuid) -> Result<bool, DatabaseError> {
        let conn = self.connect().await?;
        let count = conn
            .execute(
                "DELETE FROM workflows WHERE id = ?1",
                params![id.to_string()],
            )
            .await
            .map_err(|e| DatabaseError::Query(e.to_string()))?;
        Ok(count > 0)
    }

    async fn create_workflow_run(&self, run: &WorkflowRun) -> Result<(), DatabaseError> {
        let conn = self.connect().await?;
        let outputs_json = serde_json::to_string(&run.outputs)
            .map_err(|e| DatabaseError::Serialization(e.to_string()))?;

        conn.execute(
            r#"
            INSERT INTO workflow_runs (
                id, workflow_id, user_id, input, outputs, status,
                current_step, error, started_at, completed_at, routine_run_id
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            "#,
            params![
                run.id.to_string(),
                run.workflow_id.to_string(),
                run.user_id.as_str(),
                run.input.to_string(),
                outputs_json,
                run.status.to_string(),
                crate::db::libsql::opt_text(run.current_step.as_deref()),
                crate::db::libsql::opt_text(run.error.as_deref()),
                fmt_ts(&run.started_at),
                fmt_opt_ts(&run.completed_at),
                crate::db::libsql::opt_text_owned(run.routine_run_id.map(|id| id.to_string())),
            ],
        )
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?;
        Ok(())
    }

    async fn get_workflow_run(&self, id: Uuid) -> Result<Option<WorkflowRun>, DatabaseError> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                &format!("SELECT {WORKFLOW_RUN_COLUMNS} FROM workflow_runs WHERE id = ?1"),
                params![id.to_string()],
            )
            .await
            .map_err(|e| DatabaseError::Query(e.to_string()))?;

        match rows
            .next()
            .await
            .map_err(|e| DatabaseError::Query(e.to_string()))?
        {
            Some(row) => Ok(Some(row_to_workflow_run(&row)?)),
            None => Ok(None),
        }
    }

    async fn update_workflow_run(&self, run: &WorkflowRun) -> Result<(), DatabaseError> {
        let conn = self.connect().await?;
        let outputs_json = serde_json::to_string(&run.outputs)
            .map_err(|e| DatabaseError::Serialization(e.to_string()))?;

        conn.execute(
            r#"
            UPDATE workflow_runs SET
                outputs = ?2, status = ?3, current_step = ?4, error = ?5, completed_at = ?6
            WHERE id = ?1
            "#,
            params![
                run.id.to_string(),
                outputs_json,
                run.status.to_string(),
                crate::db::libsql::opt_text(run.current_step.as_deref()),
                crate::db::libsql::opt_text(run.error.as_deref()),
                fmt_opt_ts(&run.completed_at),
            ],
        )
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?;
        Ok(())
    }

    async fn list_workflow_runs(
        &self,
        workflow_id: Uuid,
        limit: i64,
    ) -> Result<Vec<WorkflowRun>, DatabaseError> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                &format!(
                    "SELECT {WORKFLOW_RUN_COLUMNS} FROM workflow_runs WHERE workflow_id = ?1 ORDER BY started_at DESC LIMIT ?2"
                ),
                params![workflow_id.to_string(), limit],
            )
            .await
            .map_err(|e| DatabaseError::Query(e.to_string()))?;

        let mut runs = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| DatabaseError::Query(e.to_string()))?
        {
            runs.push(row_to_workflow_run(&row)?);
        }
        Ok(runs)
    }
}
