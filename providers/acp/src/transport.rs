//! Bounded newline-JSON stdio transport with a Windows job object.
//!
//! Threads: one stdout reader, one stderr reader, one stdin writer. No thread
//! parses prose or logs prompt content. Shutdown closes stdin, waits briefly,
//! then terminates the job so descendants (MCP servers) cannot leak.

use crate::jsonrpc::{decode_line, read_line, FrameError, Incoming};
use crate::process::{spawn_in_job, JobGuard};
use std::collections::VecDeque;
use std::io::{BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

shishi_api::default_struct! {
    #[derive(Debug, Clone, Copy)]
    pub struct TransportConfig {
        pub max_stdout_line: usize = 8 * 1024 * 1024,
        pub max_stderr_line: usize = 4 * 1024,
        pub max_stderr_lines: usize = 8,
        pub writer_queue: usize = 128,
        pub shutdown_grace: Duration = Duration::from_secs(2),
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum TransportError {
    #[error("spawn failed: {0}")]
    Spawn(String),
    #[error("job assignment failed: {0}")]
    Job(String),
    #[error("ACP transport is closed")]
    Closed,
    #[error("ACP stdin write failed: {0}")]
    Write(String),
    #[error("ACP stdout read failed: {0}")]
    Read(String),
    #[error("ACP protocol error: {0}")]
    Protocol(FrameError),
    #[error("ACP transport timed out")]
    Timeout,
}

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
        let (mut child, job) = spawn_in_job(&mut command).map_err(TransportError::Job)?;
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

    pub fn send_line(&self, mut line: Vec<u8>) -> Result<(), TransportError> {
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
            if self.is_closed() {
                return Err(TransportError::Closed);
            }
            match sender.try_send(line) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Full(returned)) if Instant::now() < deadline => {
                    line = returned;
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
        self.close(self.config.shutdown_grace);
    }

    /// Cancellation and an expired deadline must not wait for a graceful exit.
    pub fn abort(&self) {
        self.job.terminate();
        self.close(Duration::ZERO);
    }

    fn close(&self, grace: Duration) {
        if self.shutdown_started.swap(true, Ordering::SeqCst) {
            return;
        }
        self.closed.store(true, Ordering::SeqCst);
        self.writer.lock().unwrap().take();
        let deadline = Instant::now() + grace;
        let mut child_lock = self.child.lock().unwrap();
        if let Some(child) = child_lock.as_mut() {
            while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
        }
        // Kill descendants even if the direct child already exited. Waiting on
        // that child's handle reaps it without another polling deadline.
        self.job.terminate();
        if let Some(child) = child_lock.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        drop(child_lock);
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
    match tx.try_send(event) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(event)) => send_event_retry(tx, event, closed, deadline),
        Err(_) => Err(()),
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
    stdout: impl Read,
    max_line: usize,
    tx: SyncSender<ReadEvent>,
    closed: Arc<AtomicBool>,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        let event = match read_line(&mut reader, &mut line, max_line.saturating_add(2)) {
            Ok(0) => ReadEvent::Eof,
            Ok(bytes) if bytes > max_line.saturating_add(2) => {
                ReadEvent::Failed(TransportError::Protocol(FrameError::TooLong {
                    bytes,
                    limit: max_line,
                }))
            }
            Ok(_) => match decode_line(&line, max_line) {
                Ok(message) => ReadEvent::Message(message),
                Err(error) => ReadEvent::Failed(TransportError::Protocol(error)),
            },
            Err(error) => ReadEvent::Failed(TransportError::Read(error.to_string())),
        };
        let finished = !matches!(&event, ReadEvent::Message(_)) || !line.ends_with(b"\n");
        if send_event(&tx, event, &closed).is_err() || finished {
            return;
        }
    }
}

fn read_stderr(stderr: impl Read, buffer: Arc<Mutex<StderrBuffer>>, max_line: usize) {
    let mut reader = BufReader::new(stderr);
    let mut line = Vec::new();
    while matches!(
        read_line(&mut reader, &mut line, max_line.saturating_add(2)),
        Ok(1..)
    ) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_stdout_cannot_be_accepted_as_a_truncated_json_prefix_at_eof() {
        for ending in ["", "\n"] {
            let mut input = br#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_vec();
            input.resize(16 * 1024, b' ');
            input.extend_from_slice(ending.as_bytes());
            let (tx, rx) = sync_channel(2);
            read_stdout(&input[..], 8192, tx, Arc::new(AtomicBool::new(false)));
            assert!(
                matches!(rx.recv().unwrap(), ReadEvent::Failed(TransportError::Protocol(
                FrameError::TooLong { bytes, limit: 8192 }
            )) if bytes == input.len())
            );
            assert!(rx.try_recv().is_err());
        }
    }

    fn queued_transport() -> (Transport, Receiver<Vec<u8>>) {
        let (writer, receiver) = sync_channel(1);
        let (_, incoming) = sync_channel(1);
        let config = TransportConfig::default();
        (
            Transport {
                child: Mutex::new(None),
                job: JobGuard::create().unwrap(),
                writer: Mutex::new(Some(writer)),
                incoming: Mutex::new(incoming),
                stderr: Arc::new(Mutex::new(StderrBuffer::new(1, 16))),
                threads: Mutex::new(Vec::new()),
                closed: Arc::new(AtomicBool::new(false)),
                shutdown_started: AtomicBool::new(false),
                write_error: Arc::new(Mutex::new(None)),
                config,
            },
            receiver,
        )
    }

    #[test]
    fn full_writer_queue_reuses_the_original_buffer_when_capacity_returns() {
        let (transport, receiver) = queued_transport();
        transport.send_line(vec![1]).unwrap();
        let frame = vec![7; 4 * 1024 * 1024];
        let original = frame.as_ptr() as usize;
        thread::scope(|scope| {
            let writer = scope.spawn(|| transport.send_line(frame));
            thread::sleep(Duration::from_millis(30));
            assert!(!writer.is_finished());
            assert_eq!(receiver.recv().unwrap(), vec![1]);
            let actual = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(actual.as_ptr() as usize, original);
            assert!(actual.iter().all(|byte| *byte == 7));
            writer.join().unwrap().unwrap();
        });
    }

    #[test]
    fn full_writer_queue_keeps_backpressure_and_observes_shutdown() {
        let (transport, _receiver) = queued_transport();
        transport.send_line(vec![1]).unwrap();
        thread::scope(|scope| {
            let writer = scope.spawn(|| transport.send_line(vec![2; 1024]));
            thread::sleep(Duration::from_millis(30));
            assert!(!writer.is_finished());
            let started = Instant::now();
            transport.shutdown();
            assert!(matches!(
                writer.join().unwrap(),
                Err(TransportError::Closed)
            ));
            assert!(started.elapsed() < Duration::from_secs(1));
        });
        let (transport, _receiver) = queued_transport();
        transport.send_line(vec![1]).unwrap();
        let started = Instant::now();
        assert!(matches!(
            transport.send_line(vec![2]),
            Err(TransportError::Write(_))
        ));
        assert!(started.elapsed() >= Duration::from_secs(2));
        assert!(started.elapsed() < Duration::from_secs(4));
    }
}
