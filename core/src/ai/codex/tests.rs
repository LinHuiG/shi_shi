use std::io::{Read, Write};
use std::net::TcpListener;

use super::*;
use crate::ai::native::{self, Round, Rounds};
use crate::ai::prompt::Route;
use crate::config::{Config, Connection as AiConnection};

#[test]
fn composer_encodes_url_bytes() {
    assert_eq!(encode("a b/中"), "a%20b%2F%E4%B8%AD");
    assert_eq!(encode("AZaz09-._~'&?%\\"), "AZaz09-._~%27%26%3F%25%5C");
}

fn route(executable: &str) -> Route {
    let mut connection = AiConnection::default();
    connection.executable = executable.into();
    Route { kind: "classify".into(), connection, model: "synthetic".into(), effort: String::new() }
}

fn job(route: &Route, root: &Path) -> Job {
    let prepared = crate::ai::prompt::Prepared {
        prompt: "合成输入".into(),
        schema: json!({"type": "object"}),
        ..Default::default()
    };
    crate::ai::provider::fake::job(route.clone(), Config::default(), prepared, root.to_path_buf())
}

fn jwt(claims: Value) -> String {
    use base64::Engine;
    format!("h.{}.s", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string()))
}

/// Answer one request per connection in order; the raw requests come back.
fn serve(responses: Vec<(&'static str, String)>) -> (String, std::sync::mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for (kind, body) in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let mut seen = Vec::new();
            let mut buf = [0u8; 65536];
            loop {
                let n = stream.read(&mut buf).unwrap();
                seen.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&seen).to_string();
                if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                    let length = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase().strip_prefix("content-length: ").map(|v| v.parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if rest.len() >= length {
                        break;
                    }
                }
            }
            tx.send(String::from_utf8_lossy(&seen).to_string()).unwrap();
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).unwrap();
        }
    });
    (url, rx)
}

fn backend<'a>(home: &Path, url: &'a str) -> Backend<'a> {
    Backend { home: home.into(), chatgpt: url, openai: url, token: url }
}

#[test]
fn a_chatgpt_login_refreshes_near_expiry_and_keeps_the_rest_of_auth_json() {
    let home = crate::testing::temp_dir("codex-login");
    let soon = chrono::Utc::now().timestamp() + 60;
    let auth = json!({"auth_mode": "chatgpt", "OPENAI_API_KEY": null, "extra": 1, "tokens": {
        "access_token": jwt(json!({"exp": soon})), "refresh_token": "r-old", "account_id": null,
        "id_token": jwt(json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acct-1", "chatgpt_plan_type": "plus"}}))}});
    fs::write(home.join("auth.json"), auth.to_string()).unwrap();
    let fresh = jwt(json!({"exp": soon + 3600}));
    let (url, requests) =
        serve(vec![("application/json", json!({"access_token": fresh, "refresh_token": "r-new"}).to_string())]);
    let Login::ChatGpt { token, account } = login(&backend(&home, &url)).unwrap() else { panic!("ChatGPT login") };
    assert_eq!((token.as_str(), account.as_str()), (fresh.as_str(), "acct-1"));
    let sent = requests.recv().unwrap();
    assert!(
        sent.contains("\"refresh_token\":\"r-old\"")
            && sent.contains(CLIENT_ID)
            && sent.contains("originator: codex_cli_rs"),
        "{sent}"
    );
    let saved: Value = serde_json::from_str(&fs::read_to_string(home.join("auth.json")).unwrap()).unwrap();
    assert_eq!((saved["extra"].as_i64(), saved["tokens"]["refresh_token"].as_str()), (Some(1), Some("r-new")));
    assert!(saved["last_refresh"].is_string());
    assert_eq!(status_text(Some(saved)), "已用 ChatGPT 登录（plus）");

    // A token far from expiry is used as is; an API-key login needs no tokens.
    let Login::ChatGpt { token: again, .. } = login(&backend(&home, "https://unused.example")).unwrap() else {
        panic!()
    };
    assert_eq!(again, fresh);
    fs::write(home.join("auth.json"), json!({"auth_mode": "apikey", "OPENAI_API_KEY": "sk-synthetic"}).to_string())
        .unwrap();
    assert!(matches!(login(&backend(&home, "https://unused.example")).unwrap(), Login::Key(k) if k == "sk-synthetic"));
    assert_eq!(status_text(None), NOT_SIGNED_IN);
    fs::remove_file(home.join("auth.json")).unwrap();
    assert_eq!(login(&backend(&home, "https://unused.example")).err().unwrap(), NOT_SIGNED_IN);
}

#[test]
fn analysis_calls_the_responses_backend_with_the_models_instructions() {
    let home = crate::testing::temp_dir("codex-analyze");
    let token = jwt(json!({"exp": chrono::Utc::now().timestamp() + 3600}));
    fs::write(home.join("auth.json"), json!({"tokens": {"access_token": token, "account_id": "acct-9"}}).to_string())
        .unwrap();
    let models = json!({"models": [{"slug": "gpt-test", "display_name": "GPT Test", "priority": 1,
        "supported_reasoning_levels": [{"effort": "low"}], "base_instructions": "CODEX-INSTRUCTIONS"}]});
    let sse = [json!({"type": "response.output_text.delta", "delta": "{\"ok\":true}"}),
        json!({"type": "response.completed", "response": {"usage": {"input_tokens": 5, "output_tokens": 2, "total_tokens": 7}}})]
        .iter().map(|e| format!("data: {e}\n\n")).collect::<String>();
    let (url, requests) = serve(vec![("application/json", models.to_string()), ("text/event-stream", sse)]);
    let route = Route { model: "gpt-test".into(), ..route("") };
    *CATALOG.lock().unwrap() = Vec::new();
    let (value, usage) = analyze(&backend(&home, &url), &job(&route, &home), None, &mut |_| {}).unwrap();
    assert_eq!((value["ok"].as_bool(), usage["input_tokens"].as_u64()), (Some(true), Some(5)));
    let listed = requests.recv().unwrap();
    assert!(
        listed.starts_with("GET /models?client_version=0.159.0") && listed.contains("chatgpt-account-id: acct-9"),
        "{listed}"
    );
    let sent = requests.recv().unwrap();
    assert!(
        sent.starts_with("POST /responses")
            && sent.contains("CODEX-INSTRUCTIONS")
            && sent.contains("\"model\":\"gpt-test\""),
        "{sent}"
    );
    assert!(sent.contains("originator: codex_cli_rs") && sent.contains("user-agent: codex_cli_rs/0.159.0"), "{sent}");
    *CATALOG.lock().unwrap() = Vec::new();
}

#[test]
fn launchers_resolve_to_native_programs() {
    assert_eq!(validate("turbo").unwrap_err(), "无效的推理强度");
    assert_eq!(
        resolve_cli("/nonexistent/codex.exe").unwrap_err(),
        "未找到 Codex CLI，请选择 codex.exe 或 npm 安装的 codex.cmd"
    );
    let dir = crate::testing::temp_dir("codex-resolve");
    let other = dir.join("tool.bat");
    fs::write(&other, "").unwrap();
    assert_eq!(resolve_cli(&other.to_string_lossy()).unwrap_err(), "请选择 Codex 原生程序或 npm 的 Codex 启动文件");
    let npm = dir.join("codex.cmd");
    fs::write(&npm, "").unwrap();
    assert!(resolve_cli(&npm.to_string_lossy()).unwrap_err().starts_with("未找到 npm Codex 的 Windows x64 原生程序"));
    let native = dir.join("node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/codex");
    fs::create_dir_all(&native).unwrap();
    fs::write(native.join("codex.exe"), "").unwrap();
    assert!(resolve_cli(&npm.to_string_lossy()).unwrap().ends_with("codex.exe"));
    assert!(bundled_version(std::ffi::OsStr::new("0123abcd")) && !bundled_version(std::ffi::OsStr::new("v1")));
}

/// Replays results and keeps every round's prompt.
struct Scripted(Vec<Value>, Vec<String>);

impl Rounds for Scripted {
    fn round(&mut self, _: &Job, round: Round<'_>, _: &mut dyn FnMut(Event)) -> Reply {
        assert!(round.schema["properties"]["tool_call"].is_object(), "every round carries the envelope");
        self.1.push(round.prompt.into());
        Ok((self.0.remove(0), json!({"input_tokens": 3, "output_tokens": 1})))
    }
}

#[test]
fn native_rounds_send_an_empty_envelope_back_once() {
    let dir = crate::testing::temp_dir("native-rounds");
    let request = job(&route("codex"), &dir);
    let empty = || json!({"tool_call": null, "result": null});
    let mut rounds = Scripted(vec![empty(), json!({"tool_call": null, "result": {"ok": 1}})], vec![]);
    let (value, _) = native::analyze(&request, None, &mut |_| {}, "Codex", &mut rounds).unwrap();
    assert_eq!(value["ok"], 1);
    let mut rounds = Scripted(vec![empty(), empty(), empty()], vec![]);
    let error = native::analyze(&request, None, &mut |_| {}, "Codex", &mut rounds).unwrap_err();
    assert!(error.contains("模型必须返回一次工具请求或最终结果"), "{error}");
    assert_eq!(rounds.0.len(), 1, "only one correction is asked");

    // A purpose rule failure goes back with its path.
    let mut format = route("codex");
    format.kind = "format".into();
    let request = job(&format, &dir);
    let tasks = |description: &str| json!({"tool_call": null, "result": {"tasks": [{"title": "合成周报", "description": description}]}});
    let mut rounds = Scripted(vec![tasks(" "), tasks("周五前提交")], vec![]);
    let (value, _) = native::analyze(&request, None, &mut |_| {}, "Codex", &mut rounds).unwrap();
    assert_eq!(value["tasks"][0]["description"], "周五前提交");
    assert!(rounds.1[1].contains("$.tasks[0].description: description must not be blank"), "{}", rounds.1[1]);
    let request = job(&route("codex"), &dir);

    let mut rounds = Scripted(vec![json!({"tool_call": null, "result": {"ok": 1}})], vec![]);
    let mut usage = vec![];
    let (value, _) = native::analyze(
        &request,
        None,
        &mut |e| {
            if let Event::Usage(u) = e {
                usage.push(u)
            }
        },
        "Codex",
        &mut rounds,
    )
    .unwrap();
    assert_eq!(value["ok"], 1);
    assert_eq!(usage.last().unwrap()["input_tokens"], 3);
    assert!(rounds.0.is_empty());

    let mut rounds = Scripted(vec![json!({"result": {}})], vec![]);
    assert_eq!(
        native::analyze(&request, None, &mut |_| {}, "Codex", &mut rounds).unwrap_err(),
        "模型未返回有效的结构化调用或最终结果"
    );
}

#[test]
fn projects_come_from_the_newest_app_server_layout() {
    let home = crate::testing::temp_dir("codex-projects");
    assert_eq!(projects_in(&home.join("missing")).unwrap(), Vec::<Value>::new());
    let work = home.join("work");
    fs::create_dir_all(&work).unwrap();
    let make = |file: &str, name: &str| {
        let conn = rusqlite::Connection::open(home.join(file)).unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (id TEXT, name TEXT, position INTEGER, created_at_ms INTEGER);
             CREATE TABLE project_roots (project_id TEXT, position INTEGER, path TEXT);",
        )
        .unwrap();
        for (i, (id, path)) in [("p1", work.to_str().unwrap()), ("p2", "/synthetic/gone")].iter().enumerate() {
            conn.execute("INSERT INTO projects VALUES (?1, ?2, ?3, 0)", rusqlite::params![id, name, i as i64]).unwrap();
            conn.execute("INSERT INTO project_roots VALUES (?1, 0, ?2)", [id, path]).unwrap();
        }
    };
    make("state_4.sqlite", "旧");
    make("state_5.sqlite", "新");
    let rows = projects_in(&home).unwrap();
    assert_eq!(rows, vec![json!({"id": "codex:p1", "name": "新", "cwd": work.to_str().unwrap()})]);
    fs::remove_dir_all(home).unwrap();
}
