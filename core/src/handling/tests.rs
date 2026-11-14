//! Synthetic data only: an in-memory database, files under a test
//! directory and scripted agents.

use std::sync::mpsc;
use std::time::Duration;

use super::agent::fake::{Backend, Fake};
use super::*;
use crate::testing::temp_dir;

const TASK: &str = "task-h";

async fn setup() -> (Arc<Ctx>, Arc<Fake>) {
    let mut config = Config { codex: "/synthetic/codex".into(), codex_name: "Codex".into(), ..Default::default() };
    config.codex_models = vec![json!({"model": "m1"}), json!({"model": "m2"})];
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
async fn failed_project_reads_do_not_keep_background_loops_awake() {
    let (ctx, fake) = setup().await;
    *fake.projects_error.lock().unwrap() = true;
    let at = time::now();
    for year in crate::ai::calendar::wanted_years(crate::ai::work::shanghai_date(at).unwrap()) {
        ctx.db
            .save_holiday_year(crate::ai::calendar::Year { year, fetched_at: at, days: Default::default() })
            .await
            .unwrap();
    }
    let runtime = crate::Runtime::start(ctx.clone(), true).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = ctx.db.revision();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let growth = ctx.db.revision() - before;
    drop(runtime);
    let _ = std::fs::remove_dir_all(&ctx.root);
    assert!(growth <= 2, "idle revision grew by {growth}");
}

#[tokio::test]
async fn prepare_offers_only_configured_conversation_connections() {
    let (ctx, _) = setup().await;
    let p = call(&ctx, "handling_prepare", json!({"task_id": TASK})).await.unwrap();
    assert_eq!(p["connections"][0]["id"], DEFAULT_CODEX);
    assert_eq!(p["connections"][0]["models"].as_array().unwrap().len(), 2);
    assert!(p.get("projects").is_none(), "projects are read per connection");
    ctx.config
        .update(&ctx.db, |c| {
            c.ai_connections = vec![
                Connection {
                    id: "c-dsh".into(),
                    driver: "dsh".into(),
                    executable: "synthetic-dsh.cmd".into(),
                    ..Default::default()
                },
                Connection { id: "api".into(), driver: "api".into(), ..Default::default() },
                Connection { id: "unset".into(), driver: "codex".into(), ..Default::default() },
            ];
            Ok(())
        })
        .await
        .unwrap();
    let p = call(&ctx, "handling_prepare", json!({"task_id": TASK})).await.unwrap();
    let drivers: Vec<_> = p["connections"].as_array().unwrap().iter().map(|c| c["driver"].clone()).collect();
    assert_eq!(drivers, [json!("codex"), json!("dsh")]);
    ctx.config
        .update(&ctx.db, |c| {
            c.codex.clear();
            c.ai_connections.retain(|c| c.driver == "api");
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(call(&ctx, "handling_prepare", json!({"task_id": TASK})).await.unwrap()["connections"], json!([]));
    assert!(call(&ctx, "handling_start", json!({"task_id": TASK, "connection_id": "api", "cwd": ctx.root.join("w")}))
        .await
        .is_err());
    let _ = std::fs::remove_dir_all(&ctx.root);
}

#[tokio::test]
async fn projects_come_only_from_the_chosen_connections_client() {
    let (ctx, _) = setup().await;
    let dsh = Connection {
        id: "c-dsh".into(),
        driver: "dsh".into(),
        executable: "synthetic-dsh.cmd".into(),
        ..Default::default()
    };
    ctx.config.update(&ctx.db, |c| Ok(c.ai_connections.push(dsh))).await.unwrap();
    let ids = |v: Value| v.as_array().unwrap().iter().map(|p| p["id"].clone()).collect::<Vec<_>>();
    let codex = call(&ctx, "handling_projects", json!({"connection_id": DEFAULT_CODEX})).await.unwrap();
    assert_eq!(ids(codex), [json!("codex:p")]);
    let dsh = call(&ctx, "handling_projects", json!({"connection_id": "c-dsh"})).await.unwrap();
    assert_eq!(ids(dsh), [json!("dsh:p")]);
    assert!(call(&ctx, "handling_projects", json!({"connection_id": "missing"})).await.is_err());
    let prepared = call(&ctx, "handling_prepare", json!({"task_id": TASK})).await.unwrap();
    let drivers = prepared["connections"].as_array().unwrap().iter().map(|c| c["driver"].clone()).collect::<Vec<_>>();
    assert_eq!(drivers, [json!("codex"), json!("dsh")]);
    let shown =
        call(&ctx, "handling_projects", json!({"connection_id": prepared["connections"][0]["id"]})).await.unwrap();
    assert_eq!(offered(&ctx).await, shown, "analysis suggests from the dialog's default client");
    let _ = std::fs::remove_dir_all(&ctx.root);
}

#[tokio::test]
async fn codex_composes_then_binds_the_desktop_thread_through_its_brief() {
    let (ctx, fake) = setup().await;
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
    let mut run = ctx.db.handling_run(id.clone()).await.unwrap().unwrap();
    assert_eq!(
        (run.status.as_str(), run.thread_id.as_str(), run.model.as_str(), run.effort.as_str()),
        ("waiting", "", "", "")
    );
    assert!(fake.turns.lock().unwrap().is_empty());
    assert!(ctx.handling.live(&id).is_none());
    assert!(fake.opened.lock().unwrap().is_empty());
    let composed = fake.composed.lock().unwrap().clone();
    assert_eq!(composed.len(), 1);
    assert_eq!(composed[0].0, cwd);
    let kickoff = &composed[0].1;
    assert_eq!(started["kickoff"], *kickoff);
    assert!(kickoff.contains(&id) && kickoff.contains("handling brief") && kickoff.contains("| Out-String"));
    assert!(kickoff.contains("10013"));
    assert!(kickoff.contains(&format!("--root '{}'", ctx.host.install_root().display())));
    assert!(command(&ctx, "handling_kickoff", TASK, "r'b").contains("handling brief 'r''b'"));
    let (calls, logs) = ctx
        .db
        .test_read(|c| {
            let calls: f64 =
                c.query_row("SELECT total(calls) FROM usage_daily WHERE purpose = 'main'", [], |r| r.get(0))?;
            let logs: i64 =
                c.query_row("SELECT count(*) FROM activity WHERE kind = 'handling_round'", [], |r| r.get(0))?;
            Ok((calls, logs))
        })
        .await
        .unwrap();
    assert_eq!((calls, logs), (0.0, 0));
    let reopened = call(&ctx, "handling_open", json!({"run_id": id})).await.unwrap();
    assert_eq!(reopened["kickoff"], *kickoff);
    assert_eq!(fake.composed.lock().unwrap().len(), 2);
    call(&ctx, "handling_brief", json!({"run_id": id})).await.unwrap();
    assert_eq!(ctx.db.handling_run(id.clone()).await.unwrap().unwrap().status, "waiting");
    let brief = call(&ctx, "handling_brief", json!({"run_id": id, "thread_id": "th-1"})).await.unwrap();
    let text = brief.as_str().unwrap();
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
    run = ctx.db.handling_run(id.clone()).await.unwrap().unwrap();
    assert_eq!((run.status.as_str(), run.thread_id.as_str()), ("completed", "th-1"));
    call(&ctx, "handling_open", json!({"run_id": id})).await.unwrap();
    assert_eq!(*fake.opened.lock().unwrap(), ["th-1"]);
    call(&ctx, "handling_brief", json!({"run_id": id, "thread_id": "th-2"})).await.unwrap();
    assert_eq!(ctx.db.handling_run(id.clone()).await.unwrap().unwrap().thread_id, "th-2");
    run.thread_id.clear();
    ctx.db.save_handling_run(run).await.unwrap();
    assert_eq!(
        call(&ctx, "handling_open", json!({"run_id": id})).await.unwrap_err().0,
        "没有记录到 Codex 会话编号，请在 Codex 中查找这条对话，或重新开始处置"
    );
    let _ = std::fs::remove_dir_all(&ctx.root);
}

#[tokio::test]
async fn a_running_first_turn_blocks_a_second_start_and_dsh_is_raised_with_its_project() {
    let (ctx, fake) = setup().await;
    let dsh = Connection {
        id: "c-dsh".into(),
        driver: "dsh".into(),
        executable: "synthetic-dsh.cmd".into(),
        ..Default::default()
    };
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
    let dsh = Connection {
        id: "c-dsh".into(),
        driver: "dsh".into(),
        executable: "synthetic-dsh.cmd".into(),
        ..Default::default()
    };
    ctx.config.update(&ctx.db, |c| Ok(c.ai_connections.push(dsh))).await.unwrap();
    fake.script.lock().unwrap().push(("", Err("模型不可用".into())));
    let id =
        call(&ctx, "handling_start", json!({"task_id": TASK, "cwd": ctx.root.join("w"), "connection_id": "c-dsh"}))
            .await
            .unwrap()["run_id"]
            .as_str()
            .unwrap()
            .to_string();
    let run = settled(&ctx, &id).await;
    assert_eq!((run.status.as_str(), run.error.as_str()), ("failed", "模型不可用"));
    assert_eq!(call(&ctx, "handling_open", json!({"run_id": id})).await.unwrap_err().0, "会话尚未建立，请稍候");
    let mut running = run.clone();
    running.status = "running".into();
    ctx.db.save_handling_run(running).await.unwrap();
    let waiting = call(&ctx, "handling_start", json!({"task_id": TASK, "cwd": ctx.root.join("w")})).await.unwrap();
    ctx.db.interrupt_handling_runs().await.unwrap();
    assert_eq!(ctx.db.handling_run(id).await.unwrap().unwrap().status, "interrupted");
    assert_eq!(
        ctx.db.handling_run(waiting["run_id"].as_str().unwrap().into()).await.unwrap().unwrap().status,
        "waiting"
    );
    let _ = std::fs::remove_dir_all(&ctx.root);
}

#[tokio::test]
async fn the_snapshot_carries_the_latest_run_head() {
    let (ctx, _) = setup().await;
    let id = call(&ctx, "handling_start", json!({"task_id": TASK, "cwd": ctx.root.join("w"), "project_name": "工作"}))
        .await
        .unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let list = crate::state::snapshot(&ctx, &json!({"page": "tasks"})).await.unwrap();
    assert_eq!(
        list["handling"][0],
        json!({"id": id, "task_id": TASK, "status": "waiting", "thread_id": "", "driver": "codex",
        "connection_name": "Codex", "project_name": "工作", "error": ""})
    );
    let _ = std::fs::remove_dir_all(&ctx.root);
}
