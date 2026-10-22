use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

include!("src/ui_bundle.rs");

fn main() {
    // Packaging supplies a fresh nonce so even an unchanged source tree gets
    // a newly compiled timestamp. An explicit packaging override supports a
    // requested publication time without changing ordinary build timestamps.
    println!("cargo:rerun-if-env-changed=SHISHI_PACKAGE_BUILD_NONCE");
    println!("cargo:rerun-if-env-changed=SHISHI_PACKAGE_PUBLISHED_AT");
    println!("cargo:rerun-if-changed=src");
    let clock = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let built = env::var("SHISHI_PACKAGE_PUBLISHED_AT")
        .map(|text| {
            text.parse::<u64>()
                .expect("SHISHI_PACKAGE_PUBLISHED_AT must be Unix seconds")
        })
        .unwrap_or(clock);
    assert!(built > 0, "publication time must be positive");
    println!("cargo:rustc-env=SHISHI_BUILD_TIME={built}");
    bundle_ui();
    println!("cargo:rerun-if-changed=assets/shishi.ico");
    println!("cargo:rerun-if-changed=assets/shishi.rc");
    println!("cargo:rerun-if-changed=assets/windows.manifest");
    println!("cargo:rerun-if-env-changed=WINDRES");
    println!("cargo:rerun-if-env-changed=RC");
    if env::var("CARGO_CFG_TARGET_OS").unwrap() == "windows" {
        let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
        if try_windres(&out) || try_rc(&out) {
            return;
        }
        panic!("Install windres or the Windows SDK rc.exe, or set WINDRES/RC");
    }
}

/// Embed the UI as one HTML document; `assets/ui/manifest.txt` defines it.
fn bundle_ui() {
    println!("cargo:rerun-if-changed=assets/ui");
    let dir = Path::new("assets/ui");
    let html = assemble(&|path| fs::read_to_string(dir.join(path)))
        .unwrap_or_else(|e| panic!("cannot assemble assets/ui: {e}"));
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("ui.html");
    fs::write(out, html).expect("write ui.html");
}

fn try_windres(out: &Path) -> bool {
    let tool = env::var("WINDRES").unwrap_or_else(|_| "windres.exe".into());
    let target = out.join("shishi-icon.o");
    let status = Command::new(&tool)
        .args(["-i", "assets/shishi.rc", "-O", "coff", "-o"])
        .arg(&target)
        .status();
    match status {
        Ok(status) if status.success() => {
            println!("cargo:rustc-link-arg={}", target.display());
            true
        }
        _ => false,
    }
}

fn try_rc(out: &Path) -> bool {
    let Some(rc) = find_rc() else {
        return false;
    };
    let target = out.join("shishi-icon.res");
    let status = Command::new(rc)
        .args(["/nologo", "/fo"])
        .arg(&target)
        .arg("assets/shishi.rc")
        .status();
    match status {
        Ok(status) if status.success() => {
            println!("cargo:rustc-link-arg={}", target.display());
            true
        }
        _ => false,
    }
}

/// MSVC builds can use the Windows SDK resource compiler when GNU windres is
/// not installed. The SDK is not always on PATH, so inspect the standard layout.
fn find_rc() -> Option<PathBuf> {
    if let Ok(value) = env::var("RC") {
        let path = PathBuf::from(value);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Ok(root) = env::var("WindowsSdkDir") {
        let path = PathBuf::from(root).join("bin");
        if let Some(found) = search_rc(&path) {
            return Some(found);
        }
    }
    search_rc(Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin"))
}

fn search_rc(root: &Path) -> Option<PathBuf> {
    let mut versions = fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    versions.sort();
    versions
        .into_iter()
        .rev()
        .map(|version| version.join("x64/rc.exe"))
        .find(|path| path.is_file())
}
