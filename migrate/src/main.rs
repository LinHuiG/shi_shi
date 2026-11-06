//! `shishi-migrate <install-dir>`: the installer runs this once after copying
//! the new files and deletes it afterwards. It converts the 1.1.68 data in the
//! install's data directory to the new format and leaves the old files as
//! they are. Records the new format cannot hold are skipped and listed; the
//! result is printed for the install log and kept as one activity entry.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use shishi_core::data::import::{config_from, summary, write_config, Legacy, Report};
use shishi_core::sources::Source;
use shishi_core::tasks::model::Task;
use shishi_core::tools::Tool;
use shishi_core::Db;

/// Old config field per prompt target.
const PROMPTS: [(&str, &str); 8] = [
    ("handling", "handling_prompt"),
    ("tool_call", "tool_call_prompt"),
    ("prompt_review", "review_prompt"),
    ("archive", "archive_prompt"),
    ("classify", "classify_prompt"),
    ("format", "format_prompt"),
    ("learning", "learning_prompt"),
    ("report", "report_prompt"),
];

fn main() -> ExitCode {
    let Some(root) = std::env::args_os().nth(1) else {
        eprintln!("用法：shishi-migrate <安装目录>");
        return ExitCode::from(2);
    };
    match run(Path::new(&root)) {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("1.1.68数据迁移失败：{e}");
            ExitCode::FAILURE
        }
    }
}

fn run(root: &Path) -> Result<String, String> {
    let data = shishi_core::data::directory(root)?;
    let old = data.join("state.sqlite3");
    let new = data.join("shishi.db");
    if !old.is_file() {
        return Ok("未发现1.1.68数据".into());
    }
    if new.exists() {
        return Ok("已是新版格式，跳过迁移".into());
    }
    let mut report = Report::default();
    let (settings, legacy) = read(&data, &old, &mut report)?;
    let config = config_from(&settings, &mut report);

    let db = Db::memory().map_err(|e| e.0)?;
    credentials::import(&data, &db, &mut report);
    let report = db.import_blocking(legacy, report).map_err(|e| e.0)?;
    write_config(&data.join("config.json"), &config).map_err(|e| e.0)?;
    // The new database appears in one rename, so an interrupted run leaves no
    // half-written shishi.db and the next install simply runs again.
    let staged = data.join("shishi.db.migrating");
    let _ = fs::remove_file(&staged);
    db.save_as_blocking(&staged).map_err(|e| e.0)?;
    fs::rename(&staged, &new).map_err(|e| e.to_string())?;

    let mut text = summary(&report);
    for s in &report.skipped {
        text.push_str("\n  ");
        text.push_str(s);
    }
    Ok(text)
}

/// The old settings object and the records that map onto the new tables.
fn read(data: &Path, path: &Path, report: &mut Report) -> Result<(Value, Legacy), String> {
    let error = |e: rusqlite::Error| format!("无法读取旧数据库：{e}");
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(error)?;
    let (payload, config): (String, String) = conn
        .query_row("SELECT payload, config FROM metadata WHERE id = 1", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(error)?;
    let state: Value = serde_json::from_str(&payload).map_err(|e| format!("旧数据主记录无法解析：{e}"))?;
    let settings: Value = match fs::read(data.join("settings").join(&config)) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            report.skipped.push(format!("旧设置文件无法解析，设置、工具和提示词使用新版默认：{e}"));
            Value::Null
        }),
        Err(e) => {
            report.skipped.push(format!("旧设置文件无法读取，设置、工具和提示词使用新版默认：{e}"));
            Value::Null
        }
    };

    let mut legacy = Legacy::default();
    let numbers = &state["task_numbers"];
    let mut rows = conn.prepare("SELECT id, payload FROM tasks ORDER BY ordinal").map_err(error)?;
    let rows = rows.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).map_err(error)?;
    for row in rows {
        let (id, payload) = row.map_err(error)?;
        let value: Value = serde_json::from_str(&payload).unwrap_or_default();
        match serde_json::from_value::<Task>(value.clone()) {
            Ok(mut t) => {
                t.number = numbers[&t.id].as_i64().unwrap_or(0);
                // Messages, schedules and run history stay behind, so nothing
                // that points at them comes along.
                t.evidence = Default::default();
                t.schedule_id.clear();
                t.scheduled_for.clear();
                t.generation.clear();
                t.generation_error.clear();
                if value["enabled_tool_ids"].as_array().is_some_and(|a| !a.is_empty()) {
                    report.skipped.push(format!("事项「{}」：单独启用的工具已改为全局工具设置", t.title));
                }
                legacy.tasks.push(t);
            }
            Err(e) => report.skipped.push(format!("事项{id}：无法解析（{e}）")),
        }
    }

    for value in settings["tools"].as_array().into_iter().flatten() {
        match serde_json::from_value::<Tool>(value.clone()) {
            Ok(t) => {
                let execution = ["working_directory", "output_encoding"]
                    .iter()
                    .any(|k| value[k].as_str().is_some_and(|s| !s.is_empty() && !s.eq_ignore_ascii_case("utf-8")))
                    || value["timeout_seconds"].as_u64().is_some_and(|n| n != 60)
                    || value["rate_limit_per_minute"].as_u64().is_some_and(|n| n != 60);
                if execution {
                    report.skipped.push(format!("工具「{}」：旧的工作目录、超时、频率或编码设置新版不再支持", t.name));
                }
                legacy.tools.push(t);
            }
            Err(e) => report.skipped.push(format!("工具{}：无法解析（{e}）", value["name"])),
        }
    }

    for (target, field) in PROMPTS {
        if let Some(text) = settings[field].as_str() {
            legacy.prompts.push((target.into(), text.into()));
        }
    }

    let memory = &state["analysis_memory"];
    legacy.temporary_memory = memory["temporary"].as_str().unwrap_or_default().into();
    legacy.archived_memory = match memory["archived"].as_str() {
        Some(text) => text.into(),
        None => memory["archived_entries"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| {
                let text = e["text"].as_str()?.trim();
                (!text.is_empty()).then(|| format!("- {}：{text}", e["key"].as_str().unwrap_or_default()))
            })
            .collect::<Vec<_>>()
            .join("\n"),
    };

    for value in state["chat_sources"].as_array().into_iter().flatten() {
        match serde_json::from_value::<Source>(value.clone()) {
            Ok(s) => legacy.sources.push(s),
            Err(e) => report.skipped.push(format!("消息来源{}：无法解析（{e}）", value["name"])),
        }
    }
    Ok((settings, legacy))
}

mod credentials {
    use super::*;

    /// Saved API keys: 1.1.68 kept them all in one DPAPI-protected file of
    /// `{version, entries: {id: {secret, updated_unix}}}`; each is stored
    /// again on its own. A file or entry that cannot be read is listed and
    /// the key has to be entered again in settings.
    pub fn import(data: &Path, db: &Db, report: &mut Report) {
        let path = data.join("credentials").join("credentials.bin");
        let Ok(sealed) = fs::read(&path) else { return };
        let entries = shishi_core::ai::credentials::unprotect(&sealed)
            .and_then(|plain| serde_json::from_slice::<Value>(&plain).map_err(|e| e.to_string()));
        let entries = match entries {
            Ok(v) if v["version"] == 1 => v["entries"].as_object().cloned().unwrap_or_default(),
            Ok(v) => {
                return report.skipped.push(format!("已保存的密钥：版本{}无法识别，需在设置中重新填写", v["version"]))
            }
            Err(e) => return report.skipped.push(format!("已保存的密钥无法解密，需在设置中重新填写（{e}）")),
        };
        let mut n = 0;
        for (id, entry) in entries {
            let saved = entry["secret"]
                .as_str()
                .ok_or_else(|| "内容无法识别".to_string())
                .and_then(|secret| db.set_credential_blocking(id.clone(), secret.into()).map_err(|e| e.0));
            match saved {
                Ok(()) => n += 1,
                Err(e) => report.skipped.push(format!("AI接入{id}的密钥：{e}，需在设置中重新填写")),
            }
        }
        report.counts.insert("credentials".into(), n.into());
    }
}
