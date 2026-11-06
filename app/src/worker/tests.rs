use super::*;

#[test]
fn framing_skips_foreign_lines_and_rejects_oversized_messages() {
    let request =
        Request { id: "r1".into(), manifest: "plugin.json".into(), settings: true, input: json!({"method": "m"}) };
    let mut bytes = b"partial".to_vec();
    write_line(&mut bytes, &request).unwrap();
    let mut reader = bytes.as_slice();
    assert_eq!(read_line(&mut reader).unwrap().unwrap(), "partial\n");
    let line = read_line(&mut reader).unwrap().unwrap();
    assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), request);
    assert!(read_line(&mut reader).unwrap().is_none());
    assert_eq!(parse_response(r#"{"id":"r1","ok":true,"result":1}"#, "r1").unwrap()["result"], 1);
    assert!(parse_response(r#"{"id":"r1","ok":true}"#, "r2").is_none());
    assert!(parse_response(r#"{"id":"r1"}"#, "r1").is_none());
    assert!(parse_response("plugin log text", "r1").is_none());
    assert!(serde_json::from_str::<Request>(r#"{"id":"a","manifest":"m","settings":false,"input":{},"x":1}"#).is_err());
    assert!(write_line(&mut Vec::new(), &"x".repeat(MAX as usize)).is_err());
}

#[test]
fn serve_answers_each_request_until_input_ends() {
    let mut input = Vec::new();
    for (id, settings) in [("a", false), ("b", true)] {
        let request = Request { id: id.into(), manifest: "m".into(), settings, input: json!({"method": "fetch"}) };
        write_line(&mut input, &request).unwrap();
    }
    let mut output = Vec::new();
    serve(input.as_slice(), &mut output, &mut |r| {
        if r.settings {
            Err("拒绝".into())
        } else {
            Ok(r.input["method"].clone())
        }
    })
    .unwrap();
    let text = String::from_utf8(output).unwrap();
    let a = text.lines().find_map(|l| parse_response(l, "a")).unwrap();
    let b = text.lines().find_map(|l| parse_response(l, "b")).unwrap();
    assert_eq!((a["ok"].clone(), a["result"].clone()), (json!(true), json!("fetch")));
    assert_eq!((b["ok"].clone(), b["error"].clone()), (json!(false), json!("拒绝")));
}

/// A synthetic worker: answers `ok` with its pid for method `ok`, hangs
/// for `hang`, closes its stdout but keeps running for `close`, and exits
/// for anything else. `exec` keeps the hang in the killed process.
#[cfg(unix)]
fn script() -> Result<Command, String> {
    let mut command = Command::new("sh");
    command.args([
        "-c",
        r#"echo noise; while read -r line; do
        [ -z "$line" ] && continue
        id=$(printf '%s' "$line" | sed 's/^{"id":"\([^"]*\)".*/\1/')
        case "$line" in
          *'"method":"ok"'*) printf '\n{"id":"%s","ok":true,"result":"%s"}\n' "$id" "$$" ;;
          *'"method":"hang"'*) exec sleep 30 ;;
          *'"method":"close"'*) exec sleep 30 >&- ;;
          *) exit 7 ;;
        esac
      done"#,
    ]);
    Ok(command)
}

#[cfg(unix)]
fn client(command: fn() -> Result<Command, String>, timeout: Duration) -> Client {
    Client { worker: Mutex::new(None), command, timeout }
}

#[cfg(unix)]
fn call(client: &Client, input: Value, cancel: Option<&AtomicBool>) -> Result<Value, String> {
    client.call(Path::new("p/plugin.json"), false, input, cancel)
}

#[cfg(unix)]
#[test]
fn one_worker_is_reused_and_a_failed_call_rebuilds_it() {
    let client = client(script, CALL_TIMEOUT);
    let ok = || call(&client, json!({"method": "ok"}), None).unwrap();
    let first = ok();
    assert_eq!(ok(), first, "the same process serves serial calls");
    let error = call(&client, json!({"method": "crash"}), None).unwrap_err();
    assert!(error.contains("退出码") && error.contains("工作进程已回收"), "{error}");
    let next = ok();
    assert_ne!(next, first, "the next call starts a new worker");
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(300));
            cancel.store(true, Ordering::Release);
        });
        assert!(call(&client, json!({"method": "hang"}), Some(&cancel)).unwrap_err().contains("已取消"));
    });
    assert!(started.elapsed() < Duration::from_secs(5));
    let after_cancel = ok();
    assert_ne!(after_cancel, next, "a cancelled call ends its worker");
    let started = Instant::now();
    let error = call(&client, json!({"method": "close"}), None).unwrap_err();
    assert!(error.contains("输出已关闭") && !error.contains("退出码"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(5), "a live worker without stdout is not waited on");
    assert_ne!(ok(), after_cancel, "a worker that closed its output is replaced");
}

#[cfg(unix)]
#[test]
fn the_time_limit_covers_a_worker_that_stops_reading_or_answering() {
    fn deaf() -> Result<Command, String> {
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 30"]);
        Ok(command)
    }
    let started = Instant::now();
    // Far more than a pipe buffer: the write itself cannot finish.
    let error = call(&client(deaf, Duration::from_secs(1)), json!({"method": "ok", "data": "x".repeat(4 << 20)}), None)
        .unwrap_err();
    assert!(error.contains("超时"), "{error}");
    let client = client(script, Duration::from_secs(1));
    assert!(call(&client, json!({"method": "hang"}), None).unwrap_err().contains("超时"));
    assert!(started.elapsed() < Duration::from_secs(5));
    call(&client, json!({"method": "ok"}), None).unwrap();
}

/// A plugin printing without pause, during and between calls, is held
/// back by its own pipe: the host keeps at most a few lines.
#[cfg(unix)]
#[test]
fn continuous_output_is_bounded_while_idle_and_calls_still_answer() {
    fn noisy() -> Result<Command, String> {
        let count = std::env::current_exe().unwrap().with_extension(format!("noise-{}", std::process::id()));
        let mut command = Command::new("sh");
        // Lines under PIPE_BUF stay whole next to the answers; the count
        // file is empty while being rewritten.
        command.env("COUNT", count).args([
            "-c",
            r#"pad=$(printf '%0400d' 0)
          (i=0; while kill -0 $$ 2>/dev/null; do i=$((i+1)); echo "$i" > "$COUNT"; echo "log $pad"; done) &
          while read -r line; do
            [ -z "$line" ] && continue
            id=$(printf '%s' "$line" | sed 's/^{"id":"\([^"]*\)".*/\1/')
            n=; while [ -z "$n" ]; do n=$(cat "$COUNT"); done
            printf '\n{"id":"%s","ok":true,"result":%s}\n' "$id" "$n"
          done"#,
        ]);
        Ok(command)
    }
    let client = client(noisy, CALL_TIMEOUT);
    let printed = || call(&client, json!({"method": "ok"}), None).unwrap().as_u64().unwrap();
    let before = printed();
    std::thread::sleep(Duration::from_secs(1));
    let idle = printed() - before;
    // Pipe (64 KiB) plus reader buffer and hand-off hold about 200 lines.
    assert!(idle < 1000, "{idle} lines printed while idle");
    for _ in 0..5 {
        printed();
    }
    client.reset();
    // The printer notices its parent is gone after at most one more line.
    std::thread::sleep(Duration::from_millis(200));
    let _ =
        std::fs::remove_file(std::env::current_exe().unwrap().with_extension(format!("noise-{}", std::process::id())));
}
