//! Reading IPC parameters. Missing optional fields read as empty; required
//! fields say which one.

use serde_json::Value;

use crate::{Error, Result};

pub trait Params {
    fn str_of(&self, key: &str) -> &str;
    fn need_str(&self, key: &str) -> Result<&str>;
    fn strings(&self, key: &str) -> Vec<String>;
}

impl Params for Value {
    fn str_of(&self, key: &str) -> &str {
        self.get(key).and_then(Value::as_str).unwrap_or("")
    }

    fn need_str(&self, key: &str) -> Result<&str> {
        Some(self.str_of(key).trim()).filter(|v| !v.is_empty()).ok_or_else(|| Error(format!("缺少参数 {key}")))
    }

    fn strings(&self, key: &str) -> Vec<String> {
        self[key].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect()
    }
}
