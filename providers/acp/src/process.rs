//! Process-tree ownership: every child runs in a Windows job object with
//! `KILL_ON_JOB_CLOSE`, so closing or terminating the job reaps MCP servers
//! and other descendants. Other platforms only own the direct child.

use std::io::Read;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::*, System::Diagnostics::ToolHelp::*, System::JobObjects::*, System::Threading::*,
};

/// One unnamed kill-on-close job; a no-op off Windows.
pub struct JobGuard(#[cfg_attr(not(windows), allow(dead_code))] usize);

impl JobGuard {
    pub fn terminate(&self) {
        #[cfg(windows)]
        unsafe {
            TerminateJobObject(self.0 as HANDLE, 1);
        }
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            CloseHandle(self.0 as HANDLE);
        }
    }
}

/// Spawn suspended, assign to a fresh job, then resume the primary thread, so
/// the child cannot start a grandchild outside the job.
pub fn spawn_in_job(command: &mut Command) -> Result<(Child, JobGuard), String> {
    #[cfg(windows)]
    unsafe {
        use std::os::windows::{io::AsRawHandle, process::CommandExt};
        let job = JobGuard(CreateJobObjectW(std::ptr::null(), std::ptr::null()) as usize);
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size = std::mem::size_of_val(&info) as u32;
        SetInformationJobObject(job.0 as HANDLE, JobObjectExtendedLimitInformation, &info as *const _ as _, size);
        // CREATE_NO_WINDOW | CREATE_SUSPENDED
        let mut child = command.creation_flags(0x0800_0000 | 0x4).spawn().map_err(|e| e.to_string())?;
        if AssignProcessToJobObject(job.0 as HANDLE, child.as_raw_handle() as HANDLE) == 0 || !resume(child.id()) {
            let _ = child.kill();
            return Err(format!("无法管理子进程生命周期：{}", std::io::Error::last_os_error()));
        }
        Ok((child, job))
    }
    #[cfg(not(windows))]
    Ok((command.spawn().map_err(|e| e.to_string())?, JobGuard(0)))
}

#[cfg(windows)]
unsafe fn resume(pid: u32) -> bool {
    let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
    let mut entry: THREADENTRY32 = std::mem::zeroed();
    entry.dwSize = std::mem::size_of_val(&entry) as u32;
    let mut more = Thread32First(snapshot, &mut entry);
    while more != 0 && entry.th32OwnerProcessID != pid {
        more = Thread32Next(snapshot, &mut entry);
    }
    CloseHandle(snapshot);
    if more == 0 {
        return false;
    }
    let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
    let resumed = ResumeThread(thread) != u32::MAX;
    CloseHandle(thread);
    resumed
}

/// Run a short command (`dsh --version`, `--dump-config`) to completion,
/// killing its tree on cancellation or at the deadline.
pub fn output_until(mut command: Command, deadline: Instant, cancel: &AtomicBool) -> Result<Output, String> {
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let (mut child, job) = spawn_in_job(&mut command)?;
    let drain = |mut pipe: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            bytes
        })
    };
    let stdout = drain(Box::new(child.stdout.take().unwrap()));
    let stderr = drain(Box::new(child.stderr.take().unwrap()));
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        let cancelled = cancel.load(Ordering::SeqCst);
        if cancelled || Instant::now() >= deadline {
            job.terminate();
            let _ = child.kill();
            let _ = child.wait();
            return Err(if cancelled { "DSH 就绪检查已取消" } else { "DSH 就绪检查已超时" }.into());
        }
        thread::sleep(Duration::from_millis(10));
    };
    job.terminate();
    Ok(Output { status, stdout: stdout.join().unwrap(), stderr: stderr.join().unwrap() })
}

#[cfg(test)]
mod tests;
