//! A scripted plugin host and a data directory with one installed plugin.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::plugin::{self, REQUIRED};
use crate::{Ctx, Host, HostEvent, Result};

#[derive(Default)]
pub struct FakeHost {
    replies: Mutex<BTreeMap<String, VecDeque<Result<Value>>>>,
    pub calls: Mutex<Vec<Value>>,
}

impl FakeHost {
    /// Queue the next answer for `method`, or `method@path` for one source's
    /// config path; unqueued calls get a default.
    pub fn reply(&self, method: &str, reply: Result<Value>) {
        self.replies.lock().unwrap().entry(method.into()).or_default().push_back(reply);
    }
}

impl Host for FakeHost {
    fn event(&self, _: HostEvent) {}

    fn plugin_call(&self, _: &Path, _: bool, input: Value, _: Option<Arc<AtomicBool>>) -> Result<Value> {
        self.calls.lock().unwrap().push(input.clone());
        let method = input["method"].as_str().unwrap_or_default().to_string();
        let until = input["params"]["until"].clone();
        let scoped = format!("{method}@{}", input["config"]["path"].as_str().unwrap_or_default());
        let queued = {
            let mut replies = self.replies.lock().unwrap();
            match replies.get_mut(&scoped).and_then(VecDeque::pop_front) {
                Some(reply) => Some(reply),
                None => replies.get_mut(&method).and_then(VecDeque::pop_front),
            }
        };
        let mut value = match queued {
            Some(reply) => reply?,
            None => match method.as_str() {
                "validate" => json!({"valid": true, "account_key": "acct", "message": ""}),
                "fetch_incremental" => json!({"messages": [], "cursor": {}}),
                "fetch_range" => json!({"messages": []}),
                "lookup_messages" => json!({"messages": []}),
                "discover" => json!({"candidates": [], "warnings": []}),
                _ => json!({}),
            },
        };
        if method.starts_with("fetch") && value.get("until").is_none() {
            value["until"] = until;
        }
        Ok(value)
    }
}

pub struct Fixture {
    pub ctx: Arc<Ctx>,
    pub host: Arc<FakeHost>,
}

impl Fixture {
    /// A data directory with plugin `demo` installed, optionally with a
    /// settings page declaring the `probe` method.
    pub fn new(label: &str, settings: bool) -> Self {
        let root = crate::testing::temp_dir(label);
        let src = root.join("plugin-src");
        let manifest = plugin::fixture(&src, "demo", "1");
        if settings {
            std::fs::write(src.join("page.html"), "<html>demo</html>").unwrap();
            let mut value: Value = serde_json::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
            value["files"].as_array_mut().unwrap().push("page.html".into());
            value["settings"] = json!({"api_version": 1, "entrypoint": "page.html", "methods": ["probe"]});
            std::fs::write(&manifest, value.to_string()).unwrap();
        }
        plugin::install(&root, &manifest).unwrap();
        let host = Arc::new(FakeHost::default());
        let ctx = Ctx::open(root, host.clone()).unwrap();
        Self { ctx, host }
    }

    /// Save an enabled source reading `path` and return it.
    pub async fn source(&self, name: &str, path: &str) -> super::Source {
        let source = json!({"id": "", "plugin_id": "demo", "name": name, "enabled": true, "config": {"path": path}});
        serde_json::from_value(self.call("source_save", json!({"source": source})).await.unwrap()).unwrap()
    }

    pub async fn sources(&self) -> Vec<super::Source> {
        self.ctx.db.sources().await.unwrap()
    }

    pub async fn call(&self, method: &str, p: Value) -> Result<Value> {
        crate::dispatch(&self.ctx, method, p).await
    }
}

/// A minimal valid plugin package for tests.
pub fn fixture(dir: &Path, id: &str, version: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("p.dll"), format!("MZ{version}")).unwrap();
    let manifest = json!({
        "id": id, "name": "测试插件", "version": version, "api_version": 1,
        "entrypoint": "p.dll", "files": ["p.dll"],
        "capabilities": REQUIRED,
        "config_fields": [{"key": "path", "label": "文件", "type": "text", "required": true}],
    });
    fs::write(dir.join("plugin.json"), manifest.to_string()).unwrap();
    dir.join("plugin.json")
}

/// Build a ZIP in memory for tests: `(name, content)` entries, deflated.
pub fn build(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut directory = Vec::new();
    for (name, content) in entries {
        let packed = miniz_oxide::deflate::compress_to_vec(content, 6);
        let crc = crc32fast::hash(content);
        let offset = out.len() as u32;
        let head = |sig: u32, v: &mut Vec<u8>| v.extend_from_slice(&sig.to_le_bytes());
        head(0x0403_4b50, &mut out);
        for v in [20u16, 0, 8, 0, 0] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for v in [crc, packed.len() as u32, content.len() as u32] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&packed);
        head(0x0201_4b50, &mut directory);
        for v in [20u16, 20, 0, 8, 0, 0] {
            directory.extend_from_slice(&v.to_le_bytes());
        }
        for v in [crc, packed.len() as u32, content.len() as u32] {
            directory.extend_from_slice(&v.to_le_bytes());
        }
        for v in [name.len() as u16, 0, 0, 0, 0] {
            directory.extend_from_slice(&v.to_le_bytes());
        }
        directory.extend_from_slice(&0u32.to_le_bytes());
        directory.extend_from_slice(&offset.to_le_bytes());
        directory.extend_from_slice(name.as_bytes());
    }
    let start = out.len() as u32;
    let size = directory.len() as u32;
    out.extend_from_slice(&directory);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    for v in [0u16, 0, entries.len() as u16, entries.len() as u16] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&start.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}
