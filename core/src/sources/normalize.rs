//! Turning plugin results into stored messages, and the checks every plugin
//! answer must pass before anything is committed.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde_json::{json, Value};

use super::Source;
use crate::messages::{key, Message};
use crate::{bail, time, Error, Result};

fn scoped(source: &str, external: &str) -> String {
    format!("{source}:{external}")
}

/// Namespace a batch under its source. One invalid row rejects the batch.
pub fn normalize(source: &Source, value: &Value) -> Result<Vec<Message>> {
    let mut messages =
        Vec::<Message>::deserialize(&value["messages"]).map_err(|e| Error(format!("消息格式无效：{e}")))?;
    let latest = time::now() + 300_000;
    for m in &mut messages {
        if m.id.trim().is_empty() || m.time <= 0 || m.time > latest {
            bail!("插件返回的消息标识或时间无效");
        }
        let raw_chat = std::mem::take(&mut m.chat);
        m.external_id = m.id.clone();
        m.source_id = source.id.clone();
        m.source_name = source.name.clone();
        m.inserted_at = 0;
        m.updated_at = 0;
        m.id = key(&source.id, &raw_chat, &m.external_id);
        m.images.truncate(8);
        for (i, image) in m.images.iter_mut().enumerate() {
            image.id = format!("{}:image:{i}", m.id);
            if !std::path::Path::new(&image.path).is_absolute() {
                image.path.clear();
            }
        }
        m.chat = scoped(&source.id, &raw_chat);
        if !m.reply.is_empty() {
            m.reply = key(&source.id, &raw_chat, &m.reply);
        }
        if !m.from.is_empty() {
            m.from = scoped(&source.id, &m.from);
        }
        if !m.to.is_empty() {
            m.to = scoped(&source.id, &m.to);
        }
        if m.withdrawn {
            m.text.clear();
            m.images.clear();
        }
    }
    Ok(messages)
}

/// The account a `validate` (or `fetch_scan`) answer belongs to.
pub fn identity(source: &Source, result: &Value) -> Result<String> {
    if result["valid"] != true {
        bail!("{}", result["message"].as_str().unwrap_or("插件配置校验失败"));
    }
    let key =
        result["account_key"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| Error::new("插件未返回来源身份标识"))?;
    if !source.account_key.is_empty() && source.account_key != key {
        bail!("文件属于另一个账号，请新建消息来源");
    }
    Ok(key.into())
}

/// The incremental checkpoint must lie inside this round's window.
pub fn checkpoint(source: &Source, value: &Value, since: i64, until: i64) -> Result<i64> {
    value["until"]
        .as_i64()
        .filter(|t| *t >= source.last_scan && *t <= until && *t >= since)
        .ok_or_else(|| Error::new("插件返回的增量检查点无效"))
}

/// What one source read: rows, the cursor to commit and the checkpoint.
pub type Fetched = (Vec<Message>, Value, i64);

fn page_token(result: &Value) -> Result<Option<String>> {
    match result.get("next_page") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(t)) if !t.is_empty() => Ok(Some(t.clone())),
        _ => bail!("插件续页令牌无效"),
    }
}

/// Read a frozen `fetch_scan`: every page must keep the first page's account,
/// cursor and checkpoint, make progress and never repeat a row or token.
pub fn scan_pages(
    source: &Source,
    since: i64,
    until: i64,
    tracked: Vec<String>,
    mut call: impl FnMut(Value) -> Result<Value>,
) -> Result<Fetched> {
    let mut params = json!({"cursor": source.cursor, "since": since, "until": until, "tracked": tracked});
    let mut boundary: Option<(String, Value, i64)> = None;
    let mut tokens = BTreeSet::new();
    let mut received = BTreeSet::new();
    let mut messages = Vec::new();
    loop {
        let result = call(params)?;
        let account = identity(source, &result)?;
        let checkpoint = checkpoint(source, &result, since, until)?;
        let cursor = result.get("cursor").ok_or_else(|| Error::new("插件未返回增量游标"))?;
        let current = (account, cursor.clone(), checkpoint);
        if boundary.as_ref().is_some_and(|frozen| frozen != &current) {
            bail!("插件续页的账号、游标或检查点发生变化");
        }
        boundary.get_or_insert(current);
        let rows = normalize(source, &result)?;
        if rows.iter().any(|m| !received.insert(m.id.clone())) {
            bail!("插件冻结分页返回了重复消息");
        }
        let more = page_token(&result)?;
        if more.is_some() && rows.is_empty() {
            bail!("插件续页没有读取进展");
        }
        messages.extend(rows);
        let Some(token) = more else { break };
        if !tokens.insert(token.clone()) {
            bail!("插件续页令牌重复");
        }
        params = json!({"page": token});
    }
    let (_, cursor, checkpoint) = boundary.expect("at least one page");
    Ok((messages, cursor, checkpoint))
}

/// External ids of tracked messages the increment did not already return.
pub fn missing_tracked(tracked: Vec<(String, String)>, messages: &[Message]) -> Vec<String> {
    let received: BTreeSet<&str> = messages.iter().map(|m| m.id.as_str()).collect();
    let mut requested = BTreeSet::new();
    tracked
        .into_iter()
        .filter_map(|(id, external)| {
            (!received.contains(id.as_str()) && requested.insert(external.clone())).then_some(external)
        })
        .collect()
}

#[cfg(test)]
mod tests;
