//! Caller-owned restricted analysis profile assets and readiness verification.
//!
//! The installed launcher composes `--patch` overlays after the profile and
//! home patch layers (dsh README, Profiles). Using `--patch` keeps the user's
//! `$DSH_HOME/profiles/acp` untouched while still sharing the user's DSH home
//! and authentication. The patch disables every built-in tool/skill/web/fs/
//! shell/subagent/instructions provider; the only tools left are the MCP
//! servers supplied by the caller in `session/new`.

use crate::client::AcpLaunch;
use crate::process::{run_bounded_command, BoundedOutput};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub const SUPPORTED_RUNTIME_VERSION: &str = "0.2.1-rc.2";
pub const RESTRICTED_PATCH: &str = include_str!("../assets/restricted-analysis.patch.yml");

/// Every row that must be literally disabled in the composed `--dump-config`.
pub const REQUIRED_DISABLED: &[&str] = &[
    "plugin-manager",
    "tool-plugin-manager",
    "hmr",
    "config-editor",
    "settings",
    "tool-working-directory",
    "sandbox",
    "sandbox-policy",
    "bash-sandbox",
    "pwsh-sandbox",
    "shell-env",
    "tool-bash",
    "tool-pwsh",
    "tool-jobs",
    "fs-observation-policy",
    "fs-sandbox",
    "tool-fs",
    "tool-fs-search",
    "agent-instructions",
    "skill",
    "skill-filesystem",
    "tool-skill",
    "commands",
    "command-feedback",
    "command-goal",
    "command-compact",
    "goal",
    "goal-round-driver",
    "plan-mode",
    "subagent",
    "subagent-spawn-in-process",
    "subagent-fork-in-process",
    "tool-subagent-control",
    "tool-subagent-list-agents",
    "tool-subagent",
    "tool-subagent-fork",
    "ptc-runtime",
    "workflow-ptc",
    "tool-workflow",
    "tool-todo",
    "tool-goal",
    "web",
    "web-search-deepseek",
    "web-deepseek-api",
    "web-fetch-http",
    "tool-web",
    "mcp-resources",
    "user-questions",
    "repeat-tool-reminder",
    "spill-local",
    "spill-policy",
];

/// Active entries allowed in the exact 0.2.1-rc.2 ACP composition with the
/// overlay. Any extra active entry means the composition cannot be verified
/// (for example a user plugin), so readiness fails closed.
pub const ALLOWED_ACTIVE_IDS: &[&str] = &[
    "timer",
    "llm",
    "deepseek-llm-api-extensions",
    "session",
    "session-log-deepseek",
    "typert",
    "typert-loader",
    "typert-gateway",
    "remote-access",
    "session-title",
    "agent",
    "plugin-package-inventory-deepseek",
    "agent-default-model",
    "jobs",
    "llm-retry",
    "authorization",
    "deepseek-account",
    "credentials",
    "llm-pi-ai",
    "session-persistence-jsonl",
    "attachment-local",
    "session-query-sqlite",
    "working-directory",
    "session-projection",
    "storage",
    "storage-json",
    "storage-domain",
    "session-projection-cache",
    "otel",
    "session-telemetry-otel",
    "subprocess",
    "approval",
    "permission",
    "token-meter",
    "compaction-basic",
    "timeout-policy",
    "session-checkpoint-policy",
    "tool-result-pruner",
    "image-offload",
    "tools",
    "system-prompt",
    "agent-loop",
    "llm-deepseek",
    "llm-deepseek-account",
    "acp-app-startup",
    "acp",
];

const FORBIDDEN_ACTIVE_PREFIXES: &[&str] = &[
    "@deepseek-ai/dsh-skill",
    "@deepseek-ai/dsh-agent-instructions",
    "@deepseek-ai/dsh-web",
    "@deepseek-ai/dsh-subagent",
    "@deepseek-ai/dsh-workflow",
    "@deepseek-ai/dsh-commands",
    "@deepseek-ai/dsh-command-",
    "@deepseek-ai/dsh-fs-",
    "@deepseek-ai/dsh-bash-",
    "@deepseek-ai/dsh-pwsh-",
    "@deepseek-ai/dsh-sandbox",
    "@deepseek-ai/dsh-shell-env",
    "@deepseek-ai/dsh-mcp-resources",
    "@deepseek-ai/dsh-plan-mode",
    "@deepseek-ai/dsh-goal",
    "@deepseek-ai/dsh-user-questions",
    "@deepseek-ai/dsh-repeat-tool-reminder",
    "@deepseek-ai/dsh-spill-",
];

#[derive(Debug, Clone)]
pub struct MaterializedProfile {
    pub directory: PathBuf,
    pub patch: PathBuf,
    pub launch_json: PathBuf,
    pub profile_dir: PathBuf,
    pub agents_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub enum RestrictedError {
    Io(String),
    InvalidDirectory(String),
    LaunchJson(String),
}

impl std::fmt::Display for RestrictedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(message) => write!(f, "profile asset I/O failed: {message}"),
            Self::InvalidDirectory(message) => write!(f, "invalid profile directory: {message}"),
            Self::LaunchJson(message) => write!(f, "launch metadata failed: {message}"),
        }
    }
}

impl std::error::Error for RestrictedError {}

impl MaterializedProfile {
    /// Build the restricted analysis launch with the empty agents home and the
    /// caller's shared DSH home. Use this instead of hand-building
    /// `DshLaunch`, so the AGENTS/skills root cannot silently fall back to the
    /// user's global `.agents` directory.
    pub fn analysis_launch(
        &self,
        dsh_cmd: impl Into<PathBuf>,
        workdir: impl Into<PathBuf>,
        dsh_home: impl Into<PathBuf>,
    ) -> DshLaunch {
        let mut launch = DshLaunch::new(dsh_cmd, &self.patch, workdir);
        launch.dsh_home = Some(dsh_home.into());
        launch.agents_home = Some(self.agents_dir.clone());
        launch
    }
}

/// Write the caller-owned overlay, launch metadata and a README into `dir`.
/// The function never writes to `$DSH_HOME` or the user's global profile.
pub fn materialize_profile(dir: &Path) -> Result<MaterializedProfile, RestrictedError> {
    if dir.as_os_str().is_empty() {
        return Err(RestrictedError::InvalidDirectory("empty path".into()));
    }
    fs::create_dir_all(dir).map_err(|error| RestrictedError::Io(error.to_string()))?;
    let patch = dir.join("restricted-analysis.patch.yml");
    write_atomic(&patch, RESTRICTED_PATCH.as_bytes())?;

    // A complete profile template for an isolated DSH home. The production
    // launch path uses the `--patch` overlay so the shared user home/auth is
    // preserved; this template is an inspectable fallback, not a global write.
    let profile_dir = dir.join("profile");
    fs::create_dir_all(&profile_dir).map_err(|error| RestrictedError::Io(error.to_string()))?;
    write_atomic(
        &profile_dir.join("package.json"),
        serde_json::to_vec_pretty(&json!({
            "name": "dsh-profile-shishi-analysis",
            "private": true,
            "dependencies": {},
            "dsh": {"profile": {"bundles": [
                "@deepseek-ai/dsh-base",
                "@deepseek-ai/dsh-acp-app"
            ]}}
        }))
        .map_err(|error| RestrictedError::LaunchJson(error.to_string()))?
        .as_slice(),
    )?;
    write_atomic(
        &profile_dir.join("cordis.patch.yml"),
        RESTRICTED_PATCH.as_bytes(),
    )?;
    let agents_dir = dir.join("agents");
    fs::create_dir_all(&agents_dir).map_err(|error| RestrictedError::Io(error.to_string()))?;

    let launch_json = dir.join("launch.json");
    let launch = json!({
        "version": 1,
        "profile": "acp",
        "patch": patch.to_string_lossy(),
        "argv": ["--profile", "acp", "--patch", patch.to_string_lossy()],
        "profile_asset": profile_dir.to_string_lossy(),
        "agents_home": agents_dir.to_string_lossy(),
        "environment": {
            "DSH_PERMISSION_MODE": "read-only",
            "DSH_TELEMETRY_DISABLED": "1",
            "DSH_AGENTS_HOME": agents_dir.to_string_lossy()
        },
        "note": "Apply this patch with the dsh launcher. Do not copy the patch into $DSH_HOME/profiles."
    });
    write_atomic(
        &launch_json,
        serde_json::to_vec_pretty(&launch)
            .map_err(|error| RestrictedError::LaunchJson(error.to_string()))?
            .as_slice(),
    )?;
    let readme = dir.join("README.txt");
    write_atomic(
        &readme,
        b"ShiShi restricted ACP analysis profile\n\n\
The app applies this directory as a `--patch` overlay on the shipped dsh `acp`\n\
profile. It never writes the user's global profile. The patch disables built-in\n\
tools and leaves only the per-session MCP servers listed in session/new:\n\
  shishi_analysis -> read_context, read_image\n\
  shishi_submit   -> submit_analysis\n\n\
The empty agents/ directory is passed as DSH_AGENTS_HOME for analysis so a\n\
user-global AGENTS.md or skills/ tree is not injected. profile/ is a complete\n\
template for an isolated DSH_HOME if the shared-home launch is unavailable.\n\
\n\
Readiness is verified with the installed runtime version and a composed\n\
`--dump-config`; unknown active entries fail closed.\n",
    )?;
    Ok(MaterializedProfile {
        directory: dir.to_path_buf(),
        patch,
        launch_json,
        profile_dir,
        agents_dir,
    })
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), RestrictedError> {
    let parent = path
        .parent()
        .ok_or_else(|| RestrictedError::InvalidDirectory(path.display().to_string()))?;
    fs::create_dir_all(parent).map_err(|error| RestrictedError::Io(error.to_string()))?;
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temp, bytes).map_err(|error| RestrictedError::Io(error.to_string()))?;
    match fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temp);
            Err(RestrictedError::Io(error.to_string()))
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReadinessConfig {
    pub dsh_cmd: PathBuf,
    pub dsh_home: PathBuf,
    pub agents_home: Option<PathBuf>,
    pub patch: PathBuf,
    pub workdir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpEntry {
    pub id: String,
    pub name: String,
    pub disabled: Option<String>,
}

impl DumpEntry {
    pub fn is_disabled(&self) -> bool {
        self.disabled.as_deref() == Some("true")
    }
}

#[derive(Debug, Clone)]
pub struct Readiness {
    pub runtime_version: String,
    pub active_count: usize,
    pub disabled_count: usize,
    pub active_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum ReadinessError {
    ProfileMissing(PathBuf),
    ProfileUnreadable { path: PathBuf, message: String },
    PatchMissing(PathBuf),
    ProfileInvalid(String),
    RuntimeVersion(String),
    DumpFailed(String),
    DumpUnparsable(String),
    RequiredNotDisabled(String),
    UnverifiedActive { id: String, name: String },
    AcpBridgeMissing,
    Spawn(String),
}

impl std::fmt::Display for ReadinessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProfileMissing(path) => write!(
                f,
                "ACP 环境尚未初始化，缺少配置：{}",
                path.display()
            ),
            Self::ProfileUnreadable { path, message } => write!(f, "ACP 配置不可读：{}；请检查目录权限。{message}", path.display()),
            Self::PatchMissing(path) => write!(f, "应用受限补丁缺失：{}；请重试或修复应用安装。", path.display()),
            Self::ProfileInvalid(message) => write!(f, "ACP 配置内容无效，请检查配置后重试（不会自动覆盖）：{message}"),
            Self::RuntimeVersion(version) => write!(
                f,
                "dsh runtime version {version:?} is not the verified {SUPPORTED_RUNTIME_VERSION}; refusing unverified tool isolation"
            ),
            Self::DumpFailed(message) => write!(f, "dsh --dump-config failed: {message}"),
            Self::DumpUnparsable(message) => {
                write!(f, "dsh --dump-config output is not parseable: {message}")
            }
            Self::RequiredNotDisabled(id) => write!(
                f,
                "restricted profile did not disable `{id}`; refusing analysis"
            ),
            Self::UnverifiedActive { id, name } => write!(
                f,
                "restricted profile has an unverified active entry `{id}` (`{name}`); refusing analysis"
            ),
            Self::AcpBridgeMissing => {
                f.write_str("restricted profile has no active `acp` bridge; refusing analysis")
            }
            Self::Spawn(message) => write!(f, "could not run dsh for readiness: {message}"),
        }
    }
}

impl std::error::Error for ReadinessError {}

impl ReadinessError {
    /// Guidance is data for display/copying, never an automatically executed repair.
    pub fn guidance(&self, config: &ReadinessConfig) -> String {
        if !matches!(self, Self::ProfileMissing(_)) {
            return self.to_string();
        }
        let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "''"));
        format!("{self}\n请在 PowerShell 中执行以下命令，再点击“刷新模型”。使用当前选中的启动器和 DSH_HOME，不会重置已有配置。\n$env:DSH_HOME={}; & {} --profile acp --help\n若初始化失败，请检查启动器版本、文件权限及依赖；不要通过手动添加模型绕过此检查。", quote(&config.dsh_home), quote(&config.dsh_cmd))
    }
}

/// Explicit user-triggered setup only. Dump mode initializes the shipped profile
/// without booting plugins or submitting a prompt. Existing profiles are never reset.
pub fn initialize_acp(
    config: &ReadinessConfig,
    confirmed: bool,
) -> Result<Readiness, ReadinessError> {
    if !confirmed {
        return Err(ReadinessError::Spawn("请先确认初始化 ACP 环境".into()));
    }
    static SETUP: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = SETUP
        .try_lock()
        .map_err(|_| ReadinessError::Spawn("ACP 正在初始化，请稍后重试".into()))?;
    match check_readiness(config) {
        Ok(ready) => return Ok(ready),
        Err(ReadinessError::ProfileMissing(_)) => {}
        Err(error) => return Err(error),
    }
    if !config.patch.is_file() {
        return Err(ReadinessError::PatchMissing(config.patch.clone()));
    }
    let version = run_dsh(
        &config.dsh_cmd,
        &["--version".into()],
        config,
        Duration::from_secs(10),
    )?;
    let version = String::from_utf8_lossy(&version.stdout).trim().to_string();
    if version != SUPPORTED_RUNTIME_VERSION {
        return Err(ReadinessError::RuntimeVersion(version));
    }
    let dump = run_dsh(
        &config.dsh_cmd,
        &[
            "--profile".into(),
            "acp".into(),
            "--patch".into(),
            config.patch.to_string_lossy().to_string(),
            "--dump-config".into(),
        ],
        config,
        Duration::from_secs(30),
    )?;
    verify_dump(&version, &String::from_utf8_lossy(&dump.stdout))?;
    check_readiness(config)
}

/// Verify the installed runtime and the composed profile before any analysis
/// session is allowed. This is read-only: missing profile => error, never
/// initialize or rewrite it.
pub fn check_readiness(config: &ReadinessConfig) -> Result<Readiness, ReadinessError> {
    let profile_manifest = config.dsh_home.join("profiles/acp/package.json");
    let manifest = fs::read(&profile_manifest).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ReadinessError::ProfileMissing(profile_manifest.clone())
        } else {
            ReadinessError::ProfileUnreadable {
                path: profile_manifest.clone(),
                message: error.to_string(),
            }
        }
    })?;
    let manifest: Value = serde_json::from_slice(&manifest)
        .map_err(|error| ReadinessError::ProfileInvalid(error.to_string()))?;
    let bundles = manifest
        .get("dsh")
        .and_then(|dsh| dsh.get("profile"))
        .and_then(|profile| profile.get("bundles"))
        .and_then(Value::as_array)
        .ok_or_else(|| ReadinessError::ProfileInvalid("missing dsh.profile.bundles".into()))?;
    let bundles = bundles
        .iter()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    if !bundles.contains("@deepseek-ai/dsh-base") || !bundles.contains("@deepseek-ai/dsh-acp-app") {
        return Err(ReadinessError::ProfileInvalid(
            "profile is not the shipped acp bundle stack".into(),
        ));
    }
    if !config.patch.is_file() {
        return Err(ReadinessError::PatchMissing(config.patch.clone()));
    }
    if !config.dsh_cmd.is_file() {
        return Err(ReadinessError::Spawn(format!(
            "launcher not found: {}",
            config.dsh_cmd.display()
        )));
    }

    let version = run_dsh(
        &config.dsh_cmd,
        &["--version".to_string()],
        config,
        Duration::from_secs(10),
    )?;
    let runtime_version = String::from_utf8_lossy(&version.stdout).trim().to_string();
    if runtime_version != SUPPORTED_RUNTIME_VERSION {
        return Err(ReadinessError::RuntimeVersion(runtime_version));
    }

    let dump = run_dsh(
        &config.dsh_cmd,
        &[
            "--profile".to_string(),
            "acp".to_string(),
            "--patch".to_string(),
            config.patch.to_string_lossy().to_string(),
            "--dump-config".to_string(),
        ],
        config,
        Duration::from_secs(30),
    )?;
    let text = String::from_utf8_lossy(&dump.stdout);
    verify_dump(&runtime_version, &text)
}

fn run_dsh(
    dsh_cmd: &Path,
    args: &[String],
    config: &ReadinessConfig,
    timeout: Duration,
) -> Result<BoundedOutput, ReadinessError> {
    let mut launch = AcpLaunch::new(dsh_cmd).args(args.iter().cloned());
    if let Some(workdir) = &config.workdir {
        launch = launch.cwd(workdir);
    }
    launch = launch.env("DSH_PERMISSION_MODE", "read-only");
    launch = launch.env("DSH_TELEMETRY_DISABLED", "1");
    launch = launch.env("DSH_HOME", &config.dsh_home);
    if let Some(agents_home) = &config.agents_home {
        launch = launch.env("DSH_AGENTS_HOME", agents_home);
    }
    let command: Command = launch
        .command()
        .map_err(|error| ReadinessError::Spawn(error.to_string()))?;
    let output = run_bounded_command(command, timeout, 4 * 1024 * 1024, 16 * 1024)
        .map_err(ReadinessError::Spawn)?;
    if output.timed_out {
        return Err(ReadinessError::DumpFailed("timed out".into()));
    }
    if !output.status.is_some_and(|status| status.success()) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let line = stderr
            .lines()
            .next()
            .unwrap_or("no stderr")
            .chars()
            .take(240)
            .collect::<String>();
        return Err(ReadinessError::DumpFailed(line));
    }
    Ok(output)
}

/// Parse the line-oriented YAML emitted by `dsh --dump-config` and verify the
/// restriction. Kept public for fixture tests and integration diagnostics.
pub fn verify_dump(runtime_version: &str, dump: &str) -> Result<Readiness, ReadinessError> {
    if runtime_version != SUPPORTED_RUNTIME_VERSION {
        return Err(ReadinessError::RuntimeVersion(runtime_version.to_string()));
    }
    let entries = parse_dump(dump)?;
    if entries.is_empty() {
        return Err(ReadinessError::DumpUnparsable("no entries".into()));
    }
    let by_id = entries
        .iter()
        .map(|entry| (entry.id.as_str(), entry))
        .collect::<std::collections::HashMap<_, _>>();
    for required in REQUIRED_DISABLED {
        match by_id.get(required) {
            Some(entry) if entry.is_disabled() => {}
            Some(_) => return Err(ReadinessError::RequiredNotDisabled((*required).into())),
            None => return Err(ReadinessError::RequiredNotDisabled((*required).into())),
        }
    }
    for entry in entries.iter().filter(|entry| !entry.is_disabled()) {
        if !ALLOWED_ACTIVE_IDS.contains(&entry.id.as_str()) {
            return Err(ReadinessError::UnverifiedActive {
                id: entry.id.clone(),
                name: entry.name.clone(),
            });
        }
        if FORBIDDEN_ACTIVE_PREFIXES
            .iter()
            .any(|prefix| entry.name.starts_with(prefix))
        {
            return Err(ReadinessError::UnverifiedActive {
                id: entry.id.clone(),
                name: entry.name.clone(),
            });
        }
    }
    let acp_active = entries
        .iter()
        .any(|entry| entry.id == "acp" && !entry.is_disabled());
    if !acp_active {
        return Err(ReadinessError::AcpBridgeMissing);
    }
    let active_ids = entries
        .iter()
        .filter(|entry| !entry.is_disabled())
        .map(|entry| entry.id.clone())
        .collect::<Vec<_>>();
    Ok(Readiness {
        runtime_version: runtime_version.to_string(),
        active_count: active_ids.len(),
        disabled_count: entries.len().saturating_sub(active_ids.len()),
        active_ids,
    })
}

pub fn parse_dump(dump: &str) -> Result<Vec<DumpEntry>, ReadinessError> {
    let mut entries = Vec::new();
    let mut current: Option<DumpEntry> = None;
    for line in dump.lines() {
        let trimmed = line.trim_end();
        if trimmed.starts_with('#') || trimmed.trim().is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("- id:") {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            current = Some(DumpEntry {
                id: unquote(rest.trim()),
                name: String::new(),
                disabled: None,
            });
        } else if let Some(rest) = trimmed.strip_prefix("  name:") {
            if let Some(entry) = current.as_mut() {
                entry.name = unquote(rest.trim());
            }
        } else if let Some(rest) = trimmed.strip_prefix("  disabled:") {
            if let Some(entry) = current.as_mut() {
                entry.disabled = Some(rest.trim().to_string());
            }
        }
    }
    if let Some(entry) = current.take() {
        entries.push(entry);
    }
    if entries.iter().any(|entry| entry.id.is_empty()) {
        return Err(ReadinessError::DumpUnparsable("empty entry id".into()));
    }
    Ok(entries)
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 {
        let first = value.as_bytes()[0];
        let last = value.as_bytes()[value.len() - 1];
        if (first == b'\'' && last == b'\'') || (first == b'"' && last == b'"') {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

#[derive(Debug, Clone)]
pub struct DshLaunch {
    pub dsh_cmd: PathBuf,
    pub profile: String,
    pub patch: Option<PathBuf>,
    pub dsh_home: Option<PathBuf>,
    pub agents_home: Option<PathBuf>,
    pub workdir: PathBuf,
    /// Analysis default is `read-only`; handling clears this so the user's
    /// existing permission/sandbox configuration is retained.
    pub permission_mode: Option<String>,
    pub disable_telemetry: bool,
}

impl DshLaunch {
    /// Analysis launch defaults. This is a restricted launch, not a handling
    /// launch: it forces read-only mode and disables telemetry.
    pub fn new(
        dsh_cmd: impl Into<PathBuf>,
        patch: impl Into<PathBuf>,
        workdir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            dsh_cmd: dsh_cmd.into(),
            profile: "acp".into(),
            patch: Some(patch.into()),
            dsh_home: None,
            agents_home: None,
            workdir: workdir.into(),
            permission_mode: Some("read-only".into()),
            disable_telemetry: true,
        }
    }

    /// Preserve the user's configured permission/sandbox and telemetry choices
    /// for handling sessions. Only use this with the user's normal ACP profile;
    /// the restricted analysis overlay is not applied.
    pub fn handling(mut self) -> Self {
        self.permission_mode = None;
        self.disable_telemetry = false;
        self.patch = None;
        self
    }

    fn base_launch(&self) -> AcpLaunch {
        let mut launch = AcpLaunch::new(&self.dsh_cmd)
            .args(["--profile", self.profile.as_str()])
            .cwd(&self.workdir);
        if let Some(patch) = &self.patch {
            launch = launch
                .arg("--patch")
                .arg(patch.to_string_lossy().to_string());
        }
        if let Some(mode) = &self.permission_mode {
            launch = launch.env("DSH_PERMISSION_MODE", mode);
        }
        if self.disable_telemetry {
            launch = launch.env("DSH_TELEMETRY_DISABLED", "1");
        }
        if let Some(dsh_home) = &self.dsh_home {
            launch = launch.env("DSH_HOME", dsh_home);
        }
        if let Some(agents_home) = &self.agents_home {
            launch = launch.env("DSH_AGENTS_HOME", agents_home);
        }
        launch
    }

    pub fn command(&self) -> Result<Command, ReadinessError> {
        self.base_launch()
            .command()
            .map_err(|error| ReadinessError::Spawn(error.to_string()))
    }

    pub fn acp_launch(&self) -> Result<AcpLaunch, ReadinessError> {
        Ok(self.base_launch())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL_DUMP: &str = include_str!("../tests/fixtures/restricted-dump.yml");

    #[test]
    fn real_runtime_dump_verifies() {
        let readiness = verify_dump(SUPPORTED_RUNTIME_VERSION, REAL_DUMP).unwrap();
        assert_eq!(readiness.runtime_version, SUPPORTED_RUNTIME_VERSION);
        assert!(readiness.active_ids.iter().any(|id| id == "acp"));
        assert!(!readiness.active_ids.iter().any(|id| id == "tool-fs"));
        assert!(readiness.active_count > 30);
    }

    #[test]
    fn unknown_active_entry_fails_closed() {
        let mut dump = REAL_DUMP.to_string();
        dump.push_str("- id: user-tool-plugin\n  name: '@deepseek-ai/dsh-tool-user'\n");
        let error = verify_dump(SUPPORTED_RUNTIME_VERSION, &dump).unwrap_err();
        assert!(matches!(error, ReadinessError::UnverifiedActive { .. }));
    }

    #[test]
    fn required_disabled_entry_fails_closed() {
        let mut dump = REAL_DUMP.to_string();
        dump = dump.replace(
            "- id: tool-fs\n  name: '@deepseek-ai/dsh-tool-fs'\n  disabled: true",
            "- id: tool-fs\n  name: '@deepseek-ai/dsh-tool-fs'\n  disabled: false",
        );
        let error = verify_dump(SUPPORTED_RUNTIME_VERSION, &dump).unwrap_err();
        assert!(matches!(error, ReadinessError::RequiredNotDisabled(id) if id == "tool-fs"));
    }

    #[test]
    fn runtime_version_mismatch_fails_closed() {
        let error = verify_dump("0.2.0", REAL_DUMP).unwrap_err();
        assert!(matches!(error, ReadinessError::RuntimeVersion(_)));
    }

    #[test]
    fn parse_quotes_and_missing_disabled() {
        let entries =
            parse_dump("- id: a\n  name: '@x/y'\n- id: b\n  name: \"@x/z\"\n  disabled: true\n")
                .unwrap();
        assert_eq!(entries[0].id, "a");
        assert_eq!(entries[0].name, "@x/y");
        assert!(!entries[0].is_disabled());
        assert_eq!(entries[1].name, "@x/z");
        assert!(entries[1].is_disabled());
    }

    #[test]
    fn materialize_writes_only_under_caller_directory() {
        let base = std::env::temp_dir().join(format!(
            "shishi-acp-profile-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let profile = materialize_profile(&base).unwrap();
        assert!(profile.patch.is_file());
        assert!(profile.launch_json.is_file());
        assert!(profile.profile_dir.join("package.json").is_file());
        assert!(profile.profile_dir.join("cordis.patch.yml").is_file());
        assert!(profile.agents_dir.is_dir());
        assert_eq!(fs::read_dir(&profile.agents_dir).unwrap().count(), 0);
        let patch = fs::read_to_string(&profile.patch).unwrap();
        for id in ["tool-fs", "tool-pwsh", "agent-instructions", "tool-skill"] {
            assert!(patch.contains(&format!("- id: {id}")), "missing {id}");
        }
        let launch: Value =
            serde_json::from_slice(&fs::read(&profile.launch_json).unwrap()).unwrap();
        assert_eq!(launch["profile"], "acp");
        assert_eq!(launch["environment"]["DSH_PERMISSION_MODE"], "read-only");
        assert_eq!(
            launch["environment"]["DSH_AGENTS_HOME"],
            profile.agents_dir.to_string_lossy().as_ref()
        );
        let staged = profile
            .analysis_launch("dsh.cmd", &base, &base)
            .acp_launch()
            .unwrap();
        assert!(staged
            .args
            .iter()
            .any(|arg| arg.to_string_lossy() == "--patch"));
        assert!(staged.env.iter().any(|(key, value)| {
            key.to_string_lossy() == "DSH_AGENTS_HOME"
                && value.to_string_lossy() == profile.agents_dir.to_string_lossy()
        }));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn analysis_launch_applies_patch_but_handling_does_not() {
        let base = std::env::temp_dir().join(format!(
            "shishi-acp-launch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&base).unwrap();
        let patch = base.join("patch.yml");
        std::fs::write(&patch, RESTRICTED_PATCH).unwrap();
        let analysis = DshLaunch::new("dsh.cmd", &patch, &base)
            .acp_launch()
            .unwrap();
        assert!(analysis
            .args
            .iter()
            .any(|arg| arg.to_string_lossy() == "--patch"));
        assert!(analysis.env.iter().any(|(key, value)| {
            key.to_string_lossy() == "DSH_PERMISSION_MODE" && value.to_string_lossy() == "read-only"
        }));
        let handling = DshLaunch::new("dsh.cmd", &patch, &base)
            .handling()
            .acp_launch()
            .unwrap();
        assert!(!handling
            .args
            .iter()
            .any(|arg| arg.to_string_lossy() == "--patch"));
        assert!(!handling
            .env
            .iter()
            .any(|(key, _)| key.to_string_lossy() == "DSH_PERMISSION_MODE"));
        assert!(!handling
            .env
            .iter()
            .any(|(key, _)| key.to_string_lossy() == "DSH_TELEMETRY_DISABLED"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn missing_profile_does_not_initialize_global_profile() {
        let base = std::env::temp_dir().join(format!(
            "shishi-acp-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let home = base.join("home");
        fs::create_dir_all(&home).unwrap();
        let patch = base.join("patch.yml");
        fs::write(&patch, RESTRICTED_PATCH).unwrap();
        let config = ReadinessConfig {
            dsh_cmd: PathBuf::from("C:/nonexistent/dsh.cmd"),
            dsh_home: home.clone(),
            agents_home: None,
            patch,
            workdir: None,
        };
        let error = check_readiness(&config).unwrap_err();
        assert!(matches!(error, ReadinessError::ProfileMissing(_)));
        let guidance = error.guidance(&config);
        assert!(guidance.contains("ACP 环境尚未初始化"));
        assert!(guidance.contains("$env:DSH_HOME="));
        assert!(guidance.contains("& 'C:/nonexistent/dsh.cmd' --profile acp --help"));
        assert!(!home.join("profiles/acp").exists());
        // A directory in place of the manifest is unreadable, not uninitialized.
        fs::create_dir_all(home.join("profiles/acp/package.json")).unwrap();
        assert!(matches!(
            check_readiness(&config),
            Err(ReadinessError::ProfileUnreadable { .. })
        ));
        fs::remove_dir(home.join("profiles/acp/package.json")).unwrap();
        fs::write(home.join("profiles/acp/package.json"), "not json").unwrap();
        assert!(matches!(
            check_readiness(&config),
            Err(ReadinessError::ProfileInvalid(_))
        ));
        fs::write(home.join("profiles/acp/package.json"), r#"{"dsh":{"profile":{"bundles":["@deepseek-ai/dsh-base","@deepseek-ai/dsh-acp-app"]}}}"#).unwrap();
        fs::remove_file(&config.patch).unwrap();
        assert!(matches!(
            check_readiness(&config),
            Err(ReadinessError::PatchMissing(_))
        ));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn initialization_guidance_quotes_paths_as_powershell_literals() {
        let config = ReadinessConfig {
            dsh_cmd: PathBuf::from("D:/Fixture's Harness/$tools/dsh.cmd"),
            dsh_home: PathBuf::from("D:/Fixture's Home/$data"),
            agents_home: None,
            patch: PathBuf::new(),
            workdir: None,
        };
        let text =
            ReadinessError::ProfileMissing(config.dsh_home.join("profiles/acp/package.json"))
                .guidance(&config);
        assert!(text.contains("$env:DSH_HOME='D:/Fixture''s Home/$data'; & 'D:/Fixture''s Harness/$tools/dsh.cmd' --profile acp --help"));
        let invalid = ReadinessError::ProfileInvalid("fixture".into()).guidance(&config);
        assert!(
            !invalid.contains("--help"),
            "never suggest initialization for an existing invalid profile"
        );
    }

    #[cfg(windows)]
    #[test]
    fn explicit_setup_initializes_once_without_prompt_or_overwriting_existing_profile() {
        let base = std::env::temp_dir().join(format!(
            "shishi-acp-setup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        let profile = materialize_profile(&base.join("assets")).unwrap();
        let home = base.join("fixture home");
        fs::create_dir_all(&home).unwrap();
        let launcher = base.join("fixture dsh.cmd");
        let script = format!("@echo off\r\nif \"%~1\"==\"--version\" (echo {SUPPORTED_RUNTIME_VERSION}& exit /b 0)\r\nif not \"%~5\"==\"--dump-config\" exit /b 9\r\necho dump>>\"%DSH_HOME%\\calls.txt\"\r\nif not exist \"%DSH_HOME%\\profiles\\acp\" mkdir \"%DSH_HOME%\\profiles\\acp\"\r\nif not exist \"%DSH_HOME%\\profiles\\acp\\package.json\" copy /y \"%~dp0manifest.json\" \"%DSH_HOME%\\profiles\\acp\\package.json\" >nul\r\ntype \"%~dp0dump.yml\"\r\n");
        fs::write(&launcher, &script).unwrap();
        let manifest = r#"{"dsh":{"profile":{"bundles":["@deepseek-ai/dsh-base","@deepseek-ai/dsh-acp-app"]}}}"#;
        fs::write(base.join("manifest.json"), manifest).unwrap();
        fs::write(base.join("dump.yml"), REAL_DUMP).unwrap();
        let config = ReadinessConfig {
            dsh_cmd: launcher.clone(),
            dsh_home: home.clone(),
            agents_home: Some(profile.agents_dir),
            patch: profile.patch,
            workdir: Some(base.clone()),
        };
        assert!(initialize_acp(&config, false).is_err());
        assert!(!home.join("profiles").exists());
        fs::write(
            &launcher,
            script.replace(SUPPORTED_RUNTIME_VERSION, "unsupported"),
        )
        .unwrap();
        assert!(matches!(
            initialize_acp(&config, true),
            Err(ReadinessError::RuntimeVersion(_))
        ));
        assert!(!home.join("profiles").exists());
        fs::write(&launcher, &script).unwrap();
        initialize_acp(&config, true).unwrap();
        let actual = home.join("profiles/acp/package.json");
        assert_eq!(fs::read_to_string(&actual).unwrap(), manifest);
        let calls = fs::read_to_string(home.join("calls.txt")).unwrap();
        assert_eq!(
            calls.lines().count(),
            2,
            "setup dump followed by read-only readiness dump"
        );
        fs::write(&actual, format!("{manifest}\n")).unwrap();
        initialize_acp(&config, true).unwrap();
        assert_eq!(
            fs::read_to_string(&actual).unwrap(),
            format!("{manifest}\n")
        );
        fs::write(&actual, "invalid existing configuration").unwrap();
        assert!(matches!(
            initialize_acp(&config, true),
            Err(ReadinessError::ProfileInvalid(_))
        ));
        assert_eq!(
            fs::read_to_string(&actual).unwrap(),
            "invalid existing configuration"
        );
        fs::remove_file(&actual).unwrap();
        fs::write(&launcher, format!("@echo off\r\nif \"%~1\"==\"--version\" (echo {SUPPORTED_RUNTIME_VERSION}& exit /b 0)\r\necho fixture failure 1>&2\r\nexit /b 3\r\n")).unwrap();
        assert!(matches!(
            initialize_acp(&config, true),
            Err(ReadinessError::DumpFailed(_))
        ));
        assert!(!actual.exists());
        fs::remove_dir_all(base).unwrap();
    }
}
