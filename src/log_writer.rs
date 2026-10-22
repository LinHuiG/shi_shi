//! One consumer owns log append and rotation. Producers submit complete records.
use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::mpsc::{self, SyncSender},
    thread::JoinHandle,
};

enum Command {
    Record { directory: PathBuf, value: Value },
    Flush(SyncSender<Result<(), String>>),
    Stop(SyncSender<Result<(), String>>),
}

pub(crate) struct LogWriter {
    sender: SyncSender<Command>,
    worker: Option<JoinHandle<()>>,
}

impl LogWriter {
    pub fn start() -> Result<Self, String> {
        // Backpressure bounds memory without silently discarding accepted logs.
        let (sender, receiver) = mpsc::sync_channel(512);
        let worker = std::thread::Builder::new()
            .name("shishi-log-writer".into())
            .spawn(move || {
                let mut error = None;
                while let Ok(command) = receiver.recv() {
                    match command {
                        Command::Record { directory, value } => {
                            if let Err(e) = append(&directory, &value) {
                                error = Some(e);
                            }
                        }
                        Command::Flush(reply) => {
                            let _ = reply.send(error.take().map_or(Ok(()), Err));
                        }
                        Command::Stop(reply) => {
                            let _ = reply.send(error.take().map_or(Ok(()), Err));
                            break;
                        }
                    }
                }
            })
            .map_err(|e| format!("日志线程启动失败：{e}"))?;
        Ok(Self {
            sender,
            worker: Some(worker),
        })
    }

    pub fn submit(&self, directory: PathBuf, value: Value) {
        let _ = self.sender.send(Command::Record { directory, value });
    }

    pub fn flush(&self) -> Result<(), String> {
        self.barrier(false)
    }

    pub fn stop(&self) -> Result<(), String> {
        self.barrier(true)
    }

    fn barrier(&self, stop: bool) -> Result<(), String> {
        let (sender, receiver) = mpsc::sync_channel(1);
        self.sender
            .send(if stop {
                Command::Stop(sender)
            } else {
                Command::Flush(sender)
            })
            .map_err(|_| "日志线程已结束".to_string())?;
        receiver.recv().map_err(|_| "日志写入未完成".to_string())?
    }
}

impl Drop for LogWriter {
    fn drop(&mut self) {
        let _ = self.stop();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn append(directory: &Path, value: &Value) -> Result<(), String> {
    let at = value["time"].as_i64().ok_or("日志时间无效")?;
    let day = crate::model::date_from(at);
    let path = directory.join(format!("{day}.jsonl"));
    if fs::metadata(&path).is_ok_and(|m| m.len() > 8_000_000) {
        let mut suffix = at;
        let mut rotated = directory.join(format!("{day}-{suffix}.jsonl"));
        while rotated.exists() {
            suffix = suffix.saturating_add(1);
            rotated = directory.join(format!("{day}-{suffix}.jsonl"));
        }
        fs::rename(&path, rotated).map_err(|e| format!("日志轮转失败：{e}"))?;
    }
    let mut bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(&bytes))
        .map_err(|e| format!("日志写入失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rotation_does_not_overwrite_a_same_timestamp_archive() {
        let directory =
            std::env::temp_dir().join(format!("shishi-log-rotation-{}", crate::model::id()));
        fs::create_dir_all(&directory).unwrap();
        let at = crate::model::now();
        let day = crate::model::date_from(at);
        let current = directory.join(format!("{day}.jsonl"));
        fs::File::create(&current)
            .unwrap()
            .set_len(8_000_001)
            .unwrap();
        let prior = directory.join(format!("{day}-{at}.jsonl"));
        fs::write(&prior, b"previous archive").unwrap();
        let writer = LogWriter::start().unwrap();
        writer.submit(
            directory.clone(),
            json!({"time":at,"kind":"after_rotation","detail":{}}),
        );
        writer.stop().unwrap();
        assert_eq!(fs::read(&prior).unwrap(), b"previous archive");
        assert_eq!(
            fs::metadata(directory.join(format!("{day}-{}.jsonl", at + 1)))
                .unwrap()
                .len(),
            8_000_001
        );
        let record: Value = serde_json::from_slice(&fs::read(current).unwrap()).unwrap();
        assert_eq!(record["kind"], "after_rotation");
        drop(writer);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn concurrent_producers_preserve_complete_records_and_flush_all_prior_messages() {
        let directory = std::env::temp_dir().join(format!("shishi-log-{}", crate::model::id()));
        fs::create_dir_all(&directory).unwrap();
        let writer = LogWriter::start().unwrap();
        let at = crate::model::now();
        std::thread::scope(|scope| {
            for producer in 0..8 {
                let writer = &writer;
                let directory = &directory;
                scope.spawn(move || {
                    for index in 0..80 {
                        writer.submit(directory.clone(), json!({"time":at,"kind":"synthetic","detail":{"producer":producer,"index":index,"text":"中🙂".repeat(50)}}));
                    }
                });
            }
        });
        writer.flush().unwrap();
        let text =
            fs::read_to_string(directory.join(format!("{}.jsonl", crate::model::date_from(at))))
                .unwrap();
        let values = text
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 640);
        let unique = values
            .iter()
            .map(|v| {
                (
                    v["detail"]["producer"].as_u64().unwrap(),
                    v["detail"]["index"].as_u64().unwrap(),
                )
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), 640);
        writer.submit(
            directory.clone(),
            json!({"time":at,"kind":"last","detail":{}}),
        );
        writer.stop().unwrap();
        assert_eq!(
            fs::read_to_string(directory.join(format!("{}.jsonl", crate::model::date_from(at))))
                .unwrap()
                .lines()
                .count(),
            641
        );
        drop(writer);
        fs::remove_dir_all(directory).unwrap();
    }
}
