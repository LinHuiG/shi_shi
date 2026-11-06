//! The clients a handling session is handed to. ShiShi runs only the first
//! turn, headless, so the chosen model and effort apply and the session id
//! is known: Codex through `codex exec --json`, DSH through an ACP child
//! that is shut down afterwards so the DSH client can take the session
//! over. Neither is restricted by ShiShi: Codex runs without a sandbox and
//! DSH permission requests are allowed.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{json, Value};
use shishi_acp::SessionUpdate;

use crate::ai::dsh as ai_dsh;
use crate::config::Connection;
use crate::{process, Error, Result};

const RPC: Duration = Duration::from_secs(20);

pub struct Turn {
    pub text: String,
    pub cwd: PathBuf,
    pub model: String,
    pub effort: String,
    pub timeout: Duration,
}

pub struct Outcome {
    pub status: &'static str,
    pub conclusion: String,
    pub usage: Value,
}

pub trait Agent: Send {
    /// Run the first turn; `thread` receives the session id as soon as the
    /// client reports it.
    fn run(&self, turn: &Turn, thread: &mut dyn FnMut(String)) -> Result<Outcome>;
}

pub trait Backends: Send + Sync {
    fn agent(&self, c: &Connection, root: &Path) -> Result<Box<dyn Agent>>;
    /// The local projects of one client driver (`codex` or `dsh`).
    fn projects(&self, driver: &str) -> Result<Vec<Value>>;
    /// Bring the session up in the desktop app; DSH only raises its window.
    fn open(&self, c: &Connection, thread: &str) -> Result<()>;
}

pub struct Native;

impl Backends for Native {
    fn agent(&self, c: &Connection, root: &Path) -> Result<Box<dyn Agent>> {
        Ok(match c.driver.as_str() {
            "dsh" => Box::new(Dsh { connection: c.clone(), root: root.into() }),
            _ => Box::new(Codex { executable: c.executable.clone() }),
        })
    }

    fn projects(&self, driver: &str) -> Result<Vec<Value>> {
        match driver {
            "dsh" => {
                let Ok(home) = ai_dsh::home() else { return Ok(Vec::new()) };
                shishi_acp::workspace::projects(&home).map_err(Error::new)
            }
            _ => crate::ai::codex::projects().map_err(Error),
        }
    }

    fn open(&self, c: &Connection, thread: &str) -> Result<()> {
        match c.driver.as_str() {
            "codex" => crate::ai::codex::open_session(thread),
            "dsh" if crate::process::shell_open("dsh://open") => Ok(()),
            "dsh" => Err("无法唤起 DSH，请手动打开".into()),
            _ => Err(crate::ai::provider::unsupported(c)),
        }
        .map_err(Error)
    }
}

// ---------------------------------------------------------------- Codex

struct Codex {
    executable: String,
}

impl Agent for Codex {
    fn run(&self, turn: &Turn, thread: &mut dyn FnMut(String)) -> Result<Outcome> {
        let mut cmd = Command::new(crate::ai::codex::resolve_cli(&self.executable).map_err(Error)?);
        cmd.env_remove("CODEX_THREAD_ID").env_remove("CODEX_APP_TOOLS_PIPE_PATH");
        cmd.args(["exec", "--json", "--skip-git-repo-check", "--sandbox", "danger-full-access", "-C"]).arg(&turn.cwd);
        if !turn.model.is_empty() {
            cmd.arg("-m").arg(&turn.model);
        }
        if !turn.effort.is_empty() {
            cmd.arg("-c").arg(format!("model_reasoning_effort={}", json!(turn.effort)));
        }
        cmd.arg("-");
        let (mut conclusion, mut usage, mut failed) = (String::new(), Value::Null, None);
        let run = process::run_cancellable(cmd, Some(turn.text.as_bytes()), turn.timeout, None, &mut |line| {
            let Ok(v) = serde_json::from_str::<Value>(line) else { return };
            match v["type"].as_str().unwrap_or("") {
                "thread.started" => thread(v["thread_id"].as_str().unwrap_or("").into()),
                "turn.completed" => {
                    let u = &v["usage"];
                    usage = json!({"input_tokens": u["input_tokens"], "output_tokens": u["output_tokens"],
                        "cached_input_tokens": u["cached_input_tokens"]});
                }
                "turn.failed" => failed = v["error"]["message"].as_str().map(String::from),
                "error" => failed = v["message"].as_str().map(String::from),
                "item.completed" if v["item"]["type"] == "agent_message" => {
                    conclusion = v["item"]["text"].as_str().unwrap_or("").into();
                }
                _ => {}
            }
        });
        if let Some(message) = failed {
            return Err(Error(message));
        }
        run.map_err(Error)?;
        Ok(Outcome { status: "completed", conclusion, usage })
    }
}

// ---------------------------------------------------------------- DSH

struct Dsh {
    connection: Connection,
    root: PathBuf,
}

/// The first allowing option of a permission request, the lasting one first.
fn allow(options: &[shishi_acp::client::PermissionOption]) -> Option<&str> {
    ["allow_always", "allow_once"]
        .iter()
        .find_map(|kind| options.iter().find(|o| o.kind == *kind))
        .map(|o| o.option_id.as_str())
}

impl Agent for Dsh {
    fn run(&self, turn: &Turn, thread: &mut dyn FnMut(String)) -> Result<Outcome> {
        let (client, _job) = ai_dsh::discovery_client(&self.connection, &self.root, &turn.cwd).map_err(Error)?;
        let session = client.new_session(&turn.cwd, &[], RPC).map_err(|e| Error(e.to_string()))?.session_id;
        thread(session.clone());
        if !turn.model.is_empty() {
            let (provider, model) = ai_dsh::model_route(&turn.model);
            client.select_model(&session, &provider, &model, RPC).map_err(|e| Error(e.to_string()))?;
        }
        if !turn.effort.is_empty() {
            client.select_effort(&session, &turn.effort, RPC).map_err(|e| Error(e.to_string()))?;
        }
        let client = std::sync::Arc::new(client);
        let (tx, rx) = mpsc::channel();
        let worker = {
            let (client, session, text, timeout) = (client.clone(), session.clone(), turn.text.clone(), turn.timeout);
            std::thread::spawn(move || client.prompt(&session, &text, &[], timeout, &mut |e| drop(tx.send(e))))
        };
        let mut answer = String::new();
        loop {
            match rx.recv_timeout(Duration::from_millis(300)) {
                Ok(event) if event.session_id == session => {
                    if let SessionUpdate::AgentMessageChunk { text } = event.update {
                        answer.push_str(&text);
                    }
                }
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    for p in client.permissions_for(&session) {
                        if let Some(option) = allow(&p.options) {
                            let _ = client.respond_permission(&p.request_id, option);
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let result = worker.join().map_err(|_| Error::new("DSH 会话线程异常结束"));
        // Release the session writer, then list the session in the DSH client.
        client.shutdown();
        shishi_acp::workspace::publish(&ai_dsh::home().map_err(Error)?, &session).map_err(Error)?;
        let result = result?.map_err(|e| Error(e.to_string()))?;
        let usage = result.usage.map_or(Value::Null, |u| {
            json!({"input_tokens": u.input_tokens, "output_tokens": u.output_tokens,
                "cached_input_tokens": u.cached_read_tokens})
        });
        let status = match result.stop_reason.as_str() {
            "end_turn" => "completed",
            "cancelled" => "interrupted",
            other => crate::bail!("DSH 停止：{other}"),
        };
        Ok(Outcome { status, conclusion: answer, usage })
    }
}

#[cfg(test)]
pub(crate) mod fake;

#[cfg(test)]
mod tests;
