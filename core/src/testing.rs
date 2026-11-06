//! Test helpers: isolated directories under the target dir, never the live data.

use std::path::PathBuf;

use crate::messages::{key, Message};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

pub fn temp_dir(label: &str) -> PathBuf {
    let base = std::env::var_os("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/test-data"));
    let dir = base.join(format!("{label}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn sample(source: &str, external: &str, at: i64, text: &str) -> Message {
    Message {
        id: key(source, "c", external),
        source_id: source.into(),
        source_name: "来源".into(),
        external_id: external.into(),
        chat: "c".into(),
        sender: "张三".into(),
        time: at,
        text: text.into(),
        kind: "text".into(),
        private: true,
        ..Default::default()
    }
}
