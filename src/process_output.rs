//! Bounded subprocess output, independent of child lifecycle and application state.
use std::{
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

// Generous enough for existing streamed analysis/tool responses, but finite.
pub(super) const STDOUT_LIMIT: u64 = 64 * 1024 * 1024;
pub(super) const STDERR_LIMIT: u64 = 8 * 1024 * 1024;
pub(super) const LINE_LIMIT: usize = 8 * 1024 * 1024;
const POLL_BYTES: usize = 256 * 1024;

pub(super) struct Files(pub Vec<PathBuf>);
impl Drop for Files {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

pub(super) fn check_size(path: &Path, limit: u64) -> Result<(), String> {
    let size = fs::metadata(path).map_err(|e| e.to_string())?.len();
    if size > limit {
        return Err("子进程输出超过大小限制，已停止本次操作".into());
    }
    Ok(())
}

pub(super) fn read(path: &Path, limit: u64) -> Result<String, String> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > limit {
        return Err("子进程输出超过大小限制，未载入完整输出".into());
    }
    // Preserve the previous contract for a non-UTF-8 console response.
    Ok(String::from_utf8(bytes).unwrap_or_default())
}

pub(super) struct Reader {
    reader: BufReader<fs::File>,
    pending: Vec<u8>,
    line_limit: usize,
}
impl Reader {
    pub fn new(path: &Path, line_limit: usize) -> Result<Self, String> {
        Ok(Self {
            reader: BufReader::new(fs::File::open(path).map_err(|e| e.to_string())?),
            pending: Vec::new(),
            line_limit,
        })
    }
    /// Yield after a bounded amount of work so cancellation/deadlines stay observable.
    pub fn poll(&mut self, progress: &mut dyn FnMut(&str)) -> Result<bool, String> {
        let mut consumed = 0;
        while consumed < POLL_BYTES {
            let buffer = self.reader.fill_buf().map_err(|e| e.to_string())?;
            if buffer.is_empty() {
                return Ok(false);
            }
            let count = buffer
                .iter()
                .position(|b| *b == b'\n')
                .map_or(buffer.len(), |n| n + 1)
                .min(POLL_BYTES - consumed);
            if self.pending.len().saturating_add(count) > self.line_limit {
                return Err("子进程单行输出超过大小限制，已停止本次操作".into());
            }
            self.pending.extend_from_slice(&buffer[..count]);
            self.reader.consume(count);
            consumed += count;
            if self.pending.ends_with(b"\n") {
                if let Ok(line) = std::str::from_utf8(&self.pending) {
                    progress(line.trim());
                }
                self.pending.clear();
            }
        }
        Ok(true)
    }
    pub fn finish(
        &mut self,
        progress: &mut dyn FnMut(&str),
        mut check: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        check()?;
        while self.poll(progress)? {
            check()?;
        }
        if !self.pending.is_empty() {
            if let Ok(line) = std::str::from_utf8(&self.pending) {
                progress(line.trim());
            }
            self.pending.clear();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_utf8_lines_are_assembled_once_and_unterminated_tail_is_delivered() {
        let path = std::env::temp_dir().join(format!("shishi-output-{}", crate::model::id()));
        let _cleanup = Files(vec![path.clone()]);
        fs::write(&path, "中🙂").unwrap();
        let mut reader = Reader::new(&path, 100).unwrap();
        let mut lines = vec![];
        reader.poll(&mut |s| lines.push(s.to_owned())).unwrap();
        assert!(lines.is_empty());
        use std::io::Write;
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all("文\n尾".as_bytes())
            .unwrap();
        reader
            .finish(&mut |s| lines.push(s.to_owned()), || Ok(()))
            .unwrap();
        assert_eq!(lines, ["中🙂文", "尾"]);
    }
    #[test]
    fn output_work_and_allocations_are_bounded() {
        let path = std::env::temp_dir().join(format!("shishi-output-{}", crate::model::id()));
        let _cleanup = Files(vec![path.clone()]);
        fs::write(&path, b"abcdefghijkl").unwrap();
        assert!(Reader::new(&path, 8).unwrap().poll(&mut |_| {}).is_err());
        assert!(check_size(&path, 8).is_err());
        assert!(read(&path, 8).is_err());
        assert_eq!(read(&path, 12).unwrap(), "abcdefghijkl");
        fs::write(&path, "x\n".repeat(POLL_BYTES)).unwrap();
        assert!(Reader::new(&path, 8).unwrap().poll(&mut |_| {}).unwrap());
    }
}
