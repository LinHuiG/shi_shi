//! AI memory: a temporary note maintained by analysis and an archived note
//! written by archiving. Each keeps only its current text; `revision` exists
//! for optimistic saves, not as history.

use serde_json::Value;

use crate::{Ctx, Error, Result};

pub const TEMPORARY_LIMIT: usize = 2000;
pub const ARCHIVED_LIMIT: usize = 4000;

pub struct Memory {
    pub text: String,
    pub revision: i64,
    pub updated: i64,
}

pub async fn save(ctx: &Ctx, p: &Value) -> Result<Value> {
    let kind = p["kind"].as_str().ok_or_else(|| Error::new("请选择记忆类型"))?;
    let expected = p["expected"].as_u64().ok_or_else(|| Error::new("缺少记忆校验值，请重新读取"))? as i64;
    let (kind, name, limit) = match kind {
        "temporary" => ("temporary", "临时", TEMPORARY_LIMIT),
        "archived" => ("archived", "归档", ARCHIVED_LIMIT),
        _ => crate::bail!("未知记忆类型"),
    };
    let text = p["text"].as_str().ok_or_else(|| Error::new("记忆正文无效"))?;
    if text.chars().count() > limit {
        crate::bail!("{name}记忆最多 {limit} 字符，未保存");
    }
    ctx.db.save_memory(kind, name, expected, text.into()).await
}
