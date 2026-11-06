//! Child processes owned by this program (AI drivers and the app's own
//! helpers): every child runs in a kill-on-close job, output comes through
//! pipes with bounded buffers, and cancellation, the time limit and
//! application shutdown stop the whole process tree.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use shishi_acp::process::{spawn_in_job, JobGuard};

use crate::exec::CANCELLED;

// Generous enough for streamed analysis and tool responses, but finite.
const STDOUT_LIMIT: u64 = 64 * 1024 * 1024;
const STDERR_LIMIT: usize = 8 * 1024 * 1024;
const LINE_LIMIT: usize = 8 * 1024 * 1024;
// Lines buffered between the stdout reader and the caller; a full channel
// makes the reader (and so the child) wait instead of growing memory.
const QUEUED_LINES: usize = 64;
// How often cancellation is checked while no line arrives. This is a
// channel wait, not a file poll: a line wakes the caller at once.
const TICK: Duration = Duration::from_millis(100);
// After the child exits, descendants still holding the pipes get this long.
const EXIT_GRACE: Duration = Duration::from_secs(2);
const TOO_LARGE: &str = "子进程输出超过大小限制，已停止本次操作";

static STOPPING: AtomicBool = AtomicBool::new(false);
static JOBS: Mutex<Vec<Arc<JobGuard>>> = Mutex::new(Vec::new());

pub fn check_running() -> Result<(), String> {
    if STOPPING.load(Ordering::SeqCst) {
        Err("软件正在退出，未完成工作将在重启后继续".into())
    } else {
        Ok(())
    }
}

/// Stop every owned child; no new child starts afterwards.
pub fn shutdown() {
    let jobs = JOBS.lock().unwrap();
    STOPPING.store(true, Ordering::SeqCst);
    for job in jobs.iter() {
        job.terminate();
    }
}

pub struct OwnedChild {
    pub child: std::process::Child,
    job: Arc<JobGuard>,
}

impl OwnedChild {
    /// End the whole tree; the child is reaped on drop.
    pub fn terminate(&mut self) {
        self.job.terminate();
        let _ = self.child.kill();
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.terminate();
        JOBS.lock().unwrap().retain(|j| !Arc::ptr_eq(j, &self.job));
        let _ = self.child.wait();
    }
}

pub fn spawn_owned(cmd: &mut Command) -> Result<OwnedChild, String> {
    let mut jobs = JOBS.lock().unwrap();
    check_running()?;
    let (child, job) = spawn_in_job(cmd)?;
    let job = Arc::new(job);
    jobs.push(job.clone());
    Ok(OwnedChild { child, job })
}

/// Run to completion within `timeout`; returns `(stdout, stderr)`.
pub fn run(cmd: Command, input: Option<&[u8]>, timeout: Duration) -> Result<(String, String), String> {
    run_cancellable(cmd, input, timeout, None, &mut |_| {})
}

fn timed_out(timeout: Duration) -> String {
    format!("操作超过 {} 秒，已停止本次子进程；下次可重试", timeout.as_secs().max(1))
}

enum Piece {
    Line(Vec<u8>),
    Fail(String),
}

/// Run `cmd` with `input` on stdin. `line` gets each non-empty stdout line
/// (trailing whitespace trimmed, non-UTF-8 lines skipped) as it arrives;
/// `cancel` is checked after every line and at least every `TICK`.
pub fn run_cancellable(
    mut cmd: Command,
    input: Option<&[u8]>,
    timeout: Duration,
    cancel: Option<&AtomicBool>,
    line: &mut dyn FnMut(&str),
) -> Result<(String, String), String> {
    let cancelled = || cancel.is_some_and(|c| c.load(Ordering::SeqCst));
    if cancelled() {
        return Err(CANCELLED.into());
    }
    cmd.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut owned = spawn_owned(&mut cmd)?;
    if let (Some(bytes), Some(mut stdin)) = (input, owned.child.stdin.take()) {
        let bytes = bytes.to_vec();
        // Dropping stdin after the write gives the child its EOF.
        std::thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
    }
    let stdout = owned.child.stdout.take().ok_or("无法读取子进程输出")?;
    let stderr = owned.child.stderr.take().ok_or("无法读取子进程输出")?;
    let (tx, rx) = mpsc::sync_channel(QUEUED_LINES);
    let errors = Arc::new(Mutex::new(Vec::new()));
    {
        let (tx, errors) = (tx.clone(), errors.clone());
        std::thread::spawn(move || read_stderr(stderr, &errors, STDERR_LIMIT, &tx));
    }
    std::thread::spawn(move || read_lines(stdout, LINE_LIMIT, STDOUT_LIMIT, &tx));
    let start = Instant::now();
    let mut out = Vec::new();
    let mut exited: Option<Instant> = None;
    let mut open = true;
    loop {
        let interrupted = cancelled();
        let elapsed = start.elapsed();
        if interrupted || elapsed >= timeout {
            owned.terminate();
            return Err(if interrupted { CANCELLED.into() } else { timed_out(timeout) });
        }
        let wait = (timeout - elapsed).min(TICK);
        if !open {
            // Both pipes closed but the child still runs: wait for its exit
            // here so the time limit and cancellation keep applying.
            if owned.child.try_wait().map_err(|e| e.to_string())?.is_some() {
                break;
            }
            std::thread::sleep(wait);
            continue;
        }
        match rx.recv_timeout(wait) {
            Ok(Piece::Line(bytes)) => {
                if let Ok(text) = std::str::from_utf8(&bytes) {
                    let text = text.trim_end();
                    if !text.is_empty() {
                        line(text);
                    }
                }
                out.extend_from_slice(&bytes);
            }
            Ok(Piece::Fail(error)) => {
                owned.terminate();
                return Err(error);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => open = false,
            Err(mpsc::RecvTimeoutError::Timeout) => match exited {
                Some(at) if at.elapsed() >= EXIT_GRACE => break,
                Some(_) => {}
                None => {
                    if owned.child.try_wait().map_err(|e| e.to_string())?.is_some() {
                        // Ends descendants that inherited the pipes (Windows job).
                        owned.job.terminate();
                        exited = Some(Instant::now());
                    }
                }
            },
        }
    }
    let code = owned.child.wait().map_err(|e| e.to_string())?;
    check_running()?;
    if cancelled() {
        return Err(CANCELLED.into());
    }
    let stderr = String::from_utf8_lossy(&errors.lock().unwrap()).into_owned();
    if !code.success() {
        return Err(format!("进程退出 {}：{}", code, stderr.chars().take(1400).collect::<String>()));
    }
    // A non-UTF-8 console response reads as empty, as before.
    Ok((String::from_utf8(out).unwrap_or_default(), stderr))
}

/// Send stdout line by line; a line or the total over its limit fails.
fn read_lines(stdout: impl Read, line_limit: usize, total_limit: u64, tx: &mpsc::SyncSender<Piece>) {
    let mut reader = BufReader::new(stdout);
    let mut total = 0u64;
    loop {
        let mut bytes = Vec::new();
        let read = (&mut reader).take(line_limit as u64 + 1).read_until(b'\n', &mut bytes);
        let piece = match read {
            Ok(0) => return,
            Ok(n) if n > line_limit => Piece::Fail("子进程单行输出超过大小限制，已停止本次操作".into()),
            Ok(n) => {
                total += n as u64;
                if total > total_limit {
                    Piece::Fail(TOO_LARGE.into())
                } else {
                    Piece::Line(bytes)
                }
            }
            Err(e) => Piece::Fail(e.to_string()),
        };
        let stop = matches!(piece, Piece::Fail(_));
        if tx.send(piece).is_err() || stop {
            return;
        }
    }
}

/// Keep stderr up to `limit`, read to the end so the child never blocks on it.
fn read_stderr(mut stderr: impl Read, errors: &Mutex<Vec<u8>>, limit: usize, tx: &mpsc::SyncSender<Piece>) {
    let mut buffer = [0u8; 8192];
    loop {
        match stderr.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let mut kept = errors.lock().unwrap();
                if kept.len() + n > limit {
                    drop(kept);
                    let _ = tx.send(Piece::Fail(TOO_LARGE.into()));
                    return;
                }
                kept.extend_from_slice(&buffer[..n]);
            }
        }
    }
}

/// Hand a URL to its registered app.
pub fn shell_open(url: &str) -> bool {
    #[cfg(windows)]
    {
        let target = url.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        // SAFETY: every pointer is null or a NUL-terminated buffer that outlives the call.
        use std::ptr::{null, null_mut};
        let result = unsafe {
            windows_sys::Win32::UI::Shell::ShellExecuteW(null_mut(), null(), target.as_ptr(), null(), null(), 1)
        };
        result as isize > 32
    }
    #[cfg(not(windows))]
    {
        let _ = url;
        false
    }
}

#[cfg(test)]
mod tests;
