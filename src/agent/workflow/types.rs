//! Core types for the workflow engine.
//!
//! A workflow is a multi-step execution plan with sequential steps, parallel
//! fan-out, conditional branching, loops, and data flow between steps via
//! `{{key}}` template substitution.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A workflow definition: a named sequence of steps that can be persisted and re-run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub user_id: String,
    pub steps: Vec<WorkflowStep>,
    /// JSON Schema for the workflow input (optional).
    #[serde(default)]
    pub input_schema: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A single step in a workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowStep {
    /// Single LLM prompt. Output is the LLM response text.
    Prompt {
        id: String,
        prompt: String,
        #[serde(default = "default_max_tokens")]
        max_tokens: u32,
    },
    /// Execute a registered tool.
    Tool {
        id: String,
        tool_name: String,
        #[serde(default)]
        params: serde_json::Value,
    },
    /// Run branches in parallel, merge outputs.
    Parallel {
        id: String,
        branches: Vec<Vec<WorkflowStep>>,
    },
    /// Conditional execution: if condition is true run `then`, else run `otherwise`.
    Condition {
        id: String,
        condition: ConditionExpr,
        then: Vec<WorkflowStep>,
        #[serde(default)]
        otherwise: Vec<WorkflowStep>,
    },
    /// Loop over steps until exit condition is met or max iterations reached.
    Loop {
        id: String,
        steps: Vec<WorkflowStep>,
        exit_condition: ConditionExpr,
        #[serde(default = "default_max_loop_iterations")]
        max_iterations: u32,
    },
    /// Fire-and-forget: dispatch a full job to the scheduler.
    Job {
        id: String,
        title: String,
        description: String,
        #[serde(default = "default_job_max_iterations")]
        max_iterations: u32,
    },
}

impl WorkflowStep {
    /// Get the step ID.
    pub fn id(&self) -> &str {
        match self {
            WorkflowStep::Prompt { id, .. }
            | WorkflowStep::Tool { id, .. }
            | WorkflowStep::Parallel { id, .. }
            | WorkflowStep::Condition { id, .. }
            | WorkflowStep::Loop { id, .. }
            | WorkflowStep::Job { id, .. } => id,
        }
    }
}

/// Condition expression for branching and loop exit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ConditionExpr {
    /// `{{key}}` equals a literal value.
    Equals {
        key: String,
        value: serde_json::Value,
    },
    /// `{{key}}` is not empty/null.
    NotEmpty { key: String },
    /// `{{key}}` contains a substring.
    Contains { key: String, substring: String },
    /// All sub-conditions must be true.
    And { conditions: Vec<ConditionExpr> },
    /// At least one sub-condition must be true.
    Or { conditions: Vec<ConditionExpr> },
    /// Negate a sub-condition.
    Not { condition: Box<ConditionExpr> },
}

/// The accumulated outputs of all completed steps.
pub type WorkflowOutput = HashMap<String, serde_json::Value>;

/// A single execution of a workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub id: Uuid,
    pub workflow_id: Uuid,
    pub user_id: String,
    pub input: serde_json::Value,
    pub outputs: WorkflowOutput,
    pub status: WorkflowRunStatus,
    pub current_step: Option<String>,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    /// Optional link to a routine run that triggered this workflow.
    pub routine_run_id: Option<Uuid>,
}

/// Status of a workflow run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl std::fmt::Display for WorkflowRunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running => write!(f, "running"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::str::FromStr for WorkflowRunStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(format!("unknown workflow run status: {other}")),
        }
    }
}

fn default_max_tokens() -> u32 {
    4096
}

fn default_max_loop_iterations() -> u32 {
    10
}

fn default_job_max_iterations() -> u32 {
    10
}

#[cfg(test)]
mod tests {
    use crate::agent::workflow::types::{
        ConditionExpr, WorkflowRunStatus, WorkflowStep, default_job_max_iterations,
        default_max_loop_iterations, default_max_tokens,
    };

    #[test]
    fn test_workflow_step_serde_roundtrip_prompt() {
        let step = WorkflowStep::Prompt {
            id: "step1".into(),
            prompt: "Summarize {{input.text}}".into(),
            max_tokens: 2048,
        };
        let json = serde_json::to_string(&step).expect("serialize");
        let parsed: WorkflowStep = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.id(), "step1");
    }

    #[test]
    fn test_workflow_step_serde_roundtrip_tool() {
        let step = WorkflowStep::Tool {
            id: "fetch".into(),
            tool_name: "http".into(),
            params: serde_json::json!({"url": "https://example.com"}),
        };
        let json = serde_json::to_string(&step).expect("serialize");
        let parsed: WorkflowStep = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.id(), "fetch");
    }

    #[test]
    fn test_workflow_step_serde_roundtrip_parallel() {
        let step = WorkflowStep::Parallel {
            id: "fan".into(),
            branches: vec![
                vec![WorkflowStep::Prompt {
                    id: "b1".into(),
                    prompt: "branch 1".into(),
                    max_tokens: 1024,
                }],
                vec![WorkflowStep::Prompt {
                    id: "b2".into(),
                    prompt: "branch 2".into(),
                    max_tokens: 1024,
                }],
            ],
        };
        let json = serde_json::to_string(&step).expect("serialize");
        let parsed: WorkflowStep = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.id(), "fan");
    }

    #[test]
    fn test_workflow_step_serde_roundtrip_condition() {
        let step = WorkflowStep::Condition {
            id: "check".into(),
            condition: ConditionExpr::NotEmpty {
                key: "step1".into(),
            },
            then: vec![WorkflowStep::Prompt {
                id: "yes".into(),
                prompt: "proceed".into(),
                max_tokens: 1024,
            }],
            otherwise: vec![],
        };
        let json = serde_json::to_string(&step).expect("serialize");
        let parsed: WorkflowStep = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.id(), "check");
    }

    #[test]
    fn test_workflow_step_serde_roundtrip_loop() {
        let step = WorkflowStep::Loop {
            id: "retry".into(),
            steps: vec![WorkflowStep::Tool {
                id: "call".into(),
                tool_name: "http".into(),
                params: serde_json::json!({}),
            }],
            exit_condition: ConditionExpr::Equals {
                key: "call".into(),
                value: serde_json::json!("done"),
            },
            max_iterations: 5,
        };
        let json = serde_json::to_string(&step).expect("serialize");
        let parsed: WorkflowStep = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.id(), "retry");
    }

    #[test]
    fn test_condition_serde_roundtrip() {
        let cond = ConditionExpr::And {
            conditions: vec![
                ConditionExpr::NotEmpty {
                    key: "step1".into(),
                },
                ConditionExpr::Not {
                    condition: Box::new(ConditionExpr::Contains {
                        key: "step2".into(),
                        substring: "error".into(),
                    }),
                },
            ],
        };
        let json = serde_json::to_string(&cond).expect("serialize");
        let parsed: ConditionExpr = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(parsed, ConditionExpr::And { conditions } if conditions.len() == 2));
    }

    #[test]
    fn test_workflow_run_status_display_parse() {
        for status in [
            WorkflowRunStatus::Running,
            WorkflowRunStatus::Completed,
            WorkflowRunStatus::Failed,
            WorkflowRunStatus::Cancelled,
        ] {
            let s = status.to_string();
            let parsed: WorkflowRunStatus = s.parse().expect("parse status");
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn test_workflow_step_serde_roundtrip_job() {
        let step = WorkflowStep::Job {
            id: "deploy".into(),
            title: "Deploy app".into(),
            description: "Deploy the application to production".into(),
            max_iterations: 20,
        };
        let json = serde_json::to_string(&step).expect("serialize");
        let parsed: WorkflowStep = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.id(), "deploy");
    }

    #[test]
    fn test_default_values() {
        assert_eq!(default_max_tokens(), 4096);
        assert_eq!(default_max_loop_iterations(), 10);
        assert_eq!(default_job_max_iterations(), 10);
    }
}
