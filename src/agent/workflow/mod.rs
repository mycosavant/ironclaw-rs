//! Multi-step workflow engine.
//!
//! Provides sequential steps, parallel fan-out, conditional branching, loops,
//! and data flow between steps via `{{key}}` template substitution.
//! Integrates with routines through `RoutineAction::Workflow`.

pub mod compiler;
pub mod executor;
pub mod types;

pub use compiler::validate_workflow;
pub use executor::WorkflowExecutor;
pub use types::{
    ConditionExpr, Workflow, WorkflowOutput, WorkflowRun, WorkflowRunStatus, WorkflowStep,
};
