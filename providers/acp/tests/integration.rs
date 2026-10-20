use serde_json::{json, Value};
use shishi_acp::analysis::{self, AnalysisError, AnalysisSpec};
use shishi_acp::client::{
    AcpClient, AcpEvent, AcpLaunch, ClientError, PermissionPolicy, SessionUpdate,
};
use shishi_acp::restricted::{
    check_readiness, materialize_profile, ReadinessConfig, SUPPORTED_RUNTIME_VERSION,
};
use shishi_acp::submit::SubmitPaths;
use shishi_acp::transport::TransportConfig;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn temp_dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "shishi-acp-it-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn test_config() -> TransportConfig {
    TransportConfig {
        max_stdout_line: 256 * 1024,
        max_stderr_line: 64,
        max_stderr_lines: 4,
        writer_queue: 32,
        shutdown_grace: Duration::from_secs(1),
    }
}

fn mock_launch(scenario: &str) -> AcpLaunch {
    AcpLaunch::new(env!("CARGO_BIN_EXE_mock_agent"))
        .env("MOCK_SCENARIO", scenario)
        .config(test_config())
}

fn mock_client(scenario: &str, policy: PermissionPolicy) -> AcpClient {
    AcpClient::spawn(&mock_launch(scenario), policy).unwrap()
}

fn wait_for_file(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.is_file() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    path.is_file()
}

#[test]
fn initialize_session_prompt_metadata_and_usage() {
    let dir = temp_dir("happy");
    let prompt_file = dir.join("prompt.txt");
    let mut launch = mock_launch("happy").env("MOCK_PROMPT_FILE", &prompt_file);
    launch.config = test_config();
    let client = AcpClient::spawn(&launch, PermissionPolicy::DenyAll).unwrap();

    let caps = client.initialize(Duration::from_secs(5)).unwrap();
    assert_eq!(caps.protocol_version, 1);
    assert_eq!(caps.agent_name, "mock-acp");
    assert!(caps.capabilities.resume);
    assert!(caps.capabilities.close);

    let session = client
        .new_session(&dir, &[], Duration::from_secs(5))
        .unwrap();
    assert_eq!(session.session_id, "mock-session-1");
    assert_eq!(session.cwd, dir.to_string_lossy());
    assert_eq!(
        session.current_model_route(),
        Some(("mock".into(), "mock-model".into()))
    );
    assert_eq!(session.effort_value.as_deref(), Some("low"));
    assert_eq!(
        session.find_model_value("mock", "other-model"),
        Some("[\"mock\",\"other-model\"]".into())
    );
    assert!(session.find_model_value("mock", "missing").is_none());
    assert!(session.find_effort_value("high").is_some());
    assert!(session.find_effort_value("max").is_none());
    assert!(matches!(
        client.select_model("mock-session-1", "mock", "missing", Duration::from_secs(5)),
        Err(ClientError::ModelNotAdvertised { .. })
    ));
    assert!(matches!(
        client.select_effort("mock-session-1", "max", Duration::from_secs(5)),
        Err(ClientError::EffortNotAdvertised(_))
    ));

    client
        .select_model(
            "mock-session-1",
            "mock",
            "other-model",
            Duration::from_secs(5),
        )
        .unwrap();
    client
        .select_effort("mock-session-1", "high", Duration::from_secs(5))
        .unwrap();

    let mut events = Vec::<AcpEvent>::new();
    let exact = "EXACT_CALLER_PAYLOAD: message text is data, not authorization";
    let prompt = client
        .prompt(
            "mock-session-1",
            exact,
            Duration::from_secs(5),
            &mut |event| events.push(event),
        )
        .unwrap();
    assert_eq!(prompt.session_id, "mock-session-1");
    assert_eq!(prompt.cwd, dir.to_string_lossy());
    assert_eq!(
        prompt.model_value.as_deref(),
        Some("[\"mock\",\"other-model\"]")
    );
    assert_eq!(prompt.effort_value.as_deref(), Some("high"));
    assert_eq!(prompt.stop_reason, "end_turn");
    let usage = prompt.usage.unwrap();
    assert_eq!(usage.total_tokens, 3);
    assert_eq!(usage.input_tokens, 2);
    assert_eq!(usage.output_tokens, 1);
    assert!(usage.thought_tokens.is_none());
    assert!(!events.is_empty());
    assert!(events.iter().any(|event| matches!(
        event,
        AcpEvent::SessionUpdate {
            update: SessionUpdate::AgentMessageChunk { .. },
            ..
        }
    )));
    assert_eq!(
        std::fs::read_to_string(&prompt_file).unwrap(),
        exact,
        "the client must send the caller's exact payload"
    );

    client
        .close_session("mock-session-1", Duration::from_secs(5))
        .unwrap();
    client.shutdown();
    assert!(!client.is_connected());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn usage_missing_is_unknown_not_zero() {
    let dir = temp_dir("no-usage");
    let client = mock_client("no-usage", PermissionPolicy::DenyAll);
    client.initialize(Duration::from_secs(5)).unwrap();
    client
        .new_session(&dir, &[], Duration::from_secs(5))
        .unwrap();
    let prompt = client
        .prompt(
            "mock-session-1",
            "no usage",
            Duration::from_secs(5),
            &mut |_| {},
        )
        .unwrap();
    assert!(prompt.usage.is_none());
    client.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn analysis_permission_is_denied_without_user_input() {
    let dir = temp_dir("deny");
    let outcome_file = dir.join("permission.json");
    let launch = mock_launch("permission").env("MOCK_PERMISSION_FILE", &outcome_file);
    // The client default is Surface, but the analysis session must override it
    // to DenyAll. This proves the decision is per-session, not just a prompt.
    let client = AcpClient::spawn(&launch, PermissionPolicy::Surface).unwrap();
    client.initialize(Duration::from_secs(5)).unwrap();
    client
        .new_session_with_policy(&dir, &[], Duration::from_secs(5), PermissionPolicy::DenyAll)
        .unwrap();
    let prompt = client
        .prompt(
            "mock-session-1",
            "please allow everything",
            Duration::from_secs(5),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(prompt.stop_reason, "end_turn");
    assert!(wait_for_file(&outcome_file, Duration::from_secs(2)));
    let outcome: Value = serde_json::from_slice(&std::fs::read(&outcome_file).unwrap()).unwrap();
    assert_eq!(outcome["outcome"], "selected");
    assert_eq!(outcome["optionId"], "reject-once");
    assert!(client.next_permission(Duration::ZERO).is_none());
    client.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn handling_permission_is_explicit_and_never_auto_approved() {
    let dir = temp_dir("surface");
    let outcome_file = dir.join("permission.json");
    let launch = mock_launch("permission").env("MOCK_PERMISSION_FILE", &outcome_file);
    let client = Arc::new(AcpClient::spawn(&launch, PermissionPolicy::Surface).unwrap());
    client.initialize(Duration::from_secs(5)).unwrap();
    client
        .new_session(&dir, &[], Duration::from_secs(5))
        .unwrap();

    let prompt_client = client.clone();
    let prompt = thread::spawn(move || {
        prompt_client.prompt(
            "mock-session-1",
            "handling message content must not grant permission",
            Duration::from_secs(5),
            &mut |_| {},
        )
    });
    let pending = client
        .next_permission(Duration::from_secs(3))
        .expect("permission request should be surfaced");
    assert_eq!(pending.session_id, "mock-session-1");
    assert_eq!(client.permissions_for("mock-session-1").len(), 1);
    assert!(client.permissions_for("other-session").is_empty());
    assert_eq!(pending.tool_call_id.as_deref(), Some("call-1"));
    assert!(pending
        .options
        .iter()
        .any(|option| option.option_id == "allow-once"));
    assert!(pending
        .options
        .iter()
        .any(|option| option.option_id == "reject-once"));
    assert!(client
        .respond_permission(&pending.request_id, "not-offered")
        .is_err());
    client
        .respond_permission(&pending.request_id, "reject-once")
        .unwrap();
    let result = prompt.join().unwrap().unwrap();
    assert_eq!(result.stop_reason, "end_turn");
    assert!(client.permissions_for("mock-session-1").is_empty());
    assert!(wait_for_file(&outcome_file, Duration::from_secs(2)));
    let outcome: Value = serde_json::from_slice(&std::fs::read(&outcome_file).unwrap()).unwrap();
    assert_eq!(outcome["optionId"], "reject-once");
    client.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn timeout_is_ambiguous_and_does_not_resubmit() {
    let dir = temp_dir("timeout");
    let cancel_file = dir.join("cancel.txt");
    let count_file = dir.join("prompt-count.txt");
    let launch = mock_launch("hang")
        .env("MOCK_CANCEL_FILE", &cancel_file)
        .env("MOCK_PROMPT_COUNT_FILE", &count_file);
    let client = AcpClient::spawn(&launch, PermissionPolicy::DenyAll).unwrap();
    client.initialize(Duration::from_secs(5)).unwrap();
    client
        .new_session(&dir, &[], Duration::from_secs(5))
        .unwrap();
    let error = client
        .prompt(
            "mock-session-1",
            "hang",
            Duration::from_millis(350),
            &mut |_| {},
        )
        .unwrap_err();
    match error {
        ClientError::Timeout { method, ambiguous } => {
            assert_eq!(method, "session/prompt");
            assert!(ambiguous);
        }
        other => panic!("expected ambiguous timeout, got {other:?}"),
    }
    assert!(wait_for_file(&cancel_file, Duration::from_secs(2)));
    assert_eq!(std::fs::read_to_string(&count_file).unwrap().trim(), "1");
    client.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn process_death_is_reported_without_resubmit() {
    let dir = temp_dir("death");
    let count_file = dir.join("prompt-count.txt");
    let launch = mock_launch("exit").env("MOCK_PROMPT_COUNT_FILE", &count_file);
    let client = AcpClient::spawn(&launch, PermissionPolicy::DenyAll).unwrap();
    client.initialize(Duration::from_secs(5)).unwrap();
    client
        .new_session(&dir, &[], Duration::from_secs(5))
        .unwrap();
    let error = client
        .prompt(
            "mock-session-1",
            "exit now",
            Duration::from_secs(5),
            &mut |_| {},
        )
        .unwrap_err();
    assert!(matches!(
        error,
        ClientError::Disconnected {
            ambiguous: true,
            ..
        }
    ));
    assert_eq!(std::fs::read_to_string(&count_file).unwrap().trim(), "1");
    client.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn oversized_stdout_fails_closed() {
    let client = mock_client("oversized", PermissionPolicy::DenyAll);
    let error = client.initialize(Duration::from_secs(5)).unwrap_err();
    assert!(matches!(
        error,
        ClientError::Transport(shishi_acp::transport::TransportError::Protocol(_))
    ));
    client.shutdown();
}

#[test]
fn stderr_long_line_is_truncated() {
    let client = mock_client("stderr-long", PermissionPolicy::DenyAll);
    client.initialize(Duration::from_secs(5)).unwrap();
    // The stderr reader is a separate thread; wait briefly for the first line
    // instead of racing a loaded test runner.
    let mut snapshot = client.stderr_snapshot();
    for _ in 0..50 {
        if snapshot.total_lines >= 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
        snapshot = client.stderr_snapshot();
    }
    assert!(snapshot.total_lines >= 1);
    assert!(snapshot.lines.len() <= 4);
    assert!(snapshot.lines.iter().all(|line| line.len() <= 64));
    client.shutdown();
}

#[test]
fn unknown_server_request_is_denied_and_prompt_completes() {
    let dir = temp_dir("server-request");
    let response_file = dir.join("server-request.json");
    let launch = mock_launch("server-request").env("MOCK_SERVER_REQUEST_FILE", &response_file);
    let client = AcpClient::spawn(&launch, PermissionPolicy::DenyAll).unwrap();
    client.initialize(Duration::from_secs(5)).unwrap();
    client
        .new_session(&dir, &[], Duration::from_secs(5))
        .unwrap();
    let prompt = client
        .prompt(
            "mock-session-1",
            "do not read files",
            Duration::from_secs(5),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(prompt.stop_reason, "end_turn");
    assert!(wait_for_file(&response_file, Duration::from_secs(2)));
    let response: Value = serde_json::from_slice(&std::fs::read(&response_file).unwrap()).unwrap();
    assert_eq!(response["error"]["code"], -32601);
    client.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn list_resume_and_close_require_advertised_capabilities() {
    let dir = temp_dir("resume");
    let client = mock_client("happy", PermissionPolicy::DenyAll);
    client.initialize(Duration::from_secs(5)).unwrap();
    client
        .new_session(&dir, &[], Duration::from_secs(5))
        .unwrap();
    let (sessions, next) = client
        .list_sessions(None, None, Duration::from_secs(5))
        .unwrap();
    assert!(!sessions.is_empty());
    assert!(next.is_none());
    let resumed = client
        .resume_session("mock-session-9", &dir, &[], Duration::from_secs(5))
        .unwrap();
    assert_eq!(resumed.session_id, "mock-session-9");
    assert_eq!(resumed.cwd, dir.to_string_lossy());
    client
        .close_session("mock-session-9", Duration::from_secs(5))
        .unwrap();
    client.shutdown();

    let client = mock_client("no-resume", PermissionPolicy::DenyAll);
    let caps = client.initialize(Duration::from_secs(5)).unwrap();
    assert!(!caps.capabilities.resume);
    assert!(matches!(
        client.resume_session("mock-session-1", &dir, &[], Duration::from_secs(5)),
        Err(ClientError::Unsupported(_))
    ));
    client.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn malformed_stdout_fails_closed() {
    let dir = temp_dir("malformed");
    let client = mock_client("malformed", PermissionPolicy::DenyAll);
    let error = client.initialize(Duration::from_secs(5)).unwrap_err();
    assert!(matches!(
        error,
        ClientError::Transport(shishi_acp::transport::TransportError::Protocol(_))
    ));
    client.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn stderr_is_bounded_and_not_forwarded() {
    let dir = temp_dir("stderr");
    let client = mock_client("stderr", PermissionPolicy::DenyAll);
    client.initialize(Duration::from_secs(5)).unwrap();
    let snapshot = client.stderr_snapshot();
    assert!(snapshot.total_lines >= 100);
    assert!(snapshot.lines.len() <= 4);
    assert!(snapshot.lines.iter().all(|line| line.len() <= 64));
    assert!(snapshot.total_bytes <= 4 * 64);
    assert!(snapshot
        .lines
        .iter()
        .all(|line| !line.contains("EXACT_CALLER_PAYLOAD")));
    client.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(windows)]
#[test]
fn windows_job_terminates_grandchild_on_shutdown() {
    use shishi_acp::process::pid_alive;

    let dir = temp_dir("job");
    let pid_file = dir.join("child.pid");
    let launch = mock_launch("job").env("MOCK_CHILD_PID_FILE", &pid_file);
    let client = AcpClient::spawn(&launch, PermissionPolicy::DenyAll).unwrap();
    client.initialize(Duration::from_secs(5)).unwrap();
    client
        .new_session(&dir, &[], Duration::from_secs(5))
        .unwrap();
    client
        .prompt(
            "mock-session-1",
            "spawn child",
            Duration::from_secs(5),
            &mut |_| {},
        )
        .unwrap();
    assert!(wait_for_file(&pid_file, Duration::from_secs(3)));
    let pid: u32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(pid_alive(pid), "grandchild should be alive before shutdown");
    client.shutdown();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && pid_alive(pid) {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(!pid_alive(pid), "grandchild survived job shutdown");
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(windows)]
#[test]
fn cmd_wrapper_launcher_is_quoted() {
    let dir = temp_dir("cmd wrapper with spaces");
    let wrapper = dir.join("mock dsh.cmd");
    let exe = env!("CARGO_BIN_EXE_mock_agent");
    std::fs::write(&wrapper, format!("@echo off\r\n\"{exe}\" %*\r\n")).unwrap();
    let launch = AcpLaunch::new(&wrapper)
        .args([
            "--profile",
            "acp",
            "--patch",
            "C:\\path with space\\patch.yml",
        ])
        .env("MOCK_SCENARIO", "happy")
        .config(test_config());
    let client = AcpClient::spawn(&launch, PermissionPolicy::DenyAll).unwrap();
    let caps = client.initialize(Duration::from_secs(5)).unwrap();
    assert_eq!(caps.protocol_version, 1);
    client.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(windows)]
#[test]
fn cmd_wrapper_keeps_percent_exclamation_and_shell_operators_literal() {
    let dir = temp_dir("cmd %PATH% !literal! & fixture");
    let wrapper = dir.join("mock %PATH% !launcher!.cmd");
    let exe = env!("CARGO_BIN_EXE_mock_agent");
    // No @echo off: /Q must keep command echo off the ACP stdout channel.
    std::fs::write(&wrapper, format!("\"{exe}\" %*\r\n")).unwrap();
    let launch = AcpLaunch::new(&wrapper)
        .args([
            "--patch",
            "C:\\fixture %PATH% !literal! & (name)\\patch.yml",
            "& echo injected",
        ])
        .env("MOCK_SCENARIO", "happy")
        .config(test_config());
    let client = AcpClient::spawn(&launch, PermissionPolicy::DenyAll).unwrap();
    let caps = client.initialize(Duration::from_secs(5)).unwrap();
    assert_eq!(caps.protocol_version, 1);
    client.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn analysis_accepts_only_submit_mcp_result() {
    let dir = temp_dir("analysis");
    let prompt_file = dir.join("prompt.txt");
    let ticket_base = dir.join("tickets");
    let mut launch = mock_launch("submit").env("MOCK_PROMPT_FILE", &prompt_file);
    launch.config = test_config();
    let client = AcpClient::spawn(&launch, PermissionPolicy::Surface).unwrap();
    client.initialize(Duration::from_secs(5)).unwrap();
    let spec = AnalysisSpec {
        request_id: "analysis-1".into(),
        kind: "edit".into(),
        schema: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "reason": {"type": "string"},
                "changes": {"type": "array", "maxItems": 2, "items": {"type": "string"}}
            },
            "required": ["reason", "changes"]
        }),
        prompt: "EXACT_PREPARED_ANALYSIS_PAYLOAD".into(),
        workdir: dir.clone(),
        snapshot_path: dir.join("snapshot.json"),
        app_exe: PathBuf::from(env!("CARGO_BIN_EXE_submit_mcp")),
        model: Some(("mock".into(), "mock-model".into())),
        effort: Some("high".into()),
        retrieval: false,
    };
    let prepared = analysis::prepare(&ticket_base, &spec).unwrap();
    let accepted = analysis::run(
        &client,
        &spec,
        &prepared,
        Duration::from_secs(10),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(accepted.value["reason"], "mock");
    assert_eq!(accepted.schema_hash, prepared.schema_hash);
    assert!(prepared.paths.result_path.is_file());
    assert_eq!(
        std::fs::read_to_string(&prompt_file).unwrap(),
        "EXACT_PREPARED_ANALYSIS_PAYLOAD"
    );
    let usage = accepted.prompt.usage.unwrap();
    assert_eq!(usage.total_tokens, 3);
    assert_eq!(
        accepted.prompt.model_value.as_deref(),
        Some("[\"mock\",\"mock-model\"]")
    );
    assert_eq!(accepted.prompt.effort_value.as_deref(), Some("high"));
    client.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn analysis_invalid_submit_is_not_success() {
    let dir = temp_dir("analysis-invalid");
    let mut launch = mock_launch("submit").env(
        "MOCK_SUBMIT_ARGS",
        "{\"reason\":\"ok\",\"changes\":[],\"extra\":true}",
    );
    launch.config = test_config();
    let client = AcpClient::spawn(&launch, PermissionPolicy::Surface).unwrap();
    client.initialize(Duration::from_secs(5)).unwrap();
    let spec = AnalysisSpec {
        request_id: "analysis-invalid-1".into(),
        kind: "edit".into(),
        schema: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "reason": {"type": "string"},
                "changes": {"type": "array", "maxItems": 2, "items": {"type": "string"}}
            },
            "required": ["reason", "changes"]
        }),
        prompt: "EXACT".into(),
        workdir: dir.clone(),
        snapshot_path: dir.join("snapshot.json"),
        app_exe: PathBuf::from(env!("CARGO_BIN_EXE_submit_mcp")),
        model: None,
        effort: None,
        retrieval: false,
    };
    let prepared = analysis::prepare(&dir.join("tickets"), &spec).unwrap();
    let error = analysis::run(
        &client,
        &spec,
        &prepared,
        Duration::from_secs(10),
        &mut |_| {},
    )
    .unwrap_err();
    assert!(matches!(error, AnalysisError::MissingSubmission));
    assert!(!prepared.paths.result_path.exists());
    client.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn analysis_cancelled_prompt_is_not_success_even_with_result() {
    let dir = temp_dir("analysis-cancel");
    let client = mock_client("submit-cancel", PermissionPolicy::Surface);
    client.initialize(Duration::from_secs(5)).unwrap();
    let spec = AnalysisSpec {
        request_id: "analysis-cancel-1".into(),
        kind: "edit".into(),
        schema: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "reason": {"type": "string"},
                "changes": {"type": "array", "maxItems": 2, "items": {"type": "string"}}
            },
            "required": ["reason", "changes"]
        }),
        prompt: "EXACT".into(),
        workdir: dir.clone(),
        snapshot_path: dir.join("snapshot.json"),
        app_exe: PathBuf::from(env!("CARGO_BIN_EXE_submit_mcp")),
        model: None,
        effort: None,
        retrieval: false,
    };
    let prepared = analysis::prepare(&dir.join("tickets"), &spec).unwrap();
    let error = analysis::run(
        &client,
        &spec,
        &prepared,
        Duration::from_secs(10),
        &mut |_| {},
    )
    .unwrap_err();
    assert!(matches!(error, AnalysisError::Stopped(reason) if reason == "cancelled"));
    assert!(prepared.paths.result_path.is_file());
    client.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn analysis_missing_submit_result_fails() {
    let dir = temp_dir("analysis-missing");
    let client = mock_client("missing", PermissionPolicy::Surface);
    client.initialize(Duration::from_secs(5)).unwrap();
    let spec = AnalysisSpec {
        request_id: "analysis-2".into(),
        kind: "edit".into(),
        schema: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {"reason": {"type": "string"}},
            "required": ["reason"]
        }),
        prompt: "EXACT".into(),
        workdir: dir.clone(),
        snapshot_path: dir.join("snapshot.json"),
        app_exe: PathBuf::from(env!("CARGO_BIN_EXE_submit_mcp")),
        model: None,
        effort: None,
        retrieval: false,
    };
    let prepared = analysis::prepare(&dir.join("tickets"), &spec).unwrap();
    let error = analysis::run(
        &client,
        &spec,
        &prepared,
        Duration::from_secs(10),
        &mut |_| {},
    )
    .unwrap_err();
    assert!(matches!(error, AnalysisError::MissingSubmission));
    assert!(!prepared.paths.result_path.exists());
    client.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

struct McpChild {
    child: Child,
    reader: BufReader<ChildStdout>,
}

impl McpChild {
    fn spawn(ticket: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_submit_mcp"))
            .args(["--analysis-submit-mcp", &ticket.to_string_lossy()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let reader = BufReader::new(child.stdout.take().unwrap());
        Self { child, reader }
    }

    fn send(&mut self, value: Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        serde_json::to_writer(&mut *stdin, &value).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    fn read(&mut self) -> Value {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
}

impl Drop for McpChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "Requires explicit user authorization to invoke the installed Harness"]
fn installed_runtime_composes_restricted_profile_read_only() {
    let Ok(local_app_data) = std::env::var("LOCALAPPDATA") else {
        return;
    };
    let dsh = PathBuf::from(local_app_data)
        .join("Programs")
        .join("DeepSeek Harness")
        .join("resources")
        .join("runtime")
        .join("cli")
        .join("bin")
        .join("dsh.cmd");
    if !dsh.is_file() {
        eprintln!("installed NEW desktop dsh not present; skipping runtime composition test");
        return;
    }
    let dir = temp_dir("installed-runtime");
    let profile = materialize_profile(&dir.join("assets")).unwrap();
    let home = dir.join("home");
    let profile_dir = home.join("profiles").join("acp");
    std::fs::create_dir_all(&profile_dir).unwrap();
    std::fs::write(
        profile_dir.join("package.json"),
        serde_json::to_vec_pretty(&json!({
            "name": "dsh-profile-acp",
            "private": true,
            "dependencies": {},
            "dsh": {"profile": {"bundles": [
                "@deepseek-ai/dsh-base",
                "@deepseek-ai/dsh-acp-app"
            ]}}
        }))
        .unwrap(),
    )
    .unwrap();
    let agents = dir.join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    let config = ReadinessConfig {
        dsh_cmd: dsh,
        dsh_home: home,
        agents_home: Some(agents),
        patch: profile.patch,
        workdir: Some(dir.clone()),
    };
    let readiness = check_readiness(&config).unwrap();
    assert_eq!(readiness.runtime_version, SUPPORTED_RUNTIME_VERSION);
    assert!(readiness.active_ids.iter().any(|id| id == "acp"));
    assert!(!readiness.active_ids.iter().any(|id| id == "tool-fs"));
    assert!(!config
        .dsh_home
        .join("profiles")
        .join("shishi-analysis")
        .exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn submit_mcp_protocol_accepts_one_and_rejects_duplicate() {
    let dir = temp_dir("submit-mcp");
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "reason": {"type": "string", "maxLength": 10},
            "changes": {"type": "array", "maxItems": 2, "items": {"type": "string"}}
        },
        "required": ["reason", "changes"]
    });
    let (paths, _ticket) = SubmitPaths::create(&dir, "mcp-1", "edit", schema).unwrap();
    let mut server = McpChild::spawn(&paths.ticket_path);
    server.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}));
    let init = server.read();
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
    server.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    server.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}));
    let list = server.read();
    assert_eq!(list["result"]["tools"][0]["name"], "submit_analysis");
    assert_eq!(
        list["result"]["tools"][0]["inputSchema"]["additionalProperties"],
        false
    );
    server.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"submit_analysis","arguments":{"reason":"ok","changes":["a"]}}}));
    let accepted = server.read();
    assert_eq!(accepted["result"]["isError"], false);
    assert!(paths.result_path.is_file());
    let accepted_result = paths
        .read_accepted(
            "mcp-1",
            "edit",
            &json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "reason": {"type": "string", "maxLength": 10},
                    "changes": {"type": "array", "maxItems": 2, "items": {"type": "string"}}
                },
                "required": ["reason", "changes"]
            }),
        )
        .unwrap();
    assert_eq!(accepted_result.value["reason"], "ok");

    server.send(json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"submit_analysis","arguments":{"reason":"again","changes":[]}}}));
    let duplicate = server.read();
    assert_eq!(duplicate["result"]["isError"], true);
    let _ = std::fs::remove_dir_all(dir);
}
#[test]
fn token_usage_requires_both_counters_and_does_not_confuse_context_capacity() {
    use shishi_acp::TokenUsage;
    assert!(TokenUsage::from_value(&serde_json::json!({"used":1000,"size":200000})).is_none());
    assert!(TokenUsage::from_value(&serde_json::json!({"inputTokens":12})).is_none());
    let usage =
        TokenUsage::from_value(&serde_json::json!({"inputTokens":12,"outputTokens":3})).unwrap();
    assert_eq!(usage.total_tokens, 15);
    assert_eq!(usage.cached_read_tokens, None);
}
