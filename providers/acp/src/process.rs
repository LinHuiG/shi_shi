//! Windows process-tree ownership and bounded command output.
//!
//! The ACP child is assigned to a job object with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Closing the last handle terminates the
//! whole tree, including MCP servers and any descendants. This is the same
//! mechanism used by the staging transport for both the runtime child and its
//! cleanup. `run_bounded_command_until` is for short, non-protocol commands such as
//! `dsh --version` and `dsh --dump-config`; it never reads a model.

use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
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
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

pub const STILL_ACTIVE: u32 = 259;

/// Owns one unnamed Windows job object. Unsupported platforms are a no-op.
pub struct JobGuard {
    #[cfg(windows)]
    handle: usize,
}

impl JobGuard {
    pub fn create() -> Result<Self, String> {
        #[cfg(windows)]
        {
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(std::io::Error::last_os_error().to_string());
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
                let error = std::io::Error::last_os_error();
                unsafe { CloseHandle(handle) };
                return Err(error.to_string());
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
            use std::os::windows::io::AsRawHandle;
            let ok =
                unsafe { AssignProcessToJobObject(self.handle as HANDLE, child.as_raw_handle()) };
            if ok == 0 {
                return Err(format!(
                    "无法管理子进程生命周期：{}",
                    std::io::Error::last_os_error()
                ));
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
            // This uniquely owned handle always has KILL_ON_JOB_CLOSE set.
            let _ = CloseHandle(self.handle as HANDLE);
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
pub fn spawn_in_job(command: &mut Command) -> Result<(Child, JobGuard), String> {
    let job = JobGuard::create()?;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW | CREATE_SUSPENDED
        command.creation_flags(0x0800_0000 | 0x0000_0004);
    }
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let assigned = job.assign(&child);
    #[cfg(windows)]
    let assigned = assigned.and_then(|_| resume_child(child.id()));
    if let Err(error) = assigned {
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
            return Err(std::io::Error::last_os_error().to_string());
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
            Err("无法恢复已受管的子进程".into())
        }
    }
}

#[derive(Debug, Default)]
pub struct BoundedOutput {
    pub status: Option<ExitStatus>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub timed_out: bool,
    pub cancelled: bool,
}

/// Share the caller's total deadline and cancellation through process startup,
/// execution and tree cleanup. An already stopped call never launches a child.
pub fn run_bounded_command_until(
    mut command: Command,
    deadline: Instant,
    max_stdout: usize,
    max_stderr: usize,
    cancel: &AtomicBool,
) -> Result<BoundedOutput, String> {
    let mut cancelled = cancel.load(Ordering::SeqCst);
    if cancelled || Instant::now() >= deadline {
        return Ok(BoundedOutput {
            cancelled,
            timed_out: !cancelled,
            ..BoundedOutput::default()
        });
    }
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let label = format!("{command:?}");
    let (mut child, job) = spawn_in_job(&mut command)?;
    let stdout = child.stdout.take().ok_or("stdout pipe missing")?;
    let stderr = child.stderr.take().ok_or("stderr pipe missing")?;
    let stdout_thread = spawn_capped_reader(stdout, max_stdout);
    let stderr_thread = spawn_capped_reader(stderr, max_stderr);
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
        cancelled = cancel.load(Ordering::SeqCst);
        if cancelled || Instant::now() >= deadline {
            timed_out = !cancelled;
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
        cancelled,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_or_expired_command_never_spawns() {
        for cancelled in [true, false] {
            let out = run_bounded_command_until(
                Command::new("shishi-nonexistent-command"),
                if cancelled {
                    Instant::now() + Duration::from_secs(30)
                } else {
                    Instant::now()
                },
                16,
                16,
                &AtomicBool::new(cancelled),
            )
            .unwrap();
            assert_eq!(out.cancelled, cancelled);
            assert_eq!(out.timed_out, !cancelled);
            assert!(out.status.is_none());
        }
    }

    #[test]
    fn bounded_output_truncates_and_returns_status() {
        let mut command = Command::new("cmd.exe");
        command.args(["/D", "/S", "/C", "echo 0123456789 & echo err 1>&2"]);
        let out = run_bounded_command_until(
            command,
            Instant::now() + Duration::from_secs(10),
            4,
            2,
            &AtomicBool::new(false),
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
        let mut command = Command::new("powershell.exe");
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Start-Sleep -Seconds 30",
        ]);
        let out = run_bounded_command_until(
            command,
            started + Duration::from_millis(250),
            1024,
            1024,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(out.timed_out);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn cancellation_reaps_the_ready_process_tree_and_output_readers() {
        let temp = crate::test_path("acp-job-test");
        std::fs::create_dir_all(&temp).unwrap();
        let pid_file = temp.join("pid.txt");
        let mut command = Command::new("powershell.exe");
        command.env("SS_TEST_PID", &pid_file).args(["-NoProfile", "-NonInteractive", "-Command",
            "$p=[Diagnostics.Process]::new(); $p.StartInfo.FileName='powershell.exe'; $p.StartInfo.Arguments='-NoProfile -NonInteractive -Command Start-Sleep -Seconds 30'; $p.StartInfo.UseShellExecute=$false; $p.StartInfo.CreateNoWindow=$true; [void]$p.Start(); [IO.File]::WriteAllText($env:SS_TEST_PID,[string]$p.Id); Start-Sleep -Seconds 30"]);
        let cancel = AtomicBool::new(false);
        let deadline = Instant::now() + Duration::from_secs(15);
        let (out, pid, cancelled_at) = thread::scope(|scope| {
            let ready = scope.spawn(|| {
                // Startup latency is not cancellation latency. Observe a live
                // descendant before asking the real bounded runner to cancel.
                let ready_deadline = Instant::now() + Duration::from_secs(10);
                let pid = loop {
                    if let Some(pid) = std::fs::read_to_string(&pid_file)
                        .ok()
                        .and_then(|v| v.trim().parse::<u32>().ok())
                    {
                        break pid;
                    }
                    assert!(
                        Instant::now() < ready_deadline,
                        "grandchild PID was not recorded"
                    );
                    thread::sleep(Duration::from_millis(10));
                };
                assert!(
                    pid_alive(pid),
                    "grandchild must be running before cancellation"
                );
                let cancelled_at = Instant::now();
                cancel.store(true, Ordering::SeqCst);
                (pid, cancelled_at)
            });
            let out = run_bounded_command_until(command, deadline, 1024, 1024, &cancel).unwrap();
            let (pid, cancelled_at) = ready.join().unwrap();
            (out, pid, cancelled_at)
        });
        assert!(out.cancelled && !out.timed_out && out.status.is_some());
        assert!(cancelled_at.elapsed() < Duration::from_secs(3));
        let exit_deadline = Instant::now() + Duration::from_secs(1);
        while pid_alive(pid) && Instant::now() < exit_deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!pid_alive(pid), "grandchild survived job termination");
        std::fs::remove_dir_all(temp).unwrap();
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
}
