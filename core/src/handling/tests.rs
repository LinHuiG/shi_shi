//! Synthetic data only: an in-memory database, files under a test
//! directory and scripted agents.

use std::sync::mpsc;
use std::time::Duration;

use super::agent::fake::{Backend, Fake};
use super::*;
use crate::config::ModelChoice;
use crate::testing::temp_dir;

const TASK: &str = "task-h";

async fn setup() -> (Arc<Ctx>, Arc<Fake>) {
    let mut config = Config { codex: "/synthetic/codex".into(), codex_name: "Codex".into(), ..Default::default() };
    config.codex_models = vec![json!({"model": "m1"}), json!({"model": "m2"})];
    config.model_profiles.get_or_insert_with(Default::default).handling =
        Some(ModelChoice { model: "m1".into(), reasoning_effort: "medium".into(), ..Default::default() });
    let ctx = Ctx::memory_in(config, temp_dir("handling")).unwrap();
    for dir in ["w", "work"] {
        std::fs::create_dir_all(ctx.root.join(dir)).unwrap();
    }
    ctx.db
        .test_write(|tx| {
            tx.execute(
                "INSERT INTO tasks(id, number, title, status, status_changed_at, created, updated)
                 VALUES (?1, 1, '合成事项', '待处理', 0, 0, 0)",
                [TASK],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let fake = Arc::new(Fake::default());
    ctx.handling.set_backends(Arc::new(Backend(fake.clone())));
    (ctx, fake)
}

async fn call(ctx: &Arc<Ctx>, method: &str, p: Value) -> Result<Value> {
    handle(ctx, method, p).await
}

/// The run once its turn has ended.
async fn settled(ctx: &Arc<Ctx>, id: &str) -> Run {
    for _ in 0..500 {
        if ctx.handling.live(id).is_none() {
            return ctx.db.handling_run(id.into()).await.unwrap().unwrap();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("turn did not end");
}

#[tokio::test]
async fn prepare_offers_conversation_connections_with_the_settings_model() {
    let (ctx, _) = setup().await;
    let p = call(&ctx, "handling_prepare", json!({"task_id": TASK})).await.unwrap();
    assert_eq!(p["connections"][0]["id"], DEFAULT_CODEX);
    assert_eq!(p["connections"][0]["models"].as_array().unwrap().len(), 2);
    assert_eq!(p["default"], json!({"connection_id": DEFAULT_CODEX, "model": "m1", "effort": "medium"}));
    assert!(p.get("projects").is_none(), "projects are read per connection");
    let _ = std::fs::remove_dir_all(&ctx.root);
}

#[tokio::test]
async fn projects_come_only_from_the_chosen_connections_client() {
    let (ctx, _) = setup().await;
    let dsh = Connection { id: "c-dsh".into(), driver: "dsh".into(), ..Default::default() };
    ctx.config.update(&ctx.db, |c| Ok(c.ai_connections.push(dsh))).await.unwrap();
    let ids = |v: Value| v.as_array().unwrap().iter().map(|p| p["id"].clone()).collect::<Vec<_>>();
    let codex = call(&ctx, "handling_projects", json!({"connection_id": DEFAULT_CODEX})).await.unwrap();
    assert_eq!(ids(codex), [json!("codex:p")]);
    let dsh = call(&ctx, "handling_projects", json!({"connection_id": "c-dsh"})).await.unwrap();
    assert_eq!(ids(dsh), [json!("dsh:p")]);
    assert!(call(&ctx, "handling_projects", json!({"connection_id": "missing"})).await.is_err());
    let prepared = call(&ctx, "handling_prepare", json!({"task_id": TASK})).await.unwrap();
    let drivers = prepared["connections"].as_array().unwrap().iter().map(|c| c["driver"].clone()).collect::<Vec<_>>();
    assert_eq!(prepared["default"]["connection_id"], "c-dsh");
    assert_eq!(drivers, [json!("dsh")], "only the settings' client is offered");
    let default = prepared["default"].clone();
    let shown = call(&ctx, "handling_projects", default).await.unwrap();
    assert_eq!(offered(&ctx).await, shown, "analysis suggests from the dialog's default client");
    let _ = std::fs::remove_dir_all(&ctx.root);
}

#[tokio::test]
async fn the_session_is_handed_over_only_after_the_first_turn() {
    let (ctx, fake) = setup().await;
    fake.script.lock().unwrap().push(("th-1", Ok("completed")));
    let cwd = ctx.root.join("work");
    let started = call(
        &ctx,
        "handling_start",
        json!({"task_id": TASK, "cwd": cwd, "project_name": "工作",
        "model": "m2", "effort": "high", "instructions": "先查报销"}),
    )
    .await
    .unwrap();
    let id = started["run_id"].as_str().unwrap().to_string();
    let run = settled(&ctx, &id).await;
    assert_eq!((run.status.as_str(), run.thread_id.as_str(), run.model.as_str()), ("completed", "th-1", "m2"));
    assert!(fake.opened.lock().unwrap().is_empty(), "nothing writes the session alongside the first turn");
    assert_eq!(ctx.config.get().model_choice("handling").model, "m1");
    let (text, at, model, effort) = fake.turns.lock().unwrap()[0].clone();
    assert_eq!((at.as_str(), model.as_str(), effort.as_str()), (cwd.to_str().unwrap(), "m2", "high"));
    assert!(text.contains("先查报销") && text.contains("合成事项"));
    assert!(text.contains(&format!("tasks get '{TASK}'")) && text.contains("call action"), "{text}");
    // The CLI takes the installation root, never the data directory, and
    // PowerShell waits for the GUI-subsystem program only when it reads the output.
    let install = ctx.host.install_root();
    assert!(text.contains(&format!("--root '{}'", install.display())), "{text}");
    assert!(!text.contains(&format!("--root '{}'", ctx.root.display())), "{text}");
    assert!(text.split("& '").skip(1).all(|c| c.lines().next().unwrap().contains("| Out-String")), "{text}");
    // A sandboxed client asks once for escalation instead of giving up silently.
    assert!(text.contains("10013") && text.contains("提升权限申请一次"), "{text}");
    let (calls, logs) = ctx
        .db
        .test_read(|c| {
            let calls: i64 = c.query_row("SELECT calls FROM usage_daily WHERE purpose = 'main'", [], |r| r.get(0))?;
            let logs: i64 =
                c.query_row("SELECT count(*) FROM activity WHERE kind = 'handling_round'", [], |r| r.get(0))?;
            Ok((calls, logs))
        })
        .await
        .unwrap();
    assert_eq!((calls, logs), (1, 1));
    call(&ctx, "handling_open", json!({"run_id": id})).await.unwrap();
    assert_eq!(*fake.opened.lock().unwrap(), ["th-1"]);
    let _ = std::fs::remove_dir_all(&ctx.root);
}

#[tokio::test]
async fn a_running_first_turn_blocks_a_second_start_and_dsh_is_raised_with_its_project() {
    let (ctx, fake) = setup().await;
    let dsh = Connection { id: "c-dsh".into(), driver: "dsh".into(), ..Default::default() };
    ctx.config.update(&ctx.db, |c| Ok(c.ai_connections.push(dsh))).await.unwrap();
    fake.script.lock().unwrap().push(("sess", Ok("completed")));
    let (release, hold) = mpsc::channel();
    *fake.hold.lock().unwrap() = Some(hold);
    let start =
        json!({"task_id": TASK, "cwd": ctx.root.join("w"), "connection_id": "c-dsh", "project_name": "合成项目"});
    let id = call(&ctx, "handling_start", start.clone()).await.unwrap()["run_id"].as_str().unwrap().to_string();
    let again = call(&ctx, "handling_start", start).await.unwrap_err();
    assert_eq!(again.0, "该事项的处置正在启动，请稍候");
    while ctx.handling.live(&id).is_some_and(|r| r.thread_id.is_empty()) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let busy = call(&ctx, "handling_open", json!({"run_id": id})).await.unwrap_err();
    assert_eq!(busy.0, "首轮进行中，结束后即可继续");
    release.send(()).unwrap();
    let run = settled(&ctx, &id).await;
    assert_eq!((run.status.as_str(), run.thread_id.as_str()), ("completed", "sess"));
    assert!(fake.opened.lock().unwrap().is_empty(), "DSH takes the session over in its own client");
    // dsh://open only raises DSH, so the reply names the project and session.
    let opened = call(&ctx, "handling_open", json!({"run_id": id})).await.unwrap();
    assert_eq!(opened["hint"], "已唤起 DSH，请在「合成项目」项目中打开会话 sess");
    assert_eq!(*fake.opened.lock().unwrap(), ["sess"]);
    let missing = json!({"task_id": TASK, "cwd": ctx.root.join("missing")});
    assert!(call(&ctx, "handling_start", missing).await.unwrap_err().0.contains("项目目录不存在"));
    let _ = std::fs::remove_dir_all(&ctx.root);
}

#[tokio::test]
async fn a_failed_turn_keeps_its_error_and_a_restart_interrupts_running_rows() {
    let (ctx, fake) = setup().await;
    fake.script.lock().unwrap().push(("", Err("模型不可用".into())));
    let id = call(&ctx, "handling_start", json!({"task_id": TASK, "cwd": ctx.root.join("w")})).await.unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let run = settled(&ctx, &id).await;
    assert_eq!((run.status.as_str(), run.error.as_str()), ("failed", "模型不可用"));
    assert_eq!(call(&ctx, "handling_open", json!({"run_id": id})).await.unwrap_err().0, "会话尚未建立，请稍候");
    let mut running = run.clone();
    running.status = "running".into();
    ctx.db.save_handling_run(running).await.unwrap();
    ctx.db.interrupt_handling_runs().await.unwrap();
    assert_eq!(ctx.db.handling_run(id).await.unwrap().unwrap().status, "interrupted");
    let _ = std::fs::remove_dir_all(&ctx.root);
}

#[tokio::test]
async fn the_snapshot_carries_the_latest_run_head() {
    let (ctx, fake) = setup().await;
    fake.script.lock().unwrap().push(("th", Ok("completed")));
    let id = call(&ctx, "handling_start", json!({"task_id": TASK, "cwd": ctx.root.join("w"), "project_name": "工作"}))
        .await
        .unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    settled(&ctx, &id).await;
    let list = crate::state::snapshot(&ctx, &json!({"page": "tasks"})).await.unwrap();
    assert_eq!(
        list["handling"][0],
        json!({"id": id, "task_id": TASK, "status": "completed", "thread_id": "th", "driver": "codex",
        "connection_name": "Codex", "project_name": "工作", "error": ""})
    );
    let _ = std::fs::remove_dir_all(&ctx.root);
}
