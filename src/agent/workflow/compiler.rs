//! Workflow validation (compile-time checks before execution).
//!
//! Checks:
//! 1. All step IDs unique (recursively, including inside Parallel/Condition/Loop)
//! 2. Forward-only output key references — `{{key}}` must reference earlier steps or "input"
//! 3. Parallel branch isolation — branches can't reference each other's outputs
//! 4. `max_iterations > 0` for loops
//! 5. Non-empty step list

use std::collections::HashSet;

use crate::agent::workflow::types::{ConditionExpr, Workflow, WorkflowStep};
use crate::error::WorkflowError;

/// Hard cap on loop max_iterations to prevent runaway execution.
/// Individual loops may set a lower value, but never higher.
pub const MAX_LOOP_ITERATIONS_CAP: u32 = 1000;

/// Maximum number of branches allowed in a single Parallel step.
const MAX_PARALLEL_BRANCHES: usize = 32;

/// Maximum nesting depth for recursive compiler validation functions.
/// Prevents stack overflow on pathologically nested workflow definitions.
const MAX_STEP_NESTING_DEPTH: usize = 16;

/// Validate a workflow definition before persisting or executing.
pub fn validate_workflow(workflow: &Workflow) -> Result<(), WorkflowError> {
    if workflow.steps.is_empty() {
        return Err(WorkflowError::Validation {
            reason: "workflow must have at least one step".into(),
        });
    }

    // Check for duplicate IDs and parallel branch limits
    let mut all_ids = HashSet::new();
    collect_step_ids(&workflow.steps, &mut all_ids, 0)?;

    // Validate loop max_iterations > 0 and within cap
    validate_loop_iterations(&workflow.steps, 0)?;

    // Check forward-only references for sequential steps
    let mut available_keys: HashSet<String> = HashSet::new();
    available_keys.insert("input".into());
    validate_step_refs(&workflow.steps, &mut available_keys, 0)?;

    Ok(())
}

/// Recursively collect all step IDs, erroring on duplicates.
///
/// Also validates parallel branch counts and enforces nesting depth limits.
fn collect_step_ids(
    steps: &[WorkflowStep],
    seen: &mut HashSet<String>,
    depth: usize,
) -> Result<(), WorkflowError> {
    if depth >= MAX_STEP_NESTING_DEPTH {
        return Err(WorkflowError::Validation {
            reason: format!(
                "workflow step nesting exceeds maximum depth of {MAX_STEP_NESTING_DEPTH}"
            ),
        });
    }
    for step in steps {
        let id = step.id().to_string();
        if !seen.insert(id.clone()) {
            return Err(WorkflowError::Validation {
                reason: format!("duplicate step ID: '{id}'"),
            });
        }
        match step {
            WorkflowStep::Parallel { id, branches, .. } => {
                if branches.len() > MAX_PARALLEL_BRANCHES {
                    return Err(WorkflowError::Validation {
                        reason: format!(
                            "parallel step '{id}' has {} branches, exceeding the limit of {MAX_PARALLEL_BRANCHES}",
                            branches.len()
                        ),
                    });
                }
                for branch in branches {
                    collect_step_ids(branch, seen, depth + 1)?;
                }
            }
            WorkflowStep::Condition {
                then, otherwise, ..
            } => {
                collect_step_ids(then, seen, depth + 1)?;
                collect_step_ids(otherwise, seen, depth + 1)?;
            }
            WorkflowStep::Loop { steps: inner, .. } => {
                collect_step_ids(inner, seen, depth + 1)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Recursively check that all Loop steps have max_iterations > 0 and within cap.
fn validate_loop_iterations(steps: &[WorkflowStep], depth: usize) -> Result<(), WorkflowError> {
    if depth >= MAX_STEP_NESTING_DEPTH {
        return Err(WorkflowError::Validation {
            reason: format!(
                "workflow step nesting exceeds maximum depth of {MAX_STEP_NESTING_DEPTH}"
            ),
        });
    }
    for step in steps {
        match step {
            WorkflowStep::Loop {
                id,
                steps: inner,
                max_iterations,
                ..
            } => {
                if *max_iterations == 0 {
                    return Err(WorkflowError::Validation {
                        reason: format!(
                            "loop step '{id}' has max_iterations=0; must be at least 1"
                        ),
                    });
                }
                if *max_iterations > MAX_LOOP_ITERATIONS_CAP {
                    return Err(WorkflowError::Validation {
                        reason: format!(
                            "loop step '{id}' has max_iterations={max_iterations}; \
                             hard cap is {MAX_LOOP_ITERATIONS_CAP}"
                        ),
                    });
                }
                validate_loop_iterations(inner, depth + 1)?;
            }
            WorkflowStep::Parallel { branches, .. } => {
                for branch in branches {
                    validate_loop_iterations(branch, depth + 1)?;
                }
            }
            WorkflowStep::Condition {
                then, otherwise, ..
            } => {
                validate_loop_iterations(then, depth + 1)?;
                validate_loop_iterations(otherwise, depth + 1)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Validate that template references in steps only reference earlier steps or "input".
fn validate_step_refs(
    steps: &[WorkflowStep],
    available: &mut HashSet<String>,
    depth: usize,
) -> Result<(), WorkflowError> {
    if depth >= MAX_STEP_NESTING_DEPTH {
        return Err(WorkflowError::Validation {
            reason: format!(
                "workflow step nesting exceeds maximum depth of {MAX_STEP_NESTING_DEPTH}"
            ),
        });
    }
    for step in steps {
        // Collect template refs from this step and check they're all available
        let refs = collect_template_refs_for_step(step);
        for r in &refs {
            if !available.contains(r) {
                return Err(WorkflowError::Validation {
                    reason: format!(
                        "step '{}' references '{{{{{}}}}}' which is not defined by an earlier step",
                        step.id(),
                        r
                    ),
                });
            }
        }

        match step {
            WorkflowStep::Parallel { branches, .. } => {
                // Each branch sees the current available keys but not sibling branches
                let mut branch_outputs = Vec::new();
                for branch in branches {
                    let mut branch_available = available.clone();
                    validate_step_refs(branch, &mut branch_available, depth + 1)?;
                    // Collect new keys added by this branch
                    let new_keys: HashSet<String> =
                        branch_available.difference(available).cloned().collect();
                    branch_outputs.push(new_keys);
                }
                // After parallel, all branch outputs become available
                for keys in branch_outputs {
                    available.extend(keys);
                }
            }
            WorkflowStep::Condition {
                then, otherwise, ..
            } => {
                // Both branches see the current available keys
                let mut then_available = available.clone();
                validate_step_refs(then, &mut then_available, depth + 1)?;
                let mut else_available = available.clone();
                validate_step_refs(otherwise, &mut else_available, depth + 1)?;
                // Only keys produced by both branches are guaranteed available after
                let then_new: HashSet<String> =
                    then_available.difference(available).cloned().collect();
                let else_new: HashSet<String> =
                    else_available.difference(available).cloned().collect();
                let both: HashSet<String> = then_new.intersection(&else_new).cloned().collect();
                available.extend(both);
            }
            WorkflowStep::Loop { steps: inner, .. } => {
                // Validate the loop body as a sequential block.
                let mut loop_available = available.clone();
                validate_step_refs(inner, &mut loop_available, depth + 1)?;
                // Loop outputs are available after
                available.extend(loop_available);
            }
            _ => {}
        }

        // After processing, this step's output is available to subsequent steps
        available.insert(step.id().to_string());
    }
    Ok(())
}

/// Collect all `{{key}}` references from a step's template strings.
fn collect_template_refs_for_step(step: &WorkflowStep) -> HashSet<String> {
    let mut refs = HashSet::new();
    match step {
        WorkflowStep::Prompt { prompt, .. } => {
            collect_template_refs(prompt, &mut refs);
        }
        WorkflowStep::Tool { params, .. } => {
            collect_template_refs_from_value(params, &mut refs);
        }
        WorkflowStep::Job {
            title, description, ..
        } => {
            collect_template_refs(title, &mut refs);
            collect_template_refs(description, &mut refs);
        }
        WorkflowStep::Condition { condition, .. } => {
            collect_condition_refs(condition, &mut refs);
        }
        WorkflowStep::Loop { exit_condition, .. } => {
            collect_condition_refs(exit_condition, &mut refs);
        }
        WorkflowStep::Parallel { .. } => {}
    }
    refs
}

/// Parse `{{key}}` patterns from a string, extracting the top-level key.
pub(crate) fn collect_template_refs(s: &str, refs: &mut HashSet<String>) {
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        let after_open = &rest[start + 2..];
        if let Some(end) = after_open.find("}}") {
            let key = after_open[..end].trim();
            // Extract top-level key (before any dot)
            let top_key = key.split('.').next().unwrap_or(key);
            if !top_key.is_empty() {
                refs.insert(top_key.to_string());
            }
            rest = &after_open[end + 2..];
        } else {
            break;
        }
    }
}

/// Recursively collect template refs from a JSON value.
fn collect_template_refs_from_value(value: &serde_json::Value, refs: &mut HashSet<String>) {
    match value {
        serde_json::Value::String(s) => collect_template_refs(s, refs),
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_template_refs_from_value(v, refs);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values() {
                collect_template_refs_from_value(v, refs);
            }
        }
        _ => {}
    }
}

/// Collect template refs from a condition expression.
fn collect_condition_refs(cond: &ConditionExpr, refs: &mut HashSet<String>) {
    match cond {
        ConditionExpr::Equals { key, .. }
        | ConditionExpr::NotEmpty { key }
        | ConditionExpr::Contains { key, .. } => {
            let top = key.split('.').next().unwrap_or(key);
            if !top.is_empty() {
                refs.insert(top.to_string());
            }
        }
        ConditionExpr::And { conditions } | ConditionExpr::Or { conditions } => {
            for c in conditions {
                collect_condition_refs(c, refs);
            }
        }
        ConditionExpr::Not { condition } => {
            collect_condition_refs(condition, refs);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use chrono::Utc;

    use crate::agent::workflow::compiler::{collect_template_refs, validate_workflow};
    use crate::agent::workflow::types::{Workflow, WorkflowStep};

    fn make_workflow(steps: Vec<WorkflowStep>) -> Workflow {
        Workflow {
            id: uuid::Uuid::new_v4(),
            name: "test".into(),
            description: "".into(),
            user_id: "u1".into(),
            steps,
            input_schema: serde_json::json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn test_empty_workflow_rejected() {
        let w = make_workflow(vec![]);
        assert!(validate_workflow(&w).is_err());
    }

    #[test]
    fn test_valid_workflow() {
        let w = make_workflow(vec![
            WorkflowStep::Prompt {
                id: "s1".into(),
                prompt: "Hello {{input}}".into(),
                max_tokens: 1024,
            },
            WorkflowStep::Tool {
                id: "s2".into(),
                tool_name: "echo".into(),
                params: serde_json::json!({"text": "{{s1}}"}),
            },
        ]);
        assert!(validate_workflow(&w).is_ok());
    }

    #[test]
    fn test_duplicate_ids_rejected() {
        let w = make_workflow(vec![
            WorkflowStep::Prompt {
                id: "same".into(),
                prompt: "a".into(),
                max_tokens: 1024,
            },
            WorkflowStep::Prompt {
                id: "same".into(),
                prompt: "b".into(),
                max_tokens: 1024,
            },
        ]);
        let err = validate_workflow(&w).unwrap_err();
        assert!(err.to_string().contains("duplicate step ID"));
    }

    #[test]
    fn test_forward_ref_rejected() {
        let w = make_workflow(vec![
            WorkflowStep::Prompt {
                id: "s1".into(),
                prompt: "uses {{s2}} which is not yet defined".into(),
                max_tokens: 1024,
            },
            WorkflowStep::Prompt {
                id: "s2".into(),
                prompt: "ok".into(),
                max_tokens: 1024,
            },
        ]);
        let err = validate_workflow(&w).unwrap_err();
        assert!(err.to_string().contains("s2"));
    }

    #[test]
    fn test_collect_template_refs() {
        let mut refs = HashSet::new();
        collect_template_refs("Hello {{input.text}}, result is {{step1}}", &mut refs);
        assert!(refs.contains("input"));
        assert!(refs.contains("step1"));
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn test_duplicate_ids_in_parallel_rejected() {
        let w = make_workflow(vec![WorkflowStep::Parallel {
            id: "fan".into(),
            branches: vec![
                vec![WorkflowStep::Prompt {
                    id: "dup".into(),
                    prompt: "a".into(),
                    max_tokens: 1024,
                }],
                vec![WorkflowStep::Prompt {
                    id: "dup".into(),
                    prompt: "b".into(),
                    max_tokens: 1024,
                }],
            ],
        }]);
        let err = validate_workflow(&w).unwrap_err();
        assert!(err.to_string().contains("duplicate step ID"));
    }

    #[test]
    fn test_input_ref_valid() {
        let w = make_workflow(vec![WorkflowStep::Prompt {
            id: "s1".into(),
            prompt: "Process {{input}}".into(),
            max_tokens: 1024,
        }]);
        assert!(validate_workflow(&w).is_ok());
    }

    #[test]
    fn test_loop_zero_iterations_rejected() {
        use crate::agent::workflow::types::ConditionExpr;
        let w = make_workflow(vec![WorkflowStep::Loop {
            id: "loop1".into(),
            steps: vec![WorkflowStep::Prompt {
                id: "inner".into(),
                prompt: "iterate".into(),
                max_tokens: 1024,
            }],
            exit_condition: ConditionExpr::NotEmpty {
                key: "inner".into(),
            },
            max_iterations: 0,
        }]);
        let err = validate_workflow(&w).unwrap_err();
        assert!(err.to_string().contains("max_iterations=0"));
    }

    #[test]
    fn test_parallel_branch_limit_exceeded() {
        // Build a parallel step with 33 branches (exceeds MAX_PARALLEL_BRANCHES=32)
        let branches: Vec<Vec<WorkflowStep>> = (0..33)
            .map(|i| {
                vec![WorkflowStep::Prompt {
                    id: format!("b{i}"),
                    prompt: "hi".into(),
                    max_tokens: 1024,
                }]
            })
            .collect();
        let w = make_workflow(vec![WorkflowStep::Parallel {
            id: "fan".into(),
            branches,
        }]);
        let err = validate_workflow(&w).unwrap_err();
        assert!(err.to_string().contains("exceeding the limit of 32"));
    }

    #[test]
    fn test_parallel_branch_limit_at_boundary() {
        // 32 branches should be accepted
        let branches: Vec<Vec<WorkflowStep>> = (0..32)
            .map(|i| {
                vec![WorkflowStep::Prompt {
                    id: format!("b{i}"),
                    prompt: "hi".into(),
                    max_tokens: 1024,
                }]
            })
            .collect();
        let w = make_workflow(vec![WorkflowStep::Parallel {
            id: "fan".into(),
            branches,
        }]);
        assert!(validate_workflow(&w).is_ok());
    }

    #[test]
    fn test_nesting_depth_limit_exceeded() {
        use crate::agent::workflow::types::ConditionExpr;
        // Build a deeply nested workflow: Loop(Loop(Loop(...))) 17 levels deep
        let mut inner = vec![WorkflowStep::Prompt {
            id: "leaf".into(),
            prompt: "hi".into(),
            max_tokens: 1024,
        }];
        for i in (0..17).rev() {
            inner = vec![WorkflowStep::Loop {
                id: format!("loop{i}"),
                steps: inner,
                exit_condition: ConditionExpr::NotEmpty {
                    key: "input".into(),
                },
                max_iterations: 1,
            }];
        }
        let w = make_workflow(inner);
        let err = validate_workflow(&w).unwrap_err();
        assert!(err.to_string().contains("nesting exceeds maximum depth"));
    }

    #[test]
    fn test_nesting_depth_at_boundary() {
        use crate::agent::workflow::types::ConditionExpr;
        // 15 loops wrapping a leaf prompt: deepest recursive call processes the
        // leaf at depth 15, which is below MAX_STEP_NESTING_DEPTH (16). Should succeed.
        let mut inner = vec![WorkflowStep::Prompt {
            id: "leaf".into(),
            prompt: "hi".into(),
            max_tokens: 1024,
        }];
        for i in (0..15).rev() {
            inner = vec![WorkflowStep::Loop {
                id: format!("loop{i}"),
                steps: inner,
                exit_condition: ConditionExpr::NotEmpty {
                    key: "input".into(),
                },
                max_iterations: 1,
            }];
        }
        let w = make_workflow(inner);
        assert!(validate_workflow(&w).is_ok());
    }
}
