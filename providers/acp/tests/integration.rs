use serde_json::{json, Value};
use shishi_acp::client::{
    AcpClient, AcpEvent, AcpLaunch, ClientError, PendingPermission, PermissionPolicy, SessionUpdate,
};
use shishi_acp::dsh::{check_readiness, materialize_profile};
use shishi_acp::submit::{self as analysis, AnalysisError, AnalysisSpec, SubmitPaths};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const RPC_TIMEOUT: Duration = Duration::from_secs(5);

fn temp_dir(label: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
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

/// The synthetic agent and submit server in `tests/support` are examples,
/// which `cargo test` builds next to the test's own directory.
fn example(name: &str) -> PathBuf {
    let deps = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
    deps.parent().unwrap().join("examples").join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

fn mock_launch(scenario: &str) -> AcpLaunch {
    AcpLaunch::new(example("mock_agent")).env("MOCK_SCENARIO", scenario)
}

fn mock_client(scenario: &str, policy: PermissionPolicy) -> AcpClient {
    AcpClient::spawn(&mock_launch(scenario), policy).unwrap()
}

fn mock_session(launch: AcpLaunch, dir: &Path, policy: PermissionPolicy) -> AcpClient {
    let client = AcpClient::spawn(&launch, policy).unwrap();
    client.initialize(RPC_TIMEOUT).unwrap();
    client.new_session(dir, &[], RPC_TIMEOUT).unwrap();
    client
}

fn wait_for_permission(client: &AcpClient) -> PendingPermission {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(pending) = client.permissions_for("mock-session-1").pop() {
            return pending;
        }
        assert!(Instant::now() < deadline, "permission request was not surfaced");
        thread::sleep(Duration::from_millis(10));
    }
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
    let launch = mock_launch("happy").env("MOCK_PROMPT_FILE", &prompt_file).env("MOCK_NOTIFICATIONS", json!([
        {"jsonrpc":"2.0","method":"unknown/notification","params":{"ignored":true}},
        {"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"other","update":{"sessionUpdate":"config_option_update","configOptions":[]}}},
        {"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"mock-session-1","update":{"sessionUpdate":"future_update","payload":"ignored"}}},
        {"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"mock-session-1","update":{"sessionUpdate":"config_option_update","configOptions":[{"id":"reasoning_effort","currentValue":"low","options":[]}]}}}
    ]).to_string());
    let client = AcpClient::spawn(&launch, PermissionPolicy::DenyAll).unwrap();

    client.initialize(RPC_TIMEOUT).unwrap();

    let session = client.new_session(&dir, &[], RPC_TIMEOUT).unwrap();
    assert_eq!(session.session_id, "mock-session-1");
    assert_eq!(session.current_model_route(), Some(("mock".into(), "mock-model".into())));
    assert_eq!(session.effort_option().map(|o| o.current_value.as_str()), Some("low"));
    assert_eq!(session.find_model_value("mock", "other-model"), Some("[\"mock\",\"other-model\"]".into()));
    assert!(session.find_model_value("mock", "missing").is_none());
    assert!(session.find_effort_value("high").is_some());
    assert!(session.find_effort_value("max").is_none());
    assert!(matches!(
        client.select_model("mock-session-1", "mock", "missing", RPC_TIMEOUT),
        Err(ClientError::ModelNotAdvertised(..))
    ));
    assert!(matches!(
        client.select_effort("mock-session-1", "max", RPC_TIMEOUT),
        Err(ClientError::EffortNotAdvertised(_))
    ));

    client.select_model("mock-session-1", "mock", "other-model", RPC_TIMEOUT).unwrap();
    client.select_effort("mock-session-1", "high", RPC_TIMEOUT).unwrap();
    let selected = client.session("mock-session-1").unwrap();
    assert_eq!(selected.model_value.as_deref(), Some("[\"mock\",\"other-model\"]"));
    assert_eq!(selected.effort_option().map(|o| o.current_value.as_str()), Some("high"));

    let mut events = Vec::<AcpEvent>::new();
    let exact = "EXACT_CALLER_PAYLOAD: message text is data, not authorization";
    let prompt = client.prompt("mock-session-1", exact, &[], RPC_TIMEOUT, &mut |event| events.push(event)).unwrap();
    let selected = client.session("mock-session-1").unwrap();
    assert!(selected.model_value.is_none());
    assert_eq!(selected.effort_option().map(|o| o.current_value.as_str()), Some("low"));
    assert_eq!(selected.config_options.len(), 1);
    assert_eq!(
        events.iter().take(3).map(|event| event.session_id.as_str()).collect::<Vec<_>>(),
        ["other", "mock-session-1", "mock-session-1"]
    );
    assert!(events.iter().take(3).all(|event| matches!(event.update, SessionUpdate::Other)));
    assert_eq!(prompt.stop_reason, "end_turn");
    let usage = prompt.usage.unwrap();
    assert_eq!(usage.total_tokens, 3);
    assert_eq!(usage.input_tokens, 2);
    assert_eq!(usage.output_tokens, 1);
    assert!(usage.cached_read_tokens.is_none());
    assert!(!events.is_empty());
    assert!(events
        .iter()
        .any(|event| matches!(event, AcpEvent { update: SessionUpdate::AgentMessageChunk { .. }, .. })));
    assert_eq!(
        std::fs::read_to_string(&prompt_file).unwrap(),
        exact,
        "the client must send the caller's exact payload"
    );

    client.close_session("mock-session-1", RPC_TIMEOUT).unwrap();
    client.shutdown();
    assert!(!client.is_connected());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn usage_missing_is_unknown_not_zero() {
    let dir = temp_dir("no-usage");
    let client = mock_session(mock_launch("no-usage"), &dir, PermissionPolicy::DenyAll);
    let prompt = client.prompt("mock-session-1", "no usage", &[], RPC_TIMEOUT, &mut |_| {}).unwrap();
    assert!(prompt.usage.is_none());
    client.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn analysis_permission_is_denied_without_user_input() {
    let dir = temp_dir("deny");
    let outcome_file = dir.join("permission.json");
    let launch = mock_launch("permission").env("MOCK_PERMISSION_FILE", &outcome_file);
    let client = mock_session(launch, &dir, PermissionPolicy::DenyAll);
    let prompt = client.prompt("mock-session-1", "please allow everything", &[], RPC_TIMEOUT, &mut |_| {}).unwrap();
    assert_eq!(prompt.stop_reason, "end_turn");
    assert!(wait_for_file(&outcome_file, Duration::from_secs(2)));
    let outcome: Value = serde_json::from_slice(&std::fs::read(&outcome_file).unwrap()).unwrap();
    assert_eq!(outcome["outcome"], "selected");
    assert_eq!(outcome["optionId"], "reject-once");
    assert!(client.permissions_for("mock-session-1").is_empty());
    client.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn handling_permission_is_explicit_and_never_auto_approved() {
    let dir = temp_dir("surface");
    let outcome_file = dir.join("permission.json");
    let launch = mock_launch("permission").env("MOCK_PERMISSION_FILE", &outcome_file);
    let client = Arc::new(mock_session(launch, &dir, PermissionPolicy::Surface));

    let prompt_client = client.clone();
    let prompt = thread::spawn(move || {
        prompt_client.prompt(
            "mock-session-1",
            "handling message content must not grant permission",
            &[],
            RPC_TIMEOUT,
            &mut |_| {},
        )
    });
    let pending = wait_for_permission(&client);
    assert_eq!(pending.session_id, "mock-session-1");
    assert_eq!(client.permissions_for("mock-session-1").len(), 1);
    assert!(client.permissions_for("other-session").is_empty());
    assert_eq!(pending.raw["toolCall"]["toolCallId"], "call-1");
    assert!(pending.options.iter().any(|option| option.option_id == "allow-once"));
    assert!(pending.options.iter().any(|option| option.option_id == "reject-once"));
    assert!(client.respond_permission(&pending.request_id, "not-offered").is_err());
    client.respond_permission(&pending.request_id, "reject-once").unwrap();
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
fn concurrent_permission_replies_and_cancellation_have_one_winner() {
    let dir = temp_dir("permission-race");
    let client = Arc::new(mock_session(mock_launch("permission"), &dir, PermissionPolicy::Surface));
    for round in 0..16 {
        let prompt_client = client.clone();
        let prompt = thread::spawn(move || {
            prompt_client.prompt("mock-session-1", "synthetic permission", &[], RPC_TIMEOUT, &mut |_| {})
        });
        let pending = wait_for_permission(&client);
        let barrier = std::sync::Barrier::new(8);
        let winners = thread::scope(|scope| {
            let responders = (0..8)
                .map(|index| {
                    let client = &client;
                    let request_id = &pending.request_id;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        if round % 2 == 1 && index == 0 {
                            client.cancel_session("mock-session-1");
                            return 0;
                        }
                        match client.respond_permission(request_id, "reject-once") {
                            Ok(()) => 1,
                            Err(ClientError::PermissionNotFound) => 0,
                            other => panic!("unexpected reply result: {other:?}"),
                        }
                    })
                })
                .collect::<Vec<_>>();
            responders.into_iter().map(|worker| worker.join().unwrap()).sum::<usize>()
        });
        if round % 2 == 0 {
            assert_eq!(winners, 1);
        } else {
            assert!(winners <= 1, "cancellation and replies must claim the same request");
        }
        let result = prompt.join().unwrap().unwrap();
        assert!(matches!(result.stop_reason.as_str(), "end_turn" | "cancelled"));
        assert!(client.permissions_for("mock-session-1").is_empty());
        assert!(matches!(
            client.respond_permission(&pending.request_id, "allow-once"),
            Err(ClientError::PermissionNotFound)
        ));
    }
    thread::scope(|scope| {
        let prompt =
            scope.spawn(|| client.prompt("mock-session-1", "abort pending approval", &[], RPC_TIMEOUT, &mut |_| {}));
        let pending = wait_for_permission(&client);
        scope.spawn(|| client.abort());
        client.abort();
        assert!(matches!(prompt.join().unwrap(), Err(ClientError::Disconnected(_))));
        assert!(!client.is_connected());
        assert!(client.permissions_for("mock-session-1").is_empty());
        assert!(matches!(
            client.respond_permission(&pending.request_id, "allow-once"),
            Err(ClientError::PermissionNotFound)
        ));
    });
    client.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn timeout_is_ambiguous_and_does_not_resubmit() {
    let dir = temp_dir("timeout");
    let cancel_file = dir.join("cancel.txt");
    let count_file = dir.join("prompt-count.txt");
    let launch = mock_launch("hang").env("MOCK_CANCEL_FILE", &cancel_file).env("MOCK_PROMPT_COUNT_FILE", &count_file);
    let client = mock_session(launch, &dir, PermissionPolicy::DenyAll);
    let error = client.prompt("mock-session-1", "hang", &[], Duration::from_millis(350), &mut |_| {}).unwrap_err();
    match error {
        ClientError::Timeout(method) => assert_eq!(method, "session/prompt"),
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
    let client = mock_session(launch, &dir, PermissionPolicy::DenyAll);
    let error = client.prompt("mock-session-1", "exit now", &[], RPC_TIMEOUT, &mut |_| {}).unwrap_err();
    assert!(matches!(error, ClientError::Disconnected(_)));
    assert_eq!(std::fs::read_to_string(&count_file).unwrap().trim(), "1");
    client.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn unknown_server_request_is_denied_and_prompt_completes() {
    let dir = temp_dir("server-request");
    let response_file = dir.join("server-request.json");
    let launch = mock_launch("server-request").env("MOCK_SERVER_REQUEST_FILE", &response_file);
    let client = mock_session(launch, &dir, PermissionPolicy::DenyAll);
    let prompt = client.prompt("mock-session-1", "do not read files", &[], RPC_TIMEOUT, &mut |_| {}).unwrap();
    assert_eq!(prompt.stop_reason, "end_turn");
    assert!(wait_for_file(&response_file, Duration::from_secs(2)));
    let response: Value = serde_json::from_slice(&std::fs::read(&response_file).unwrap()).unwrap();
    assert_eq!(response["error"]["code"], -32601);
    client.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn resume_and_close_require_advertised_capabilities() {
    let dir = temp_dir("resume");
    let client = mock_session(mock_launch("happy"), &dir, PermissionPolicy::DenyAll);
    let resumed = client.resume_session("mock-session-9", &dir, &[], RPC_TIMEOUT).unwrap();
    assert_eq!(resumed.session_id, "mock-session-9");
    client.close_session("mock-session-9", RPC_TIMEOUT).unwrap();
    client.shutdown();

    let client = mock_client("no-resume", PermissionPolicy::DenyAll);
    client.initialize(RPC_TIMEOUT).unwrap();
    assert!(matches!(
        client.resume_session("mock-session-1", &dir, &[], RPC_TIMEOUT),
        Err(ClientError::Unsupported(_))
    ));
    client.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn invalid_initialize_frames_fail_closed() {
    for scenario in ["malformed", "oversized", "bad-version"] {
        let client = mock_client(scenario, PermissionPolicy::DenyAll);
        let error = client.initialize(RPC_TIMEOUT).unwrap_err();
        match (scenario, error) {
            ("bad-version", ClientError::Unsupported(message)) => {
                assert!(message.contains("version 2"))
            }
            (_, ClientError::Protocol(_)) => {
                assert_ne!(scenario, "bad-version")
            }
            (_, error) => panic!("{scenario}: {error:?}"),
        }
        client.shutdown();
    }
}

#[cfg(windows)]
#[test]
fn windows_job_reaps_descendants_on_normal_and_forced_shutdown() {
    fn pid_alive(pid: u32) -> bool {
        use windows_sys::Win32::{Foundation::*, System::Threading::*};
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
            let mut code = 0;
            let alive = !handle.is_null() && GetExitCodeProcess(handle, &mut code) != 0 && code == 259;
            CloseHandle(handle);
            alive
        }
    }

    for close in [AcpClient::shutdown, AcpClient::abort] {
        let dir = temp_dir("job");
        let pid_file = dir.join("child.pid");
        let launch = mock_launch("job").env("MOCK_CHILD_PID_FILE", &pid_file);
        let client = mock_session(launch, &dir, PermissionPolicy::DenyAll);
        client.prompt("mock-session-1", "spawn child", &[], RPC_TIMEOUT, &mut |_| {}).unwrap();
        assert!(wait_for_file(&pid_file, Duration::from_secs(3)));
        let pid: u32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        assert!(pid_alive(pid), "grandchild should be alive before shutdown");
        close(&client);
        assert!(!client.is_connected());
        close(&client);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && pid_alive(pid) {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(!pid_alive(pid), "grandchild survived job shutdown");
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(windows)]
#[test]
fn cmd_wrapper_launcher_is_quoted() {
    let dir = temp_dir("cmd wrapper with spaces");
    let wrapper = dir.join("mock dsh.cmd");
    let exe = example("mock_agent");
    std::fs::write(&wrapper, format!("@echo off\r\n\"{}\" %*\r\n", exe.display())).unwrap();
    let launch = AcpLaunch::new(&wrapper)
        .args(["--profile", "acp", "--patch", "C:\\path with space\\patch.yml"])
        .env("MOCK_SCENARIO", "happy");
    let client = AcpClient::spawn(&launch, PermissionPolicy::DenyAll).unwrap();
    client.initialize(RPC_TIMEOUT).unwrap();
    client.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(windows)]
#[test]
fn cmd_wrapper_keeps_percent_exclamation_and_shell_operators_literal() {
    let dir = temp_dir("cmd %PATH% !literal! & fixture");
    let wrapper = dir.join("mock %PATH% !launcher!.cmd");
    let exe = example("mock_agent");
    // No @echo off: /Q must keep command echo off the ACP stdout channel.
    std::fs::write(&wrapper, format!("\"{}\" %*\r\n", exe.display())).unwrap();
    let launch = AcpLaunch::new(&wrapper)
        .args(["--patch", "C:\\fixture %PATH% !literal! & (name)\\patch.yml", "& echo injected"])
        .env("MOCK_SCENARIO", "happy");
    let client = AcpClient::spawn(&launch, PermissionPolicy::DenyAll).unwrap();
    client.initialize(RPC_TIMEOUT).unwrap();
    client.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn analysis_submission_contract() {
    for scenario in ["accepted", "close-error", "no-close", "invalid", "cancelled", "missing"] {
        let dir = temp_dir(&format!("analysis-{scenario}"));
        let prompt_file = dir.join("prompt.txt");
        let requests_file = dir.join("requests.jsonl");
        let accepted_scenario = matches!(scenario, "accepted" | "close-error" | "no-close");
        let wire_scenario = match scenario {
            "cancelled" => "submit-cancel",
            "missing" => "missing",
            _ => "submit",
        };
        let mut launch = mock_launch(wire_scenario)
            .env("MOCK_PROMPT_FILE", &prompt_file)
            .env("MOCK_REQUESTS_FILE", &requests_file)
            .env(
                "MOCK_CLOSE_MODE",
                match scenario {
                    "close-error" => "error",
                    "no-close" => "unsupported",
                    _ => "",
                },
            );
        if scenario == "invalid" {
            launch = launch.env("MOCK_SUBMIT_ARGS", r#"{"reason":"ok","changes":[],"extra":true}"#);
        }
        let client = AcpClient::spawn(&launch, PermissionPolicy::DenyAll).unwrap();
        client.initialize(RPC_TIMEOUT).unwrap();
        let spec = AnalysisSpec {
            images: vec![],
            request_id: format!("analysis-{scenario}"),
            kind: "edit".into(),
            schema: json!({"type":"object","additionalProperties":false,"properties":{
                "reason":{"type":"string"},"changes":{"type":"array","maxItems":2,"items":{"type":"string"}}
            },"required":["reason","changes"]}),
            prompt: "EXACT_PREPARED_ANALYSIS_PAYLOAD".into(),
            workdir: dir.clone(),
            app_exe: example("submit_mcp"),
            model: accepted_scenario.then(|| ("mock".into(), "mock-model".into())),
            effort: accepted_scenario.then(|| "high".into()),
        };
        let prepared = analysis::prepare(&dir.join("tickets"), &spec).unwrap();
        let result = analysis::run(&client, &spec, &prepared, Duration::from_secs(10), &mut |_| {});
        assert_eq!(prepared.paths.result_path.is_file(), accepted_scenario || scenario == "cancelled", "{scenario}");
        match scenario {
            _ if accepted_scenario => {
                let accepted = result.unwrap();
                assert_eq!(accepted.value["reason"], "mock");
                assert_eq!(accepted.session_id, "mock-session-1");
                assert_eq!(accepted.session_closed, scenario == "accepted");
                assert_eq!(std::fs::read_to_string(&prompt_file).unwrap(), spec.prompt);
                assert_eq!(accepted.prompt.usage.unwrap().total_tokens, 3);
            }
            "cancelled" => assert!(matches!(result, Err(AnalysisError::Stopped(reason)) if reason == "cancelled")),
            _ => assert!(matches!(result, Err(AnalysisError::MissingSubmission)), "{scenario}: {result:?}"),
        }
        let requests = std::fs::read_to_string(requests_file)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        let mut expected = vec!["initialize", "session/new"];
        if accepted_scenario {
            expected.extend(["session/set_config_option", "session/set_config_option"]);
            assert_eq!(
                requests[2]["params"],
                json!({"sessionId":"mock-session-1", "configId":"model", "value":"[\"mock\",\"mock-model\"]"})
            );
            assert_eq!(
                requests[3]["params"],
                json!({"sessionId":"mock-session-1", "configId":"reasoning_effort", "value":"high"})
            );
        }
        expected.push("session/prompt");
        if scenario != "no-close" {
            expected.push("session/close");
        }
        assert_eq!(requests.iter().map(|r| r["method"].as_str().unwrap()).collect::<Vec<_>>(), expected, "{scenario}");
        client.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }
}

struct McpChild {
    child: Child,
    reader: BufReader<ChildStdout>,
}

impl McpChild {
    fn spawn(ticket: &Path) -> Self {
        let mut child = Command::new(example("submit_mcp"))
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
    let home = dir.join("home");
    let profile = materialize_profile(&dir.join("assets"), &home).unwrap();
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
    let config = profile.readiness(&dsh, &home, &dir);
    let readiness = check_readiness(&config).unwrap();
    assert!(!readiness.runtime_version.is_empty());
    assert!(readiness.active_ids.iter().any(|id| id == "acp"));
    assert!(!readiness.active_ids.iter().any(|id| id == "tool-fs"));
    assert!(!home.join("profiles").join("shishi-analysis").exists());
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
    let paths = SubmitPaths::create(&dir, "mcp-1", "edit", schema).unwrap();
    let mut server = McpChild::spawn(&paths.ticket_path);
    server.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}));
    let init = server.read();
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
    server.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    server.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}));
    let list = server.read();
    assert_eq!(list["result"]["tools"][0]["name"], "submit_analysis");
    assert_eq!(list["result"]["tools"][0]["inputSchema"]["additionalProperties"], false);
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
    assert_eq!(accepted_result["reason"], "ok");

    server.send(json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"submit_analysis","arguments":{"reason":"again","changes":[]}}}));
    let duplicate = server.read();
    assert_eq!(duplicate["result"]["isError"], true);
    let _ = std::fs::remove_dir_all(dir);
}
#[test]
fn token_usage_requires_both_counters_and_does_not_confuse_context_capacity() {
    use shishi_acp::TokenUsage;
    for value in [
        json!({"used":1000,"size":200000}),
        json!({"inputTokens":12}),
        json!({"totalTokens":15,"outputTokens":3}),
        json!({"inputTokens":u64::MAX,"outputTokens":1}),
    ] {
        assert!(TokenUsage::from_value(&value).is_none(), "{value}");
    }
    let usage = TokenUsage::from_value(&serde_json::json!({"inputTokens":12,"outputTokens":3})).unwrap();
    assert_eq!(usage.total_tokens, 15);
    assert_eq!(usage.cached_read_tokens, None);
}
