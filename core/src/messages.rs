//! Messages as scans produce them and the image previews. Storage lives in
//! `data::messages`.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::params::Params;
use crate::{time, Ctx, Error, Result};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ImageRef {
    pub id: String,
    pub path: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Message {
    pub inserted_at: i64,
    pub updated_at: i64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageRef>,
    pub source_id: String,
    pub source_name: String,
    pub external_id: String,
    pub id: String,
    pub chat: String,
    pub from: String,
    pub sender: String,
    pub recipient: String,
    pub to: String,
    pub group: String,
    pub time: i64,
    pub text: String,
    pub kind: String,
    pub mention_me: bool,
    pub mention_all: bool,
    pub private: bool,
    pub mine: bool,
    pub withdrawn: bool,
    pub reply: String,
}

impl Message {
    pub fn relevant(&self) -> bool {
        !self.withdrawn && (self.mine || self.mention_me || self.mention_all || self.private)
    }
}

/// The stored identity: source + chat + external id, length-prefixed so ids
/// containing the separator stay unambiguous.
pub fn key(source: &str, chat: &str, external: &str) -> String {
    format!("{}:{source}:{}:{chat}:{external}", source.len(), chat.len())
}

pub fn fingerprint(text: &str, withdrawn: bool) -> String {
    use sha2::Digest;
    format!("sha256:{}:{:x}", u8::from(withdrawn), sha2::Sha256::digest(text.as_bytes()))
}

pub const REVISION: &str = "messages_revision";

pub fn cutoff(days: u32) -> i64 {
    time::now() - i64::from(days.max(1)) * time::DAY
}

const PREVIEW_LIMIT: u64 = 10 * 1024 * 1024;
const PREVIEW_MISSING: &str = "图片未下载、已失效或超过10 MiB，请先在消息客户端打开图片";

pub(crate) fn preview_key(path: &Path) -> Option<String> {
    use sha2::Digest;
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > PREVIEW_LIMIT {
        return None;
    }
    let modified = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
    let digest = sha2::Sha256::digest(format!("{}\0{}\0{modified}", path.display(), meta.len()));
    Some(format!("{digest:x}")[..16].to_string())
}

/// The cached JPEG preview of `path` as base64 and its cache key (path,
/// size and modification time); `missing` when the file is unusable.
/// Blocking: call it off the executor.
pub(crate) fn preview_data(
    root: &Path,
    host: &std::sync::Arc<dyn crate::Host>,
    path: &Path,
    missing: &str,
) -> Result<(String, String)> {
    let key = preview_key(path).ok_or_else(|| Error::new(missing))?;
    let cache = root.join("media").join(format!("{key}.b64"));
    if let Ok(data) = std::fs::read_to_string(&cache) {
        return Ok((key, data));
    }
    let data = host.image_preview(path)?;
    if data.len() <= 1_000_000 {
        std::fs::create_dir_all(cache.parent().unwrap())?;
        std::fs::write(&cache, &data)?;
    }
    Ok((key, data))
}

/// `image_preview`: a JPEG data URL, decoded by the host and cached under
/// `media/` by path, size and modification time.
pub async fn image_preview(ctx: &Ctx, p: &Value) -> Result<Value> {
    let image = p.str_of("image_id").to_string();
    let path = ctx.db.image_path(image).await?;
    let path = std::path::PathBuf::from(path.ok_or("图片不存在或消息已撤回")?);
    let (root, host) = (ctx.root.clone(), ctx.host.clone());
    let (_, data) = crate::exec::blocking(move || preview_data(&root, &host, &path, PREVIEW_MISSING)).await?;
    Ok(json!({ "url": format!("data:image/jpeg;base64,{data}") }))
}

#[cfg(test)]
pub use crate::testing::sample;

#[cfg(test)]
mod tests;
