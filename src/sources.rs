use crate::{
    engine::{reconcile_withdrawals, App},
    model::*,
    plugins::{self, Plugin},
    process,
    work_hours::AiMode,
};
use serde_json::{json, Value};
use std::{collections::BTreeSet, fs, path::Path, sync::atomic::Ordering};

fn scoped(source: &str, external: &str) -> String {
    format!("{source}:{external}")
}
/// Message identity is source + chat + external id. Length prefixes make the
/// key unambiguous even when ids contain the separator. A plain source+external
/// key would let the same external id in two chats overwrite each other.
fn message_key(source: &str, chat: &str, external: &str) -> String {
    format!(
        "{}:{}:{}:{}:{}",
        source.len(),
        source,
        chat.len(),
        chat,
        external
    )
}
fn normalize(source: &ChatSource, value: &Value) -> Result<Vec<Message>, String> {
    let mut messages: Vec<Message> = serde_json::from_value(value["messages"].clone())
        .map_err(|e| format!("消息格式无效：{e}"))?;
    if messages.len() > 100_000 {
        return Err("单次消息数量超过上限".into());
    }
    for m in &mut messages {
        if m.id.trim().is_empty()
            || m.id.len() > 1024
            || m.chat.len() > 1024
            || m.reply.len() > 1024
            || m.time <= 0
            || m.time > now() + 300_000
            || m.text.len() > 1024 * 1024
        {
            return Err("插件返回的消息标识、时间或长度无效".into());
        }
        let raw_chat = m.chat.clone();
        m.external_id = m.id.clone();
        m.source_id = source.id.clone();
        m.source_name = source.name.clone();
        m.id = message_key(&source.id, &raw_chat, &m.id);
        m.images.truncate(8);
        for (i, image) in m.images.iter_mut().enumerate() {
            image.id = format!("{}:image:{i}", m.id);
            if image.path.len() > 4096 || !std::path::Path::new(&image.path).is_absolute() {
                image.path.clear();
            }
        }
        m.chat = scoped(&source.id, &raw_chat);
        if !m.reply.is_empty() {
            m.reply = message_key(&source.id, &raw_chat, &m.reply);
        }
        if !m.from.is_empty() {
            m.from = scoped(&source.id, &m.from);
        }
        if !m.to.is_empty() {
            m.to = scoped(&source.id, &m.to);
        }
        if m.withdrawn {
            m.text.clear();
            m.images.clear();
        }
    }
    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn encoded(bytes: &[u8]) -> String {
        use windows_sys::Win32::Security::Cryptography::{
            CryptBinaryToStringA, CRYPT_STRING_BASE64, CRYPT_STRING_NOCRLF,
        };
        let mut len = 0;
        let flags = CRYPT_STRING_BASE64 | CRYPT_STRING_NOCRLF;
        unsafe {
            assert_ne!(
                CryptBinaryToStringA(
                    bytes.as_ptr(),
                    bytes.len() as u32,
                    flags,
                    std::ptr::null_mut(),
                    &mut len
                ),
                0
            );
            let mut out = vec![0; len as usize];
            assert_ne!(
                CryptBinaryToStringA(
                    bytes.as_ptr(),
                    bytes.len() as u32,
                    flags,
                    out.as_mut_ptr(),
                    &mut len
                ),
                0
            );
            String::from_utf8(out)
                .unwrap()
                .trim_end_matches('\0')
                .into()
        }
    }
    #[test]
    fn message_identity_is_scoped_by_source_and_chat_and_external_id() {
        let source = ChatSource {
            id: "source".into(),
            name: "Fixture".into(),
            ..Default::default()
        };
        let rows = normalize(
            &source,
            &json!({"messages":[
                {"id":"same","chat":"chat-a","text":"甲","time":now(),"private":true},
                {"id":"same","chat":"chat-b","text":"乙","time":now(),"private":true},
                {"id":"same","chat":"chat-b","text":"乙2","time":now(),"private":true}
            ]}),
        )
        .unwrap();
        assert_eq!(rows.len(), 3);
        assert_ne!(rows[0].id, rows[1].id);
        assert_eq!(rows[1].id, rows[2].id);
        assert_eq!(rows[0].chat, "source:chat-a");
        assert_eq!(rows[1].chat, "source:chat-b");
        assert_eq!(rows[0].external_id, "same");
        assert_eq!(rows[1].external_id, "same");
    }

    #[test]
    fn browser_upload_uses_existing_zip_validation_and_cleans_temporary_files() {
        let app = fixture();
        let package = encoded(include_bytes!("../tests/fixtures/plugins/root.zip"));
        let v = json!({"name":"插件 包.ZIP","base64":package});
        assert_eq!(app.install_plugin_upload(&v).unwrap()["id"], "zip-fixture");
        let dir = app.root.join("data/plugins/zip-fixture");
        assert_eq!(
            fs::read(dir.join("plugin.dll")).unwrap(),
            b"inert DLL fixture"
        );
        assert_eq!(
            app.install_plugin_upload(&v).unwrap()["install_status"],
            "unchanged"
        );
        let invalid = json!({"name":"unsafe.zip","base64":encoded(include_bytes!("../tests/fixtures/plugins/traversal.zip"))});
        assert!(app.install_plugin_upload(&invalid).is_err());
        for v in [
            json!({"name":"wrong.exe","base64":package}),
            json!({"name":"x.zip","base64":"%%%"}),
            json!({"name":"x.zip","base64":encoded(b"not zip")}),
            json!({"name":"x.zip","base64":"A".repeat((32usize*1024*1024).div_ceil(3)*4+1)}),
        ] {
            assert!(app.install_plugin_upload(&v).is_err());
        }
        assert_eq!(
            fs::read_dir(app.root.join("data/tmp/plugin-uploads"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            fs::read(dir.join("plugin.dll")).unwrap(),
            b"inert DLL fixture"
        );
    }
    fn fixture() -> App {
        App::load(std::env::temp_dir().join(format!("shishi-sources-{}", id()))).unwrap()
    }
    fn source(name: &str) -> ChatSource {
        ChatSource {
            id: id(),
            name: name.into(),
            plugin_id: "fixture".into(),
            enabled: true,
            updated: 1,
            ..Default::default()
        }
    }
    #[test]
    fn deletion_and_retention_prevent_reimport_without_deleting_tasks() {
        let app = fixture();
        let source = source("Fixture");
        let m = row(&source);
        let mut old = m.clone();
        old.id = "old".into();
        old.time = now() - 91 * 86_400_000;
        {
            let mut s = app.state.lock().unwrap();
            s.chat_sources.push(source.clone());
            s.tasks.push(Task {
                id: "retained-task".into(),
                evidence: TaskEvidence {
                    direct: vec![m.id.clone(), old.id.clone()],
                    ..Default::default()
                },
                ..Default::default()
            });
        }
        app.commit_scan(
            vec![(
                source.clone(),
                Ok((vec![m.clone(), old.clone()], json!(1), now())),
            )],
            now(),
        )
        .unwrap();
        assert_eq!(app.state.lock().unwrap().messages.len(), 2);
        // The task is unarchived, so its old evidence is deliberately retained
        // beyond the display retention window for a later archive.
        assert!(app.state.lock().unwrap().messages.contains_key(&old.id));
        app.delete_messages(&json!({"ids":[m.id]})).unwrap();
        app.commit_scan(
            vec![(source.clone(), Ok((vec![m.clone(), old], json!(2), now())))],
            now(),
        )
        .unwrap();
        let reload = App::load(app.root.clone()).unwrap();
        let s = reload.state.lock().unwrap();
        assert!(!s.messages.contains_key(&m.id));
        assert!(s.messages.contains_key("old"));
        assert!(s.deleted_messages.contains_key(&m.id));
        assert_eq!(s.tasks.len(), 1);
        assert_eq!(s.tasks[0].evidence.all().len(), 2);
        assert_eq!(s.chat_sources[0].cursor, json!(2));
    }
    fn row(source: &ChatSource) -> Message {
        normalize(source,&json!({"messages":[{"id":"same","chat":"same-chat","reply":"parent","text":"fixture","time":now(),"private":true}]})).unwrap().remove(0)
    }
    fn install_fixture(app: &App, parent: &str) {
        let dir = app.root.join(parent).join("fixture");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("fixture.dll"), b"not executable").unwrap();
        fs::write(dir.join("plugin.json"),json!({"id":"fixture","name":"Fixture","version":"1","api_version":1,"entrypoint":"fixture.dll","files":["fixture.dll"],"capabilities":["discover","validate","fetch_incremental","fetch_range","lookup_messages"],"config_fields":[]}).to_string()).unwrap();
    }
    #[test]
    fn scan_counts_unique_persisted_changes_and_preserves_failure_boundaries() {
        let app = fixture();
        let source = source("Fixture");
        let mut first = row(&source);
        app.state.lock().unwrap().chat_sources.push(source.clone());
        let commit =
            |rows| app.commit_scan(vec![(source.clone(), Ok((rows, json!(1), now())))], now());
        let (counts, _, _) = commit(vec![first.clone(); 3]).unwrap();
        assert_eq!(
            (
                counts.received,
                counts.added,
                counts.updated,
                counts.duplicates
            ),
            (3, 1, 0, 2)
        );
        first.source_name = "Renamed".into();
        first.reply = "Changed metadata".into();
        let mut second = first.clone();
        second.id = "second".into();
        let mut expired = first.clone();
        expired.id = "expired".into();
        expired.time = now() - 400 * 86_400_000i64;
        let mut deleted = first.clone();
        deleted.id = "deleted".into();
        app.state
            .lock()
            .unwrap()
            .deleted_messages
            .insert(deleted.id.clone(), now());
        let (counts, _, _) = commit(vec![
            first.clone(),
            second.clone(),
            second.clone(),
            expired,
            deleted,
        ])
        .unwrap();
        assert_eq!(
            (
                counts.received,
                counts.added,
                counts.updated,
                counts.unchanged,
                counts.duplicates,
                counts.ignored
            ),
            (5, 1, 0, 1, 1, 2)
        );
        first.text = "Actual edit".into();
        second.withdrawn = true;
        second.text.clear();
        let (counts, _, _) = commit(vec![first.clone(), first.clone(), second]).unwrap();
        assert_eq!((counts.added, counts.updated, counts.duplicates), (0, 2, 1));
        let mut stale = source.clone();
        stale.updated += 1;
        let (counts, errors, sources) = app
            .commit_scan(
                vec![
                    (stale, Ok((vec![first.clone()], json!(2), now()))),
                    (source.clone(), Err("Synthetic failure".into())),
                ],
                now(),
            )
            .unwrap();
        assert_eq!(
            (
                counts.received,
                counts.ignored,
                counts.added,
                sources,
                errors.len()
            ),
            (1, 1, 0, 0, 2)
        );
        let previous = app.state.lock().unwrap().messages[&first.id].text.clone();
        crate::test_support::persistence_failure(&app.root, true).unwrap();
        first.text = "Uncommitted".into();
        assert!(commit(vec![first.clone()]).is_err());
        assert_eq!(app.state.lock().unwrap().messages[&first.id].text, previous);
    }
    #[test]
    fn scan_update_cursor_ignores_renames_replies_and_failed_saves() {
        let app = fixture();
        let source = source("Fixture");
        let mut m = row(&source);
        app.state.lock().unwrap().chat_sources.push(source.clone());
        let commit = |m: Message| {
            app.commit_scan(
                vec![(source.clone(), Ok((vec![m], json!(1), now())))],
                now(),
            )
        };
        commit(m.clone()).unwrap();
        let first = app.state.lock().unwrap().messages[&m.id].clone();
        {
            let mut s = app.state.lock().unwrap();
            s.analyzed.insert(m.id.clone(), fingerprint(&first));
            crate::message_progress::advance_analysis(&mut s);
        }
        m.source_name = "Renamed source".into();
        m.group = "Renamed group".into();
        m.reply = "another reference".into();
        commit(m.clone()).unwrap();
        {
            let s = app.state.lock().unwrap();
            assert_eq!(s.messages[&m.id].updated_at, first.updated_at);
            assert!(crate::context_budget::pending(&s).is_empty());
        }
        m.text = "A real change".into();
        commit(m.clone()).unwrap();
        let edited = app.state.lock().unwrap().messages[&m.id].clone();
        assert!(edited.updated_at > first.updated_at);
        assert_eq!(edited.inserted_at, first.inserted_at);
        assert_eq!(edited.time, first.time);
        assert_eq!(
            crate::context_budget::pending(&app.state.lock().unwrap()).len(),
            1
        );
        m.withdrawn = true;
        commit(m.clone()).unwrap();
        let withdrawn = app.state.lock().unwrap().messages[&m.id].clone();
        assert!(withdrawn.updated_at > edited.updated_at);
        let clock = app.state.lock().unwrap().message_updated_at;
        crate::test_support::persistence_failure(&app.root, true).unwrap();
        m.text = "Not committed".into();
        assert!(commit(m).is_err());
        assert_eq!(app.state.lock().unwrap().message_updated_at, clock);
    }
    #[test]
    fn no_bundled_plugins_and_uninstall_keeps_evidence() {
        let app = fixture();
        install_fixture(&app, "plugins");
        assert!(plugins::catalog(&app.root).0.is_empty());
        install_fixture(&app, "data/plugins");
        let source = source("Fixture source");
        let message = row(&source);
        {
            let mut s = app.state.lock().unwrap();
            s.chat_sources.push(source.clone());
            s.messages.insert(message.id.clone(), message.clone());
        }
        assert!(app.plugin_catalog().unwrap()["plugins"][0]
            .get("enabled")
            .is_none());
        {
            let _busy = app.plugin_access.read().unwrap();
            assert!(app.plugin_uninstall(&json!({"id":"fixture"})).is_err());
        }
        app.plugin_uninstall(&json!({"id":"fixture"})).unwrap();
        assert!(plugins::catalog(&app.root).0.is_empty());
        let restored = App::load(app.root.clone()).unwrap();
        let s = restored.state.lock().unwrap();
        assert!(!s.chat_sources[0].enabled);
        let mut expected = message.clone();
        expected.inserted_at = message.time;
        expected.updated_at = s.messages[&message.id].updated_at;
        assert!(expected.updated_at > 0);
        assert!(s.messages[&message.id] == expected);
        assert!(app.root.join("plugins/fixture/fixture.dll").is_file());
    }
    #[test]
    fn uninstall_failed_save_restores_plugin_and_state() {
        let app = fixture();
        install_fixture(&app, "data/plugins");
        app.state
            .lock()
            .unwrap()
            .chat_sources
            .push(source("Kept source"));
        crate::test_support::persistence_failure(&app.root, true).unwrap();
        assert!(app.plugin_uninstall(&json!({"id":"fixture"})).is_err());
        assert!(plugins::find(&app.root, "fixture").is_ok());
        assert!(app.state.lock().unwrap().chat_sources[0].enabled);
    }
    #[test]
    fn plugin_upgrade_and_same_version_repair_preserve_source_state() {
        let app = fixture();
        install_fixture(&app, "data/plugins");
        install_fixture(&app, "candidate");
        let candidate = app.root.join("candidate/fixture");
        let installed = app.root.join("data/plugins/fixture");
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(candidate.join("plugin.json")).unwrap()).unwrap();
        manifest["version"] = json!("2");
        fs::write(candidate.join("plugin.json"), manifest.to_string()).unwrap();
        fs::write(candidate.join("fixture.dll"), b"new inert payload").unwrap();
        {
            let mut s = app.state.lock().unwrap();
            let mut source = source("Configured source");
            source.config = json!({"account":"fixture-account"});
            source.cursor = json!({"offset":456});
            source.last_scan = 123;
            source.enabled = false;
            let message = row(&source);
            s.messages.insert(message.id.clone(), message);
            s.chat_sources.push(source);
            app.save(&s).unwrap();
        }
        let before = json!(*app.state.lock().unwrap());
        let path = candidate.join("plugin.json");
        {
            let _reading = app.plugin_access.read().unwrap();
            assert!(app
                .install_plugin(path.to_str().unwrap())
                .unwrap_err()
                .contains("正在读取"));
            assert_eq!(
                plugins::find(&app.root, "fixture")
                    .unwrap()
                    .manifest
                    .version,
                "1"
            );
        }
        assert_eq!(
            app.install_plugin(path.to_str().unwrap()).unwrap()["install_status"],
            "updated"
        );
        assert_eq!(
            plugins::find(&app.root, "fixture")
                .unwrap()
                .manifest
                .version,
            "2"
        );
        assert_eq!(json!(*app.state.lock().unwrap()), before);
        let modified = fs::metadata(installed.join("fixture.dll"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            app.install_plugin(path.to_str().unwrap()).unwrap()["install_status"],
            "unchanged"
        );
        assert_eq!(
            fs::metadata(installed.join("fixture.dll"))
                .unwrap()
                .modified()
                .unwrap(),
            modified
        );
        fs::write(
            candidate.join("fixture.dll"),
            b"repaired same-version payload",
        )
        .unwrap();
        assert_eq!(
            app.install_plugin(path.to_str().unwrap()).unwrap()["install_status"],
            "updated"
        );
        assert_eq!(
            fs::read(installed.join("fixture.dll")).unwrap(),
            b"repaired same-version payload"
        );
        assert_eq!(json!(*app.state.lock().unwrap()), before);
        assert_eq!(
            fs::read_dir(app.root.join("data/plugins")).unwrap().count(),
            1
        );
    }
    #[test]
    fn plugin_installation_does_not_rewrite_state_and_keeps_undeclared_files() {
        let app = fixture();
        install_fixture(&app, "data/plugins");
        install_fixture(&app, "candidate");
        let candidate = app.root.join("candidate/fixture");
        let installed = app.root.join("data/plugins/fixture");
        fs::write(installed.join("user-note.txt"), b"user data").unwrap();
        let before = json!(*app.state.lock().unwrap());
        let path = candidate.join("plugin.json");
        fs::remove_file(candidate.join("fixture.dll")).unwrap();
        assert!(app.install_plugin(path.to_str().unwrap()).is_err());
        assert_eq!(
            fs::read(installed.join("fixture.dll")).unwrap(),
            b"not executable"
        );
        assert_eq!(
            fs::read(installed.join("user-note.txt")).unwrap(),
            b"user data"
        );
        // Installing executable files no longer rewrites unrelated application data.
        crate::test_support::persistence_failure(&app.root, true).unwrap();
        fs::write(candidate.join("fixture.dll"), b"new payload").unwrap();
        app.install_plugin(path.to_str().unwrap()).unwrap();
        assert_eq!(
            fs::read(installed.join("fixture.dll")).unwrap(),
            b"new payload"
        );
        assert_eq!(json!(*app.state.lock().unwrap()), before);
        let backup = fs::read_dir(app.root.join("data/plugins"))
            .unwrap()
            .flatten()
            .find(|v| v.file_name().to_string_lossy().starts_with(".previous-"))
            .unwrap()
            .path();
        assert_eq!(
            fs::read(backup.join("user-note.txt")).unwrap(),
            b"user data"
        );
        assert!(!backup.join("fixture.dll").exists());
    }
    #[test]
    fn source_toggle_preserves_progress_rejects_stale_writes_and_never_loads_disabled_plugins() {
        let app = fixture();
        let mut original = source("Existing");
        original.cursor = json!({"offset":17});
        original.last_scan = 123;
        original.last_error = "last read failed".into();
        app.state
            .lock()
            .unwrap()
            .chat_sources
            .push(original.clone());
        let off = app
            .source_toggle(&json!({"id":original.id,"updated":original.updated,"enabled":false}))
            .unwrap();
        assert_eq!(off["enabled"], false);
        assert_eq!(off["cursor"], original.cursor);
        assert_eq!(off["last_scan"], 123);
        assert_eq!(off["last_error"], original.last_error);
        assert_eq!(off["config"], original.config);
        assert!(app
            .source_toggle(&json!({"id":original.id,"updated":original.updated,"enabled":true}))
            .unwrap_err()
            .contains("已变化"));
        let before = json!(*app.state.lock().unwrap());
        assert!(app
            .source_toggle(&json!({"id":original.id,"updated":off["updated"],"enabled":true}))
            .is_err());
        assert_eq!(json!(*app.state.lock().unwrap()), before);
        app.source_toggle(&json!({"id":original.id,"updated":off["updated"],"enabled":false}))
            .unwrap();
        assert_eq!(json!(*app.state.lock().unwrap()), before);
        // No enabled source means even an installed invalid DLL is never called.
        install_fixture(&app, "data/plugins");
        assert!(app.scan_data().unwrap_err().contains("启用一个消息来源"));
        let (_, _, committed) = app
            .commit_scan(
                vec![(
                    original.clone(),
                    Ok((vec![row(&original)], json!({"offset":18}), now())),
                )],
                now(),
            )
            .unwrap();
        assert_eq!(committed, 0);
        assert_eq!(
            app.state.lock().unwrap().chat_sources[0].cursor,
            original.cursor
        );
    }
    #[test]
    fn source_toggle_save_failure_keeps_previous_state() {
        let app = fixture();
        let original = source("Existing");
        app.state
            .lock()
            .unwrap()
            .chat_sources
            .push(original.clone());
        let before = json!(*app.state.lock().unwrap());
        crate::test_support::persistence_failure(&app.root, true).unwrap();
        assert!(app
            .source_toggle(&json!({"id":original.id,"updated":original.updated,"enabled":false}))
            .is_err());
        assert_eq!(json!(*app.state.lock().unwrap()), before);
    }
    #[test]
    fn source_namespaces_replies_and_rejects_invalid_messages() {
        let a = source("a");
        let b = source("b");
        let first = row(&a);
        let second = row(&b);
        assert_ne!(first.id, second.id);
        assert_ne!(first.chat, second.chat);
        assert_ne!(first.reply, second.reply);
        assert_eq!(first.external_id, "same");
        assert!(normalize(&a, &json!({"messages":[{"time":now()}]})).is_err());
    }
    #[test]
    fn failed_source_and_failed_save_never_advance_its_cursor() {
        let app = fixture();
        let a = source("a");
        let b = source("b");
        {
            let mut s = app.state.lock().unwrap();
            s.chat_sources = vec![a.clone(), b.clone()];
        }
        let at = now();
        let (_, errors, count) = app
            .commit_scan(
                vec![
                    (a.clone(), Ok((vec![row(&a)], json!({"offset":1}), at))),
                    (b.clone(), Err("fixture read failure".into())),
                ],
                at,
            )
            .unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(count, 1);
        let before = app.state.lock().unwrap().clone();
        assert_eq!(before.chat_sources[0].cursor, json!({"offset":1}));
        assert_eq!(before.chat_sources[1].last_scan, 0);
        assert!(before.chat_sources[1].cursor.is_null());
        crate::test_support::persistence_failure(&app.root, true).unwrap();
        assert!(app
            .commit_scan(vec![(a, Ok((vec![], json!({"offset":2}), at + 1)))], at + 1)
            .is_err());
        assert_eq!(
            app.state.lock().unwrap().chat_sources[0].cursor,
            json!({"offset":1})
        );
    }
    #[test]
    #[ignore = "Set SHISHI_TEST_PLUGIN to the built example plugin directory"]
    fn native_json_plugin_installs_discovers_reads_multiple_sources_and_preserves_failed_cursor() {
        let package = std::env::var("SHISHI_TEST_PLUGIN").expect("plugin fixture package");
        let app = fixture();
        let package = std::path::PathBuf::from(package);
        let package = if package.is_dir() {
            package.join("plugin.json")
        } else {
            package
        };
        app.install_plugin(&package.to_string_lossy()).unwrap();
        let mut sources = vec![];
        for n in 0..2 {
            let path = app.root.join(format!("messages-{n}.json"));
            fs::write(&path,json!({"messages":[{"id":"same","chat":"same","text":"synthetic fixture","time":now()-1000}]}).to_string()).unwrap();
            let discovery = app
                .source_discover(&json!({"plugin_id":"json-file","config":{"scan_root":app.root}}))
                .unwrap();
            assert!(!discovery["candidates"].as_array().unwrap().is_empty());
            let saved=app.source_save(&json!({"source":{"plugin_id":"json-file","name":format!("fixture{n}"),"config":{"file":path,"scan_root":app.root},"enabled":true}})).unwrap();
            sources.push(saved);
        }
        app.state.lock().unwrap().config.auto_ai = false;
        let result = app.scan(AiMode::Automatic).unwrap();
        assert_eq!(result["sources"], 2);
        let saved = app.state.lock().unwrap().clone();
        assert_eq!(saved.messages.len(), 2);
        assert_eq!(saved.calls, 0);
        fs::remove_file(app.root.join("messages-1.json")).unwrap();
        let result = app.scan(AiMode::Automatic).unwrap();
        assert_eq!(result["sources"], 1);
        assert_eq!(result["warnings"].as_array().unwrap().len(), 1);
        let after = app.state.lock().unwrap().clone();
        assert_eq!(saved.chat_sources[1].cursor, after.chat_sources[1].cursor);
        app.source_range(&json!({"id":sources[0]["id"],"since":now()-86_400_000,"until":now()}))
            .unwrap();
        let reloaded = App::load(app.root.clone()).unwrap();
        let state = reloaded.state.lock().unwrap();
        assert_eq!(state.chat_sources[0].cursor, after.chat_sources[0].cursor);
        assert_eq!(state.calls, 0);
        drop(state);
        // Exercise the real scheduler outside work hours: cache/UI update, zero AI calls.
        fs::write(app.root.join("messages-0.json"),json!({"messages":[{"id":"after-hours","chat":"room","text":"after-hours synthetic message","private":true,"time":now()-1000}]}).to_string()).unwrap();
        {
            let mut s = app.state.lock().unwrap();
            s.config.enabled = true;
            s.config.auto_ai = true;
            let current =
                chrono::NaiveTime::parse_from_str(&stamp(now())[11..16], "%H:%M").unwrap();
            s.config.work_end = current.format("%H:%M").to_string();
            s.config.work_start = current
                .overflowing_add_signed(chrono::Duration::minutes(1))
                .0
                .format("%H:%M")
                .to_string();
            s.last_scan = 0;
            s.last_scan_finished_at = 0;
        }
        app.last_attempt.store(0, Ordering::SeqCst);
        app.scan_tick();
        app.tick();
        let snapshot = app.snapshot();
        assert!(snapshot["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["external_id"] == "after-hours"));
        assert_eq!(snapshot["usage"]["calls"], 0);
        assert_eq!(snapshot["in_work_hours"], false);
        assert!(snapshot["pending"].as_u64().unwrap() > 0);
    }
}
fn validate(plugin: &Plugin, source: &ChatSource, work: &Path) -> Result<String, String> {
    plugin.validate_fields(&source.config)?;
    let result = plugin.call("validate", &source.config, json!({}), work)?;
    if result["valid"] != true {
        return Err(result["message"]
            .as_str()
            .unwrap_or("插件配置校验失败")
            .into());
    }
    let key = result["account_key"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("插件未返回来源身份标识")?;
    if !source.account_key.is_empty() && source.account_key != key {
        return Err("文件属于另一个账号，请新建消息来源".into());
    }
    Ok(key.into())
}
fn fetch(
    root: &Path,
    source: &ChatSource,
    tracked: Vec<String>,
    until: i64,
) -> Result<(Vec<Message>, Value, i64), String> {
    let plugin = plugins::find(root, &source.plugin_id)?;
    let work = crate::storage::data_dir(&root)
        .join("tmp/plugins")
        .join(&source.id);
    validate(&plugin, source, &work)?;
    let since = if source.last_scan == 0 {
        until - 86_400_000
    } else {
        (source.last_scan - 600_000).max(until - 86_400_000)
    };
    let value = plugin.call(
        "fetch_incremental",
        &source.config,
        json!({"cursor":source.cursor,"since":since,"until":until}),
        &work,
    )?;
    let checkpoint = value["until"]
        .as_i64()
        .filter(|t| *t >= source.last_scan && *t <= until && *t >= since)
        .ok_or("插件返回的增量检查点无效")?;
    if value.get("cursor").is_none() {
        return Err("插件未返回增量游标".into());
    }
    let mut messages = normalize(source, &value)?;
    // Re-read tracked evidence so withdrawals and edited messages are reconciled.
    for ids in tracked.chunks(500) {
        let value = plugin.call(
            "lookup_messages",
            &source.config,
            json!({"ids":ids,"until":until}),
            &work,
        )?;
        messages.extend(normalize(source, &value)?);
    }
    Ok((messages, value["cursor"].clone(), checkpoint))
}
struct ScanGuard<'a>(&'a std::sync::atomic::AtomicBool);
#[derive(Default, Debug, serde::Serialize)]
struct ScanCounts {
    received: usize,
    added: usize,
    updated: usize,
    unchanged: usize,
    duplicates: usize,
    ignored: usize,
}
impl Drop for ScanGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}
impl App {
    pub fn plugin_uninstall(&self, v: &Value) -> Result<Value, String> {
        let _guard = self
            .plugin_access
            .try_write()
            .map_err(|_| "插件正在读取或配置，请稍后重试")?;
        let pid = v["id"].as_str().ok_or("缺少插件编号")?;
        let plugin = plugins::find(&self.root, pid)?;
        let parent = crate::storage::data_dir(&self.root)
            .join("plugins")
            .canonicalize()
            .map_err(|e| e.to_string())?;
        if plugin.dir.parent() != Some(parent.as_path()) {
            return Err("插件目录不属于安装目录，未卸载".into());
        }
        // Move out of the discoverable directory first; a failed state save restores it.
        let storage = crate::storage::data_dir(&self.root).join("tmp/removed-plugins");
        fs::create_dir_all(&storage).map_err(|e| e.to_string())?;
        let retired = storage.join(id());
        let mut lock = self.state.lock().unwrap();
        let mut s = lock.clone();
        for source in s.chat_sources.iter_mut().filter(|s| s.plugin_id == pid) {
            source.enabled = false;
            source.updated = now().max(source.updated + 1);
        }
        Self::audit(
            &mut s,
            "plugin_uninstall",
            "",
            "卸载插件，保留来源配置、历史消息和事项",
            Value::Null,
            json!({"plugin_id":pid}),
        );
        fs::rename(&plugin.dir, &retired)
            .map_err(|e| format!("无法卸载，请等待插件调用结束：{e}"))?;
        if let Err(e) = self.save(&s) {
            let restored = fs::rename(&retired, &plugin.dir);
            return Err(format!(
                "{e}；恢复插件：{}",
                if restored.is_ok() {
                    "成功"
                } else {
                    "失败，安装包保留在临时目录"
                }
            ));
        }
        *lock = s;
        drop(lock);
        // Only package-declared plain files are deleted. Unrelated files are retained.
        for file in plugin
            .manifest
            .files
            .iter()
            .map(String::as_str)
            .chain(std::iter::once("plugin.json"))
        {
            let path = retired.join(file);
            if path.is_file() {
                let _ = fs::remove_file(&path);
            }
            let mut dir = path.parent();
            while let Some(p) = dir {
                if p == retired || !p.starts_with(&retired) {
                    break;
                }
                let _ = fs::remove_dir(p);
                dir = p.parent();
            }
        }
        let _ = fs::remove_dir(&retired);
        Ok(json!(true))
    }
    pub fn plugin_catalog(&self) -> Result<Value, String> {
        let _guard = self.plugin_access.read().map_err(|_| "插件状态不可用")?;
        let (plugins, errors) = plugins::catalog(&self.root);
        Ok(
            json!({"plugins":plugins.into_iter().map(|p|p.manifest).collect::<Vec<_>>(),"errors":errors,"sources":self.state.lock().unwrap().chat_sources}),
        )
    }
    pub fn install_plugin(&self, path: &str) -> Result<Value, String> {
        let _guard = self
            .plugin_access
            .try_write()
            .map_err(|_| "插件正在读取或配置，请稍后重试")?;
        let installed = plugins::install(&self.root, Path::new(path))?;
        let mut result = json!(installed.manifest);
        result["install_status"] = json!(installed.status);
        Ok(result)
    }
    pub fn install_plugin_upload(&self, v: &Value) -> Result<Value, String> {
        use windows_sys::Win32::Security::Cryptography::{
            CryptStringToBinaryA, CRYPT_STRING_BASE64, CRYPT_STRING_STRICT,
        };
        const LIMIT: usize = 32 * 1024 * 1024;
        let name = v["name"].as_str().ok_or("缺少插件包名称")?;
        if !name.to_ascii_lowercase().ends_with(".zip") {
            return Err("请拖入插件 ZIP 包".into());
        }
        let encoded = v["base64"].as_str().ok_or("缺少插件包内容")?;
        if encoded.len() > LIMIT.div_ceil(3) * 4 {
            return Err("拖入的 ZIP 超过32 MiB，请点击安装区域选择文件".into());
        }
        let mut length = 0u32;
        let flags = CRYPT_STRING_BASE64 | CRYPT_STRING_STRICT;
        // Decode with the Windows API already available on supported systems.
        let decode = |output: *mut u8, length: &mut u32| unsafe {
            CryptStringToBinaryA(
                encoded.as_ptr(),
                encoded.len() as u32,
                flags,
                output,
                length,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ) != 0
        };
        if encoded.is_empty()
            || !encoded.is_ascii()
            || !decode(std::ptr::null_mut(), &mut length)
            || length as usize > LIMIT
        {
            return Err("插件包传输不完整，请重试".into());
        }
        let mut bytes = vec![0; length as usize];
        if !decode(bytes.as_mut_ptr(), &mut length) {
            return Err("插件包传输不完整，请重试".into());
        }
        bytes.truncate(length as usize);
        if bytes.len() > LIMIT || !bytes.starts_with(b"PK\x03\x04") {
            return Err("插件包不是有效的 ZIP 文件".into());
        }
        // Browser drops expose file contents, never a trusted filesystem path.
        let parent = crate::storage::data_dir(&self.root).join("tmp/plugin-uploads");
        fs::create_dir_all(&parent).map_err(|e| e.to_string())?;
        let path = parent.join(format!("{}.zip", id()));
        let result = (|| {
            fs::write(&path, bytes).map_err(|e| e.to_string())?;
            self.install_plugin(path.to_str().ok_or("插件临时路径无效")?)
        })();
        let _ = fs::remove_file(&path);
        result
    }
    pub fn source_discover(&self, v: &Value) -> Result<Value, String> {
        let _guard = self.plugin_access.read().map_err(|_| "插件状态不可用")?;
        let plugin = plugins::find(&self.root, v["plugin_id"].as_str().ok_or("请选择插件")?)?;
        if plugin.manifest.settings.is_some() {
            return Ok(
                json!({"settings":crate::plugin_settings::begin(self, &plugin, &v["config"])?}),
            );
        }
        let work = crate::storage::data_dir(&self.root)
            .join("tmp/discovery")
            .join(id());
        plugin.call("discover", &v["config"], json!({}), &work)
    }
    pub fn source_save(&self, v: &Value) -> Result<Value, String> {
        let _guard = self.plugin_access.read().map_err(|_| "插件状态不可用")?;
        let mut source: ChatSource =
            serde_json::from_value(v["source"].clone()).map_err(|e| e.to_string())?;
        if source.name.trim().is_empty() || source.name.chars().count() > 100 {
            return Err("来源名称应为 1–100 字".into());
        }
        let plugin = plugins::find(&self.root, &source.plugin_id)?;
        let validated_key = if source.enabled {
            Some(validate(
                &plugin,
                &ChatSource {
                    account_key: String::new(),
                    ..source.clone()
                },
                &crate::storage::data_dir(&self.root)
                    .join("tmp/validate")
                    .join(id()),
            )?)
        } else {
            None
        };
        let mut lock = self.state.lock().unwrap();
        let mut state = lock.clone();
        if source.id.is_empty() {
            if state.chat_sources.len() >= 16 {
                return Err("最多配置 16 个消息来源".into());
            }
            source.account_key = validated_key.unwrap_or_default();
            source.id = id();
            source.cursor = Value::Null;
            source.last_scan = 0;
            source.last_error.clear();
            source.updated = now();
            state.chat_sources.push(source.clone());
        } else {
            let old = state
                .chat_sources
                .iter_mut()
                .find(|s| s.id == source.id)
                .ok_or("消息来源不存在")?;
            if old.updated != source.updated || old.plugin_id != source.plugin_id {
                return Err("消息来源已变化，请刷新后重试".into());
            }
            // A different account/path must have its own identity; never relabel old evidence.
            if old.config != source.config
                && old.last_scan > 0
                && (validated_key.as_deref() != Some(old.account_key.as_str())
                    || old.account_key.is_empty())
            {
                return Err(
                    "此来源已有消息。更换账号或文件请新建来源；可停用旧来源并保留证据。".into(),
                );
            }
            source.account_key = validated_key.unwrap_or_else(|| old.account_key.clone());
            source.cursor = old.cursor.clone();
            source.last_scan = old.last_scan;
            source.last_error = old.last_error.clone();
            source.updated = now().max(old.updated + 1);
            *old = source.clone();
        }
        Self::audit(
            &mut state,
            "source_save",
            "",
            "保存消息来源",
            Value::Null,
            json!({"id":source.id,"name":source.name,"enabled":source.enabled}),
        );
        self.save(&state)?;
        *lock = state;
        Ok(json!(source))
    }
    /// Disabling remains available without a usable plugin; enabling validates
    /// the existing account before changing collection state.
    pub fn source_toggle(&self, value: &Value) -> Result<Value, String> {
        let _access = self.plugin_access.read().map_err(|_| "插件状态不可用")?;
        let source_id = value["id"].as_str().ok_or("缺少来源编号")?;
        let expected = value["updated"].as_i64().ok_or("缺少来源版本")?;
        let enabled = value["enabled"].as_bool().ok_or("缺少开关状态")?;
        let before = {
            let state = self.state.lock().unwrap();
            let source = state
                .chat_sources
                .iter()
                .find(|s| s.id == source_id)
                .ok_or("消息来源不存在")?;
            if source.updated != expected {
                return Err("消息来源已变化，请刷新后重试".into());
            }
            if source.enabled == enabled {
                return Ok(json!(source));
            }
            source.clone()
        };
        let account_key = if enabled {
            let plugin = plugins::find(&self.root, &before.plugin_id)?;
            Some(validate(
                &plugin,
                &before,
                &crate::storage::data_dir(&self.root)
                    .join("tmp/validate")
                    .join(id()),
            )?)
        } else {
            None
        };
        let mut lock = self.state.lock().unwrap();
        let mut state = lock.clone();
        let source = state
            .chat_sources
            .iter_mut()
            .find(|s| s.id == source_id)
            .ok_or("消息来源不存在")?;
        if source.updated != expected {
            return Err("消息来源已变化，请刷新后重试".into());
        }
        source.enabled = enabled;
        source.updated = now().max(source.updated.saturating_add(1));
        if let Some(account_key) = account_key {
            source.account_key = account_key;
        }
        let result = json!(source);
        Self::audit(
            &mut state,
            "source_toggle",
            "",
            if enabled {
                "启用消息来源"
            } else {
                "停用消息来源，保留配置与读取进度"
            },
            json!({"id":before.id,"enabled":before.enabled}),
            json!({"id":before.id,"enabled":enabled}),
        );
        self.save(&state)?;
        *lock = state;
        Ok(result)
    }
    pub fn source_remove(&self, v: &Value) -> Result<Value, String> {
        let mut lock = self.state.lock().unwrap();
        let mut state = lock.clone();
        let index = state
            .chat_sources
            .iter()
            .position(|s| Some(s.id.as_str()) == v["id"].as_str())
            .ok_or("消息来源不存在")?;
        if Some(state.chat_sources[index].updated) != v["updated"].as_i64() {
            return Err("消息来源已变化".into());
        }
        let old = state.chat_sources.remove(index);
        Self::audit(
            &mut state,
            "source_remove",
            "",
            "移除消息来源，保留历史证据",
            json!({"id":old.id,"name":old.name}),
            Value::Null,
        );
        if !state.chat_sources.iter().any(|s| s.enabled) {
            state.config.enabled = false;
        }
        self.save(&state)?;
        *lock = state;
        Ok(json!(true))
    }
    pub fn scan(&self, mode: AiMode) -> Result<Value, String> {
        if mode == AiMode::Automatic && !self.state.lock().unwrap().config.enabled {
            return Ok(json!({"skipped":"增量消息扫描已关闭"}));
        }
        self.scan_with_analysis(mode == AiMode::Manual)
    }
    pub fn refresh_messages(&self) -> Result<Value, String> {
        self.scan_with_analysis(false)
    }
    fn scan_with_analysis(&self, analyze: bool) -> Result<Value, String> {
        let result = self.scan_data();
        if result.as_ref().err().is_some_and(|e| e == "扫描正在运行") {
            return result;
        }
        self.last_attempt.store(now(), Ordering::SeqCst);
        {
            self.commit_state(|s| {
                s.last_scan_finished_at = now();
                Ok(())
            })?;
        }
        let mut result = result?;
        result["analysis"] = if analyze {
            self.analyze_after_scan(AiMode::Manual)?
        } else {
            json!({"queued":true})
        };
        Ok(result)
    }
    fn scan_data(&self) -> Result<Value, String> {
        let plugin_guard = self.plugin_access.read().map_err(|_| "插件状态不可用")?;
        process::check_running()?;
        if self.busy.swap(true, Ordering::SeqCst) {
            return Err("扫描正在运行".into());
        }
        let _guard = ScanGuard(&self.busy);
        let start = now();
        self.last_attempt.store(start, Ordering::SeqCst);
        let jobs = {
            let mut state_lock = self.state.lock().unwrap();
            let mut s = state_lock.clone();
            let sources = s
                .chat_sources
                .iter()
                .filter(|source| source.enabled)
                .cloned()
                .collect::<Vec<_>>();
            if sources.is_empty() {
                return Err("请先配置并启用一个消息来源".into());
            }
            s.pending_scan = true;
            self.save(&s)?;
            *state_lock = s;
            let s = &*state_lock;
            let tracked = s
                .tasks
                .iter()
                .filter(|t| !t.archived)
                .flat_map(|t| t.evidence.all())
                .collect::<BTreeSet<_>>();
            sources
                .into_iter()
                .map(|source| {
                    let ids = s
                        .messages
                        .values()
                        .filter(|m| {
                            m.source_id == source.id
                                && (tracked.contains(&m.id)
                                    || (m.time
                                        >= now()
                                            - s.config.analysis_lookback_hours as i64 * 3_600_000
                                        && ["image", "richText"].contains(&m.kind.as_str())
                                        && (m.images.is_empty()
                                            || m.images.iter().any(|i| i.path.is_empty()))))
                        })
                        .map(|m| m.external_id.clone())
                        .collect::<Vec<_>>();
                    (source, ids)
                })
                .collect::<Vec<_>>()
        };
        let mut results = vec![];
        for chunk in jobs.chunks(8) {
            results.extend(std::thread::scope(|scope| {
                let handles = chunk
                    .iter()
                    .map(|(source, ids)| {
                        scope.spawn(move || {
                            (
                                source.clone(),
                                fetch(&self.root, source, ids.clone(), start),
                            )
                        })
                    })
                    .collect::<Vec<_>>();
                handles
                    .into_iter()
                    .map(|h| {
                        h.join().unwrap_or_else(|_| {
                            (ChatSource::default(), Err("插件读取线程异常".into()))
                        })
                    })
                    .collect::<Vec<_>>()
            }));
        }
        let (counts, errors, successful) = self.commit_scan(results, start)?;
        drop(plugin_guard);
        self.log(
            "scan",
            json!({"rows":counts.received,"added":counts.added,"updated":counts.updated,"scan_counts":counts,"sources":successful,"errors":errors,"elapsed_ms":now()-start}),
        );
        if successful == 0 && !errors.is_empty() {
            return Err(errors.join("；"));
        }
        Ok(
            json!({"rows":counts.received,"added":counts.added,"updated":counts.updated,"sources":successful,"warnings":errors}),
        )
    }
    fn commit_scan(
        &self,
        results: Vec<(ChatSource, Result<(Vec<Message>, Value, i64), String>)>,
        start: i64,
    ) -> Result<(ScanCounts, Vec<String>, usize), String> {
        let mut lock = self.state.lock().unwrap();
        let mut s = lock.clone();
        let keep = crate::messages::protected(&s);
        let mut counts = ScanCounts::default();
        let mut seen = BTreeSet::new();
        let mut accepted = BTreeSet::new();
        let mut errors = vec![];
        let mut successful = 0;
        for (source, result) in results {
            if let Ok((rows, _, _)) = &result {
                counts.received += rows.len();
            }
            let Some(index) = s
                .chat_sources
                .iter()
                .position(|x| x.id == source.id && x.updated == source.updated && x.enabled)
            else {
                if let Ok((rows, _, _)) = &result {
                    counts.ignored += rows.len();
                }
                errors.push(format!("{}配置已变化，本次结果已忽略", source.name));
                continue;
            };
            match result {
                Ok((rows, cursor, checkpoint)) => {
                    for m in rows {
                        if !seen.insert(m.id.clone()) {
                            counts.duplicates += 1;
                        }
                        if crate::messages::allowed_with(&s, &m, &keep) {
                            accepted.insert(m.id.clone());
                            if s.messages
                                .get(&m.id)
                                .is_none_or(|old| fingerprint(old) != fingerprint(&m))
                            {
                                s.analysis_retries.remove(&m.id);
                                s.analysis_retry_counts.remove(&m.id);
                            }
                            crate::message_progress::insert(&mut s, m);
                        } else {
                            counts.ignored += 1;
                        }
                    }
                    s.chat_sources[index].cursor = cursor;
                    s.chat_sources[index].last_scan = checkpoint;
                    s.chat_sources[index].last_error.clear();
                    successful += 1;
                    if source.last_scan > 0 && start - source.last_scan > 86_400_000 {
                        Self::audit(
                            &mut s,
                            "scan_gap",
                            "",
                            "来源离线超过24小时，可按时间段补读",
                            json!({"source":source.id,"from":source.last_scan}),
                            json!(start),
                        );
                    }
                }
                Err(e) => {
                    s.chat_sources[index].last_error = e.clone();
                    errors.push(format!("{}：{e}", source.name));
                }
            }
        }
        reconcile_withdrawals(&mut s);
        crate::message_progress::advance_analysis(&mut s);
        crate::message_progress::advance_filter(&mut s);
        // Retention pruning must not rebase audit/message indices while an
        // archive candidate is being generated against the same state.
        if !self.archive_busy.load(std::sync::atomic::Ordering::SeqCst) {
            crate::messages::prune(&mut s);
            crate::images::prune(&self.root, s.config.message_retention_days);
        }
        if successful > 0 {
            s.last_scan = start;
        }
        let pending = !crate::context_budget::pending(&s).is_empty();
        crate::context_budget::mark_pending(&mut s, pending, now());
        s.pending_scan = false;
        s.last_error = errors.join("；");
        // Count persisted net changes once per identity, without changing the
        // established merge order or treating display metadata as message edits.
        for id in accepted {
            if let Some(message) = s.messages.get(&id) {
                match lock.messages.get(&id) {
                    None => counts.added += 1,
                    Some(old) if fingerprint(old) != fingerprint(message) => counts.updated += 1,
                    Some(_) => counts.unchanged += 1,
                }
            }
        }
        self.save(&s)?;
        *lock = s;
        Ok((counts, errors, successful))
    }
    pub fn source_range(&self, v: &Value) -> Result<Value, String> {
        let plugin_guard = self.plugin_access.read().map_err(|_| "插件状态不可用")?;
        if self.busy.swap(true, Ordering::SeqCst) {
            return Err("扫描正在运行".into());
        }
        let _guard = ScanGuard(&self.busy);
        let since = v["since"].as_i64().ok_or("缺少开始时间")?;
        let until = v["until"].as_i64().ok_or("缺少结束时间")?;
        if since <= 0 || until < since || until > now() || until - since > 31 * 86_400_000 {
            return Err("请选择过去31天以内的有效时间段（单次最多31天）".into());
        }
        let source = self
            .state
            .lock()
            .unwrap()
            .chat_sources
            .iter()
            .find(|s| Some(s.id.as_str()) == v["id"].as_str())
            .cloned()
            .ok_or("来源不存在")?;
        let plugin = plugins::find(&self.root, &source.plugin_id)?;
        let work = crate::storage::data_dir(&self.root)
            .join("tmp/range")
            .join(id());
        validate(&plugin, &source, &work)?;
        let value = plugin.call(
            "fetch_range",
            &source.config,
            json!({"since":since,"until":until}),
            &work,
        )?;
        let rows = normalize(&source, &value)?;
        let count = rows.len();
        {
            let mut lock = self.state.lock().unwrap();
            let mut s = lock.clone();
            if !s
                .chat_sources
                .iter()
                .any(|x| x.id == source.id && x.updated == source.updated)
            {
                return Err("来源配置已变化，请重试".into());
            }
            let keep = crate::messages::protected(&s);
            for m in rows {
                if crate::messages::allowed_with(&s, &m, &keep) {
                    if s.messages
                        .get(&m.id)
                        .is_none_or(|old| fingerprint(old) != fingerprint(&m))
                    {
                        s.analysis_retries.remove(&m.id);
                        s.analysis_retry_counts.remove(&m.id);
                    }
                    crate::message_progress::insert(&mut s, m);
                }
            }
            reconcile_withdrawals(&mut s);
            crate::message_progress::advance_analysis(&mut s);
            crate::message_progress::advance_filter(&mut s);
            self.save(&s)?;
            *lock = s;
        }
        drop(plugin_guard);
        {
            self.commit_state(|s| {
                s.revision += 1;
                s.last_scan_finished_at = now();
                Ok(())
            })?;
        }
        let analysis = self.analyze_after_scan(AiMode::Manual)?;
        Ok(json!({"rows":count,"analysis":analysis}))
    }
}
