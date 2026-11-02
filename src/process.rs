#[path = "process_output.rs"]
mod output;
use shishi_acp::process::{spawn_in_job, JobGuard};
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
static STOPPING: AtomicBool = AtomicBool::new(false);
static JOBS: Mutex<Vec<Arc<JobGuard>>> = Mutex::new(Vec::new());
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
    for job in jobs.iter() {
        job.terminate();
    }
}
pub struct OwnedChild {
    pub child: std::process::Child,
    job: Arc<JobGuard>,
}
impl OwnedChild {
    pub fn terminate(&self) {
        self.job.terminate();
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

pub fn powershell(system_root: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(
        Path::new(system_root.as_ref()).join("System32/WindowsPowerShell/v1.0/powershell.exe"),
    );
    command.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
    ]);
    command
}

pub fn run(
    cmd: Command,
    input: Option<&str>,
    timeout: u64,
    dir: &Path,
) -> Result<(String, String), String> {
    run_cancellable(cmd, input, timeout, dir, None, &mut |_| {})
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
    run_guarded_polling(cmd, input, timeout, dir, &cancelled, &mut |line| {
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
            owned.job.terminate();
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

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    #[test]
    fn cancellation_stops_only_the_owned_process_tree_and_cleans_temporary_files() {
        let dir = std::env::temp_dir().join(format!("shishi-cancel-{}", uuid::Uuid::new_v4()));
        let flag = AtomicBool::new(false);
        let mut cmd = Command::new("powershell.exe");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command",
            // ShellExecute/Start-Process can block in desktop shell integrations
            // before emitting the PID. Launch the synthetic child directly so
            // this fixture measures our owned job and cancellation deadline.
            "$p=[Diagnostics.Process]::new(); $p.StartInfo.FileName='powershell.exe'; $p.StartInfo.Arguments='-NoProfile -NonInteractive -Command Start-Sleep -Seconds 30'; $p.StartInfo.UseShellExecute=$false; $p.StartInfo.CreateNoWindow=$true; [void]$p.Start(); [Console]::WriteLine($p.Id); Start-Sleep -Seconds 30"]);
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
