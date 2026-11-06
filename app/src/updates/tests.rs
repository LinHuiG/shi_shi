use super::*;
use std::sync::Arc;

fn fixture() -> Value {
    json!({"tag_name": "v9.10.0", "published_at": "2099-01-01T00:00:00Z", "draft": false, "prerelease": false,
        "assets": [{"name": "Shishi-Setup-9.10.0.exe", "state": "uploaded", "size": 3,
            "digest": format!("sha256:{}", "a".repeat(64)),
            "browser_download_url": format!("{BASE}v9.10.0/Shishi-Setup-9.10.0.exe")}]})
}

fn boundary(hour: i64) -> i64 {
    22_000 * DAY - 8 * 3_600_000 + hour * 3_600_000
}

/// An update cache holding a verified "abc" package.
fn cached() -> (Arc<Updates>, PathBuf, Package) {
    let root = crate::transport::test_dir("updates");
    let dir = root.join("updates");
    fs::create_dir_all(&dir).unwrap();
    let mut package = release(&fixture(), "1.0.0", 0).unwrap().unwrap();
    fs::write(dir.join("source"), b"abc").unwrap();
    package.digest = sha256(&dir.join("source")).unwrap();
    fs::write(package.path(&dir), b"abc").unwrap();
    write_json(&dir.join("ready.json"), &package).unwrap();
    (Arc::new(Updates::load(dir, |_| {})), root, package)
}

#[test]
fn release_selection_requires_newer_version_publication_and_verified_asset() {
    assert!(version("1.10.0") > version("1.9.9"));
    for invalid in ["1.2", "1.2.3-beta", "../1.2.3", "01.2.3", "1.2.3.4"] {
        assert!(version(invalid).is_none());
    }
    assert!(release(&fixture(), "1.2.0", 0).unwrap().is_some());
    assert!(release(&fixture(), "10.0.0", 0).unwrap().is_none());
    assert!(release(&fixture(), "1.2.0", i64::MAX).unwrap().is_none(), "published before this build");
    for key in ["draft", "prerelease"] {
        let mut f = fixture();
        f[key] = json!(true);
        assert!(release(&f, "1.0.0", 0).unwrap().is_none());
    }
    for (key, value) in [
        ("browser_download_url", json!("https://example.com/untrusted.exe")),
        ("digest", json!(null)),
        ("size", json!(MAX_SIZE + 1)),
    ] {
        let mut f = fixture();
        f["assets"][0][key] = value;
        assert!(release(&f, "1.0.0", 0).is_err());
    }
    assert!(build_time() > 0 && build_time() <= now());
}

#[test]
fn boundaries_fire_only_on_a_live_crossing() {
    for hour in [4, 9] {
        let at = boundary(hour);
        assert!(crossed(None, at, hour).is_none(), "startup never replays");
        assert!(crossed(Some(at + 1), at + 1000, hour).is_none());
        assert!(crossed(Some(at - 500), at + CLOCK_GAP_MS + 1, hour).is_none(), "late wake after suspend");
        assert!(crossed(Some(at + 1), at - 1, hour).is_none(), "clock rewind");
        assert_eq!(crossed(Some(at - 3_600_000), at + 500, hour), Some((22_000, at)), "long timer wait");
        assert_eq!(next_boundary(at - 1, hour), at);
        assert_eq!(next_boundary(at, hour), at + DAY);
    }
}

#[test]
fn automatic_checks_wait_sixty_to_one_twenty_minutes_and_respect_retry() {
    let at = now();
    assert_eq!(check_interval(0), 60 * 60_000);
    assert_eq!(check_interval(60), 120 * 60_000);
    let mut view = View { status: "ready".into(), checked_at: at, ..Default::default() };
    assert!(check_due(&view, true, at), "a cached package does not suppress the startup check");
    view.next_check_at = at + check_interval(0);
    assert!(!check_due(&view, true, at + 59 * 60_000));
    assert!(check_due(&view, true, at + 60 * 60_000));
    assert!(check_due(&view, false, at));
    view.retry_at = at + DAY;
    assert!(!check_due(&view, false, at + 120 * 60_000));
    let updates = Arc::new(Updates::load(crate::transport::test_dir("updates-off").join("updates"), |_| {}));
    assert_eq!(updates.check(true, false).unwrap()["checked_at"], 0, "disabled automatic check is a no-op");
}

#[test]
fn reminders_are_claimed_once_per_day_and_expire() {
    let (updates, root, package) = cached();
    assert_eq!(updates.snapshot()["status"], "ready");
    let at = boundary(9);
    let remind = Due { remind: true, install: false };
    assert_eq!(updates.schedule_tick(Some(at - 500), at, true).unwrap(), remind);
    assert!(updates.snapshot()["reminder"]["id"].is_string());
    assert!(updates.next_wake(at, true) <= at + ACTION_WINDOW_MS);
    updates.schedule_tick(Some(at), at + ACTION_WINDOW_MS, true).unwrap();
    assert!(updates.snapshot()["reminder"].is_null(), "the reminder expires");
    let dir = root.join("updates");
    let reloaded = Updates::load(dir.clone(), |_| {});
    assert_eq!(reloaded.schedule_tick(Some(at - 500), at, true).unwrap(), Due::default(), "same day is persisted");
    fs::write(package.path(&dir), b"bad").unwrap();
    let later = at + DAY;
    assert_eq!(reloaded.schedule_tick(Some(later - 500), later, true).unwrap(), Due::default(), "corrupt cache");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn due_wakes_stay_due_until_the_event_loop_runs_the_tick() {
    let (updates, root, _) = cached();
    let at = boundary(4);
    *updates.previous.lock().unwrap() = Some(at - 60_000);
    assert!(at - 60_000 >= updates.next_wake(at - 60_000, true), "startup check is due now");
    updates.change(|v| v.next_check_at = at + DAY);
    assert_eq!(updates.next_wake(at - 1, true), at);
    assert_eq!(updates.next_wake(at + 1, true), at, "the live boundary must not move to tomorrow");
    let previous = updates.previous.lock().unwrap().replace(at + 1);
    assert_eq!(updates.schedule_tick(previous, at + 1, true).unwrap(), Due { install: true, remind: false });
    assert_eq!(updates.next_wake(at + 1, true), boundary(9));
    let check = at + 60_000;
    updates.change(|v| v.next_check_at = check);
    assert_eq!(updates.next_wake(check, true), check, "periodic checks must also reach the loop");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn the_4am_install_is_due_once_per_day_without_a_page() {
    let install = Due { remind: false, install: true };
    let (updates, root, _) = cached();
    let at = boundary(4);
    assert_eq!(updates.schedule_tick(Some(at - 500), at, false).unwrap(), Due::default(), "disabled");
    assert_eq!(updates.schedule_tick(Some(at - 500), at, true).unwrap(), install);
    assert!(updates.snapshot()["reminder"].is_null(), "nothing waits on the page");
    assert_eq!(updates.schedule_tick(Some(at - 500), at, true).unwrap(), Due::default(), "claimed");
    let reloaded = Updates::load(root.join("updates"), |_| {});
    assert_eq!(reloaded.schedule_tick(Some(at - 500), at, true).unwrap(), Due::default(), "claim is persisted");
    let next = at + DAY;
    let late = next + CLOCK_GAP_MS + 1;
    assert_eq!(reloaded.schedule_tick(Some(next - 500), late, true).unwrap(), Due::default(), "observed late");
    let next = next + DAY;
    reloaded.change(|v| v.status = "downloading".into());
    assert_eq!(reloaded.schedule_tick(Some(next - 500), next, true).unwrap(), Due::default(), "busy");
    let next = next + DAY;
    reloaded.change(|v| v.status = "ready".into());
    reloaded.restarting.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(reloaded.schedule_tick(Some(next - 500), next, true).unwrap(), Due::default(), "restarting");
    reloaded.restarting.store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(reloaded.schedule_tick(Some(next - 500), next, true).unwrap(), install, "unclaimed day still due");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn failures_keep_a_verified_package_and_views_are_published() {
    let root = crate::transport::test_dir("updates-publish");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let updates = Updates::load(root.join("updates"), move |v| sink.lock().unwrap().push(v));
    updates.fail("offline".into());
    assert_eq!(seen.lock().unwrap().last().unwrap()["status"], "failed");
    updates.finish_current(1_000);
    assert_eq!(updates.snapshot()["current_until"], 31_000);
    fs::remove_dir_all(root).unwrap();
    let (updates, root, package) = cached();
    updates.fail("quota".into());
    assert_eq!(updates.snapshot()["status"], "ready");
    assert_eq!(updates.snapshot()["version"], package.version);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn only_stale_owned_cache_is_pruned() {
    let root = crate::transport::test_dir("updates-prune");
    let old = root.join("Shishi-Setup-1.1.1-aaaaaaaaaaaa.exe");
    let ready = root.join("Shishi-Setup-1.1.2-bbbbbbbbbbbb.exe");
    let partial = root.join(format!("download-{}.part", uuid::Uuid::new_v4()));
    let unknown = root.join("personal-notes.json");
    let helper = root.join(format!("apply-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&helper).unwrap();
    for path in [&old, &ready, &partial, &unknown, &helper.join("plan.json")] {
        fs::write(path, b"fixture").unwrap();
    }
    prune(&root, None, SystemTime::now());
    assert!(old.exists(), "fresh files stay");
    prune(&root, Some(&ready), SystemTime::now() + STALE_AFTER + Duration::from_secs(10));
    assert!(!old.exists() && !partial.exists() && !helper.exists());
    assert!(ready.exists() && unknown.exists());
    fs::remove_dir_all(root).unwrap();
}

/// Answer each loopback connection with the next canned reply.
fn serve(replies: Vec<(u16, &'static [u8])>) -> String {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/asset", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for (status, body) in replies {
            let mut reader = BufReader::new(listener.accept().unwrap().0);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap() > 2 {
                line.clear();
            }
            let mut stream = reader.into_inner();
            let head = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
        }
    });
    url
}

#[test]
fn checks_read_the_release_and_back_off_on_rate_limits() {
    let (updates, root, _) = cached();
    let body: &'static [u8] = fixture().to_string().into_bytes().leak();
    let url = serve(vec![(200, body), (404, b""), (429, b"")]);
    assert_eq!(updates.latest(&url).unwrap().unwrap()["tag_name"], "v9.10.0");
    assert!(updates.latest(&url).unwrap().is_none(), "no release yet");
    let error = updates.latest(&url).unwrap_err();
    assert!(error.contains("429") && updates.view.lock().unwrap().retry_at > now());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn downloads_stream_with_progress_verify_the_digest_and_stop_on_restart() {
    let (_, root, mut package) = cached();
    let progress = Arc::new(Mutex::new(vec![]));
    let seen = progress.clone();
    let updates = Updates::load(root.join("fresh"), move |v| seen.lock().unwrap().push(v["progress"].clone()));
    fs::create_dir_all(root.join("fresh")).unwrap();
    package.url = serve(vec![(200, b"abc"), (200, b"abd"), (200, b"abc")]);
    let dest = root.join("fresh/download.part");
    updates.download(&package, &dest).unwrap();
    assert_eq!(fs::read(&dest).unwrap(), b"abc");
    assert!(progress.lock().unwrap().contains(&json!(99)));
    assert_eq!(updates.download(&package, &dest).unwrap_err(), "安装包校验不一致");
    updates.restarting.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(updates.download(&package, &dest).is_err());
    fs::remove_dir_all(root).unwrap();
}
