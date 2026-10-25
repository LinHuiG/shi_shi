//! One renderer/executor for local tests and API tool calls. Templates are local
//! policy; arguments are data. See docs/TOOL_CALLING.md for the CMD contract.
use crate::tool_registry::ToolDefinition;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::Read,
    os::windows::process::CommandExt,
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

pub const OUTPUT_LIMIT: usize = 32 * 1024;
#[derive(Clone, Default, Serialize, Deserialize, Debug)]
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
        Self {
            tool: tool.into(),
            call_id: call_id.into(),
            status: "rejected".into(),
            error,
            ..Default::default()
        }
    }
}
fn decode(bytes: &[u8], truncated: bool) -> Result<String, String> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(text.trim_start_matches('\u{feff}').into()),
        Err(error) if truncated && error.error_len().is_none() => {
            Ok(std::str::from_utf8(&bytes[..error.valid_up_to()])
                .unwrap()
                .into())
        }
        Err(_) => Err("工具输出不是有效 UTF-8，请调整工具输出编码".into()),
    }
}

fn parts(tool: &ToolDefinition) -> Result<Vec<(&str, Option<&str>)>, String> {
    let template = tool.command_template.as_str();
    if template.chars().any(|c| c.is_control() && c != '\t') {
        return Err("命令模板不能包含换行或控制字符".into());
    }
    let mut rest = template;
    let mut result = Vec::new();
    let mut quoted = false;
    while let Some(start) = rest.find("{{") {
        let prefix = &rest[..start];
        for c in prefix.chars() {
            if c == '"' {
                quoted = !quoted;
            }
        }
        let end = rest[start + 2..].find("}}").ok_or("参数占位符未闭合")? + start + 2;
        let name = &rest[start + 2..end];
        if quoted
            || start > 0 && !rest[..start].ends_with(char::is_whitespace)
            || rest[end + 2..]
                .chars()
                .next()
                .is_some_and(|c| !c.is_whitespace() && !['&', '|', '>', '<', ')'].contains(&c))
        {
            return Err("每个 {{参数名}} 必须独占一个参数，前面留空格，外面无需加引号".into());
        }
        if !tool.parameters.iter().any(|p| p.name == name) {
            return Err(format!("模板包含未定义参数 {name}"));
        }
        if prefix.contains("}}") {
            return Err("命令模板占位符无效".into());
        }
        result.push((prefix, Some(name)));
        rest = &rest[end + 2..];
    }
    if rest.contains("}}") {
        return Err("命令模板占位符无效".into());
    }
    result.push((rest, None));
    Ok(result)
}
pub fn validate_template(tool: &ToolDefinition) -> Result<(), String> {
    parts(tool).map(|_| ())
}
fn quote(value: &str) -> Result<String, String> {
    // CMD, BAT and executable argv have different quote/percent reparsing rules.
    // This shared, deliberately explicit subset is lossless for all three.
    // Never silently normalize/re-encode an argument to make it executable.
    if value.contains(['"', '%', '\r', '\n', '\0'])
        || value.ends_with('\\')
        || value.chars().any(|c| c.is_control() && c != '\t')
    {
        return Err("参数包含 CMD 通用引用不能无损表达的双引号、百分号、控制字符或末尾反斜杠；请使用工具约定的编码参数或客户包装命令".into());
    }
    Ok(format!("\"{value}\""))
}
pub fn render(tool: &ToolDefinition, args: &Value) -> Result<String, String> {
    tool.validate(true)?;
    let args = tool.arguments(args)?;
    let mut command = String::new();
    for (text, name) in parts(tool)? {
        command.push_str(text);
        if let Some(name) = name {
            let value = &args[name];
            command.push_str(&quote(value.as_str().unwrap())?);
        }
    }
    // cmd.exe's command line limit is lower than CreateProcessW's limit.
    if command.encode_utf16().count() > 7900 {
        return Err("生成的 CMD 命令超过长度上限".into());
    }
    Ok(command)
}
struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
    error: Option<String>,
}
fn capture(mut pipe: impl Read, tx: mpsc::Sender<(bool, Captured)>, stderr: bool) {
    let mut captured = Captured {
        bytes: Vec::new(),
        truncated: false,
        error: None,
    };
    let mut buffer = [0u8; 8192];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                let keep = n.min(OUTPUT_LIMIT - captured.bytes.len());
                captured.bytes.extend_from_slice(&buffer[..keep]);
                captured.truncated |= keep < n;
                // Always drain both pipes, including after the retention limit.
            }
            Err(e) => {
                captured.error = Some(format!("输出读取失败：{e}"));
                break;
            }
        }
    }
    let _ = tx.send((stderr, captured));
}
#[cfg(test)]
pub fn execute(
    tool: &ToolDefinition,
    call_id: &str,
    args: &Value,
    cancel: &AtomicBool,
    deadline: Instant,
) -> CommandResult {
    execute_with_start(
        tool,
        call_id,
        args,
        || cancel.load(Ordering::SeqCst),
        deadline,
        |cmd| crate::process::spawn_owned(cmd).map_err(|e| (false, e)),
    )
}
pub fn execute_with_start(
    tool: &ToolDefinition,
    call_id: &str,
    args: &Value,
    cancelled: impl Fn() -> bool,
    deadline: Instant,
    start: impl FnOnce(&mut Command) -> Result<crate::process::OwnedChild, (bool, String)>,
) -> CommandResult {
    let started = Instant::now();
    let command = match render(tool, args) {
        Ok(c) => c,
        Err(e) => return CommandResult::rejected(&tool.name, call_id, e),
    };
    let mut result = CommandResult {
        tool: tool.name.clone(),
        call_id: call_id.into(),
        ..Default::default()
    };
    if cancelled() || started >= deadline {
        result.cancelled = cancelled();
        result.timed_out = !result.cancelled;
        result.status = if result.cancelled {
            "cancelled"
        } else {
            "timed_out"
        }
        .into();
        return result;
    }
    let mut cmd = Command::new("cmd.exe");
    cmd.args(["/d", "/v:off", "/s", "/c"])
        .raw_arg(format!("\"{command}\""))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match start(&mut cmd) {
        Ok(child) => child,
        Err((rejected, e)) => {
            result.status = if rejected { "rejected" } else { "start_failed" }.into();
            result.error = e;
            return result;
        }
    };
    let (tx, rx) = mpsc::channel();
    let stdout = child.child.stdout.take().unwrap();
    let stderr = child.child.stderr.take().unwrap();
    let out_tx = tx.clone();
    let out = std::thread::spawn(move || capture(stdout, out_tx, false));
    let err = std::thread::spawn(move || capture(stderr, tx, true));
    let limit =
        deadline.min(started + Duration::from_secs(crate::tool_registry::COMMAND_TIMEOUT_SECONDS));
    let mut exited = false;
    let mut captures = Vec::new();
    loop {
        captures.extend(rx.try_iter());
        result.cancelled = cancelled() || crate::process::stopping();
        result.timed_out = Instant::now() >= limit;
        if result.cancelled || result.timed_out {
            child.terminate();
            break;
        }
        match child.child.try_wait() {
            Ok(Some(code)) => {
                result.exit_code = code.code();
                exited = true;
            }
            Ok(None) => (),
            Err(e) => {
                result.error = e.to_string();
                child.terminate();
                break;
            }
        }
        if exited && captures.len() == 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    // Kill the owned tree before joining readers; inherited pipe handles cannot
    // outlive cancellation or keep this run blocked after its deadline.
    child.terminate();
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
            Err(error) => {
                result.error.push_str(&error);
            }
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
