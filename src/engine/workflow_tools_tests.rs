use super::*;
use crate::tool_registry::ToolDefinition;

#[test]
fn large_project_catalog_remains_bounded_only_at_the_ai_request_boundary() {
    let root = std::env::temp_dir().join(format!("ss-project-budget-{}", id()));
    let app = App::load(root.clone()).unwrap();
    app.state.lock().unwrap().handling.local_projects = (0..80)
        .map(|i| json!({"id":format!("local:{i}"),"name":format!("Synthetic {i}"),"cwd":root}))
        .collect();
    for kind in ["classify", "format", "edit"] {
        let context = app.capture_prepare_context(kind, true, None).unwrap();
        assert_eq!(context.projects.as_array().unwrap().len(), 60);
        assert_eq!(context.projects[0]["id"], "local:ai-workdir");
    }
    assert_eq!(
        app.handling_projects().unwrap().as_array().unwrap().len(),
        81
    );
    drop(app);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn requests_freeze_actual_tools_projects_and_their_own_purpose_route() {
    let root = std::env::temp_dir().join(format!("shishi-prepared-tools-{}", id()));
    let app = App::load(root.clone()).unwrap();
    let mut state = app.state.lock().unwrap().clone();
    state.config.codex.clear();
    state
        .config
        .ai_connections
        .push(crate::ai_backend::Connection {
            id: "synthetic-api".into(),
            name: "Synthetic API".into(),
            driver: "api".into(),
            base_url: "https://example.com/v1".into(),
            ..Default::default()
        });
    state.config.tools = vec![ToolDefinition {
        id: "one".into(),
        name: "synthetic_lookup".into(),
        description: "Synthetic lookup".into(),
        command_template: "echo synthetic_private_template".into(),
        enabled: true,
        ..Default::default()
    }];
    state.config.model_profiles.as_mut().unwrap().analysis = Some(ModelChoice {
        connection_id: "synthetic-api".into(),
        model: "analysis-model".into(),
        reasoning_effort: "".into(),
    });
    state.config.model_profiles.as_mut().unwrap().review = Some(ModelChoice {
        connection_id: "synthetic-api".into(),
        model: "archive-model".into(),
        reasoning_effort: "".into(),
    });
    assert!(state
        .config
        .model_profiles
        .as_ref()
        .unwrap()
        .legacy_tool_call
        .is_none());
    let projects =
        json!([{"id":"codex:source:project","name":"Synthetic project","cwd":"D:/synthetic"}]);
    for kind in ["classify", "format", "edit", "archive"] {
        let mut context = app
            .capture_prepare_context_from(&state, kind, true, projects.clone())
            .unwrap();
        let prepared = app
            .prepare_ai_request_with(
                kind,
                &json!({"_context_frozen":true}),
                AiMode::Manual,
                &mut context,
            )
            .unwrap();
        assert_eq!(prepared.config.selected_connection, "synthetic-api");
        assert_eq!(
            prepared.config.model,
            if kind == "archive" {
                "archive-model"
            } else {
                "analysis-model"
            }
        );
        assert!(prepared.prepared.tools.as_ref().unwrap()["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "synthetic_lookup"));
        assert!(!prepared
            .prepared
            .prompt
            .contains("synthetic_private_template"));
        assert!(!ai::public_payload(&prepared.payload)
            .to_string()
            .contains("command_template"));
        if kind != "archive" {
            assert_eq!(prepared.payload["available_projects"], projects);
        }
    }
    // A single-task operation uses the same global registry, even when the old
    // persisted task allowlist is empty or contains unrelated historical IDs.
    state.tasks.push(Task {
        id: "task".into(),
        title: "Synthetic task".into(),
        ..Default::default()
    });
    *app.state.lock().unwrap() = state;
    let context = app
        .capture_prepare_context("edit", true, Some("task"))
        .unwrap();
    assert_eq!(context.config.tools.len(), 1);
    app.state.lock().unwrap().tasks[0]
        .enabled_tool_ids
        .push("removed-legacy-tool".into());
    let context = app
        .capture_prepare_context("edit", true, Some("task"))
        .unwrap();
    assert_eq!(context.config.tools.len(), 1);
    drop(app);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn one_sentence_edit_saves_the_project_in_the_same_transaction_and_stale_edits_do_not_overwrite_it()
{
    let root = std::env::temp_dir().join(format!("shishi-edit-project-{}", id()));
    let app = App::load(root.clone()).unwrap();
    let task: Task = serde_json::from_value(
        app.task_edit(&json!({"task":{"title":"Synthetic task"}}))
            .unwrap(),
    )
    .unwrap();
    let result = json!({"changes":[{"field":"next_step","value":"Inspect synthetic evidence"}],"tag_changes":[],"reason":"Synthetic edit","suggested_project_id":"synthetic-project","suggested_project_reason":"Matching directory"});
    let edit_audit = app.state.lock().unwrap().audit.len();
    crate::test_support::persistence_failure(&root, true).unwrap();
    assert!(app
        .complete_ai_edit(&task, task.clone(), "Synthetic edit", result.clone())
        .is_err());
    assert!(app.state.lock().unwrap().tasks[0]
        .suggested_project_id
        .is_empty());
    crate::test_support::persistence_failure(&root, false).unwrap();
    let saved = app
        .complete_ai_edit(&task, task.clone(), "Synthetic edit", result.clone())
        .unwrap();
    assert_eq!(saved["suggested_project_id"], "synthetic-project");
    assert!(app
        .complete_ai_edit(&task, task.clone(), "Stale edit", result)
        .is_err());
    assert_eq!(
        app.state.lock().unwrap().tasks[0].suggested_project_id,
        "synthetic-project"
    );
    app.undo(edit_audit).unwrap();
    assert!(app.state.lock().unwrap().tasks[0]
        .suggested_project_id
        .is_empty());
    drop(app);
    let _ = std::fs::remove_dir_all(root);
}
