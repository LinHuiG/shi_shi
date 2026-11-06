//! DSH launches, the caller-owned restricted analysis profile and its
//! readiness check.
//!
//! The launcher composes `--patch` overlays after the profile and home layers,
//! so the restricted patch leaves `$DSH_HOME/profiles/acp` and the user's
//! credentials untouched. The patch disables every built-in tool, skill, web,
//! fs, shell, subagent and instructions provider; only the MCP servers given
//! in `session/new` remain. Readiness checks the composed `--dump-config`, not
//! the version number: unknown active entries fail closed.

use crate::client::AcpLaunch;
use crate::process::output_until;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};
use yaml_serde::{Mapping, Value};

pub const RESTRICTED_PATCH: &str = include_str!("../assets/restricted-analysis.patch.yml");

/// Every row allowed to stay active, from the verified 0.2.1-rc.2 and 0.2.2-rc.1
/// compositions. Any other active row fails closed, whatever the version.
const ALLOWED_ACTIVE: &str = "timer llm deepseek-llm-api-extensions session session-log-deepseek typert typert-loader
    typert-gateway remote-access session-title agent plugin-package-inventory-deepseek agent-default-model jobs
    llm-retry authorization deepseek-account credentials llm-pi-ai session-persistence-jsonl attachment-local
    session-query-sqlite working-directory session-projection storage storage-json storage-domain
    session-projection-cache otel session-telemetry-otel subprocess approval permission token-meter
    compaction-basic timeout-policy session-checkpoint-policy tool-result-pruner image-offload tools tool-executor-native
    system-prompt agent-loop llm-deepseek llm-deepseek-account acp-app-startup acp";

/// A user-owned ACP session keeps the user's permission and telemetry settings.
pub fn handling_launch(dsh_cmd: &Path, cwd: &Path, dsh_home: &Path, patch: Option<&Path>) -> AcpLaunch {
    let launch = AcpLaunch::new(dsh_cmd).args(["--profile", "acp"]).cwd(cwd).env("DSH_HOME", dsh_home);
    match patch {
        Some(patch) => launch.arg("--patch").arg(patch),
        None => launch,
    }
}

/// Per-request profile files: the restricted patch (plus inherited
/// providers) and an empty agents home, so no global AGENTS.md or skills load.
#[derive(Debug, Clone)]
pub struct MaterializedProfile {
    pub patch: PathBuf,
    /// The provider-only overlay for handling sessions, when providers exist.
    pub providers: Option<PathBuf>,
    pub agents_dir: PathBuf,
}

pub fn materialize_profile(dir: &Path, dsh_home: &Path) -> Result<MaterializedProfile, String> {
    let agents_dir = dir.join("agents");
    fs::create_dir_all(&agents_dir).map_err(|e| e.to_string())?;
    let patch = dir.join("restricted-analysis.patch.yml");
    fs::write(&patch, RESTRICTED_PATCH).map_err(|e| e.to_string())?;
    let providers = inherit_providers(&patch, dsh_home)?;
    Ok(MaterializedProfile { patch, providers, agents_dir })
}

impl MaterializedProfile {
    pub fn analysis_launch(&self, dsh_cmd: &Path, workdir: &Path, dsh_home: &Path) -> AcpLaunch {
        restricted(
            AcpLaunch::new(dsh_cmd).args(["--profile", "acp"]).arg("--patch").arg(&self.patch),
            workdir,
            dsh_home,
            &self.agents_dir,
        )
    }

    pub fn readiness(&self, dsh_cmd: &Path, dsh_home: &Path, workdir: &Path) -> ReadinessConfig {
        ReadinessConfig {
            dsh_cmd: dsh_cmd.into(),
            dsh_home: dsh_home.into(),
            agents_dir: self.agents_dir.clone(),
            patch: self.patch.clone(),
            workdir: workdir.into(),
        }
    }
}

fn restricted(launch: AcpLaunch, workdir: &Path, dsh_home: &Path, agents: &Path) -> AcpLaunch {
    launch
        .cwd(workdir)
        .env("DSH_PERMISSION_MODE", "read-only")
        .env("DSH_TELEMETRY_DISABLED", "1")
        .env("DSH_HOME", dsh_home)
        .env("DSH_AGENTS_HOME", agents)
}

/// Copy only the built-in `llm-pi-ai` provider definitions of the desktop and
/// acp profiles (acp overrides desktop; a disabled acp row drops them),
/// never other plugins or tools, into a temporary overlay appended to `patch`.
fn inherit_providers(patch: &Path, home: &Path) -> Result<Option<PathBuf>, String> {
    let (mut providers, mut config) = (Mapping::new(), Mapping::new());
    for name in ["desktop", "acp"] {
        let text = match fs::read_to_string(home.join("profiles").join(name).join("cordis.patch.yml")) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err("DSH 供应商配置不可读，请检查 profile 权限".into()),
        };
        let document: Value = yaml_serde::from_str(&text).map_err(|_| "DSH profile YAML 配置无效；未修改原文件")?;
        let rows = match &document {
            Value::Null => continue,
            rows => rows.as_sequence().ok_or("DSH profile 必须为配置行列表")?,
        };
        for row in rows.iter().filter(|r| r["id"].as_str() == Some("llm-pi-ai")) {
            if row["name"].as_str().is_some_and(|n| n != "@deepseek-ai/dsh-llm-pi-ai") {
                return Err("DSH 内置供应商行使用了未验证插件，拒绝继承".into());
            }
            if row["disabled"].as_bool() == Some(true) || row["disabled"].as_str() == Some("true") {
                if name == "acp" {
                    providers.clear();
                }
                continue;
            }
            if let (Some(own), "acp") = (row["config"].as_mapping(), name) {
                config = own.clone();
            }
            for (key, value) in row["config"]["providers"].as_mapping().into_iter().flatten() {
                providers.insert(key.clone(), chat_role_compat(value));
            }
        }
    }
    if providers.is_empty() {
        return Ok(None);
    }
    config.insert("providers".into(), Value::Mapping(providers));
    let row = Mapping::from_iter([
        ("id".into(), "llm-pi-ai".into()),
        ("name".into(), "@deepseek-ai/dsh-llm-pi-ai".into()),
        ("config".into(), Value::Mapping(config)),
    ]);
    let text = yaml_serde::to_string(&vec![Value::Mapping(row)]).map_err(|_| "无法生成供应商临时配置")?;
    let overlay = patch.with_file_name("providers.patch.yml");
    fs::write(&overlay, &text)
        .and_then(|_| fs::write(patch, format!("{RESTRICTED_PATCH}\n{text}")))
        .map_err(|_| "无法写入供应商临时配置")?;
    Ok(Some(overlay))
}

/// DeepSeek Chat Completions accepts `system`, not `developer`: default an
/// omitted `supportsDeveloperRole` to false for those models only.
fn chat_role_compat(provider: &Value) -> Value {
    let mut next = provider.clone();
    if provider["compat"]["supportsDeveloperRole"].as_bool().is_some() {
        return next;
    }
    let route_api = provider["api"].as_str().unwrap_or("");
    for model in next.get_mut("models").and_then(Value::as_sequence_mut).into_iter().flatten() {
        let id = model["id"].as_str().unwrap_or("").to_ascii_lowercase();
        if !id.contains("deepseek") || model["api"].as_str().unwrap_or(route_api) != "openai-completions" {
            continue;
        }
        if let Some(map) = model.as_mapping_mut() {
            let compat = map.entry("compat".into()).or_insert_with(|| Value::Mapping(Mapping::new()));
            if let Some(compat) = compat.as_mapping_mut() {
                compat.entry("supportsDeveloperRole".into()).or_insert(Value::Bool(false));
            }
        }
    }
    next
}

#[derive(Debug, Clone)]
pub struct ReadinessConfig {
    pub dsh_cmd: PathBuf,
    pub dsh_home: PathBuf,
    pub agents_dir: PathBuf,
    pub patch: PathBuf,
    pub workdir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Readiness {
    pub runtime_version: String,
    pub active_ids: Vec<String>,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ReadinessError {
    #[error("ACP 环境尚未初始化，缺少配置：{}", .0.display())]
    ProfileMissing(PathBuf),
    #[error("{0}")]
    Failed(String),
}

fn failed<T>(message: impl Into<String>) -> Result<T, ReadinessError> {
    Err(ReadinessError::Failed(message.into()))
}

impl ReadinessError {
    /// Setup commands to show the user, never to run automatically.
    pub fn guidance(&self, config: &ReadinessConfig) -> String {
        if !matches!(self, Self::ProfileMissing(_)) {
            return self.to_string();
        }
        let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "''"));
        format!("{self}\n请在 PowerShell 中执行以下命令，再点击“刷新模型”。使用当前选中的启动器和 DSH_HOME，不会重置已有配置。\n$env:DSH_HOME={}; & {} --profile acp --help\n若初始化失败，请检查启动器版本、文件权限及依赖；不要通过手动添加模型绕过此检查。", quote(&config.dsh_home), quote(&config.dsh_cmd))
    }
}

/// Explicit user-triggered setup: the dump probe initializes the shipped acp
/// profile without booting plugins or sending a prompt, and never resets an
/// existing profile.
pub fn initialize_acp(config: &ReadinessConfig) -> Result<Readiness, ReadinessError> {
    static SETUP: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = SETUP.try_lock().or_else(|_| failed("ACP 正在初始化，请稍后重试"))?;
    match check_readiness(config) {
        Err(ReadinessError::ProfileMissing(_)) => {}
        other => return other,
    }
    probe(config, Instant::now() + Duration::from_secs(40), &AtomicBool::new(false))?;
    check_readiness(config)
}

pub fn check_readiness(config: &ReadinessConfig) -> Result<Readiness, ReadinessError> {
    check_readiness_until(config, Instant::now() + Duration::from_secs(40), &AtomicBool::new(false))
}

/// Read-only: a missing profile is reported, never created.
pub fn check_readiness_until(
    config: &ReadinessConfig,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<Readiness, ReadinessError> {
    let manifest = config.dsh_home.join("profiles/acp/package.json");
    let bytes = match fs::read(&manifest) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(ReadinessError::ProfileMissing(manifest)),
        Err(e) => return failed(format!("ACP 配置不可读：{}；请检查目录权限。{e}", manifest.display())),
        Ok(bytes) => bytes,
    };
    let bundles =
        serde_json::from_slice::<serde_json::Value>(&bytes).ok().map(|m| m["dsh"]["profile"]["bundles"].clone());
    let has = |bundle: &str| bundles.as_ref().and_then(|b| b.as_array()).is_some_and(|b| b.iter().any(|v| v == bundle));
    if !has("@deepseek-ai/dsh-base") || !has("@deepseek-ai/dsh-acp-app") {
        return failed(
            "ACP 配置内容无效，请检查配置后重试（不会自动覆盖）：profile is not the shipped acp bundle stack",
        );
    }
    probe(config, deadline, cancel)
}

/// Record `dsh --version`, then verify the composed dump: the composition,
/// not the version number, decides.
fn probe(config: &ReadinessConfig, deadline: Instant, cancel: &AtomicBool) -> Result<Readiness, ReadinessError> {
    let run = |args: &[&std::ffi::OsStr], limit: u64| {
        let launch = restricted(
            AcpLaunch::new(&config.dsh_cmd).args(args),
            &config.workdir,
            &config.dsh_home,
            &config.agents_dir,
        );
        let command =
            launch.command().map_err(|e| ReadinessError::Failed(format!("could not run dsh for readiness: {e}")))?;
        let output = output_until(command, deadline.min(Instant::now() + Duration::from_secs(limit)), cancel)
            .map_err(ReadinessError::Failed)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return failed(format!("dsh --dump-config failed: {}", stderr.lines().next().unwrap_or("no stderr")));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    let version = run(&["--version".as_ref()], 10)?;
    let dump = run(
        &["--profile".as_ref(), "acp".as_ref(), "--patch".as_ref(), config.patch.as_os_str(), "--dump-config".as_ref()],
        30,
    )?;
    verify_dump(&version, &dump)
}

/// Verify the line-oriented YAML of `dsh --dump-config`.
pub fn verify_dump(runtime_version: &str, dump: &str) -> Result<Readiness, ReadinessError> {
    let mut entries: Vec<(String, bool)> = Vec::new();
    for line in dump.lines() {
        if let Some(id) = line.strip_prefix("- id:") {
            entries.push((id.trim().trim_matches(['\'', '"']).into(), false));
        } else if let (Some(value), Some(entry)) = (line.strip_prefix("  disabled:"), entries.last_mut()) {
            entry.1 = value.trim() == "true";
        }
    }
    let active_ids = entries.into_iter().filter(|e| !e.1).map(|e| e.0).collect::<Vec<_>>();
    if let Some(id) = active_ids.iter().find(|id| !ALLOWED_ACTIVE.split_whitespace().any(|a| a == *id)) {
        return failed(format!("restricted profile has an unverified active entry `{id}`; refusing analysis"));
    }
    if !active_ids.iter().any(|id| id == "acp") {
        return failed("restricted profile has no active `acp` bridge; refusing analysis");
    }
    Ok(Readiness { runtime_version: runtime_version.into(), active_ids })
}

#[cfg(test)]
mod tests;
