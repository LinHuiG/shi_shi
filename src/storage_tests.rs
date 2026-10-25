use super::*;
use crate::model::{State, Task};

struct Fixture {
    base: PathBuf,
    root: PathBuf,
    source: PathBuf,
    dest: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let base =
            std::env::temp_dir().join(format!("shishi-data-location-{}", crate::model::id()));
        let root = base.join("app");
        let source = root.join("data");
        let dest = base.join("新数据目录");
        fs::create_dir_all(&dest).unwrap();
        let app = App::load(root.clone()).unwrap();
        {
            let mut state = app.state.lock().unwrap();
            state.tasks.push(Task {
                id: "synthetic-task".into(),
                title: "Synthetic retained task".into(),
                ..Default::default()
            });
            app.save(&state).unwrap();
        }
        Self {
            base,
            root,
            source,
            dest,
        }
    }
    fn plan(&self, copy: bool) -> TransferPlan {
        TransferPlan {
            root: self.root.clone(),
            source: canonical(&data_dir(&self.root)).unwrap(),
            destination: canonical(&self.dest).unwrap(),
            copy,
            parent_pid: 0,
            parent_rect: [0; 4],
            restart_args: vec![],
            expected_locator: read_locator(&self.root).unwrap(),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if self.base.parent() == Some(std::env::temp_dir().as_path())
            && self
                .base
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("shishi-data-location-")
        {
            let _ = fs::remove_dir_all(&self.base);
        }
    }
}

#[test]
fn complete_copy_preserves_bytes_history_and_paths_and_reports_byte_progress() {
    let f = Fixture::new();
    for dir in [
        "plugins/synthetic",
        "media",
        "reports",
        "empty-child",
        "webview",
        "tmp",
    ] {
        fs::create_dir_all(f.source.join(dir)).unwrap();
    }
    fs::write(
        f.source.join("plugins/synthetic/payload.dll"),
        b"synthetic binary bytes",
    )
    .unwrap();
    fs::write(
        f.source.join("media/example.note"),
        b"synthetic image summary",
    )
    .unwrap();
    fs::write(f.source.join("reports/example.md"), b"synthetic report").unwrap();
    fs::write(f.source.join("large.bin"), vec![87u8; 3 * 1024 * 1024 + 17]).unwrap();
    fs::write(f.source.join("webview/locked.bin"), b"ephemeral cache").unwrap();
    fs::write(f.source.join("tmp/synthetic.tmp"), b"temporary").unwrap();
    let before = json!(crate::managed_data::load_from_directory(&f.source).unwrap());
    let plan = f.plan(true);
    let events = Mutex::new(vec![]);
    let _source = lock_source(&plan.source).unwrap();
    transfer(&plan, |p| events.lock().unwrap().push(p)).unwrap();
    assert_eq!(
        fs::read(f.dest.join("large.bin")).unwrap(),
        fs::read(f.source.join("large.bin")).unwrap()
    );
    assert_eq!(
        json!(crate::managed_data::load_from_directory(&f.dest).unwrap()),
        before
    );
    assert_eq!(
        json!(crate::managed_data::load_from_directory(&f.source).unwrap()),
        before
    );
    assert!(f.dest.join("empty-child").is_dir());
    assert!(f.dest.join("plugins/synthetic/payload.dll").is_file());
    assert!(!f.dest.join("webview").exists());
    assert!(!f.dest.join("tmp").exists());
    let events = events.lock().unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.percent(), 100);
    assert_eq!(last.total, last.copied);
    assert!(last.total > 3 * 1024 * 1024);
    for pair in events.windows(2) {
        assert!(pair[1].copied >= pair[0].copied);
        assert!(pair[1].percent() >= pair[0].percent());
    }
    assert!(events.iter().any(|p| p.copied > 0 && p.copied < p.total));
    let app = App::load(f.root.clone()).unwrap();
    assert_eq!(data_dir(&f.root), plan.destination);
    assert_eq!(
        app.state.lock().unwrap().tasks[0].title,
        "Synthetic retained task"
    );
    app.log("synthetic", json!({}));
    let state = app.state.lock().unwrap();
    crate::snapshots::ensure(&app.root, &state, crate::model::now()).unwrap();
    assert!(crate::snapshots::dir(&app.root).starts_with(canonical(&f.dest).unwrap()));
    drop(state);
    assert_eq!(app.snapshot()["data_directory"], json!(plan.destination));
}

#[test]
fn switch_without_copy_uses_new_or_existing_data_and_atomically_replaces_locator() {
    let mut f = Fixture::new();
    let before = json!(crate::managed_data::load_from_directory(&f.source).unwrap());
    transfer(&f.plan(false), |_| {}).unwrap();
    let app = App::load(f.root.clone()).unwrap();
    assert!(app.state.lock().unwrap().tasks.is_empty());
    assert_eq!(
        json!(crate::managed_data::load_from_directory(&f.source).unwrap()),
        before
    );
    let existing = f.base.join("existing");
    fs::create_dir(&existing).unwrap();
    let state = State {
        tasks: vec![Task {
            id: "existing-synthetic-task".into(),
            title: "Existing retained task".into(),
            ..Default::default()
        }]
        .into(),
        ..Default::default()
    };
    fs::write(
        existing.join("state.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    f.dest = existing;
    transfer(&f.plan(false), |_| {}).unwrap();
    let reloaded = App::load(f.root.clone()).unwrap();
    assert_eq!(
        reloaded.state.lock().unwrap().tasks[0].title,
        "Existing retained task"
    );
    assert_eq!(
        json!(crate::managed_data::load_from_directory(&f.source).unwrap()),
        before
    );
}

#[test]
fn collisions_active_destination_and_stale_locator_do_not_overwrite_or_switch() {
    let f = Fixture::new();
    let plan = f.plan(true);
    fs::write(f.dest.join("existing.txt"), b"must keep").unwrap();
    assert!(transfer(&plan, |_| {}).unwrap_err().contains("已有文件"));
    assert_eq!(fs::read(f.dest.join("existing.txt")).unwrap(), b"must keep");
    assert!(!f.root.join(LOCATOR).exists());
    let active = data_lock(&f.dest).unwrap();
    assert!(target(&f.root, &json!({"directory":f.dest}), false).is_err());
    drop(active);
    let lock = transition_lock(&f.root).unwrap();
    assert!(transition_lock(&f.root).is_err());
    drop(lock);
    fs::write(f.root.join(LOCATOR), b"changed").unwrap();
    assert!(transfer(&plan, |_| {}).unwrap_err().contains("设置已变化"));
    assert_eq!(fs::read(f.root.join(LOCATOR)).unwrap(), b"changed");
}

#[test]
fn source_changes_and_copy_errors_preserve_the_original_locator() {
    let f = Fixture::new();
    let plan = f.plan(true);
    let changed = std::sync::atomic::AtomicBool::new(false);
    let error = transfer(&plan, |p| {
        if p.copied > 0 && !changed.swap(true, Ordering::SeqCst) {
            fs::write(f.source.join("late-file.txt"), b"late data").unwrap();
        }
    })
    .unwrap_err();
    assert!(error.contains("源目录"));
    assert!(!f.root.join(LOCATOR).exists());
    assert!(f.source.join(crate::state_store::FILE).is_file());
    let other = Fixture::new();
    fs::write(other.source.join("payload.txt"), b"keep source").unwrap();
    let plan = other.plan(true);
    let injected = std::sync::atomic::AtomicBool::new(false);
    assert!(transfer(&plan, |p| {
        if p.copied == 0 && !injected.swap(true, Ordering::SeqCst) {
            fs::write(other.dest.join("payload.txt"), b"concurrent destination").unwrap();
        }
    })
    .is_err());
    assert_eq!(
        fs::read(other.dest.join("payload.txt")).unwrap(),
        b"concurrent destination"
    );
    assert!(!other.root.join(LOCATOR).exists());
}

#[test]
fn invalid_locations_state_and_junctions_are_rejected_without_fallback() {
    let f = Fixture::new();
    assert!(target(&f.root, &json!({"directory":f.source}), true).is_err());
    assert!(target(&f.root, &json!({"directory":f.base}), false).is_err());
    let nested = f.source.join("child");
    fs::create_dir(&nested).unwrap();
    assert!(target(&f.root, &json!({"directory":nested}), true).is_err());
    fs::write(f.dest.join("state.json"), b"{}").unwrap();
    assert!(target(&f.root, &json!({"directory":f.dest}), false).is_err());
    fs::write(f.root.join(LOCATOR), b"{}").unwrap();
    assert!(App::load(f.root.clone()).is_err());
    fs::remove_file(f.root.join(LOCATOR)).unwrap();
    let link = f.base.join("junction");
    let status = Command::new("cmd.exe")
        .args(["/c", "mklink", "/J"])
        .arg(&link)
        .arg(&f.dest)
        .creation_flags(0x08000000)
        .output()
        .unwrap();
    assert!(status.status.success());
    assert!(plain_directory(&link).is_err());
    fs::remove_dir(link).unwrap();
}

#[test]
fn external_managed_data_cleanup_is_scoped_to_configured_directory() {
    let f = Fixture::new();
    transfer(&f.plan(true), |_| {}).unwrap();
    initialize(&f.root).unwrap();
    fs::write(f.dest.join("state.json"), b"invalid state").unwrap();
    let unrelated = f.base.join("unrelated.txt");
    fs::write(&unrelated, b"keep unrelated").unwrap();
    let inspection = crate::managed_data::inspect(&f.root);
    assert!(inspection
        .incompatible
        .iter()
        .any(|e| e.relative == "data/state.json"));
    crate::managed_data::cleanup(&f.root, &inspection).unwrap();
    assert!(f.dest.join("legacy").is_dir());
    assert!(f.source.join(crate::state_store::FILE).is_file());
    assert_eq!(fs::read(unrelated).unwrap(), b"keep unrelated");
}

#[test]
fn only_managed_absolute_ai_workdir_is_rebased_and_no_setting_is_removed() {
    let f = Fixture::new();
    let old = f.source.join("ai-work/custom");
    fs::create_dir_all(&old).unwrap();
    let mut state = crate::managed_data::load_state(&f.root).unwrap();
    state.config.analysis_workdir = old.to_string_lossy().into();
    crate::state_store::Store::open(&f.source)
        .unwrap()
        .save(&state)
        .unwrap();
    transfer(&f.plan(true), |_| {}).unwrap();
    let app = App::load(f.root.clone()).unwrap();
    assert_eq!(
        PathBuf::from(&app.state.lock().unwrap().config.analysis_workdir),
        canonical(&f.dest).unwrap().join("ai-work/custom")
    );
    assert!(old.is_dir());
    let outside = Fixture::new();
    let external = outside.base.join("external-workspace");
    let mut state = crate::managed_data::load_state(&outside.root).unwrap();
    state.config.analysis_workdir = external.to_string_lossy().into();
    crate::state_store::Store::open(&outside.source)
        .unwrap()
        .save(&state)
        .unwrap();
    transfer(&outside.plan(true), |_| {}).unwrap();
    assert_eq!(
        App::load(outside.root.clone())
            .unwrap()
            .state
            .lock()
            .unwrap()
            .config
            .analysis_workdir,
        external.to_string_lossy()
    );
}

#[test]
fn copied_plugin_paths_are_rebased_by_declared_field_type() {
    let f = Fixture::new();
    let plugin = f.source.join("plugins/synthetic");
    fs::create_dir_all(&plugin).unwrap();
    let local = f.source.join("synthetic-source.json");
    fs::write(&local, b"synthetic source").unwrap();
    fs::write(plugin.join("plugin.json"),json!({"id":"synthetic","name":"Synthetic source","version":"1.0","api_version":1,"entrypoint":"synthetic.dll","files":["synthetic.dll"],"capabilities":[],"config_fields":[{"key":"file","label":"File","type":"file"},{"key":"note","label":"Note","type":"text"}]}).to_string()).unwrap();
    let mut state = crate::managed_data::load_state(&f.root).unwrap();
    state.chat_sources.push(crate::model::ChatSource {
        id: "source".into(),
        plugin_id: "synthetic".into(),
        config: json!({"file":local,"note":local}),
        ..Default::default()
    });
    crate::state_store::Store::open(&f.source)
        .unwrap()
        .save(&state)
        .unwrap();
    transfer(&f.plan(true), |_| {}).unwrap();
    let app = App::load(f.root.clone()).unwrap();
    let state = app.state.lock().unwrap();
    let config = &state.chat_sources[0].config;
    assert_eq!(
        PathBuf::from(config["file"].as_str().unwrap()),
        canonical(&f.dest).unwrap().join("synthetic-source.json")
    );
    assert_eq!(config["note"], json!(local));
}
