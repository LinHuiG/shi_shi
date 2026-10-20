//! Bounded newline-JSON stdio transport with a Windows job object.
//!
//! Threads: one stdout reader, one stderr reader, one stdin writer. No thread
//! parses prose or logs prompt content. Shutdown closes stdin, waits briefly,
//! then terminates the job so descendants (MCP servers) cannot leak.

use crate::jsonrpc::{decode_line, FrameError, Incoming};
use crate::process::{spawn_in_job, JobGuard};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct TransportConfig {
    pub max_stdout_line: usize,
    pub max_stderr_line: usize,
    pub max_stderr_lines: usize,
    pub writer_queue: usize,
    pub shutdown_grace: Duration,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            max_stdout_line: 8 * 1024 * 1024,
            max_stderr_line: 4 * 1024,
            max_stderr_lines: 8,
            writer_queue: 128,
            shutdown_grace: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, Clone)]
pub enum TransportError {
    Spawn(String),
    Job(String),
    Closed,
    Write(String),
    Read(String),
    Protocol(FrameError),
    Timeout,
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(message) => write!(f, "spawn failed: {message}"),
            Self::Job(message) => write!(f, "job assignment failed: {message}"),
            Self::Closed => f.write_str("ACP transport is closed"),
            Self::Write(message) => write!(f, "ACP stdin write failed: {message}"),
            Self::Read(message) => write!(f, "ACP stdout read failed: {message}"),
            Self::Protocol(error) => write!(f, "ACP protocol error: {error}"),
            Self::Timeout => f.write_str("ACP transport timed out"),
        }
    }
}

impl std::error::Error for TransportError {}

#[derive(Debug)]
enum ReadEvent {
    Message(Incoming),
    Eof,
    Failed(TransportError),
}

struct StderrBuffer {
    lines: VecDeque<String>,
    total_bytes: usize,
    total_lines: usize,
    max_lines: usize,
    max_line_bytes: usize,
    max_total_bytes: usize,
}

impl StderrBuffer {
    fn new(max_lines: usize, max_line_bytes: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            total_bytes: 0,
            total_lines: 0,
            max_lines,
            max_line_bytes,
            max_total_bytes: max_lines.saturating_mul(max_line_bytes),
        }
    }

    fn push(&mut self, line: &[u8]) {
        self.total_lines += 1;
        let text = String::from_utf8_lossy(line)
            .trim_end_matches(['\r', '\n'])
            .chars()
            .filter(|c| !c.is_control() || *c == '\t')
            .take(self.max_line_bytes)
            .collect::<String>();
        if text.is_empty() {
            return;
        }
        self.total_bytes += text.len();
        self.lines.push_back(text);
        while self.lines.len() > self.max_lines {
            if let Some(removed) = self.lines.pop_front() {
                self.total_bytes = self.total_bytes.saturating_sub(removed.len());
            }
        }
        while self.total_bytes > self.max_total_bytes {
            if let Some(removed) = self.lines.pop_front() {
                self.total_bytes = self.total_bytes.saturating_sub(removed.len());
            } else {
                break;
            }
        }
    }

    fn snapshot(&self) -> StderrSnapshot {
        StderrSnapshot {
            lines: self.lines.iter().cloned().collect(),
            total_lines: self.total_lines,
            total_bytes: self.total_bytes,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StderrSnapshot {
    pub lines: Vec<String>,
    pub total_lines: usize,
    pub total_bytes: usize,
}

pub struct Transport {
    child: Mutex<Option<Child>>,
    job: JobGuard,
    writer: Mutex<Option<SyncSender<Vec<u8>>>>,
    incoming: Mutex<Receiver<ReadEvent>>,
    stderr: Arc<Mutex<StderrBuffer>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    closed: Arc<AtomicBool>,
    shutdown_started: AtomicBool,
    write_error: Arc<Mutex<Option<String>>>,
    config: TransportConfig,
}

impl Transport {
    pub fn spawn(mut command: Command, config: TransportConfig) -> Result<Self, TransportError> {
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        let (mut child, job) = spawn_in_job(command).map_err(TransportError::Job)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| TransportError::Spawn("stdin pipe".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| TransportError::Spawn("stdout pipe".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| TransportError::Spawn("stderr pipe".into()))?;

        let (event_tx, event_rx) = sync_channel::<ReadEvent>(256);
        let (write_tx, write_rx) = sync_channel::<Vec<u8>>(config.writer_queue);
        let closed = Arc::new(AtomicBool::new(false));
        let write_error = Arc::new(Mutex::new(None));
        let stderr_buffer = Arc::new(Mutex::new(StderrBuffer::new(
            config.max_stderr_lines,
            config.max_stderr_line,
        )));

        let reader_closed = closed.clone();
        let max_line = config.max_stdout_line;
        let stdout_thread = thread::spawn(move || {
            read_stdout(stdout, max_line, event_tx, reader_closed);
        });

        let stderr_buffer_thread = stderr_buffer.clone();
        let max_stderr_line = config.max_stderr_line;
        let stderr_thread = thread::spawn(move || {
            read_stderr(stderr, stderr_buffer_thread, max_stderr_line);
        });

        let writer_error = write_error.clone();
        let writer_thread = thread::spawn(move || {
            write_stdin(stdin, write_rx, writer_error);
        });

        Ok(Self {
            child: Mutex::new(Some(child)),
            job,
            writer: Mutex::new(Some(write_tx)),
            incoming: Mutex::new(event_rx),
            stderr: stderr_buffer,
            threads: Mutex::new(vec![stdout_thread, stderr_thread, writer_thread]),
            closed,
            shutdown_started: AtomicBool::new(false),
            write_error,
            config,
        })
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub fn send_line(&self, line: Vec<u8>) -> Result<(), TransportError> {
        if self.is_closed() {
            return Err(TransportError::Closed);
        }
        if let Some(error) = self.write_error.lock().unwrap().clone() {
            return Err(TransportError::Write(error));
        }
        let sender = self
            .writer
            .lock()
            .unwrap()
            .clone()
            .ok_or(TransportError::Closed)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match sender.try_send(line.clone()) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Full(_)) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(TrySendError::Full(_)) => {
                    return Err(TransportError::Write("stdin queue is full".into()))
                }
                Err(TrySendError::Disconnected(_)) => return Err(TransportError::Closed),
            }
        }
    }

    /// Receive one protocol message. Only the ACP request loop calls this, so
    /// notifications and server requests share the same ordered stream.
    pub fn recv(&self, timeout: Duration) -> Result<Incoming, TransportError> {
        let event = {
            let receiver = self.incoming.lock().unwrap();
            receiver
                .recv_timeout(timeout)
                .map_err(|error| match error {
                    std::sync::mpsc::RecvTimeoutError::Timeout => TransportError::Timeout,
                    std::sync::mpsc::RecvTimeoutError::Disconnected => TransportError::Closed,
                })?
        };
        match event {
            ReadEvent::Message(message) => Ok(message),
            ReadEvent::Eof => Err(TransportError::Closed),
            ReadEvent::Failed(error) => Err(error),
        }
    }

    pub fn try_recv(&self) -> Option<Result<Incoming, TransportError>> {
        let receiver = self.incoming.lock().unwrap();
        match receiver.try_recv() {
            Ok(ReadEvent::Message(message)) => Some(Ok(message)),
            Ok(ReadEvent::Eof) => Some(Err(TransportError::Closed)),
            Ok(ReadEvent::Failed(error)) => Some(Err(error)),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(TransportError::Closed)),
        }
    }

    pub fn stderr_snapshot(&self) -> StderrSnapshot {
        self.stderr.lock().unwrap().snapshot()
    }

    /// Idempotent, bounded teardown. Closing the writer drops the child stdin;
    /// the grace period lets the server drain; then the job is terminated.
    pub fn shutdown(&self) {
        if self.shutdown_started.swap(true, Ordering::SeqCst) {
            return;
        }
        self.closed.store(true, Ordering::SeqCst);
        self.writer.lock().unwrap().take();
        let deadline = Instant::now() + self.config.shutdown_grace;
        let mut child_lock = self.child.lock().unwrap();
        if let Some(child) = child_lock.as_mut() {
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Ok(None) => {
                        self.job.terminate();
                        let kill_deadline = Instant::now() + Duration::from_secs(2);
                        while Instant::now() < kill_deadline {
                            if let Ok(Some(_)) = child.try_wait() {
                                break;
                            }
                            thread::sleep(Duration::from_millis(10));
                        }
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    Err(_) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
        drop(child_lock);
        self.job.terminate();
        let threads = std::mem::take(&mut *self.threads.lock().unwrap());
        for handle in threads {
            let _ = handle.join();
        }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn send_event(tx: &SyncSender<ReadEvent>, event: ReadEvent, closed: &AtomicBool) -> Result<(), ()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match tx.try_send(event) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(event)) if !closed.load(Ordering::SeqCst) => {
                if Instant::now() >= deadline {
                    return Err(());
                }
                thread::sleep(Duration::from_millis(2));
                // Keep ownership so the next attempt can retry.
                return send_event_retry(tx, event, closed, deadline);
            }
            Err(_) => return Err(()),
        }
    }
}

fn send_event_retry(
    tx: &SyncSender<ReadEvent>,
    mut event: ReadEvent,
    closed: &AtomicBool,
    deadline: Instant,
) -> Result<(), ()> {
    loop {
        if closed.load(Ordering::SeqCst) || Instant::now() >= deadline {
            return Err(());
        }
        match tx.try_send(event) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(returned)) => {
                event = returned;
                thread::sleep(Duration::from_millis(2));
            }
            Err(_) => return Err(()),
        }
    }
}

fn read_stdout(
    mut stdout: impl Read,
    max_line: usize,
    tx: SyncSender<ReadEvent>,
    closed: Arc<AtomicBool>,
) {
    let mut reader = BufReader::new(&mut stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        let mut too_long = false;
        loop {
            let available = match reader.fill_buf() {
                Ok(available) => available,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let _ = send_event(
                        &tx,
                        ReadEvent::Failed(TransportError::Read(error.to_string())),
                        &closed,
                    );
                    return;
                }
            };
            if available.is_empty() {
                if line.is_empty() {
                    let _ = send_event(&tx, ReadEvent::Eof, &closed);
                } else {
                    let _ = dispatch_line(&line, max_line, &tx, &closed);
                }
                return;
            }
            if let Some(position) = available.iter().position(|byte| *byte == b'\n') {
                let take = position + 1;
                if line.len() + take > max_line + 2 {
                    too_long = true;
                } else {
                    line.extend_from_slice(&available[..take]);
                }
                reader.consume(take);
                break;
            }
            if line.len() + available.len() > max_line + 2 {
                too_long = true;
            } else {
                line.extend_from_slice(available);
            }
            let consumed = available.len();
            reader.consume(consumed);
        }
        if too_long {
            let _ = send_event(
                &tx,
                ReadEvent::Failed(TransportError::Protocol(FrameError::TooLong {
                    bytes: line.len(),
                    limit: max_line,
                })),
                &closed,
            );
            return;
        }
        if dispatch_line(&line, max_line, &tx, &closed).is_err() {
            return;
        }
    }
}

fn dispatch_line(
    line: &[u8],
    max_line: usize,
    tx: &SyncSender<ReadEvent>,
    closed: &AtomicBool,
) -> Result<(), ()> {
    match decode_line(line, max_line) {
        Ok(message) => send_event(tx, ReadEvent::Message(message), closed),
        Err(error) => {
            let _ = send_event(
                tx,
                ReadEvent::Failed(TransportError::Protocol(error)),
                closed,
            );
            Err(())
        }
    }
}

fn read_stderr(mut stderr: impl Read, buffer: Arc<Mutex<StderrBuffer>>, max_line: usize) {
    let mut reader = BufReader::new(&mut stderr);
    let mut line = Vec::new();
    loop {
        line.clear();
        let mut too_long = false;
        loop {
            let available = match reader.fill_buf() {
                Ok(available) => available,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            };
            if available.is_empty() {
                if !line.is_empty() {
                    buffer.lock().unwrap().push(&line);
                }
                return;
            }
            if let Some(position) = available.iter().position(|byte| *byte == b'\n') {
                let take = position + 1;
                if line.len() + take <= max_line + 2 {
                    line.extend_from_slice(&available[..take]);
                } else {
                    too_long = true;
                }
                reader.consume(take);
                break;
            }
            if line.len() + available.len() <= max_line + 2 {
                line.extend_from_slice(available);
            } else {
                too_long = true;
            }
            let consumed = available.len();
            reader.consume(consumed);
        }
        let _ = too_long;
        buffer.lock().unwrap().push(&line);
    }
}

fn write_stdin(
    mut stdin: impl Write,
    rx: Receiver<Vec<u8>>,
    error_slot: Arc<Mutex<Option<String>>>,
) {
    while let Ok(line) = rx.recv() {
        if let Err(error) = stdin.write_all(&line).and_then(|_| stdin.flush()) {
            *error_slot.lock().unwrap() = Some(error.to_string());
            return;
        }
    }
}
