//! On-demand raster reads. Paths are supplied only by the selected source, never by AI arguments.
use crate::{analysis_context::Meter, engine::App, model::*, process};
use serde_json::{json, Value};
use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    process::Command,
};
pub const LIMIT: usize = 2;

pub fn tool() -> Value {
    json!({"name":"read_image","description":"按image_id查看本批相关消息的本地图片，最多2次；只在文字不足时使用。未下载则不可用。","inputSchema":{"type":"object","additionalProperties":false,"properties":{"image_id":{"type":"string","maxLength":512}},"required":["image_id"]},"annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}})
}
fn key(image: &ImageRef) -> Option<String> {
    let meta = fs::metadata(&image.path).ok()?;
    if !meta.is_file() || meta.len() > 10 * 1024 * 1024 {
        return None;
    }
    let mut h = DefaultHasher::new();
    image.path.hash(&mut h);
    meta.len().hash(&mut h);
    meta.modified().ok()?.hash(&mut h);
    Some(format!("{:016x}", h.finish()))
}
pub fn brief(images: &[ImageRef]) -> Value {
    json!(images
        .iter()
        .take(8)
        .map(|i| json!({"id":i.id,"available":!i.path.is_empty()}))
        .collect::<Vec<_>>())
}
pub fn attach(root: &Path, payload: &mut Value) {
    let mut images = vec![];
    if let Some(records) = payload["_local_context"]["records"].as_array_mut() {
        for record in records {
            if record["kind"] != "message" {
                continue;
            }
            let refs: Vec<ImageRef> =
                serde_json::from_value(record["data"]["images"].clone()).unwrap_or_default();
            if refs.is_empty() {
                continue;
            }
            record["data"]["images"] = brief(&refs);
            for image in refs {
                let cache = key(&image);
                images.push(
                    json!({"id":image.id,"message_id":record["id"],"path":image.path,"key":cache}),
                );
            }
        }
    }
    if images.is_empty() {
        return;
    }
    // The private catalog is the only authority for read_image. No arbitrary file names are accepted.
    payload["_local_context"]["images"] = json!(images);
    payload["_local_context"]["image_cache"] = json!(root.join("data/media"));
    for field in ["changes", "context"] {
        if let Some(rows) = payload[field].as_array_mut() {
            for row in rows {
                if let Some(refs) = row["images"].as_array_mut() {
                    for image in refs {
                        if let Some(found) = images.iter().find(|v| v["id"] == image["id"]) {
                            image["available"] = json!(found["key"].is_string());
                            if let Some(k) = found["key"].as_str() {
                                if let Ok(text) = fs::read_to_string(
                                    root.join("data/media").join(format!("{k}.note")),
                                ) {
                                    image["summary"] =
                                        json!(text.chars().take(300).collect::<String>());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
fn preview(cache: &Path, image: &ImageRef) -> Result<(String, String), String> {
    let key = key(image).ok_or("图片未下载、已失效或超过10 MiB，请先在消息客户端打开图片")?;
    use std::io::Read;
    let mut signature = [0u8; 8];
    let mut source = fs::File::open(&image.path).map_err(|_| "图片不可读")?;
    source
        .read_exact(&mut signature)
        .map_err(|_| "图片格式无效")?;
    if !(signature.starts_with(b"\x89PNG\r\n\x1a\n")
        || signature.starts_with(b"\xff\xd8\xff")
        || signature.starts_with(b"GIF87a")
        || signature.starts_with(b"GIF89a")
        || signature.starts_with(b"BM"))
    {
        return Err("只支持 PNG/JPEG/GIF/BMP 图片，不读取其他文件".into());
    }
    fs::create_dir_all(cache).map_err(|e| e.to_string())?;
    let cached = cache.join(format!("{key}.b64"));
    if let Ok(data) = fs::read_to_string(&cached) {
        if data.len() <= 1_000_000 {
            return Ok((key, data));
        }
    }
    let job = cache.join(id());
    fs::create_dir_all(&job).map_err(|e| e.to_string())?;
    let script = job.join("preview.ps1");
    fs::write(
        &script,
        format!("\u{feff}{}", include_str!("../assets/image-preview.ps1")),
    )
    .map_err(|e| e.to_string())?;
    let mut command = Command::new(
        PathBuf::from(std::env::var_os("SystemRoot").unwrap_or("C:\\Windows".into()))
            .join("System32/WindowsPowerShell/v1.0/powershell.exe"),
    );
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&script);
    let result = process::run(
        command,
        Some(&json!({"path":image.path}).to_string()),
        20,
        &job,
    );
    let _ = fs::remove_file(&script);
    let _ = fs::remove_dir(&job);
    let (data, _) = result
        .map_err(|_| "图片不可解码（仅支持本地 PNG/JPEG/GIF/BMP），或超出尺寸限制".to_string())?;
    let data = data.trim().to_string();
    if data.is_empty()
        || data.len() > 1_000_000
        || !data
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"+/=".contains(&c))
    {
        return Err("图片转换失败".into());
    }
    fs::write(&cached, &data).map_err(|e| e.to_string())?;
    Ok((key, data))
}
pub fn redact(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if map.get("type").is_some_and(|v| v == "image") {
                if let Some(data) = map.remove("data") {
                    map.insert(
                        "image_data_omitted".into(),
                        json!(data.as_str().map(str::len).unwrap_or(0)),
                    );
                }
            }
            for v in map.values_mut() {
                redact(v);
            }
        }
        Value::Array(rows) => {
            for v in rows {
                redact(v)
            }
        }
        _ => {}
    }
}
pub fn read(snapshot: &Value, meter: &mut Meter, args: &Value) -> Result<Value, String> {
    if snapshot["expires"].as_i64().unwrap_or(0) < now() {
        return Err("图片授权已过期".into());
    }
    if meter.image_calls >= LIMIT {
        return Err("本批已达到2次图片读取上限".into());
    }
    meter.image_calls += 1;
    let object = args.as_object().ok_or("缺少图片编号")?;
    if object.len() != 1 {
        return Err("只接受image_id，不接受文件路径".into());
    }
    let id = object
        .get("image_id")
        .and_then(Value::as_str)
        .ok_or("缺少图片编号")?;
    let image = snapshot["images"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|i| i["id"] == id)
        .ok_or("图片不在本批允许范围内")?;
    let reference = ImageRef {
        id: id.into(),
        path: image["path"].as_str().unwrap_or("").into(),
    };
    if key(&reference).as_deref() != image["key"].as_str() || !image["key"].is_string() {
        return Err("图片不可用或文件已变化，请重新扫描".into());
    }
    let cache = Path::new(snapshot["image_cache"].as_str().ok_or("未配置图片缓存")?);
    let (cache_key, data) = preview(cache, &reference)?;
    let mut hash = DefaultHasher::new();
    data.hash(&mut hash);
    let hash = format!("{:016x}", hash.finish());
    if !meter.image_hashes.insert(hash) {
        return Ok(
            json!({"content":[{"type":"text","text":"这张图片在本批已经查看，不重复传入。"}]}),
        );
    }
    meter.images.insert(id.into(), cache_key);
    meter
        .seen
        .insert(image["message_id"].as_str().unwrap_or("").into());
    meter.image_bytes += data.len() * 3 / 4;
    Ok(
        json!({"content":[{"type":"text","text":format!("图片 {id}，仅本地图片资料，长边≤1280像素。")},{"type":"image","mimeType":"image/jpeg","data":data}]}),
    )
}
pub fn notes(meter: &Meter, value: &Value) -> Value {
    json!(value["image_notes"]
        .as_array()
        .into_iter()
        .flatten()
        .take(LIMIT)
        .filter_map(|v| {
            let id = v["image_id"].as_str()?;
            let key = meter.images.get(id)?;
            let summary = v["summary"].as_str()?.trim();
            if summary.is_empty() || summary.chars().count() > 300 {
                return None;
            }
            Some(json!({"key":key,"summary":summary}))
        })
        .collect::<Vec<_>>())
}
pub fn commit(root: &Path, notes: &Value) -> Result<(), String> {
    let dir = root.join("data/media");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    for note in notes.as_array().into_iter().flatten() {
        let key = note["key"].as_str().unwrap_or("");
        if key.len() != 16 || !key.bytes().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        fs::write(
            dir.join(format!("{key}.note")),
            note["summary"].as_str().unwrap_or(""),
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}
pub fn prune(root: &Path, days: u32) {
    let dir = root.join("data/media");
    if !dir.is_dir() {
        return;
    }
    let marker = dir.join("pruned.txt");
    let today = format!("{}:{days}", date());
    if fs::read_to_string(&marker).is_ok_and(|v| v == today) {
        return;
    }
    let cutoff =
        std::time::SystemTime::now() - std::time::Duration::from_secs(u64::from(days) * 86400);
    for item in fs::read_dir(&dir).into_iter().flatten().flatten() {
        let path = item.path();
        if !matches!(
            path.extension().and_then(|v| v.to_str()),
            Some("b64" | "note")
        ) {
            continue;
        }
        if item
            .metadata()
            .is_ok_and(|m| m.is_file() && m.modified().is_ok_and(|t| t < cutoff))
        {
            let _ = fs::remove_file(path);
        }
    }
    let _ = fs::write(marker, today);
}
impl App {
    pub fn image_preview(&self, args: &Value) -> Result<Value, String> {
        let s = self.state.lock().unwrap();
        let image = s
            .messages
            .values()
            .filter(|m| !m.withdrawn)
            .flat_map(|m| m.images.iter())
            .find(|i| Some(i.id.as_str()) == args["image_id"].as_str())
            .cloned()
            .ok_or("图片不存在或消息已撤回")?;
        drop(s);
        let (_, data) = preview(&self.root.join("data/media"), &image)?;
        Ok(json!({"url":format!("data:image/jpeg;base64,{data}")}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn image_scope_budget_and_notes_never_accept_arbitrary_paths() {
        let mut meter = Meter::default();
        let snapshot = json!({"expires":now()+60000,"images":[],"image_cache":"unused"});
        assert!(read(&snapshot, &mut meter, &json!({"image_id":"other"})).is_err());
        assert!(read(
            &snapshot,
            &mut meter,
            &json!({"image_id":"other","path":"secret.png"})
        )
        .is_err());
        assert_eq!(meter.image_calls, 2);
        assert!(read(&snapshot, &mut meter, &json!({"image_id":"other"}))
            .unwrap_err()
            .contains("上限"));
        assert_eq!(
            notes(
                &meter,
                &json!({"image_notes":[{"image_id":"other","summary":"forged"}]})
            ),
            json!([])
        );
        let reloaded: Meter = serde_json::from_value(json!(meter)).unwrap();
        assert_eq!(reloaded.image_calls, 2);
    }
    #[test]
    fn image_paths_stay_in_private_catalog_and_empty_images_preserve_fingerprints() {
        let message = Message {
            id: "m".into(),
            images: vec![ImageRef {
                id: "image1".into(),
                path: "C:/missing/private.png".into(),
            }],
            ..Default::default()
        };
        let mut p = json!({"changes":[crate::context_budget::message(&message,500)],"_local_context":{"records":[{"kind":"message","id":"m","data":message}],"expires":now()+60000}});
        attach(Path::new("unused"), &mut p);
        assert!(!crate::ai::public_payload(&p)
            .to_string()
            .contains("private.png"));
        assert!(!p["_local_context"]["records"]
            .to_string()
            .contains("private.png"));
        assert!(p["_local_context"]["images"]
            .to_string()
            .contains("private.png"));
        assert!(json!(Message::default()).get("images").is_none());
    }
    #[test]
    #[ignore = "Explicit local image fixture; invokes Windows decoder, no model"]
    fn prepare_native_image_fixture() {
        let root =
            PathBuf::from(std::env::var_os("SHISHI_IMAGE_FIXTURE").expect("fixture directory"));
        let path = root.join("fixture.png");
        assert!(path.is_file());
        let image = ImageRef {
            id: "img-1".into(),
            path: path.to_string_lossy().into(),
        };
        let mut p = json!({"changes":[{"id":"m","images":brief(std::slice::from_ref(&image))}],"_local_context":{"expires":now()+300000,"records":[{"id":"m","kind":"message","data":{"images":[image]}}]}});
        attach(&root, &mut p);
        let mut meter = Meter::default();
        let output = read(
            &p["_local_context"],
            &mut meter,
            &json!({"image_id":"img-1"}),
        )
        .unwrap();
        assert_eq!(output["content"][1]["type"], "image");
        assert!(output["content"][1]["data"]
            .as_str()
            .unwrap()
            .starts_with("/9j/"));
        assert!(read(
            &p["_local_context"],
            &mut meter,
            &json!({"image_id":"img-1"})
        )
        .unwrap()["content"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["type"] != "image"));
        assert!(read(
            &p["_local_context"],
            &mut meter,
            &json!({"image_id":"img-1"})
        )
        .is_err());
        let saved = notes(
            &meter,
            &json!({"image_notes":[{"image_id":"img-1","summary":"合成测试图片"}]}),
        );
        commit(&root, &saved).unwrap();
        fs::write(
            root.join("context.json"),
            serde_json::to_vec(&p["_local_context"]).unwrap(),
        )
        .unwrap();
    }
}
