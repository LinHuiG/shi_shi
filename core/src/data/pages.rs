//! Keyset paging shared by the inbox, evidence and history lists. The
//! cursor names the last row's sort keys, so rows inserted or deleted ahead
//! of the reader never shift a page; `seen` only keeps the page number.

use rusqlite::types::Value as Sql;
use rusqlite::{params_from_iter, Connection, Row};
use serde_json::{json, Value};

use crate::{Error, Result};

const BAD_CURSOR: &str = "翻页位置无效，请刷新重试";

fn key(value: &Value) -> Result<Sql> {
    if let Some(n) = value.as_i64() {
        Ok(Sql::Integer(n))
    } else if let Some(s) = value.as_str().filter(|s| s.len() <= 1024) {
        Ok(Sql::Text(s.into()))
    } else {
        Err(Error::new(BAD_CURSOR))
    }
}

pub struct Page<'a> {
    /// A query whose sort columns are named `k0`, `k1`, …
    pub source: &'a str,
    pub params: Vec<Sql>,
    pub keys: usize,
    pub descending: bool,
    pub cursor: Option<&'a Value>,
    pub page: usize,
    pub size: usize,
}

impl Page<'_> {
    /// `{rows,total,page,pages,page_size,next_cursor}`.
    pub fn run(self, conn: &Connection, mut row: impl FnMut(&Row) -> Result<Value>) -> Result<Value> {
        let total =
            conn.query_row(&format!("SELECT count(*) FROM ({})", self.source), params_from_iter(&self.params), |r| {
                r.get::<_, i64>(0)
            })? as usize;
        let pages = total.div_ceil(self.size).max(1);
        let page = self.page.clamp(1, pages);
        let names = (0..self.keys).map(|i| format!("k{i}")).collect::<Vec<_>>().join(",");
        let mut args = self.params;
        let mut position = String::new();
        let seen = match self.cursor.filter(|v| !v.is_null()) {
            Some(cursor) => {
                let values =
                    cursor["keys"].as_array().filter(|v| v.len() == self.keys).ok_or_else(|| Error::new(BAD_CURSOR))?;
                let slots = (0..self.keys).map(|i| format!("?{}", args.len() + i + 1)).collect::<Vec<_>>().join(",");
                position = format!(" WHERE ({names}){}({slots})", if self.descending { "<" } else { ">" });
                for v in values {
                    args.push(key(v)?);
                }
                cursor["seen"].as_u64().unwrap_or(0).min(1_000_000_000) as usize
            }
            None => (page - 1) * self.size,
        };
        let offset = if position.is_empty() { seen } else { 0 };
        let direction = if self.descending { "DESC" } else { "ASC" };
        let order = (0..self.keys).map(|i| format!("k{i} {direction}")).collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT * FROM ({}){position} ORDER BY {order} LIMIT {} OFFSET {offset}",
            self.source,
            self.size + 1
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(params_from_iter(&args))?;
        let (mut out, mut boundary, mut more) = (Vec::new(), Value::Null, false);
        while let Some(r) = rows.next()? {
            if out.len() == self.size {
                more = true;
                break;
            }
            out.push(row(r)?);
            let mut values = Vec::with_capacity(self.keys);
            for i in 0..self.keys {
                values.push(match r.get::<_, Sql>(format!("k{i}").as_str())? {
                    Sql::Integer(n) => json!(n),
                    Sql::Text(s) => json!(s),
                    _ => crate::bail!("历史排序字段无效"),
                });
            }
            boundary = json!({"keys": values, "seen": seen + out.len()});
        }
        Ok(json!({
            "rows": out, "total": total, "page": seen / self.size + 1, "pages": pages,
            "page_size": self.size, "next_cursor": if more { boundary } else { Value::Null },
        }))
    }
}
