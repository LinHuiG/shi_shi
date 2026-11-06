//! `Host::image_preview`: decode one local raster through PowerShell into
//! base64. Core owns the path choice and the cache under `media/`.
use serde_json::json;
use std::{fs, io::Read, path::Path, time::Duration};

const LIMIT: usize = 1_000_000;
const FAILED: &str = "图片不可解码（仅支持本地 PNG/JPEG/GIF/BMP），或超出尺寸限制";

fn supported(signature: &[u8]) -> bool {
    signature.starts_with(b"\x89PNG\r\n\x1a\n")
        || signature.starts_with(b"\xff\xd8\xff")
        || signature.starts_with(b"GIF87a")
        || signature.starts_with(b"GIF89a")
        || signature.starts_with(b"BM")
}

fn powershell() -> Result<std::process::Command, String> {
    let system = std::env::var_os("SystemRoot").ok_or("系统目录不可用")?;
    let mut command =
        std::process::Command::new(Path::new(&system).join("System32/WindowsPowerShell/v1.0/powershell.exe"));
    command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass"]);
    Ok(command)
}

pub fn decode(data: &Path, path: &Path) -> Result<String, String> {
    let mut signature = [0u8; 8];
    fs::File::open(path).map_err(|_| "图片不可读")?.read_exact(&mut signature).map_err(|_| "图片格式无效")?;
    if !supported(&signature) {
        return Err("只支持 PNG/JPEG/GIF/BMP 图片，不读取其他文件".into());
    }
    let job = data.join("cache").join(uuid::Uuid::new_v4().simple().to_string());
    fs::create_dir_all(&job).map_err(|e| e.to_string())?;
    let script = job.join("preview.ps1");
    let result = fs::write(&script, format!("\u{feff}{}", include_str!("../../../assets/image-preview.ps1")))
        .map_err(|e| e.to_string())
        .and_then(|_| {
            let mut command = powershell()?;
            command.arg("-File").arg(&script).current_dir(&job);
            let input = json!({"path": path}).to_string();
            shishi_core::process::run(command, Some(input.as_bytes()), Duration::from_secs(20))
                .map(|(stdout, _)| stdout)
                .map_err(|_| FAILED.to_string())
        });
    let _ = fs::remove_dir_all(&job);
    let output = result?;
    let output = output.trim();
    if output.is_empty()
        || output.len() > LIMIT
        || !output.bytes().all(|c| c.is_ascii_alphanumeric() || b"+/=".contains(&c))
    {
        return Err(FAILED.into());
    }
    Ok(output.into())
}
