//! Borrowed JSON serialization and exact character accounting.
use serde::Serialize;
use serde_json::Value;
use std::io;

pub fn canonical_json(value: &Value) -> String {
    // serde_json's default Map is a BTreeMap. Nested objects are already sorted;
    // the ordering regression must remain green if dependency features change.
    value.to_string()
}

struct Count<const CHARS: bool>(usize);
impl<const CHARS: bool> io::Write for Count<CHARS> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let used = if CHARS {
            bytes.iter().filter(|byte| **byte & 0xc0 != 0x80).count()
        } else {
            bytes.len()
        };
        self.0 = self.0.checked_sub(used).ok_or(io::ErrorKind::WriteZero)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Count encoded characters without retaining JSON. UTF-8 continuation bytes
/// do not start characters, even when the serializer splits writer calls.
pub fn chars<T: Serialize + ?Sized>(value: &T) -> usize {
    let mut count = Count::<true>(usize::MAX);
    serde_json::to_writer(&mut count, value).expect("JSON value serialization");
    usize::MAX - count.0
}

/// Stop at the byte limit; never allocate an encoded copy just to reject it.
pub fn fits_bytes<T: Serialize + ?Sized>(value: &T, limit: usize) -> bool {
    serde_json::to_writer(&mut Count::<false>(limit), value).is_ok()
}
