//! One reusable plugin process (`--plugin-worker`). Calls are serialized so a
//! cancellation or a native crash has one owner; queued calls never enter the
//! DLL. Each message is one JSON line over the child's stdin and stdout; the
//! host waits on the pipe, so an idle worker costs no wakeups.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Mutex,
    },
    time::{Duration, Instant},
};

pub const MAX: u64 = 32 * 1024 * 1024;
const CALL_TIMEOUT: Duration = Duration::from_secs(90);
/// Stdout lines waiting for the host; lines are at most `MAX` bytes.
const QUEUED_LINES: usize = 1;
/// How often a cancellable call looks at its cancel flag.
const CANCEL_CHECK: Duration = Duration::from_millis(200);

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub id: String,
    pub manifest: PathBuf,
    pub settings: bool,
    pub input: Value,
}

/// One line of at most `MAX` bytes; `None` at end of stream.
fn read_line(reader: &mut impl BufRead) -> Result<Option<String>, String> {
    let mut line = String::new();
    let n = reader.by_ref().take(MAX + 1).read_line(&mut line).map_err(|e| e.to_string())?;
    if n as u64 > MAX {
        return Err("插件通信超过 32 MiB".into());
    }
    Ok((n > 0).then_some(line))
}

/// `value` as one line. The leading newline ends any partial line a plugin
/// printed itself, so the message always starts a fresh line.
fn frame(value: &impl Serialize) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX {
        return Err("插件通信超过 32 MiB".into());
    }
    bytes.insert(0, b'\n');
    bytes.push(b'\n');
    Ok(bytes)
}

fn write_line(writer: &mut impl Write, value: &impl Serialize) -> Result<(), String> {
    let bytes = frame(value)?;
    writer.write_all(&bytes).and_then(|_| writer.flush()).map_err(|e| e.to_string())
}

/// The response for `request`; `None` for any other line (blank, or text a
/// plugin wrote to stdout).
pub fn parse_response(line: &str, request: &str) -> Option<Value> {
    let response: Value = serde_json::from_str(line.trim()).ok()?;
    (response["id"] == request && response["ok"].is_boolean()).then_some(response)
}

struct Worker {
    child: shishi_core::process::OwnedChild,
    requests: mpsc::Sender<Vec<u8>>,
    /// Stdout lines, then one error when the pipes break. Bounded, so a
    /// plugin printing faster than the host reads (or while no call is
    /// running) waits on its own pipe instead of growing host memory.
    lines: mpsc::Receiver<Result<String, String>>,
}

impl Worker {
    fn start(mut command: Command) -> Result<Self, String> {
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = shishi_core::process::spawn_owned(&mut command)?;
        let mut stdin = child.child.stdin.take().ok_or("插件工作进程输入不可用")?;
        let mut stdout = BufReader::new(child.child.stdout.take().ok_or("插件工作进程输出不可用")?);
        let (tx, lines) = mpsc::sync_channel(QUEUED_LINES);
        let (requests, queued) = mpsc::channel::<Vec<u8>>();
        // Both threads end with the worker: its pipes close or `lines` drops.
        let writer = tx.clone();
        std::thread::Builder::new()
            .name("shishi-plugin-reader".into())
            .spawn(move || loop {
                let line = read_line(&mut stdout).and_then(|line| line.ok_or_else(|| "插件工作进程输出已关闭".into()));
                let last = line.is_err();
                if tx.send(line).is_err() || last {
                    break;
                }
            })
            .map_err(|e| e.to_string())?;
        // Writes off the calling thread, so a worker that stops reading its
        // input cannot hold a call past its time limit.
        std::thread::Builder::new()
            .name("shishi-plugin-writer".into())
            .spawn(move || {
                for bytes in queued {
                    if let Err(e) = stdin.write_all(&bytes).and_then(|_| stdin.flush()) {
                        let _ = writer.send(Err(format!("插件工作进程输入已关闭：{e}")));
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { child, requests, lines })
    }

    fn exchange(&mut self, request: &Request, cancel: Option<&AtomicBool>, timeout: Duration) -> Result<Value, String> {
        let deadline = Instant::now() + timeout;
        self.requests.send(frame(request)?).map_err(|_| "插件工作进程输入已关闭")?;
        loop {
            if cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
                return Err("插件操作已取消".into());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!("插件工作进程调用超时（{} 秒）", timeout.as_secs()));
            }
            let wait = if cancel.is_some() { left.min(CANCEL_CHECK) } else { left };
            match self.lines.recv_timeout(wait) {
                Ok(Ok(line)) => {
                    if let Some(response) = parse_response(&line, &request.id) {
                        return Ok(response);
                    }
                }
                Ok(Err(error)) => return Err(self.ended(error)),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(self.ended("插件工作进程输出已关闭".into())),
            }
        }
    }

    /// Why the pipes broke: the exit code if the worker has exited, else
    /// `error`. Never waits on a worker that is still running; the caller
    /// then drops it, which ends it.
    fn ended(&mut self, error: String) -> String {
        if let Err(stopping) = shishi_core::process::check_running() {
            return stopping;
        }
        // Stdout closes while the process exits; let the exit land.
        for _ in 0..10 {
            if let Ok(Some(status)) = self.child.child.try_wait() {
                let code = status.code().map_or_else(|| "未知".into(), |c| format!("0x{:08x}", c as u32));
                return format!("插件工作进程退出，退出码 {code}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        error
    }
}

fn worker_command() -> Result<Command, String> {
    let mut command = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
    command.arg("--plugin-worker");
    Ok(command)
}

/// Host side: owns at most one worker process, started on demand.
pub struct Client {
    worker: Mutex<Option<Worker>>,
    command: fn() -> Result<Command, String>,
    timeout: Duration,
}

impl Default for Client {
    fn default() -> Self {
        Self { worker: Mutex::new(None), command: worker_command, timeout: CALL_TIMEOUT }
    }
}

impl Client {
    /// End the worker so no plugin library stays loaded.
    pub fn reset(&self) {
        *self.worker.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// One call. A failure ends the worker; the next call starts a new one.
    pub fn call(
        &self,
        manifest: &Path,
        settings: bool,
        input: Value,
        cancel: Option<&AtomicBool>,
    ) -> Result<Value, String> {
        let size = serde_json::to_vec(&input).map_err(|e| e.to_string())?.len();
        if size as u64 > MAX || (settings && size > 2 * 1024 * 1024) {
            return Err("插件请求过大".into());
        }
        // The running call is bounded by its timeout, so this wait is too.
        let mut worker = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        shishi_core::process::check_running()?;
        if cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
            return Err("插件操作已取消".into());
        }
        let request = Request { id: uuid::Uuid::new_v4().to_string(), manifest: manifest.into(), settings, input };
        let started = Instant::now();
        let result = (|| {
            if worker.is_none() {
                *worker = Some(Worker::start((self.command)()?)?);
            }
            worker.as_mut().unwrap().exchange(&request, cancel, self.timeout)
        })();
        match result {
            Ok(response) if response["ok"] == true => Ok(response["result"].clone()),
            Ok(response) => Err(response["error"].as_str().unwrap_or("插件调用失败").into()),
            Err(error) => {
                *worker = None;
                // No parameters or plugin output: these can contain credentials.
                let plugin = manifest.parent().and_then(|d| d.file_name()).unwrap_or_default().to_string_lossy();
                Err(format!(
                    "插件 {plugin}，方法 {}，请求 {}，调用阶段，耗时 {}ms：{error}；工作进程已回收",
                    request.input["method"].as_str().unwrap_or(""),
                    request.id,
                    started.elapsed().as_millis(),
                ))
            }
        }
    }
}

/// Worker side: answer each request line until the host closes the input.
/// `handle` runs one request; libraries it loads stay loaded until exit.
pub fn serve(
    mut input: impl BufRead,
    mut output: impl Write,
    handle: &mut dyn FnMut(&Request) -> Result<Value, String>,
) -> Result<(), String> {
    while let Some(line) = read_line(&mut input)? {
        if line.trim().is_empty() {
            continue;
        }
        let request: Request = serde_json::from_str(&line).map_err(|e| e.to_string())?;
        let response = match handle(&request) {
            Ok(value) => json!({"id": request.id, "ok": true, "result": value}),
            Err(error) => json!({"id": request.id, "ok": false, "error": error}),
        };
        write_line(&mut output, &response)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
