use serde_json::{json, Value};
use std::{fs, path::Path};
shishi_plugin_sdk::export_plugin!(handle);

fn rows(config: &Value) -> Result<Vec<Value>, String> {
    let path = Path::new(config["file"].as_str().ok_or("请选择聊天 JSON 文件")?);
    if fs::metadata(path).map_err(|e| e.to_string())?.len() > 24 * 1024 * 1024 {
        return Err("文件不能超过 24 MiB".into());
    }
    let value: Value = serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let rows = value["messages"]
        .as_array()
        .ok_or("文件需要包含 messages 数组")?;
    for row in rows {
        if row["id"].as_str().unwrap_or("").is_empty() || row["time"].as_i64().unwrap_or(0) <= 0 {
            return Err("消息必须有非空 id 和毫秒时间戳 time".into());
        }
    }
    Ok(rows.clone())
}
pub fn handle(request: Value) -> Result<Value, String> {
    if request["protocol"] != 1 {
        return Err("协议版本不支持".into());
    }
    let config = &request["config"];
    let params = &request["params"];
    match request["method"].as_str().unwrap_or("") {
        "discover" => {
            let mut candidates = vec![];
            let current = config["file"].as_str().unwrap_or("");
            if !current.is_empty() && rows(config).is_ok() {
                candidates.push(json!({"label":current,"config":config}));
            }
            for key in ["scan_root"] {
                if let Some(dir) = config[key].as_str().filter(|s| !s.is_empty()) {
                    for entry in fs::read_dir(dir)
                        .map_err(|e| e.to_string())?
                        .take(300)
                        .flatten()
                    {
                        let path = entry.path();
                        if path.extension().and_then(|x| x.to_str()) == Some("json")
                            && path.to_string_lossy() != current
                        {
                            let candidate = json!({"file":path,"scan_root":dir});
                            if rows(&candidate).is_ok() {
                                candidates.push(json!({"label":path,"config":candidate}));
                            }
                        }
                    }
                }
            }
            Ok(
                json!({"candidates":candidates,"warnings":if candidates.is_empty(){vec!["选择一个搜索目录再自动查找，或直接选择 JSON 文件"]}else{vec![]}}),
            )
        }
        "validate" => {
            let rows = rows(config)?;
            Ok(
                json!({"valid":true,"account_key":Path::new(config["file"].as_str().unwrap()).canonicalize().map_err(|e|e.to_string())?,"message":format!("已验证 {} 条消息",rows.len())}),
            )
        }
        "fetch_incremental" | "fetch_range" | "lookup_messages" => {
            let method = request["method"].as_str().unwrap();
            let since = params["since"].as_i64().unwrap_or(0);
            let until = params["until"].as_i64().ok_or("缺少 until")?;
            let ids = params["ids"].as_array().cloned().unwrap_or_default();
            let messages = rows(config)?
                .into_iter()
                .filter(|m| {
                    if method == "lookup_messages" {
                        ids.contains(&m["id"])
                    } else {
                        let t = m["time"].as_i64().unwrap();
                        t >= since && t <= until
                    }
                })
                .collect::<Vec<_>>();
            Ok(json!({"messages":messages,"cursor":{"until":until},"until":until}))
        }
        _ => Err("不支持的接口".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_and_malformed_files_fail() {
        assert!(rows(&json!({})).is_err());
    }
}
