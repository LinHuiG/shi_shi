//! Render a tool's command template and run it. Templates are local policy;
//! arguments are data and are quoted losslessly or refused. On Windows the
//! command runs through `cmd.exe` inside a kill-on-close job object; other
//! platforms run `sh -c` in its own process group so tests can exercise the
//! same state machine.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::registry::Tool;
use crate::Result;

pub const OUTPUT_LIMIT: usize = 32 * 1024;
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Default, Serialize, Deserialize, Debug, PartialEq)]
#[serde(default)]
pub struct CommandResult {
    pub tool: String,
    pub call_id: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub truncated: bool,
    pub timed_out: bool,
    pub cancelled: bool,
    pub error: String,
}

impl CommandResult {
    pub fn rejected(tool: &str, call_id: &str, error: String) -> Self {
        Self { tool: tool.into(), call_id: call_id.into(), status: "rejected".into(), error, ..Default::default() }
    }
}

fn decode(bytes: &[u8], truncated: bool) -> std::result::Result<String, String> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(text.trim_start_matches('\u{feff}').into()),
        // A cut at the retention limit may split the last character.
        Err(e) if truncated && e.error_len().is_none() => Ok(String::from_utf8_lossy(&bytes[..e.valid_up_to()]).into()),
        Err(_) => Err("工具输出不是有效 UTF-8，请调整工具输出编码".into()),
    }
}

pub(crate) fn parts(tool: &Tool) -> Result<Vec<(&str, Option<&str>)>> {
    let template = tool.command_template.as_str();
    if template.chars().any(|c| c.is_control() && c != '\t') {
        crate::bail!("命令模板不能包含换行或控制字符");
    }
    let mut rest = template;
    let mut result = Vec::new();
    let mut quoted = false;
    while let Some(start) = rest.find("{{") {
        let prefix = &rest[..start];
        quoted ^= prefix.chars().filter(|c| *c == '"').count() % 2 == 1;
        let end = rest[start + 2..].find("}}").ok_or_else(|| crate::Error::new("参数占位符未闭合"))? + start + 2;
        let name = &rest[start + 2..end];
        if quoted
            || start > 0 && !prefix.ends_with(char::is_whitespace)
            || rest[end + 2..]
                .chars()
                .next()
                .is_some_and(|c| !c.is_whitespace() && !['&', '|', '>', '<', ')'].contains(&c))
        {
            crate::bail!("每个 {{{{参数名}}}} 必须独占一个参数，前面留空格，外面无需加引号");
        }
        if !tool.parameters.iter().any(|p| p.name == name) {
            crate::bail!("模板包含未定义参数 {name}");
        }
        if prefix.contains("}}") {
            crate::bail!("命令模板占位符无效");
        }
        result.push((prefix, Some(name)));
        rest = &rest[end + 2..];
    }
    if rest.contains("}}") {
        crate::bail!("命令模板占位符无效");
    }
    result.push((rest, None));
    Ok(result)
}

/// CMD, BAT and executable argv reparse quotes and percents differently;
/// this subset is lossless for all three. Values are never re-encoded.
fn quote(value: &str) -> Result<String> {
    if value.contains(['"', '%', '\r', '\n', '\0'])
        || value.ends_with('\\')
        || value.chars().any(|c| c.is_control() && c != '\t')
    {
        crate::bail!("参数包含 CMD 通用引用不能无损表达的双引号、百分号、控制字符或末尾反斜杠；请使用工具约定的编码参数或客户包装命令");
    }
    Ok(format!("\"{value}\""))
}

pub fn render(tool: &Tool, args: &Value) -> Result<String> {
    tool.validate(true)?;
    let args = tool.arguments(args)?;
    let mut command = String::new();
    for (text, name) in parts(tool)? {
        command.push_str(text);
        if let Some(name) = name {
            command.push_str(&quote(args[name].as_str().unwrap_or_default())?);
        }
    }
    // cmd.exe's command line limit is lower than CreateProcessW's.
    if command.encode_utf16().count() > 7900 {
        crate::bail!("生成的 CMD 命令超过长度上限");
    }
    Ok(command)
}

/// A started command whose whole process tree can be killed.
pub struct Owned {
    child: Child,
    #[cfg(windows)]
    job: shishi_acp::process::JobGuard,
}

impl Owned {
    fn terminate(&mut self) {
        #[cfg(windows)]
        self.job.terminate();
        #[cfg(not(windows))]
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", self.child.id())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = self.child.kill();
    }
}

/// Start `cmd` with its process tree owned by the returned handle.
pub fn spawn(cmd: &mut Command) -> std::result::Result<Owned, String> {
    #[cfg(windows)]
    {
        let (child, job) = shishi_acp::process::spawn_in_job(cmd)?;
        Ok(Owned { child, job })
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt;
        let child = cmd.process_group(0).spawn().map_err(|e| e.to_string())?;
        Ok(Owned { child })
    }
}

fn command(rendered: &str) -> Command {
    #[cfg(windows)]
    let mut cmd = {
        use std::os::windows::process::CommandExt;
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/d", "/v:off", "/s", "/c"]).raw_arg(format!("\"{rendered}\""));
        cmd
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", rendered]);
        cmd
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd
}

struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
    error: Option<String>,
}

fn capture(mut pipe: impl Read, tx: mpsc::Sender<(bool, Captured)>, stderr: bool) {
    let mut captured = Captured { bytes: Vec::new(), truncated: false, error: None };
    let mut buffer = [0u8; 8192];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                let keep = n.min(OUTPUT_LIMIT - captured.bytes.len());
                captured.bytes.extend_from_slice(&buffer[..keep]);
                // Keep draining past the limit so the child never blocks.
                captured.truncated |= keep < n;
            }
            Err(e) => {
                captured.error = Some(format!("输出读取失败：{e}"));
                break;
            }
        }
    }
    let _ = tx.send((stderr, captured));
}

/// Run one call. `start` performs the last authority checks and spawns;
/// `Err((true, _))` means rejected, `Err((false, _))` a start failure.
pub fn execute_with_start(
    tool: &Tool,
    call_id: &str,
    args: &Value,
    cancelled: impl Fn() -> bool,
    deadline: Instant,
    start: impl FnOnce(&mut Command) -> std::result::Result<Owned, (bool, String)>,
) -> CommandResult {
    let started = Instant::now();
    let rendered = match render(tool, args) {
        Ok(c) => c,
        Err(e) => return CommandResult::rejected(&tool.name, call_id, e.0),
    };
    let mut result = CommandResult { tool: tool.name.clone(), call_id: call_id.into(), ..Default::default() };
    if cancelled() || started >= deadline {
        result.cancelled = cancelled();
        result.timed_out = !result.cancelled;
        result.status = if result.cancelled { "cancelled" } else { "timed_out" }.into();
        return result;
    }
    let mut cmd = command(&rendered);
    let mut child = match start(&mut cmd) {
        Ok(child) => child,
        Err((rejected, e)) => {
            result.status = if rejected { "rejected" } else { "start_failed" }.into();
            result.error = e;
            return result;
        }
    };
    let (tx, rx) = mpsc::channel();
    let stdout = child.child.stdout.take().expect("piped stdout");
    let stderr = child.child.stderr.take().expect("piped stderr");
    let out_tx = tx.clone();
    let out = std::thread::spawn(move || capture(stdout, out_tx, false));
    let err = std::thread::spawn(move || capture(stderr, tx, true));
    let limit = deadline.min(started + COMMAND_TIMEOUT);
    let mut exited = false;
    let mut captures = Vec::new();
    loop {
        captures.extend(rx.try_iter());
        result.cancelled = cancelled();
        result.timed_out = Instant::now() >= limit;
        if result.cancelled || result.timed_out {
            break;
        }
        match child.child.try_wait() {
            Ok(Some(code)) => {
                result.exit_code = code.code();
                exited = true;
            }
            Ok(None) => {}
            Err(e) => {
                result.error = e.to_string();
                break;
            }
        }
        if exited && captures.len() == 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    // Kill the tree before joining readers: grandchildren holding the pipes
    // must not keep this call alive past cancellation or its deadline.
    child.terminate();
    let _ = child.child.wait();
    drop(child);
    let _ = out.join();
    let _ = err.join();
    captures.extend(rx.try_iter());
    for (stderr, captured) in captures {
        result.truncated |= captured.truncated;
        if let Some(error) = captured.error {
            result.error.push_str(&error);
        }
        match decode(&captured.bytes, captured.truncated) {
            Ok(text) if stderr => result.stderr = text,
            Ok(text) => result.stdout = text,
            Err(error) => result.error.push_str(&error),
        }
    }
    result.status = if result.cancelled {
        "cancelled"
    } else if result.timed_out {
        "timed_out"
    } else if result.exit_code != Some(0) {
        "nonzero_exit"
    } else if !result.error.is_empty() {
        "output_error"
    } else {
        "completed"
    }
    .into();
    result.duration_ms = started.elapsed().as_millis() as u64;
    result
}
