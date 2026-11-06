use serde_json::{json, Value};
use std::{fs, path::Path};
shishi_plugin_sdk::export_plugin!(handle);

fn rows(config: &Value) -> Result<Vec<Value>, String> {
    let path = Path::new(config["file"].as_str().ok_or("请选择聊天 JSON 文件")?);
    if fs::metadata(path).map_err(|e| e.to_string())?.len() > 24 * 1024 * 1024 {
        return Err("文件不能超过 24 MiB".into());
    }
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    if path.extension().and_then(|x| x.to_str()) == Some("jsonl") {
        return chatlab_lines(std::str::from_utf8(&bytes).map_err(|e| e.to_string())?);
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if value.get("chatlab").is_some() {
        let messages = value["messages"].as_array().ok_or("文件需要包含 messages 数组")?;
        return chatlab(&value["meta"], value["members"].as_array().map(Vec::as_slice).unwrap_or(&[]), messages);
    }
    let rows = value["messages"].as_array().ok_or("文件需要包含 messages 数组")?;
    for row in rows {
        if row["id"].as_str().unwrap_or("").is_empty() || row["time"].as_i64().unwrap_or(0) <= 0 {
            return Err("消息必须有非空 id 和毫秒时间戳 time".into());
        }
    }
    Ok(rows.clone())
}

/// ChatLab JSONL: a header line, then member and message lines. Bad lines
/// and `#` comments are skipped, as the format allows.
fn chatlab_lines(text: &str) -> Result<Vec<Value>, String> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#'));
    let header: Value = serde_json::from_str(lines.next().ok_or("文件为空")?).map_err(|e| e.to_string())?;
    if header["_type"] != "header" {
        return Err("ChatLab JSONL 第一行必须是 header".into());
    }
    let (mut members, mut messages) = (Vec::new(), Vec::new());
    for v in lines.filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
        match v["_type"].as_str() {
            Some("member") => members.push(v),
            Some("message") => messages.push(v),
            _ => {}
        }
    }
    chatlab(&header["meta"], &members, &messages)
}

/// Map the ChatLab exchange format (https://docs.chatlab.fun, v0.0.2) to
/// the unified message. Messages without a platform ID get one from sender,
/// time and order, which stays stable while the export is only appended.
fn chatlab(meta: &Value, members: &[Value], messages: &[Value]) -> Result<Vec<Value>, String> {
    let display_name = |m: &Value| {
        ["groupNickname", "accountName"]
            .into_iter()
            .find_map(|k| m[k].as_str().filter(|s| !s.is_empty()).map(str::to_owned))
    };
    let names: std::collections::HashMap<_, _> =
        members.iter().filter_map(|m| Some((m["platformId"].as_str()?, display_name(m)?))).collect();
    let group = meta["type"] == "group";
    let name = meta["name"].as_str().unwrap_or("");
    let chat = meta["groupId"].as_str().filter(|s| !s.is_empty()).unwrap_or(name);
    let mut seen = std::collections::HashMap::new();
    let mut rows = Vec::new();
    for m in messages {
        let (from, at) = (m["sender"].as_str().unwrap_or(""), m["timestamp"].as_i64().unwrap_or(0));
        if from.is_empty() || at <= 0 {
            continue;
        }
        let id = match m["platformMessageId"].as_str().filter(|s| !s.is_empty()) {
            Some(id) => id.to_string(),
            None => {
                let n = seen.entry((from, at)).or_insert(0);
                *n += 1;
                format!("{from}-{at}-{n}")
            }
        };
        let code = m["type"].as_i64().unwrap_or(99);
        let (kind, placeholder) = kind(code);
        let text = m["content"].as_str().unwrap_or(placeholder);
        rows.push(json!({
            "id": id, "chat": chat, "group": if group { name } else { "" },
            "from": from,
            "sender": display_name(m).or_else(|| names.get(from).cloned()).unwrap_or_else(|| from.into()),
            "time": at * 1000, "text": text,
            "kind": kind,
            "private": !group, "mine": meta["ownerId"].as_str() == Some(from),
            "withdrawn": code == 81,
            "reply": m["replyToMessageId"].as_str().unwrap_or(""),
        }));
    }
    Ok(rows)
}

/// `(kind, placeholder text)` for a ChatLab message type.
fn kind(code: i64) -> (&'static str, &'static str) {
    match code {
        0 => ("text", ""),
        1 => ("image", "[图片]"),
        2 => ("voice", "[语音]"),
        3 => ("video", "[视频]"),
        4 => ("file", "[文件]"),
        5 => ("emoji", "[表情]"),
        7 => ("link", "[链接]"),
        8 => ("location", "[位置]"),
        20 => ("red_packet", "[红包]"),
        21 => ("transfer", "[转账]"),
        23 => ("call", "[通话]"),
        24 => ("share", "[分享]"),
        25 => ("reply", ""),
        26 => ("forward", "[转发]"),
        80 => ("system", "[系统消息]"),
        81 => ("recall", ""),
        _ => ("other", "[消息]"),
    }
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
            if let Some(dir) = config["scan_root"].as_str().filter(|s| !s.is_empty()) {
                for entry in fs::read_dir(dir).map_err(|e| e.to_string())?.take(300).flatten() {
                    let path = entry.path();
                    if matches!(path.extension().and_then(|x| x.to_str()), Some("json" | "jsonl"))
                        && path.to_string_lossy() != current
                    {
                        let candidate = json!({"file":path,"scan_root":dir});
                        if rows(&candidate).is_ok() {
                            candidates.push(json!({"label":path,"config":candidate}));
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
mod tests;
