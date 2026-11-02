use super::*;

struct Fixture {
    root: PathBuf,
    home: PathBuf,
    connection: Connection,
    config: Config,
    payload: Value,
    schema: Value,
}
impl Fixture {
    fn new(scenario: &str) -> Self {
        let root = std::env::temp_dir().join(format!("ss-dsh-offline-{}", crate::model::id()));
        let home = root.join("home");
        fs::create_dir_all(home.join("profiles/acp")).unwrap();
        fs::write(home.join("profiles/acp/package.json"), json!({"dsh":{"profile":{"bundles":["@deepseek-ai/dsh-base","@deepseek-ai/dsh-acp-app"]}}}).to_string()).unwrap();
        fs::write(
            home.join("dump.yml"),
            include_str!("../../providers/acp/tests/fixtures/restricted-dump.yml"),
        )
        .unwrap();
        fs::write(home.join("scenario.txt"), scenario).unwrap();
        fs::write(root.join("runtime.ps1"), include_str!("dsh_mock.ps1")).unwrap();
        let launcher = root.join("dsh.cmd");
        fs::write(
            &launcher,
            format!(
                r#"@echo off
if "%~1"=="--version" goto version
if "%~5"=="--dump-config" goto dump
powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "%~dp0runtime.ps1"
exit /b %errorlevel%
:version
echo version>>"%DSH_HOME%\probes.log"
if exist "%DSH_HOME%\version-wait" powershell.exe -NoProfile -NonInteractive -Command "Start-Sleep -Seconds 30"
if exist "%DSH_HOME%\slow-version" powershell.exe -NoProfile -NonInteractive -Command "Start-Sleep -Milliseconds 400"
echo {}
exit /b 0
:dump
echo dump>>"%DSH_HOME%\probes.log"
if exist "%DSH_HOME%\dump-wait" powershell.exe -NoProfile -NonInteractive -Command "Start-Sleep -Seconds 30"
type "%DSH_HOME%\dump.yml"
"#,
                shishi_acp::restricted::SUPPORTED_RUNTIME_VERSION
            ),
        )
        .unwrap();
        let config = Config {
            model: "[\"fixture\",\"model\"]".into(),
            reasoning_effort: String::new(),
            analysis_workdir: root.join("work").to_string_lossy().into(),
            retain_ai_traces: false,
            ..Config::default()
        };
        Self {
            root,
            home,
            config,
            connection: Connection {
                driver: "dsh".into(),
                executable: launcher.to_string_lossy().into(),
                ..Connection::default()
            },
            payload: json!({}),
            schema: json!({"type":"object","additionalProperties":false,"properties":{"answer":{"type":"string"}},"required":["answer"]}),
        }
    }
    fn request<'a>(&'a self, cancel: &'a AtomicBool) -> AnalysisRequest<'a> {
        AnalysisRequest {
            show_session: false,
            config: &self.config,
            kind: "edit",
            payload: &self.payload,
            root: &self.root,
            trace: &self.root,
            prompt: "synthetic",
            schema: &self.schema,
            tools: None,
            registered_tools: None,
            cancel,
        }
    }
    fn prepare(&self, cancel: &AtomicBool, deadline: Instant) -> Result<AnalysisProcess, String> {
        AnalysisProcess::prepare_at(
            &self.connection,
            &self.request(cancel),
            deadline,
            self.home.clone(),
        )
    }
    fn log(&self, file: &str) -> Vec<String> {
        fs::read_to_string(self.home.join(file))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
    fn wait_for(&self, file: &str, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while !self.log(file).iter().any(|line| line.starts_with(expected)) {
            assert!(Instant::now() < deadline, "missing {expected} in {file}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn restricted_process_reuses_preflight_but_isolates_sessions_and_rechecks_next_call() {
    let fixture = Fixture::new("normal");
    let cancel = AtomicBool::new(false);
    let deadline = Instant::now() + Duration::from_secs(30);
    let process = fixture.prepare(&cancel, deadline).unwrap();
    let job = process.cleanup.0.clone();
    // The running process keeps its loaded configuration when the source changes.
    fs::write(fixture.home.join("scenario.txt"), "wait").unwrap();
    for _ in 0..2 {
        let (value, _) = process
            .analyze(fixture.request(&cancel), deadline, &mut |_| {})
            .unwrap();
        assert_eq!(value["answer"], "synthetic");
        assert!(process.is_connected());
    }
    assert_eq!(fixture.log("probes.log"), ["version", "dump"]);
    let log = fixture.log("runtime.log");
    assert_eq!(log.iter().filter(|line| *line == "initialize").count(), 1);
    assert_eq!(
        log.iter().filter(|line| line.starts_with("close|")).count(),
        2
    );
    let tickets = log
        .iter()
        .filter(|line| line.starts_with("new|"))
        .map(|line| line.splitn(3, '|').nth(2).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(tickets.len(), 2);
    assert_ne!(tickets[0], tickets[1]);
    assert!(tickets.iter().all(|ticket| !Path::new(ticket).exists()));
    let pid: u32 = log[0].strip_prefix("pid|").unwrap().parse().unwrap();
    drop(process);
    assert!(!job.exists());
    assert!(!shishi_acp::process::pid_alive(pid));
    fs::write(fixture.home.join("scenario.txt"), "normal").unwrap();
    drop(fixture.prepare(&cancel, deadline).unwrap());
    assert_eq!(
        fixture.log("probes.log"),
        ["version", "dump", "version", "dump"]
    );
}

#[test]
fn optional_or_failed_session_close_keeps_result_and_retires_process() {
    for scenario in ["no-close", "close-failed"] {
        let fixture = Fixture::new(scenario);
        let cancel = AtomicBool::new(false);
        let deadline = Instant::now() + Duration::from_secs(30);
        for _ in 0..2 {
            let process = fixture.prepare(&cancel, deadline).unwrap();
            let (value, _) = process
                .analyze(fixture.request(&cancel), deadline, &mut |_| {})
                .unwrap();
            assert_eq!(value["answer"], "synthetic");
            assert!(!process.is_connected());
        }
        assert_eq!(
            fixture.log("probes.log"),
            ["version", "dump", "version", "dump"]
        );
        assert_eq!(
            fixture
                .log("runtime.log")
                .iter()
                .filter(|line| line.starts_with("prompt|"))
                .count(),
            2
        );
    }
}

#[test]
fn optional_trace_and_session_index_failures_preserve_result_and_usage() {
    let mut fixture = Fixture::new("normal");
    fixture.config.retain_ai_traces = true;
    fs::create_dir(fixture.root.join("output.txt")).unwrap();
    fs::write(fixture.home.join("ss-session-links"), b"blocked").unwrap();
    let cancel = AtomicBool::new(false);
    let deadline = Instant::now() + Duration::from_secs(30);
    let process = fixture.prepare(&cancel, deadline).unwrap();
    let mut events = vec![];
    let (value, usage) = process
        .analyze(fixture.request(&cancel), deadline, &mut |event| {
            events.push(event)
        })
        .unwrap();
    assert_eq!(value["answer"], "synthetic");
    assert_eq!(usage["input_tokens"], 10);
    assert_eq!(usage["output_tokens"], 5);
    assert!(events.contains(&AnalysisEvent::Usage(usage)));
    assert!(events.iter().any(|event| matches!(event,
        AnalysisEvent::VisibilityIssue { thread_id, .. } if thread_id == "fixture-1")));
    assert!(events.iter().any(|event| matches!(event,
        AnalysisEvent::Stream { kind, text, .. } if kind == "diagnostic" && text.contains("跟踪文件保存失败"))));
    assert!(process.is_connected());
}

#[test]
fn rejected_result_retires_the_process_and_removes_its_round_ticket() {
    let fixture = Fixture::new("invalid");
    let cancel = AtomicBool::new(false);
    let deadline = Instant::now() + Duration::from_secs(30);
    let process = fixture.prepare(&cancel, deadline).unwrap();
    assert!(process
        .analyze(fixture.request(&cancel), deadline, &mut |_| {})
        .is_err());
    assert!(!process.is_connected());
    let log = fixture.log("runtime.log");
    assert_eq!(
        log.iter()
            .filter(|line| line.starts_with("prompt|"))
            .count(),
        1
    );
    let ticket = log
        .iter()
        .find(|line| line.starts_with("new|"))
        .unwrap()
        .splitn(3, '|')
        .nth(2)
        .unwrap();
    assert!(!Path::new(ticket).exists());
}

#[test]
fn preflight_cancellation_covers_both_probes_and_cleans_owned_files() {
    for phase in ["version", "dump"] {
        let fixture = Fixture::new("normal");
        fs::write(fixture.home.join(format!("{phase}-wait")), "").unwrap();
        let cancel = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let prepare =
                scope.spawn(|| fixture.prepare(&cancel, Instant::now() + Duration::from_secs(30)));
            fixture.wait_for("probes.log", phase);
            let started = Instant::now();
            cancel.store(true, Ordering::SeqCst);
            let error = prepare.join().unwrap().err().expect("cancelled preflight");
            assert_eq!(error, crate::ai_control::CANCELLED);
            assert!(started.elapsed() < Duration::from_secs(3));
        });
        assert!(fixture.log("runtime.log").is_empty());
        assert_eq!(
            fs::read_dir(crate::storage::data_dir(&fixture.root).join("ai-work"))
                .unwrap()
                .count(),
            0
        );
    }
}

#[test]
fn one_deadline_includes_preflight_and_cancellation_reaps_active_round() {
    let fixture = Fixture::new("wait");
    fs::write(fixture.home.join("slow-version"), "").unwrap();
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let deadline = started + Duration::from_secs(4);
    let process = fixture.prepare(&cancel, deadline).unwrap();
    let error = process
        .analyze(fixture.request(&cancel), deadline, &mut |_| {})
        .unwrap_err();
    assert_eq!(error, "AI 总时限已到");
    assert!(started.elapsed() < Duration::from_secs(7));
    assert!(!process.is_connected());
    drop(process);

    fs::remove_file(fixture.home.join("slow-version")).unwrap();
    fs::remove_file(fixture.home.join("runtime.log")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let process = fixture.prepare(&cancel, deadline).unwrap();
    std::thread::scope(|scope| {
        let run = scope.spawn(|| process.analyze(fixture.request(&cancel), deadline, &mut |_| {}));
        fixture.wait_for("runtime.log", "prompt|");
        let started = Instant::now();
        cancel.store(true, Ordering::SeqCst);
        assert_eq!(
            run.join().unwrap().unwrap_err(),
            crate::ai_control::CANCELLED
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    });
    assert!(!process.is_connected());
}
