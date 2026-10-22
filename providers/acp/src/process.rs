//! Windows process-tree ownership and bounded command output.
//!
//! The ACP child is assigned to a job object with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Closing the last handle terminates the
//! whole tree, including MCP servers and any descendants. This is the same
//! mechanism used by the staging transport for both the runtime child and its
//! cleanup. `run_bounded` is for short, non-protocol commands such as
//! `dsh --version` and `dsh --dump-config`; it never reads a model.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE};
#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA,
    PROCESS_TERMINATE,
};

pub const STILL_ACTIVE: u32 = 259;

/// Owns one unnamed Windows job object. Unsupported platforms are a no-op.
pub struct JobGuard {
    #[cfg(windows)]
    handle: usize,
}

// The handle is an owned kernel handle valid across threads.
unsafe impl Send for JobGuard {}
unsafe impl Sync for JobGuard {}

impl JobGuard {
    pub fn create() -> Result<Self, String> {
        #[cfg(windows)]
        {
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err("CreateJobObjectW failed".into());
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                unsafe { CloseHandle(handle) };
                return Err("SetInformationJobObject failed".into());
            }
            Ok(Self {
                handle: handle as usize,
            })
        }
        #[cfg(not(windows))]
        {
            Ok(Self {})
        }
    }

    pub fn assign(&self, child: &Child) -> Result<(), String> {
        #[cfg(windows)]
        {
            let process =
                unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, FALSE, child.id()) };
            if process.is_null() {
                return Err("OpenProcess for job assignment failed".into());
            }
            let ok = unsafe { AssignProcessToJobObject(self.handle as HANDLE, process) };
            unsafe { CloseHandle(process) };
            if ok == 0 {
                return Err("AssignProcessToJobObject failed".into());
            }
            Ok(())
        }
        #[cfg(not(windows))]
        {
            let _ = child;
            Ok(())
        }
    }

    pub fn terminate(&self) {
        #[cfg(windows)]
        unsafe {
            let _ = TerminateJobObject(self.handle as HANDLE, 1);
        }
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            if self.handle != 0 {
                let _ = TerminateJobObject(self.handle as HANDLE, 1);
                let _ = CloseHandle(self.handle as HANDLE);
                self.handle = 0;
            }
        }
    }
}

pub fn pid_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
        if handle.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = unsafe { GetExitCodeProcess(handle, &mut code) };
        unsafe { CloseHandle(handle) };
        ok != 0 && code == STILL_ACTIVE
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        false
    }
}

/// Spawn a process suspended, assign it to a kill-on-close job, then resume its
/// primary thread. This closes the race where a child could launch a
/// grandchild before job assignment.
pub fn spawn_in_job(mut command: Command) -> Result<(Child, JobGuard), String> {
    let job = JobGuard::create()?;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW | CREATE_SUSPENDED
        command.creation_flags(0x0800_0000 | 0x0000_0004);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn {command:?} failed: {error}"))?;
    if let Err(error) = job.assign(&child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    #[cfg(windows)]
    if let Err(error) = resume_child(child.id()) {
        job.terminate();
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok((child, job))
}

#[cfg(windows)]
fn resume_child(pid: u32) -> Result<(), String> {
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        System::{Diagnostics::ToolHelp::*, Threading::*},
    };
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err("CreateToolhelp32Snapshot failed".into());
        }
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of_val(&entry) as u32;
        let mut more = Thread32First(snapshot, &mut entry);
        let mut resumed = false;
        while more != 0 {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if !thread.is_null() {
                    resumed = ResumeThread(thread) != u32::MAX;
                    CloseHandle(thread);
                }
                break;
            }
            more = Thread32Next(snapshot, &mut entry);
        }
        CloseHandle(snapshot);
        if resumed {
            Ok(())
        } else {
            Err("could not resume the job-owned child".into())
        }
    }
}

pub struct BoundedOutput {
    pub status: Option<ExitStatus>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub timed_out: bool,
}

/// Run a short command with a wall-clock deadline and memory-bounded output.
/// Stdout/stderr are drained concurrently so a child cannot deadlock on a full
/// pipe. On Windows the child is assigned to a kill-on-close job.
pub fn run_bounded(
    program: &Path,
    args: &[String],
    cwd: Option<&Path>,
    env: &[(String, String)],
    timeout: Duration,
    max_stdout: usize,
    max_stderr: usize,
) -> Result<BoundedOutput, String> {
    let mut command = Command::new(program);
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    for (key, value) in env {
        command.env(key, value);
    }
    run_bounded_command(command, timeout, max_stdout, max_stderr)
}

pub fn run_bounded_command(
    mut command: Command,
    timeout: Duration,
    max_stdout: usize,
    max_stderr: usize,
) -> Result<BoundedOutput, String> {
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let label = format!("{command:?}");
    let (mut child, job) = spawn_in_job(command)?;
    let stdout = child.stdout.take().ok_or("stdout pipe missing")?;
    let stderr = child.stderr.take().ok_or("stderr pipe missing")?;
    let stdout_thread = spawn_capped_reader(stdout, max_stdout);
    let stderr_thread = spawn_capped_reader(stderr, max_stderr);
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(error) => {
                job.terminate();
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("wait for {label} failed: {error}"));
            }
        }
        if Instant::now() >= deadline {
            timed_out = true;
            job.terminate();
            let _ = child.kill();
            break child.wait().ok();
        }
        thread::sleep(Duration::from_millis(10));
    };
    job.terminate();
    drop(job);
    let (stdout, stdout_truncated) = stdout_thread
        .join()
        .map_err(|_| "stdout reader panicked".to_string())?;
    let (stderr, stderr_truncated) = stderr_thread
        .join()
        .map_err(|_| "stderr reader panicked".to_string())?;
    Ok(BoundedOutput {
        status,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
        timed_out,
    })
}

fn spawn_capped_reader<R: Read + Send + 'static>(
    mut reader: R,
    cap: usize,
) -> thread::JoinHandle<(Vec<u8>, bool)> {
    thread::spawn(move || {
        let mut out = Vec::new();
        let mut truncated = false;
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let room = cap.saturating_sub(out.len());
                    if room > 0 {
                        out.extend_from_slice(&buf[..n.min(room)]);
                    }
                    if n > room {
                        truncated = true;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        (out, truncated)
    })
}

/// Write one line to a pipe and flush it. Used by the mock MCP/ACP servers.
pub fn write_line(mut writer: impl Write, value: &serde_json::Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_output_truncates_and_returns_status() {
        let out = run_bounded(
            Path::new("cmd.exe"),
            &[
                "/D".into(),
                "/S".into(),
                "/C".into(),
                "echo 0123456789 & echo err 1>&2".into(),
            ],
            None,
            &[],
            Duration::from_secs(10),
            4,
            2,
        )
        .unwrap();
        assert!(!out.timed_out);
        assert!(out.status.unwrap().success());
        assert!(out.stdout_truncated);
        assert!(out.stdout.len() <= 4);
        assert!(out.stderr_truncated);
        assert!(out.stderr.len() <= 2);
    }

    #[test]
    fn timeout_terminates() {
        let started = Instant::now();
        let out = run_bounded(
            Path::new("cmd.exe"),
            &[
                "/D".into(),
                "/S".into(),
                "/C".into(),
                "ping -n 30 127.0.0.1 >NUL".into(),
            ],
            None,
            &[],
            Duration::from_millis(250),
            1024,
            1024,
        )
        .unwrap();
        assert!(out.timed_out);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn job_terminates_grandchild() {
        let temp = std::env::temp_dir().join(format!(
            "shishi-acp-job-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&temp).unwrap();
        let pid_file = temp.join("pid.txt");
        // Run cmd in the background through PowerShell and record its PID.
        let ps = format!(
            "$p=Start-Process -FilePath cmd.exe -ArgumentList '/D','/S','/C','ping -n 30 127.0.0.1 >NUL' -WindowStyle Hidden -PassThru; Set-Content -Path '{}' -Value $p.Id; Start-Sleep -Seconds 30",
            pid_file.display()
        );
        let out = run_bounded(
            Path::new("powershell.exe"),
            &["-NoProfile".into(), "-Command".into(), ps],
            None,
            &[],
            Duration::from_millis(700),
            1024,
            1024,
        )
        .unwrap();
        assert!(out.timed_out);
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or(0);
        assert!(pid != 0, "grandchild pid was not recorded");
        thread::sleep(Duration::from_millis(200));
        assert!(!pid_alive(pid), "grandchild survived job termination");
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn alive_probe_reports_self_and_dead_pid() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0xFFFF_FF00));
    }

    #[test]
    fn job_guard_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<JobGuard>();
    }

    #[test]
    fn write_line_is_single_line() {
        let mut out = Vec::new();
        write_line(&mut out, &serde_json::json!({"a":1})).unwrap();
        assert_eq!(out, b"{\"a\":1}\n");
    }
}
