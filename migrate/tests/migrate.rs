//! The installer migration against a synthetic 1.1.68 data directory.

use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::Connection;
use serde_json::{json, Value};
use shishi_core::{ConfigStore, Db};

fn root(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("migrate-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("data/settings")).unwrap();
    dir
}

fn migrate(root: &Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_shishi-migrate")).arg(root).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn task(id: &str, title: &str, status: &str) -> Value {
    json!({"id": id, "title": title, "status": status, "description": "说明", "created": 1, "updated": 2,
        "tags": [{"kind": "项目", "value": "甲"}], "steps": [], "next_step": "旧字段",
        "evidence": {"direct": ["m1"]}, "schedule_id": "s1"})
}

/// The old layout: `state.sqlite3` with the main record and task rows, and
/// the settings file it names.
fn legacy(data: &Path) {
    let settings = json!({
        "interval_minutes": 15,
        "work_start": "25:99",
        "window_backdrop": "mica",
        "ai_connections": [
            {"id": "c1", "name": "接入一", "driver": "api", "base_url": "http://localhost:1", "credential_id": "k1", "models": ["m"]},
            {"id": "c1", "name": "重复编号"},
        ],
        "handling_prompt": "我自己的处理提示词",
        "classify_prompt": "",
        "tools": [
            {"id": "t1", "name": "grep_logs", "description": "查日志", "command_template": "echo {{q}}",
             "parameters": [{"name": "q", "description": "关键词", "type": "string"}], "enabled": true},
            {"id": "t2", "name": "9bad", "enabled": false},
            {"id": "t3", "name": "half", "enabled": true, "timeout_seconds": 5},
        ],
    });
    std::fs::write(data.join("settings/a.json"), settings.to_string()).unwrap();
    let state = json!({
        "schema_version": 1,
        "task_numbers": {"a": 7, "b": 3, "c": 9},
        "analysis_memory": {"temporary": "临时记忆", "archived_entries": [
            {"key": "人员", "text": " 张三负责发布 "}, {"key": "空", "text": " "}]},
        "chat_sources": [
            {"id": "s1", "plugin_id": "json", "account_key": "acc", "name": "群聊", "enabled": true, "config": {"x": 1}},
            {"id": "s2", "name": "坏来源", "enabled": "yes"},
        ],
    });
    let c = Connection::open(data.join("state.sqlite3")).unwrap();
    c.execute_batch(
        "CREATE TABLE metadata (id INTEGER PRIMARY KEY CHECK(id=1), payload TEXT NOT NULL, config TEXT NOT NULL);
         CREATE TABLE tasks (id TEXT PRIMARY KEY, ordinal INTEGER NOT NULL, payload TEXT NOT NULL);",
    )
    .unwrap();
    c.execute("INSERT INTO metadata VALUES (1, ?1, 'a.json')", [state.to_string()]).unwrap();
    let mut b = task("b", "工具单独启用", "处理中");
    b["enabled_tool_ids"] = json!(["t1"]);
    let rows = [
        ("a", task("a", "正常事项", "待处理").to_string()),
        ("b", b.to_string()),
        ("c", task("c", "状态不认识", "搁置").to_string()),
        ("d", r#"{"id":"d","title":5}"#.to_string()),
        ("e", task("e", "没有编号", "已完成").to_string()),
    ];
    for (i, (id, payload)) in rows.iter().enumerate() {
        c.execute("INSERT INTO tasks VALUES (?1, ?2, ?3)", rusqlite::params![id, i as i64, payload]).unwrap();
    }
}

#[test]
fn converts_old_data_once_and_skips_what_does_not_fit() {
    let root = root("full");
    let data = root.join("data");
    legacy(&data);
    // Another user's or machine's key file: it cannot be decrypted here.
    std::fs::create_dir_all(data.join("credentials")).unwrap();
    std::fs::write(data.join("credentials/credentials.bin"), b"not this user's").unwrap();
    let out = migrate(&root);
    assert!(out.starts_with("已迁移1.1.68数据：事项3、工具2、提示词1、记忆2、消息来源1、AI接入1"), "{out}");
    for skipped in [
        "状态不认识",
        "事项d",
        "9bad",
        "half",
        "work_start",
        "重复编号",
        "坏来源",
        "单独启用的工具",
        "已保存的密钥无法解密",
    ] {
        assert!(out.contains(skipped), "{skipped} missing from {out}");
    }
    assert!(!data.join("shishi.db.migrating").exists());
    assert!(data.join("state.sqlite3").exists(), "old files stay");

    let config = ConfigStore::load(data.join("config.json")).unwrap().get();
    assert_eq!(config.interval_minutes, 15);
    assert_eq!(config.ai_connections.len(), 1);
    assert_eq!(config.ai_connections[0].credential_id.as_deref(), Some("k1"));
    assert_eq!(config.work_start, shishi_core::Config::default().work_start);

    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let db = Db::open(&data.join("shishi.db")).unwrap();
    rt.block_on(async {
        let a = db.task_payload(json!({"id": "a"})).await.unwrap();
        assert_eq!((a["number"].as_i64(), a["title"].as_str()), (Some(7), Some("正常事项")));
        assert_eq!(a["tags"][0]["value"], "甲");
        assert_eq!(a["schedule_id"], "");
        let e = db.task_payload(json!({"id": "e"})).await.unwrap();
        assert_eq!(e["number"], 8, "a task without an old number comes after the imported ones");
        assert!(db.task_payload(json!({"id": "c"})).await.is_err());
        let mut edit = a.clone();
        edit["title"] = json!("改过的标题");
        db.edit_task(json!({"task": edit})).await.unwrap();
        assert_eq!(db.task_payload(json!({"id": "a"})).await.unwrap()["title"], "改过的标题");

        let tools = db.tools().await.unwrap();
        let enabled: Vec<_> = tools.iter().map(|t| (t.name.as_str(), t.enabled)).collect();
        assert_eq!(enabled, [("grep_logs", true), ("half", false)]);
        assert_eq!(db.sources().await.unwrap()[0].config, json!({"x": 1}));
    });
    db.checkpoint_blocking().unwrap();

    let c = Connection::open(data.join("shishi.db")).unwrap();
    let prompt = |target: &str| -> String {
        c.query_row(
            "SELECT content FROM prompt_versions WHERE target = ?1 ORDER BY rowid DESC LIMIT 1",
            [target],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(prompt("handling"), "我自己的处理提示词");
    assert_eq!(prompt("classify"), shishi_core::prompts::default("classify"));
    let memory = |kind: &str| -> String {
        c.query_row("SELECT text FROM memory WHERE kind = ?1", [kind], |r| r.get(0)).unwrap()
    };
    assert_eq!(memory("temporary"), "临时记忆");
    assert_eq!(memory("archived"), "- 人员：张三负责发布");
    let logged: String = c.query_row("SELECT detail FROM activity WHERE kind = 'migration'", [], |r| r.get(0)).unwrap();
    assert!(logged.contains("跳过"), "{logged}");
    drop(c);

    // A second install finds the new format and leaves it alone.
    assert_eq!(migrate(&root).trim(), "已是新版格式，跳过迁移");
    let c = Connection::open(data.join("shishi.db")).unwrap();
    let title: String = c.query_row("SELECT title FROM tasks WHERE id = 'a'", [], |r| r.get(0)).unwrap();
    assert_eq!(title, "改过的标题");
    drop((c, db));
    // The data thread closes its connection after the last handle drops;
    // Windows refuses to delete the file until then.
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while let Err(e) = std::fs::remove_dir_all(&root) {
        assert!(std::time::Instant::now() < until, "{e}");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[test]
fn fresh_install_has_nothing_to_convert() {
    let root = root("fresh");
    assert_eq!(migrate(&root).trim(), "未发现1.1.68数据");
    assert!(!root.join("data/shishi.db").exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn follows_a_moved_data_directory() {
    let root = root("moved");
    let moved = root.join("elsewhere");
    std::fs::create_dir_all(moved.join("settings")).unwrap();
    std::fs::write(root.join("data-location.json"), json!({"version": 1, "directory": moved}).to_string()).unwrap();
    legacy(&moved);
    assert!(migrate(&root).starts_with("已迁移1.1.68数据"));
    assert!(moved.join("shishi.db").exists() && !root.join("data/shishi.db").exists());
    std::fs::remove_dir_all(root).unwrap();
}
