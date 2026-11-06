//! Executor state: the one AI call slot, the manual requests beside it,
//! the in-memory job table for long IPC calls, and cancellation. Nothing
//! here is persisted, so a restart drops queued and running internal work
//! by construction.
//!
//! Only internal AI work (analysis, archive, reports, prompt review) takes
//! the slot. Taking it never queues: a busy slot is an immediate error,
//! except for analysis after a scan, which waits for the release event.
//! One-sentence task edits run as manual requests beside it, any number at
//! once. However the work ends, dropping the [`Call`] ends the request.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::{time, Db, Error, Result};

/// The context of one AI request: its cooperative cancellation flag and
/// the current request id, shared with provider calls.
#[derive(Clone, Default)]
pub struct Cancel(Arc<CancelInner>);

#[derive(Default)]
struct CancelInner {
    flag: AtomicBool,
    /// Stopped by the user or shutdown, not by its own timeout.
    interrupted: AtomicBool,
    notify: Notify,
    request: Mutex<String>,
}

impl Cancel {
    /// The id of the provider request now running under this context.
    pub fn request_id(&self) -> String {
        self.0.request.lock().unwrap().clone()
    }

    pub fn cancel(&self) {
        self.0.flag.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    fn interrupt(&self) {
        self.0.interrupted.store(true, Ordering::SeqCst);
        self.cancel();
    }

    fn is_interrupted(&self) -> bool {
        self.0.interrupted.load(Ordering::SeqCst)
    }

    /// The raw flag for blocking provider code.
    pub fn flag(&self) -> &AtomicBool {
        &self.0.flag
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.flag.load(Ordering::SeqCst)
    }

    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

pub const CANCELLED: &str = "本次 AI 任务已中断，可手动重新执行";
pub const BUSY: &str = "AI 正在处理其他请求，请稍后重试";
pub const ARCHIVE_BUSY: &str = "归档已排队或正在执行，请稍后重试";
pub const WAIT_TIMEOUT: &str = "等待 AI 空闲达到分析总时限，消息已保存，请稍后重试";
const MAX_JOBS: usize = 32;
const MAX_RUNNING_JOBS: usize = 8;
const LIVE_ENTRY_CHARS: usize = 12_000;
const LIVE_TOTAL_CHARS: usize = 48_000;
const LIVE_ENTRIES: usize = 64;

/// What the page shows about a running AI request.
#[derive(Clone, Default)]
pub struct AiStatus {
    pub request_id: String,
    pub source: String,
    /// What a manual request changes (the task of an edit).
    pub subject: String,
    pub started: i64,
    pub cancelling: bool,
    pub live: Live,
    pub interrupted_at: Option<i64>,
}

/// Streamed model output for the running request (`state.ai_live`).
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Live {
    pub request_id: String,
    pub phase: String,
    pub last_received: i64,
    pub revision: u64,
    pub entries: VecDeque<LiveEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_response_ms: Option<i64>,
    pub output_tokens: u64,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub generation: Value,
    pub usage_final: bool,
    pub usage_elapsed_ms: i64,
    #[serde(skip)]
    next_entry: u64,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct LiveEntry {
    pub id: String,
    pub kind: String,
    pub text: String,
}

impl Live {
    fn new(id: &str) -> Self {
        Self { request_id: id.into(), phase: "正在连接".into(), ..Default::default() }
    }

    /// Append streamed text: it extends the newest entry of the same kind,
    /// otherwise starts a new entry. Old entries drop past the limits.
    pub fn push(&mut self, kind: &str, delta: &str, started: i64) {
        let at = time::now();
        self.first_response_ms.get_or_insert((at - started).max(0));
        self.last_received = at;
        self.revision += 1;
        match self.entries.back_mut() {
            Some(last) if last.kind == kind => last.text.push_str(delta),
            _ => {
                self.next_entry += 1;
                self.entries.push_back(LiveEntry {
                    id: format!("e{}", self.next_entry),
                    kind: kind.into(),
                    text: delta.into(),
                });
            }
        }
        let last = self.entries.back_mut().unwrap();
        let count = last.text.chars().count();
        if count > LIVE_ENTRY_CHARS {
            last.text = last.text.chars().skip(count - LIVE_ENTRY_CHARS).collect();
        }
        let total = |e: &VecDeque<LiveEntry>| e.iter().map(|e| e.text.chars().count()).sum::<usize>();
        while self.entries.len() > LIVE_ENTRIES || (self.entries.len() > 1 && total(&self.entries) > LIVE_TOTAL_CHARS) {
            self.entries.pop_front();
        }
    }
}

enum Slot {
    Running,
    Done(Result<Value>),
}

struct Active {
    status: AiStatus,
    cancel: Cancel,
    manual: bool,
}

pub struct Executor {
    db: Db,
    /// The slot's request (at most one, `manual == false`) and the manual
    /// requests beside it, oldest first.
    active: Mutex<Vec<Active>>,
    /// Wakes reservations waiting for the slot.
    released: Notify,
    last_interrupt: Mutex<Option<i64>>,
    jobs: Mutex<HashMap<String, Slot>>,
}

impl Executor {
    pub fn new(db: Db) -> Self {
        Self {
            db,
            active: Mutex::default(),
            released: Notify::new(),
            last_interrupt: Mutex::default(),
            jobs: Mutex::default(),
        }
    }

    /// Take the AI call slot now; another call holding it gives [`BUSY`].
    pub fn try_call(self: &Arc<Self>, source: &str) -> Result<Call> {
        let mut active = self.active.lock().unwrap();
        if active.iter().any(|a| !a.manual) {
            return Err(Error::new(BUSY));
        }
        // Reset under the same lock as interruption, before the id is visible.
        *self.last_interrupt.lock().unwrap() = None;
        Ok(self.open(&mut active, source, "", false))
    }

    /// Start a manual request beside the slot (a one-sentence edit of
    /// `subject`): it neither waits for nor blocks the slot.
    pub fn manual_call(self: &Arc<Self>, source: &str, subject: &str) -> Call {
        self.open(&mut self.active.lock().unwrap(), source, subject, true)
    }

    fn open(self: &Arc<Self>, active: &mut Vec<Active>, source: &str, subject: &str, manual: bool) -> Call {
        let cancel = Cancel::default();
        let id = uuid::Uuid::new_v4().to_string();
        *cancel.0.request.lock().unwrap() = id.clone();
        let status = AiStatus {
            request_id: id.clone(),
            source: source.into(),
            subject: subject.into(),
            started: time::now(),
            live: Live::new(&id),
            ..Default::default()
        };
        active.push(Active { status, cancel: cancel.clone(), manual });
        self.db.bump();
        Call { exec: self.clone(), id, cancel }
    }

    /// Change the request `id` under the lock.
    fn with<R>(&self, id: &str, change: impl FnOnce(&mut Active) -> R) -> Option<R> {
        self.active.lock().unwrap().iter_mut().find(|a| a.status.request_id == id).map(change)
    }

    /// Wait for the slot, event-driven, until `deadline`. Interrupting the
    /// call it waits behind also ends the wait, so an interrupt cannot
    /// silently start the backlog again; that call failing or timing out
    /// is its own and the waiter takes the slot once it is released.
    pub async fn wait_call(self: &Arc<Self>, source: &str, deadline: Instant) -> Result<Call> {
        let mut previous: Option<Cancel> = None;
        loop {
            let released = self.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if previous.as_ref().is_some_and(Cancel::is_interrupted) {
                return Err(Error::new(CANCELLED));
            }
            if Instant::now() >= deadline {
                return Err(Error::new(WAIT_TIMEOUT));
            }
            match self.try_call(source) {
                Err(e) if e.0 == BUSY => {}
                call => return call,
            }
            previous = self.active.lock().unwrap().iter().find(|a| !a.manual).map(|a| a.cancel.clone());
            let stopped = previous.clone().unwrap_or_default();
            tokio::select! {
                _ = &mut released => {}
                _ = stopped.cancelled(), if previous.as_ref().is_some_and(|p| !p.is_cancelled()) => {}
                _ = tokio::time::sleep_until(deadline.into()) => {}
            }
        }
    }

    /// Take the slot now and run `work` within `timeout`. An interrupt gives
    /// [`CANCELLED`]; the timeout sets the cancel flag and gives
    /// "AI 调用超时（N 秒）". The slot is released however it ends.
    pub async fn run_ai<'a, F, Fut, R>(self: &Arc<Self>, source: &str, timeout: Duration, work: F) -> Result<R>
    where
        F: FnOnce(Cancel) -> Fut,
        Fut: Future<Output = Result<R>> + 'a,
    {
        run(self.try_call(source)?, timeout, work).await
    }

    /// [`Self::run_ai`], waiting up to `timeout` for the slot first.
    pub async fn wait_ai<'a, F, Fut, R>(self: &Arc<Self>, source: &str, timeout: Duration, work: F) -> Result<R>
    where
        F: FnOnce(Cancel) -> Fut,
        Fut: Future<Output = Result<R>> + 'a,
    {
        run(self.wait_call(source, Instant::now() + timeout).await?, timeout, work).await
    }

    /// [`Self::run_ai`] as a manual request beside the slot; never [`BUSY`].
    pub async fn run_manual<'a, F, Fut, R>(
        self: &Arc<Self>,
        source: &str,
        subject: &str,
        timeout: Duration,
        work: F,
    ) -> Result<R>
    where
        F: FnOnce(Cancel) -> Fut,
        Fut: Future<Output = Result<R>> + 'a,
    {
        run(self.manual_call(source, subject), timeout, work).await
    }

    /// Resolves when the slot is next released. A release just before the
    /// call is missed; callers bound the wait.
    pub async fn released(&self) {
        self.released.notified().await
    }

    pub fn ai_busy(&self) -> bool {
        self.ai_status().is_some()
    }

    pub fn ai_status(&self) -> Option<AiStatus> {
        self.active.lock().unwrap().iter().find(|a| !a.manual).map(|a| a.status.clone())
    }

    /// The manual requests running beside the slot, oldest first.
    pub fn manual_status(&self) -> Vec<AiStatus> {
        self.active.lock().unwrap().iter().filter(|a| a.manual).map(|a| a.status.clone()).collect()
    }

    pub fn last_interrupt(&self) -> Option<i64> {
        *self.last_interrupt.lock().unwrap()
    }

    /// Change the live view of request `request_id` while it runs.
    /// Runtime-only: the business revision stays.
    pub fn live_for(&self, request_id: &str, change: impl FnOnce(&mut Live, i64)) -> bool {
        let changed = self.with(request_id, |a| change(&mut a.status.live, a.status.started)).is_some();
        if changed {
            self.db.poke();
        }
        changed
    }

    /// Ask running AI work to stop. `None` stops everything (shutdown); an
    /// id stops only that request. Stopping the slot's request is recorded
    /// as the last interrupt.
    pub fn interrupt(&self, request_id: Option<&str>) -> bool {
        let at = time::now();
        let mut stopped = false;
        for a in
            self.active.lock().unwrap().iter_mut().filter(|a| request_id.is_none_or(|id| id == a.status.request_id))
        {
            a.status.cancelling = true;
            a.status.interrupted_at = Some(at);
            a.cancel.interrupt();
            if !a.manual {
                *self.last_interrupt.lock().unwrap() = Some(at);
            }
            stopped = true;
        }
        if stopped {
            self.db.bump();
        }
        stopped
    }

    /// Register a long IPC call; the page polls `job_result` with the id.
    pub fn job_start(self: &Arc<Self>, work: impl Future<Output = Result<Value>> + Send + 'static) -> Result<String> {
        let mut jobs = self.jobs.lock().unwrap();
        if jobs.values().filter(|s| matches!(s, Slot::Running)).count() >= MAX_RUNNING_JOBS {
            crate::bail!("后台操作繁忙，请等待当前操作结束");
        }
        let mut excess = (jobs.len() + 1).saturating_sub(MAX_JOBS);
        jobs.retain(|_, s| {
            let keep = excess == 0 || matches!(s, Slot::Running);
            excess -= usize::from(!keep);
            keep
        });
        let id = uuid::Uuid::new_v4().to_string();
        jobs.insert(id.clone(), Slot::Running);
        let (exec, key) = (self.clone(), id.clone());
        tokio::spawn(async move {
            let result = tokio::spawn(work).await.unwrap_or_else(|_| Err(Error::new("后台操作异常终止，请查看日志")));
            exec.jobs.lock().unwrap().insert(key, Slot::Done(result));
            exec.db.bump();
        });
        Ok(id)
    }

    /// `{done:false}` while running, then `{done:true, ok, data|error}`.
    /// Repeated queries return the same result and never resubmit.
    pub fn job_result(&self, id: &str) -> Result<Value> {
        if id.is_empty() {
            crate::bail!("缺少操作编号");
        }
        match self.jobs.lock().unwrap().get(id) {
            None => Err(Error::new("操作记录已结束或程序已重启，请查看状态与日志；不要重复提交")),
            Some(Slot::Running) => Ok(json!({"done": false})),
            Some(Slot::Done(Ok(data))) => Ok(json!({"done": true, "ok": true, "data": data})),
            Some(Slot::Done(Err(e))) => Ok(json!({"done": true, "ok": false, "error": e.0})),
        }
    }
}

async fn run<'a, F, Fut, R>(call: Call, timeout: Duration, work: F) -> Result<R>
where
    F: FnOnce(Cancel) -> Fut,
    Fut: Future<Output = Result<R>> + 'a,
{
    let cancel = call.cancel.clone();
    let result = tokio::select! {
        result = work(cancel.clone()) => result,
        _ = cancel.cancelled() => Err(Error::new(CANCELLED)),
        _ = tokio::time::sleep(timeout) => {
            cancel.cancel();
            Err(Error(format!("AI 调用超时（{} 秒）", timeout.as_secs())))
        }
    };
    if call.finish() && result.is_ok() {
        return Err(Error::new(CANCELLED));
    }
    result
}

/// A held AI request. A multi-batch round keeps one `Call` (one
/// reservation, one cancel flag) and gives each provider request its own id
/// through [`Call::next_request`]. Dropping it ends the request.
pub struct Call {
    exec: Arc<Executor>,
    id: String,
    pub cancel: Cancel,
}

impl Call {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// New request id for the next provider request of the same call.
    pub fn next_request(&mut self) -> Result<()> {
        if self.cancel.is_cancelled() {
            return Err(Error::new(CANCELLED));
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.exec
            .with(&self.id, |a| {
                a.status.request_id = id.clone();
                a.status.live = Live::new(&id);
                a.status.started = time::now();
                *a.cancel.0.request.lock().unwrap() = id.clone();
            })
            .ok_or_else(|| Error::new(CANCELLED))?;
        self.id = id;
        self.exec.db.poke();
        Ok(())
    }

    /// End the request and report whether it was cancelled. Closing and the
    /// cancel check share the lock, so a late interrupt can neither slip
    /// into an accepted result nor stop the next request.
    pub fn finish(&self) -> bool {
        let mut active = self.exec.active.lock().unwrap();
        let cancelled = self.cancel.is_cancelled();
        let Some(at) = active.iter().position(|a| a.status.request_id == self.id) else { return cancelled };
        let manual = active.remove(at).manual;
        drop(active);
        if !manual {
            self.exec.released.notify_waiters();
        }
        self.exec.db.bump();
        cancelled
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Holds a busy flag until dropped.
pub(crate) struct Flag<'a>(&'a AtomicBool);

impl<'a> Flag<'a> {
    /// Set `flag`; `None` when it was already set.
    pub(crate) fn claim(flag: &'a AtomicBool) -> Option<Self> {
        (!flag.swap(true, Ordering::SeqCst)).then(|| Self(flag))
    }
}

impl Drop for Flag<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Run a blocking call (provider client, child process wait) on the bounded
/// blocking pool so the executor thread keeps serving requests.
pub async fn blocking<R: Send + 'static>(f: impl FnOnce() -> Result<R> + Send + 'static) -> Result<R> {
    tokio::task::spawn_blocking(f).await.unwrap_or_else(|_| Err(Error::new("后台调用异常结束")))
}

#[cfg(test)]
mod tests;
