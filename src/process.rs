#[path = "process_output.rs"]
mod output;
use std::os::windows::process::CommandExt;
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use std::os::windows::io::AsRawHandle;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};
use windows_sys::Win32::{Foundation::CloseHandle, System::JobObjects::*};
static STOPPING: AtomicBool = AtomicBool::new(false);
static JOBS: Mutex<Vec<usize>> = Mutex::new(Vec::new());
pub fn stopping() -> bool {
    STOPPING.load(Ordering::SeqCst)
}
pub fn check_running() -> Result<(), String> {
    if stopping() {
        Err("软件正在退出，未完成工作将在重启后继续".into())
    } else {
        Ok(())
    }
}
pub fn shutdown() {
    let jobs = JOBS.lock().unwrap();
    STOPPING.store(true, Ordering::SeqCst);
    for &job in jobs.iter() {
        unsafe {
            TerminateJobObject(job as _, 1);
        }
    }
}
pub struct OwnedChild {
    pub child: std::process::Child,
    job: usize,
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let mut jobs = JOBS.lock().unwrap();
        jobs.retain(|j| *j != self.job);
        unsafe {
            CloseHandle(self.job as _);
        }
        drop(jobs);
        let _ = self.child.wait();
    }
}
pub fn spawn_owned(cmd: &mut Command) -> Result<OwnedChild, String> {
    let mut jobs = JOBS.lock().unwrap();
    check_running()?;
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as _,
            std::mem::size_of_val(&info) as u32,
        ) == 0
        {
            CloseHandle(job);
            return Err(std::io::Error::last_os_error().to_string());
        }
        // Assign the job before any child code can launch grandchildren.
        cmd.creation_flags(0x08000000 | 0x00000004);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                CloseHandle(job);
                return Err(e.to_string());
            }
        };
        if AssignProcessToJobObject(job, child.as_raw_handle()) == 0 {
            let e = std::io::Error::last_os_error();
            let _ = child.kill();
            let _ = child.wait();
            CloseHandle(job);
            return Err(format!("无法管理子进程生命周期：{e}"));
        }
        if let Err(e) = resume_child(child.id()) {
            let _ = child.kill();
            let _ = child.wait();
            CloseHandle(job);
            return Err(e);
        }
        jobs.push(job as usize);
        Ok(OwnedChild {
            child,
            job: job as usize,
        })
    }
}

pub fn run(
    cmd: Command,
    input: Option<&str>,
    timeout: u64,
    dir: &Path,
) -> Result<(String, String), String> {
    run_with_progress(cmd, input, timeout, dir, &mut |_| {})
}
pub fn run_with_progress(
    cmd: Command,
    input: Option<&str>,
    timeout: u64,
    dir: &Path,
    progress: &mut dyn FnMut(&str),
) -> Result<(String, String), String> {
    run_cancellable(cmd, input, timeout, dir, None, progress)
}
pub fn run_cancellable(
    cmd: Command,
    input: Option<&str>,
    timeout: u64,
    dir: &Path,
    cancel: Option<&AtomicBool>,
    progress: &mut dyn FnMut(&str),
) -> Result<(String, String), String> {
    let cancelled = || cancel.is_some_and(|c| c.load(Ordering::SeqCst));
    run_guarded(cmd, input, timeout, dir, &cancelled, progress)
}
pub fn run_guarded(
    cmd: Command,
    input: Option<&str>,
    timeout: u64,
    dir: &Path,
    cancelled: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(&str),
) -> Result<(String, String), String> {
    run_guarded_polling(cmd, input, timeout, dir, cancelled, &mut |line| {
        if !line.is_empty() {
            progress(line);
        }
    })
}
/// Empty callbacks are reader ticks, never provider response text.
pub fn run_guarded_polling(
    mut cmd: Command,
    input: Option<&str>,
    timeout: u64,
    dir: &Path,
    cancelled: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(&str),
) -> Result<(String, String), String> {
    if cancelled() {
        return Err(crate::ai_control::CANCELLED.into());
    }
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let nonce = uuid::Uuid::new_v4().to_string();
    let out = dir.join(format!("{nonce}.out"));
    let err = dir.join(format!("{nonce}.err"));
    let input_file = dir.join(format!("{nonce}.in"));
    let _files = output::Files(vec![out.clone(), err.clone(), input_file.clone()]);
    cmd.stdout(Stdio::from(
        fs::File::create(&out).map_err(|e| e.to_string())?,
    ))
    .stderr(Stdio::from(
        fs::File::create(&err).map_err(|e| e.to_string())?,
    ));
    if let Some(text) = input {
        fs::write(&input_file, text).map_err(|e| e.to_string())?;
        cmd.stdin(Stdio::from(
            fs::File::open(&input_file).map_err(|e| e.to_string())?,
        ));
    } else {
        cmd.stdin(Stdio::null());
    }
    let mut owned = spawn_owned(&mut cmd)?;
    let child = &mut owned.child;
    let mut reader = output::Reader::new(&out, output::LINE_LIMIT)?;
    let start = Instant::now();
    let code = loop {
        progress("");
        output::check_size(&out, output::STDOUT_LIMIT)?;
        output::check_size(&err, output::STDERR_LIMIT)?;
        let more = reader.poll(progress)?;

        match child.try_wait() {
            Ok(Some(code)) => break code,
            Ok(None) => {}
            Err(e) => return Err(e.to_string()),
        }
        let interrupted = cancelled();
        if interrupted || start.elapsed() > Duration::from_secs(timeout) {
            unsafe {
                TerminateJobObject(owned.job as _, 1);
            }
            let _ = child.kill();
            let _ = child.wait();
            if interrupted {
                return Err(crate::ai_control::CANCELLED.into());
            }
            return Err(format!(
                "操作超过 {timeout} 秒，已停止本次子进程；下次可重试"
            ));
        }
        if more {
            std::thread::yield_now();
        } else {
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    output::check_size(&out, output::STDOUT_LIMIT)?;
    output::check_size(&err, output::STDERR_LIMIT)?;
    reader.finish(progress, || {
        check_running()?;
        output::check_size(&out, output::STDOUT_LIMIT)?;
        output::check_size(&err, output::STDERR_LIMIT)?;
        if cancelled() {
            return Err(crate::ai_control::CANCELLED.into());
        }
        if start.elapsed() > Duration::from_secs(timeout) {
            return Err(format!(
                "操作超过 {timeout} 秒，已停止本次子进程；下次可重试"
            ));
        }
        Ok(())
    })?;
    drop(reader);
    let stdout = output::read(&out, output::STDOUT_LIMIT)?;
    let stderr = output::read(&err, output::STDERR_LIMIT)?;
    check_running()?;
    if cancelled() {
        return Err(crate::ai_control::CANCELLED.into());
    }
    if !code.success() {
        return Err(format!(
            "进程退出 {}：{}",
            code,
            stderr.chars().take(1400).collect::<String>()
        ));
    }
    Ok((stdout, stderr))
}

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

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    #[test]
    fn cancellation_stops_only_the_owned_process_tree_and_cleans_temporary_files() {
        let dir = std::env::temp_dir().join(format!("shishi-cancel-{}", uuid::Uuid::new_v4()));
        let flag = AtomicBool::new(false);
        let mut cmd = Command::new("powershell.exe");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command",
            "$p=Start-Process powershell.exe -WindowStyle Hidden -PassThru -ArgumentList '-NoProfile -NonInteractive -Command Start-Sleep -Seconds 30'; Write-Output $p.Id; Start-Sleep -Seconds 30"]);
        let mut descendant = std::ptr::null_mut();
        let start = Instant::now();
        let result = run_cancellable(
            cmd,
            Some("test input"),
            15,
            &dir,
            Some(&flag),
            &mut |line| {
                if let Ok(pid) = line.parse::<u32>() {
                    descendant = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
                    flag.store(true, Ordering::SeqCst);
                }
            },
        );
        assert_eq!(result.unwrap_err(), crate::ai_control::CANCELLED);
        assert!(start.elapsed() < Duration::from_secs(10));
        assert!(!descendant.is_null());
        let mut exit_code = 259;
        for _ in 0..20 {
            assert_ne!(unsafe { GetExitCodeProcess(descendant, &mut exit_code) }, 0);
            if exit_code != 259 {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        unsafe {
            CloseHandle(descendant);
        }
        assert_ne!(exit_code, 259, "grandchild must terminate with the request");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        assert!(
            !stopping(),
            "cancelling AI must not shut down the application"
        );
        let mut next = Command::new("cmd.exe");
        next.args(["/D", "/C", "echo next"]);
        assert_eq!(run(next, None, 5, &dir).unwrap().0.trim(), "next");
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn already_cancelled_request_never_launches() {
        let dir = std::env::temp_dir().join(format!("shishi-pre-cancel-{}", uuid::Uuid::new_v4()));
        let flag = AtomicBool::new(true);
        assert_eq!(
            run_cancellable(
                Command::new("nonexistent.exe"),
                None,
                5,
                &dir,
                Some(&flag),
                &mut |_| {}
            )
            .unwrap_err(),
            crate::ai_control::CANCELLED
        );
        assert!(!dir.exists());
    }
}
