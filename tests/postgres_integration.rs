#![cfg(feature = "postgres")]
//! Integration tests for the PostgreSQL database backend using testcontainers.
//!
//! These tests spin up a temporary PostgreSQL container with pgvector,
//! run migrations, and exercise the `Database` trait methods.
//!
//! Requires Docker to be running. Tests are skipped gracefully when Docker
//! is unavailable.

use std::sync::Arc;

use ironclaw::context::{ActionRecord, JobContext};
use ironclaw::db::Database;
use secrecy::SecretString;
use uuid::Uuid;

/// Start a PostgreSQL container with pgvector and return a Database backend.
///
/// Returns `None` if Docker is unavailable.
async fn setup_db() -> Option<Arc<dyn Database>> {
    use testcontainers_modules::postgres;
    use testcontainers_modules::testcontainers::ImageExt;
    use testcontainers_modules::testcontainers::runners::AsyncRunner;

    let container = postgres::Postgres::default()
        .with_name("pgvector/pgvector")
        .with_tag("pg17")
        .start()
        .await;

    let container = match container {
        Ok(c) => c,
        Err(e) => {
            eprintln!("skipping: Docker unavailable ({e})");
            return None;
        }
    };

    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@{}:{}/postgres", host, port);

    let config = ironclaw::config::DatabaseConfig {
        backend: ironclaw::config::DatabaseBackend::Postgres,
        url: SecretString::from(url),
        pool_size: 4,
        libsql_path: None,
        libsql_url: None,
        libsql_auth_token: None,
        libsql_encryption_key: None,
    };

    let db = ironclaw::db::connect_from_config(&config).await;
    match db {
        Ok(db) => {
            // Leak container so it stays alive for the test duration.
            // Box::leak is fine here — the container lives until the process exits.
            Box::leak(Box::new(container));
            Some(db)
        }
        Err(e) => {
            eprintln!("skipping: database setup failed ({e})");
            None
        }
    }
}

// ==================== Conversations ====================

#[tokio::test]
async fn test_pg_conversation_lifecycle() {
    let Some(db) = setup_db().await else { return };

    // Create conversation
    let conv_id = db
        .create_conversation("web", "test-user", Some("thread-1"))
        .await
        .unwrap();
    assert_ne!(conv_id, Uuid::nil());

    // Add messages
    let msg1 = db
        .add_conversation_message(conv_id, "user", "Hello!")
        .await
        .unwrap();
    let msg2 = db
        .add_conversation_message(conv_id, "assistant", "Hi there!")
        .await
        .unwrap();
    assert_ne!(msg1, msg2);

    // List messages
    let messages = db.list_conversation_messages(conv_id).await.unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[1].role, "assistant");

    // Ownership check
    assert!(
        db.conversation_belongs_to_user(conv_id, "test-user")
            .await
            .unwrap()
    );
    assert!(
        !db.conversation_belongs_to_user(conv_id, "other-user")
            .await
            .unwrap()
    );

    // Touch conversation
    db.touch_conversation(conv_id).await.unwrap();

    // List with preview
    let summaries = db
        .list_conversations_with_preview("test-user", "web", 10)
        .await
        .unwrap();
    assert!(!summaries.is_empty());
}

#[tokio::test]
async fn test_pg_conversation_with_metadata() {
    let Some(db) = setup_db().await else { return };

    let metadata = serde_json::json!({"title": "Test Chat", "pinned": true});
    let conv_id = db
        .create_conversation_with_metadata("web", "meta-user", &metadata)
        .await
        .unwrap();

    // Read metadata back
    let stored = db.get_conversation_metadata(conv_id).await.unwrap();
    assert!(stored.is_some());
    let stored = stored.unwrap();
    assert_eq!(stored["title"], "Test Chat");

    // Update a metadata field
    db.update_conversation_metadata_field(conv_id, "pinned", &serde_json::json!(false))
        .await
        .unwrap();

    let updated = db
        .get_conversation_metadata(conv_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated["pinned"], false);
}

#[tokio::test]
async fn test_pg_conversation_pagination() {
    let Some(db) = setup_db().await else { return };

    let conv_id = db
        .create_conversation("web", "page-user", None)
        .await
        .unwrap();

    // Add several messages
    for i in 0..5 {
        db.add_conversation_message(conv_id, "user", &format!("msg-{}", i))
            .await
            .unwrap();
    }

    // Paginate — first page
    let (page1, has_more) = db
        .list_conversation_messages_paginated(conv_id, None, 3)
        .await
        .unwrap();
    assert_eq!(page1.len(), 3);
    assert!(has_more);
}

// ==================== Jobs ====================

#[tokio::test]
async fn test_pg_job_lifecycle() {
    let Some(db) = setup_db().await else { return };

    let mut ctx = JobContext::with_user("job-user", "Test job", "Test job description");
    let job_id = ctx.job_id;
    ctx.conversation_id = Some(
        db.create_conversation("test", "job-user", None)
            .await
            .unwrap(),
    );

    // Save job
    db.save_job(&ctx).await.unwrap();

    // Get job
    let loaded = db.get_job(job_id).await.unwrap();
    assert!(loaded.is_some());
    let loaded = loaded.unwrap();
    assert_eq!(loaded.job_id, job_id);

    // Update status
    db.update_job_status(job_id, ironclaw::context::JobState::InProgress, None)
        .await
        .unwrap();

    // Save action
    let action = ActionRecord::new(0, "echo", serde_json::json!({"text": "hello"}));
    db.save_action(job_id, &action).await.unwrap();

    // Get actions
    let actions = db.get_job_actions(job_id).await.unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].tool_name, "echo");

    // Mark stuck and recover
    db.mark_job_stuck(job_id).await.unwrap();
    let stuck = db.get_stuck_jobs().await.unwrap();
    assert!(stuck.contains(&job_id));
}

// ==================== Settings ====================

#[tokio::test]
async fn test_pg_settings_crud() {
    let Some(db) = setup_db().await else { return };

    let user = "settings-user";

    // Initially no settings
    assert!(!db.has_settings(user).await.unwrap());
    let val = db.get_setting(user, "theme").await.unwrap();
    assert!(val.is_none());

    // Set a setting
    db.set_setting(user, "theme", &serde_json::json!("dark"))
        .await
        .unwrap();

    // Get it back
    let val = db.get_setting(user, "theme").await.unwrap();
    assert_eq!(val, Some(serde_json::json!("dark")));
    assert!(db.has_settings(user).await.unwrap());

    // Set another
    db.set_setting(user, "lang", &serde_json::json!("en"))
        .await
        .unwrap();

    // List all
    let all = db.list_settings(user).await.unwrap();
    assert_eq!(all.len(), 2);

    // Get all as map
    let map = db.get_all_settings(user).await.unwrap();
    assert_eq!(map.get("theme"), Some(&serde_json::json!("dark")));
    assert_eq!(map.get("lang"), Some(&serde_json::json!("en")));

    // Update
    db.set_setting(user, "theme", &serde_json::json!("light"))
        .await
        .unwrap();
    let val = db.get_setting(user, "theme").await.unwrap();
    assert_eq!(val, Some(serde_json::json!("light")));

    // Delete
    let deleted = db.delete_setting(user, "theme").await.unwrap();
    assert!(deleted);
    let val = db.get_setting(user, "theme").await.unwrap();
    assert!(val.is_none());

    // Delete non-existent
    let deleted = db.delete_setting(user, "nonexistent").await.unwrap();
    assert!(!deleted);
}

// ==================== Routines ====================

#[tokio::test]
async fn test_pg_routine_lifecycle() {
    let Some(db) = setup_db().await else { return };

    use ironclaw::agent::routine::{
        NotifyConfig, Routine, RoutineAction, RoutineGuardrails, RoutineRun, RunStatus, Trigger,
    };

    let routine = Routine {
        id: Uuid::new_v4(),
        user_id: "routine-user".to_string(),
        name: "daily-check".to_string(),
        description: "Check things daily".to_string(),
        trigger: Trigger::Cron {
            schedule: "0 9 * * *".to_string(),
        },
        action: RoutineAction::Lightweight {
            prompt: "Check for updates".to_string(),
            context_paths: vec![],
            max_tokens: 4096,
        },
        guardrails: RoutineGuardrails::default(),
        notify: NotifyConfig::default(),
        enabled: true,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        last_run_at: None,
        next_fire_at: None,
        run_count: 0,
        consecutive_failures: 0,
        state: serde_json::Value::Null,
    };

    // Create
    db.create_routine(&routine).await.unwrap();

    // Get by ID
    let loaded = db.get_routine(routine.id).await.unwrap();
    assert!(loaded.is_some());
    assert_eq!(loaded.unwrap().name, "daily-check");

    // Get by name
    let by_name = db
        .get_routine_by_name("routine-user", "daily-check")
        .await
        .unwrap();
    assert!(by_name.is_some());

    // List
    let list = db.list_routines("routine-user").await.unwrap();
    assert_eq!(list.len(), 1);

    // Create a run
    let run = RoutineRun {
        id: Uuid::new_v4(),
        routine_id: routine.id,
        trigger_type: "cron".to_string(),
        trigger_detail: None,
        started_at: chrono::Utc::now(),
        completed_at: None,
        status: RunStatus::Running,
        result_summary: None,
        tokens_used: None,
        job_id: None,
        created_at: chrono::Utc::now(),
    };
    db.create_routine_run(&run).await.unwrap();

    // Count running
    let count = db.count_running_routine_runs(routine.id).await.unwrap();
    assert_eq!(count, 1);

    // Complete the run
    db.complete_routine_run(run.id, RunStatus::Ok, Some("all good"), Some(150))
        .await
        .unwrap();

    // List runs
    let runs = db.list_routine_runs(routine.id, 10).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Ok);

    // Delete routine
    let deleted = db.delete_routine(routine.id).await.unwrap();
    assert!(deleted);
    assert!(db.get_routine(routine.id).await.unwrap().is_none());
}

// ==================== Workflows ====================

#[tokio::test]
async fn test_pg_workflow_lifecycle() {
    let Some(db) = setup_db().await else { return };

    use ironclaw::agent::workflow::{Workflow, WorkflowRun, WorkflowRunStatus, WorkflowStep};

    let wf = Workflow {
        id: Uuid::new_v4(),
        user_id: "wf-user".to_string(),
        name: "deploy-pipeline".to_string(),
        description: "Build and deploy".to_string(),
        steps: vec![WorkflowStep::Tool {
            id: "build".to_string(),
            tool_name: "shell".to_string(),
            params: serde_json::json!({"command": "make build"}),
        }],
        input_schema: serde_json::Value::Null,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };

    // Create
    db.create_workflow(&wf).await.unwrap();

    // Get by ID
    let loaded = db.get_workflow(wf.id).await.unwrap();
    assert!(loaded.is_some());
    assert_eq!(loaded.unwrap().name, "deploy-pipeline");

    // Get by name
    let by_name = db
        .get_workflow_by_name("wf-user", "deploy-pipeline")
        .await
        .unwrap();
    assert!(by_name.is_some());

    // List
    let list = db.list_workflows("wf-user").await.unwrap();
    assert_eq!(list.len(), 1);

    // Update
    let mut updated_wf = wf.clone();
    updated_wf.description = "Build, test, and deploy".to_string();
    db.update_workflow(&updated_wf).await.unwrap();
    let reloaded = db.get_workflow(wf.id).await.unwrap().unwrap();
    assert_eq!(reloaded.description, "Build, test, and deploy");

    // Create a run
    let run = WorkflowRun {
        id: Uuid::new_v4(),
        workflow_id: wf.id,
        user_id: "wf-user".to_string(),
        input: serde_json::Value::Null,
        outputs: Default::default(),
        status: WorkflowRunStatus::Running,
        current_step: Some("build".to_string()),
        error: None,
        started_at: chrono::Utc::now(),
        completed_at: None,
        routine_run_id: None,
    };
    db.create_workflow_run(&run).await.unwrap();

    // Get run
    let loaded_run = db.get_workflow_run(run.id).await.unwrap();
    assert!(loaded_run.is_some());
    assert_eq!(loaded_run.unwrap().status, WorkflowRunStatus::Running);

    // Complete the run
    let mut completed_run = run.clone();
    completed_run.status = WorkflowRunStatus::Completed;
    completed_run.completed_at = Some(chrono::Utc::now());
    db.update_workflow_run(&completed_run).await.unwrap();

    // List runs
    let runs = db.list_workflow_runs(wf.id, 10).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, WorkflowRunStatus::Completed);

    // Delete workflow
    let deleted = db.delete_workflow(wf.id).await.unwrap();
    assert!(deleted);
    assert!(db.get_workflow(wf.id).await.unwrap().is_none());
}

// ==================== Tool Failures ====================

#[tokio::test]
async fn test_pg_tool_failure_tracking() {
    let Some(db) = setup_db().await else { return };

    let tool = "broken_tool";

    // Record failures
    for i in 0..5 {
        db.record_tool_failure(tool, &format!("error {}", i))
            .await
            .unwrap();
    }

    // Check broken tools (threshold of 3)
    let broken = db.get_broken_tools(3).await.unwrap();
    assert!(broken.iter().any(|b| b.name == tool));

    // Mark repaired
    db.mark_tool_repaired(tool).await.unwrap();

    // Should no longer be broken (failure count reset)
    let broken = db.get_broken_tools(3).await.unwrap();
    assert!(!broken.iter().any(|b| b.name == tool));
}

// ==================== Workspace ====================

#[tokio::test]
async fn test_pg_workspace_document_lifecycle() {
    let Some(db) = setup_db().await else { return };

    let user = "ws-user";
    let path = "notes/test.md";

    // Get or create document
    let doc = db
        .get_or_create_document_by_path(user, None, path)
        .await
        .unwrap();
    assert_eq!(doc.path, path);

    // Update content
    db.update_document(doc.id, "# Hello\nThis is a test.")
        .await
        .unwrap();

    // Read back by path
    let loaded = db.get_document_by_path(user, None, path).await.unwrap();
    assert_eq!(loaded.content, "# Hello\nThis is a test.");

    // Read by ID
    let by_id = db.get_document_by_id(doc.id).await.unwrap();
    assert_eq!(by_id.id, doc.id);

    // Insert chunks
    let chunk_id = db.insert_chunk(doc.id, 0, "# Hello", None).await.unwrap();
    assert_ne!(chunk_id, Uuid::nil());
    db.insert_chunk(doc.id, 1, "This is a test.", None)
        .await
        .unwrap();

    // List paths
    let paths = db.list_all_paths(user, None).await.unwrap();
    assert!(paths.contains(&path.to_string()));

    // List directory
    let entries = db.list_directory(user, None, "notes/").await.unwrap();
    assert!(!entries.is_empty());

    // List documents
    let docs = db.list_documents(user, None).await.unwrap();
    assert!(!docs.is_empty());

    // Delete chunks
    db.delete_chunks(doc.id).await.unwrap();

    // Delete document
    db.delete_document_by_path(user, None, path).await.unwrap();
}

#[tokio::test]
async fn test_pg_workspace_fts_search() {
    let Some(db) = setup_db().await else { return };

    let user = "search-user";

    // Create a document with searchable content
    let doc = db
        .get_or_create_document_by_path(user, None, "kb/rust-guide.md")
        .await
        .unwrap();
    db.update_document(doc.id, "Learn Rust programming language")
        .await
        .unwrap();
    db.insert_chunk(
        doc.id,
        0,
        "Rust is a systems programming language focused on safety",
        None,
    )
    .await
    .unwrap();
    db.insert_chunk(
        doc.id,
        1,
        "Cargo is the Rust package manager and build tool",
        None,
    )
    .await
    .unwrap();

    // Search (FTS only, no embeddings)
    use ironclaw::workspace::SearchConfig;
    let config = SearchConfig {
        limit: 10,
        use_fts: true,
        use_vector: false,
        ..Default::default()
    };
    let results = db
        .hybrid_search(user, None, "Rust programming", None, &config)
        .await
        .unwrap();

    assert!(!results.is_empty(), "FTS search should return results");
}
