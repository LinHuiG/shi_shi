//! Command line of the main program.
use std::path::PathBuf;

#[derive(Debug, PartialEq)]
pub enum Mode {
    /// The desktop owner (window hidden with `background`).
    Desktop,
    PluginWorker,
    /// The structured-result MCP server of one DSH analysis round.
    AnalysisSubmit(PathBuf),
    ApplyUpdate(PathBuf),
    DataTransfer(PathBuf),
}

#[derive(Debug, PartialEq)]
pub struct Args {
    pub mode: Mode,
    /// Installation or isolated test root; defaults to the program directory.
    pub root: Option<PathBuf>,
    /// Serve the CLI without showing the window.
    pub background: bool,
    /// Started as a desktop owner that the CLI also owns (restart only).
    pub cli_owner: bool,
    pub no_jobs: bool,
    pub no_auto_update: bool,
    pub ui_dir: Option<PathBuf>,
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut out = Args {
        mode: Mode::Desktop,
        root: None,
        background: false,
        cli_owner: false,
        no_jobs: false,
        no_auto_update: false,
        ui_dir: None,
    };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().map(PathBuf::from).ok_or(format!("{name} 缺少路径"));
        match arg.as_str() {
            "--root" => out.root = Some(value("--root")?),
            "--ui-dir" => out.ui_dir = Some(value("--ui-dir")?),
            "--plugin-worker" => out.mode = Mode::PluginWorker,
            "--analysis-submit-mcp" => out.mode = Mode::AnalysisSubmit(value("--analysis-submit-mcp")?),
            "--apply-update" => out.mode = Mode::ApplyUpdate(value("--apply-update")?),
            "--data-transfer" => out.mode = Mode::DataTransfer(value("--data-transfer")?),
            "--background" => out.background = true,
            "--cli-owner" => out.cli_owner = true,
            "--no-background-jobs" => out.no_jobs = true,
            "--no-auto-update" => out.no_auto_update = true,
            other => return Err(format!("未知启动参数：{other}")),
        }
    }
    if out.no_jobs && out.root.is_none() {
        return Err("隔离验证选项必须同时指定 --root".into());
    }
    Ok(out)
}

impl Args {
    /// Arguments that start the same owner again. `gui`/`cli` are the
    /// owners at restart time: no GUI owner means a background restart.
    pub fn restart(&self, gui: bool, cli: bool) -> Vec<String> {
        let mut args = vec![];
        let mut path = |flag: &str, value: &Option<PathBuf>| {
            if let Some(v) = value {
                args.push(flag.to_string());
                args.push(v.to_string_lossy().into_owned());
            }
        };
        path("--root", &self.root);
        path("--ui-dir", &self.ui_dir);
        if self.no_jobs {
            args.push("--no-background-jobs".into());
        }
        if self.no_auto_update {
            args.push("--no-auto-update".into());
        }
        if !gui {
            args.push("--background".into());
        } else if cli {
            args.push("--cli-owner".into());
        }
        args
    }
}

#[cfg(test)]
mod tests;
