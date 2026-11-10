//! Codex driver. Analysis signs in the way yetone/magpie (MIT) does: the
//! login Codex keeps in `$CODEX_HOME/auth.json` calls the ChatGPT Codex
//! backend's Responses API through the shared SS tool loop, and an API-key
//! login calls api.openai.com. Handling starts in the desktop composer.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};
use shishi_api::http_client::{send, HttpRequest};
use shishi_api::{Client, ModelInfo, Wire};

use super::provider::{self, Context, Driver, Event, Job, Reply};

const CHATGPT: &str = "https://chatgpt.com/backend-api/codex";
const OPENAI: &str = "https://api.openai.com/v1";
const TOKEN: &str = "https://auth.openai.com/oauth/token";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const VERSION: &str = "0.159.0";
const NOT_SIGNED_IN: &str = "未登录 Codex，请先运行 codex login";

pub struct Codex;

impl Driver for Codex {
    fn analyze(&self, job: &Job, context: Option<&mut dyn Context>, event: &mut dyn FnMut(Event)) -> Reply {
        analyze(&Backend::default(), job, context, event)
    }
}

/// The sign-in state; never shows the account's email.
pub fn status() -> Value {
    let auth = fs::read_to_string(Backend::default().home.join("auth.json")).ok();
    json!({"status": status_text(auth.and_then(|s| serde_json::from_str(&s).ok()))})
}

/// Open a Codex thread in the desktop app.
pub fn open_session(thread: &str) -> Result<(), String> {
    uuid::Uuid::parse_str(thread).map_err(|_| "无效会话编号")?;
    if crate::process::shell_open(&format!("codex://threads/{thread}")) {
        return Ok(());
    }
    Err("无法打开 Codex；会话已保存，可在 Codex 中按会话编号查找".into())
}

pub fn compose(cwd: &Path, prompt: &str) -> crate::Result<()> {
    let url = format!("codex://threads/new?path={}&prompt={}", encode(&cwd.to_string_lossy()), encode(prompt));
    if crate::process::shell_open(&url) {
        return Ok(());
    }
    Err(crate::Error::new("无法打开 Codex，请确认已安装 Codex 桌面端"))
}

fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => char::from(b).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Where the sign-in lives and the services it talks to.
struct Backend<'a> {
    home: PathBuf,
    chatgpt: &'a str,
    openai: &'a str,
    token: &'a str,
}

impl Default for Backend<'static> {
    fn default() -> Self {
        let home = std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| {
            PathBuf::from(std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap_or_default())
                .join(".codex")
        });
        Self { home, chatgpt: CHATGPT, openai: OPENAI, token: TOKEN }
    }
}

/// The local projects of the Codex app: its app server keeps them in
/// `$CODEX_HOME/state_<n>.sqlite`; the newest layout is read, metadata only.
pub fn projects() -> Result<Vec<Value>, String> {
    projects_in(&Backend::default().home)
}

fn projects_in(home: &Path) -> Result<Vec<Value>, String> {
    let layout = |p: &PathBuf| {
        let name = p.file_name()?.to_str()?;
        name.strip_prefix("state_")?.strip_suffix(".sqlite")?.parse::<u32>().ok()
    };
    let entries = match fs::read_dir(home) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        entries => entries.map_err(|e| format!("Codex 目录不可读：{e}"))?,
    };
    let Some(db) = entries.filter_map(|e| e.ok().map(|e| e.path())).filter_map(|p| Some((layout(&p)?, p))).max() else {
        return Ok(vec![]);
    };
    let read = || -> rusqlite::Result<Vec<(String, String, String)>> {
        let conn = rusqlite::Connection::open_with_flags(&db.1, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut stmt = conn.prepare(
            "SELECT p.id, p.name, r.path FROM projects p JOIN project_roots r ON r.project_id = p.id AND r.position = 0
             ORDER BY p.position, p.created_at_ms",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    };
    let rows = read().map_err(|e| format!("Codex 项目列表不可读：{e}"))?;
    Ok(rows
        .into_iter()
        .filter(|(_, _, cwd)| Path::new(cwd).is_absolute() && Path::new(cwd).is_dir())
        .map(|(id, name, cwd)| json!({"id": format!("codex:{id}"), "name": name, "cwd": cwd}))
        .collect())
}

enum Login {
    Key(String),
    ChatGpt { token: String, account: String },
}

impl Login {
    fn secret(&self) -> &str {
        match self {
            Login::Key(key) | Login::ChatGpt { token: key, .. } => key,
        }
    }
}

/// The JSON claims of a JWT, `null` when it has none.
fn claims(jwt: &str) -> Value {
    use base64::Engine;
    let payload = jwt.split('.').nth(1).unwrap_or("").trim_end_matches('=');
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null)
}

/// The sign-in of `auth.json`; a ChatGPT token expiring within five
/// minutes is refreshed and written back as Codex CLI does.
fn login(b: &Backend<'_>) -> Result<Login, String> {
    static REFRESH: Mutex<()> = Mutex::new(());
    let _one = REFRESH.lock().unwrap();
    let path = b.home.join("auth.json");
    let text = fs::read_to_string(&path).map_err(|_| NOT_SIGNED_IN)?;
    let mut auth: Value = serde_json::from_str(&text).map_err(|_| "Codex 登录文件无效，请重新运行 codex login")?;
    if let Some(key) = auth["OPENAI_API_KEY"].as_str().filter(|k| !k.is_empty()) {
        if auth["auth_mode"] == "apikey" || !auth["tokens"].is_object() {
            return Ok(Login::Key(key.into()));
        }
    }
    let access = auth["tokens"]["access_token"].as_str().ok_or(NOT_SIGNED_IN)?;
    if claims(access)["exp"].as_i64().is_some_and(|exp| exp - 300 < chrono::Utc::now().timestamp()) {
        refresh(b, &mut auth)?;
        let temp = path.with_extension("json.shishi");
        fs::write(&temp, serde_json::to_vec_pretty(&auth).unwrap()).map_err(|e| format!("无法写回 Codex 登录：{e}"))?;
        if let Ok(meta) = fs::metadata(&path) {
            let _ = fs::set_permissions(&temp, meta.permissions());
        }
        fs::rename(&temp, &path).map_err(|e| format!("无法写回 Codex 登录：{e}"))?;
    }
    let tokens = &auth["tokens"];
    let account = tokens["account_id"].as_str().map(String::from).unwrap_or_else(|| {
        claims(tokens["id_token"].as_str().unwrap_or(""))["https://api.openai.com/auth"]["chatgpt_account_id"]
            .as_str()
            .unwrap_or("")
            .into()
    });
    Ok(Login::ChatGpt { token: tokens["access_token"].as_str().unwrap_or("").into(), account })
}

fn refresh(b: &Backend<'_>, auth: &mut Value) -> Result<(), String> {
    let expired = "Codex 登录已过期，请重新运行 codex login";
    let token = auth["tokens"]["refresh_token"].as_str().ok_or(expired)?;
    let body = json!({"client_id": CLIENT_ID, "grant_type": "refresh_token", "refresh_token": token,
        "refresh_reason": "proactive_expiry"})
    .to_string();
    let agent = user_agent(&version(&b.home));
    let headers = [("originator", "codex_cli_rs"), ("User-Agent", agent.as_str())];
    let request = HttpRequest {
        post: Some(body.as_bytes()),
        headers: &headers,
        ..HttpRequest::get(b.token, Duration::from_secs(30), 1 << 20)
    };
    let response = send(&request, None).map_err(|e| format!("Codex 登录刷新失败：{e}"))?;
    let fresh: Value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
    if response.status != 200 || !fresh["access_token"].is_string() {
        return Err(format!("{expired}（HTTP {}）", response.status));
    }
    for key in ["access_token", "id_token", "refresh_token"] {
        if let Some(value) = fresh[key].as_str() {
            auth["tokens"][key] = json!(value);
        }
    }
    auth["last_refresh"] = json!(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    Ok(())
}

/// The client version Codex last cached, else a recent release.
fn version(home: &Path) -> String {
    let cache =
        fs::read_to_string(home.join("models_cache.json")).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok());
    cache.and_then(|v| v["client_version"].as_str().map(String::from)).unwrap_or_else(|| VERSION.into())
}

fn user_agent(version: &str) -> String {
    let (os, terminal) = match std::env::consts::OS {
        "windows" => ("Windows 10.0.26100", "WindowsTerminal"),
        "macos" => ("Mac OS 15.0.0", "Apple_Terminal/455"),
        _ => ("Linux", "xterm-256color"),
    };
    let arch = if cfg!(target_arch = "aarch64") { "arm64" } else { "x86_64" };
    format!("codex_cli_rs/{version} ({os}; {arch}) {terminal}")
}

fn client(b: &Backend<'_>, login: &Login, instructions: String) -> Result<Client, String> {
    let wire = match login {
        Login::Key(_) => return Client::new(b.openai, Wire::Chat).map_err(|e| e.to_string()),
        Login::ChatGpt { account, .. } => {
            let version = version(&b.home);
            let headers = [
                ("chatgpt-account-id", account.as_str()),
                ("OpenAI-Beta", "responses=experimental"),
                ("version", &version),
                ("originator", "codex_cli_rs"),
                ("User-Agent", &user_agent(&version)),
            ];
            Wire::Codex { instructions, headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() }
        }
    };
    Client::new(b.chatgpt, wire).map_err(|e| e.to_string())
}

static CATALOG: Mutex<Vec<ModelInfo>> = Mutex::new(Vec::new());

/// The signed-in model list, fetched when `fresh` or not yet known.
fn catalog(b: &Backend<'_>, login: &Login, fresh: bool) -> Result<Vec<ModelInfo>, String> {
    let mut cached = CATALOG.lock().unwrap();
    if fresh || cached.is_empty() {
        let client = client(b, login, String::new())?;
        *cached = client
            .discover_models(login.secret(), Duration::from_secs(15))
            .map_err(|e| format!("读取 Codex 模型列表失败：{e}"))?;
    }
    Ok(cached.clone())
}

fn analyze(b: &Backend<'_>, job: &Job, context: Option<&mut dyn Context>, event: &mut dyn FnMut(Event)) -> Reply {
    let login = login(b)?;
    let (wanted, models) = (&job.route.model, catalog(b, &login, false)?);
    let model = models
        .iter()
        .find(|m| wanted.is_empty() || m.id == *wanted)
        .ok_or("所选模型已不在 Codex 可用列表，请刷新模型")?;
    let client = client(b, &login, model.instructions.clone().unwrap_or_default())?;
    provider::tool_loop(job, &model.id, context, event, login.secret(), false, |r, e| {
        client.analyze_with_events(r, true, e)
    })
}

pub fn list_models() -> Result<Value, String> {
    let b = Backend::default();
    let mut models = super::api::catalog_json(&catalog(&b, &login(&b)?, true)?);
    models["source"] = json!("Codex 登录");
    Ok(models)
}

/// A Codex route accepts any listed model and the known efforts.
pub fn validate(effort: &str) -> Result<(), String> {
    let known = ["", "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"].contains(&effort);
    known.then_some(()).ok_or_else(|| "无效的推理强度".into())
}

fn status_text(auth: Option<Value>) -> String {
    let Some(auth) = auth else { return NOT_SIGNED_IN.into() };
    if auth["tokens"]["access_token"].is_string() && auth["auth_mode"] != "apikey" {
        let plan = claims(auth["tokens"]["id_token"].as_str().unwrap_or(""))["https://api.openai.com/auth"]
            ["chatgpt_plan_type"]
            .clone();
        return format!("已用 ChatGPT 登录{}", plan.as_str().map(|p| format!("（{p}）")).unwrap_or_default());
    }
    if auth["OPENAI_API_KEY"].as_str().is_some_and(|k| !k.is_empty()) {
        return "已用 API Key 登录".into();
    }
    NOT_SIGNED_IN.into()
}

/// The first usable Codex CLI of this machine, or empty.
pub fn detect_cli() -> String {
    let mut paths = vec![];
    if let Some(root) = std::env::var_os("LOCALAPPDATA") {
        let root = PathBuf::from(root);
        let bundled = root.join("OpenAI/Codex/bin");
        paths.extend(latest_bundled_cli(&bundled));
        paths.push(bundled.join("codex.exe"));
        let base = root.join("Programs/Codex");
        let mut children: Vec<_> =
            fs::read_dir(&base).into_iter().flatten().flatten().map(|e| e.path().join("codex.exe")).collect();
        children.sort_by(|a, b| b.cmp(a));
        paths.extend(children);
        paths.push(base.join("codex.exe"));
    }
    if let Some(root) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&root) {
            paths.extend(
                ["codex.exe", "codex.cmd"]
                    .into_iter()
                    .chain(cfg!(not(windows)).then_some("codex"))
                    .map(|name| dir.join(name)),
            );
        }
    }
    if let Some(root) = std::env::var_os("USERPROFILE") {
        paths.push(PathBuf::from(root).join(".codex/bin/codex.exe"));
    }
    if let Some(root) = std::env::var_os("APPDATA") {
        paths.push(PathBuf::from(root).join("npm/codex.cmd"));
    }
    paths.into_iter().filter(|p| p.is_file()).find_map(|p| resolve_cli(&p.to_string_lossy()).ok()).unwrap_or_default()
}

/// Reparse points (junctions, symlinks) never lead to a launcher.
#[cfg(windows)]
fn reparse(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn reparse(meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}

fn bundled_version(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|name| (8..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn bundled_cli_root(path: &Path) -> Option<&Path> {
    if !path.is_absolute()
        || !path.file_name()?.eq_ignore_ascii_case("codex.exe")
        || !bundled_version(path.parent()?.file_name()?)
    {
        return None;
    }
    let bin = path.parent()?.parent()?;
    (bin.file_name()?.eq_ignore_ascii_case("bin")
        && bin.parent()?.file_name()?.eq_ignore_ascii_case("Codex")
        && bin.parent()?.parent()?.file_name()?.eq_ignore_ascii_case("OpenAI"))
    .then_some(bin)
}

fn latest_bundled_cli(bin: &Path) -> Option<PathBuf> {
    let root = fs::symlink_metadata(bin).ok()?;
    if !root.is_dir() || reparse(&root) {
        return None;
    }
    let newest = fs::read_dir(bin).ok()?.flatten().filter_map(|entry| {
        let dir = fs::symlink_metadata(entry.path())
            .ok()
            .filter(|dir| bundled_version(&entry.file_name()) && dir.is_dir() && !reparse(dir))?;
        let path = entry.path().join("codex.exe");
        let file = fs::symlink_metadata(&path).ok().filter(|file| file.is_file() && !reparse(file) && dir.is_dir())?;
        Some((file.modified().ok(), file.created().ok(), path))
    });
    newest.max().map(|(_, _, path)| path)
}

/// Recover removed desktop versions within the saved installation only. npm
/// launchers resolve directly to their native binary without a shell.
pub fn resolve_cli(path: &str) -> Result<String, String> {
    let p = Path::new(path);
    if !p.is_file() {
        if fs::metadata(p).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound) {
            if let Some(replacement) = bundled_cli_root(p).and_then(latest_bundled_cli) {
                return Ok(replacement.to_string_lossy().into());
            }
        }
        return Err("未找到 Codex CLI，请选择 codex.exe 或 npm 安装的 codex.cmd".into());
    }
    if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")) {
        return Ok(path.into());
    }
    // Development hosts run the native `codex` binary directly.
    if cfg!(not(windows)) && p.file_name().is_some_and(|s| s == "codex") {
        return Ok(path.into());
    }
    if !p.file_name().is_some_and(|s| s.eq_ignore_ascii_case("codex.cmd") || s.eq_ignore_ascii_case("codex.ps1")) {
        return Err("请选择 Codex 原生程序或 npm 的 Codex 启动文件".into());
    }
    let mut pending = vec![(p.parent().unwrap().join("node_modules/@openai"), 0)];
    let mut seen = 0;
    while let Some((dir, depth)) = pending.pop() {
        if depth > 9 || seen > 4000 {
            continue;
        }
        if let Ok(entries) = fs::read_dir(dir) {
            for e in entries.flatten() {
                seen += 1;
                let path = e.path();
                let Ok(meta) = fs::symlink_metadata(&path) else { continue };
                if reparse(&meta) {
                    continue;
                }
                if meta.is_file() && e.file_name().eq_ignore_ascii_case("codex.exe") {
                    let arch = path.to_string_lossy().to_ascii_lowercase();
                    if arch.contains("x86_64") || arch.contains("win32-x64") {
                        return Ok(path.to_string_lossy().into());
                    }
                } else if meta.is_dir() {
                    pending.push((path, depth + 1));
                }
            }
        }
    }
    Err("未找到 npm Codex 的 Windows x64 原生程序。请完整安装 Codex CLI，或直接选择其 codex.exe".into())
}

#[cfg(test)]
mod tests;
