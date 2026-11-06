use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

include!("src/ui_bundle.rs");

fn main() {
    // Release packaging may pin the publication time; otherwise use the clock.
    println!("cargo:rerun-if-env-changed=SHISHI_PACKAGE_PUBLISHED_AT");
    let built = env::var("SHISHI_PACKAGE_PUBLISHED_AT")
        .map(|t| t.parse::<u64>().expect("SHISHI_PACKAGE_PUBLISHED_AT must be Unix seconds"))
        .unwrap_or_else(|_| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs());
    println!("cargo:rustc-env=SHISHI_BUILD_TIME={built}");

    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../assets");
    println!("cargo:rerun-if-changed={}", assets.join("ui").display());
    let ui = assets.join("ui");
    let html = assemble(&|path| fs::read_to_string(ui.join(path)))
        .unwrap_or_else(|e| panic!("cannot assemble assets/ui: {e}"));
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    fs::write(out.join("ui.html"), html).expect("write ui.html");

    for file in ["shishi.ico", "shishi.rc", "windows.manifest"] {
        println!("cargo:rerun-if-changed={}", assets.join(file).display());
    }
    println!("cargo:rerun-if-env-changed=WINDRES");
    println!("cargo:rerun-if-env-changed=RC");
    if env::var("CARGO_CFG_TARGET_OS").unwrap() == "windows" {
        // shishi.rc names its files relative to the workspace root.
        let workspace = assets.parent().unwrap();
        if !(windres(workspace, &out) || rc(workspace, &out)) {
            panic!("Install windres or the Windows SDK rc.exe, or set WINDRES/RC");
        }
    }
}

fn link(status: std::io::Result<std::process::ExitStatus>, target: &Path) -> bool {
    let ok = status.is_ok_and(|s| s.success());
    if ok {
        // Only the GUI program carries the icon and manifest.
        println!("cargo:rustc-link-arg-bin=shishi={}", target.display());
    }
    ok
}

fn windres(workspace: &Path, out: &Path) -> bool {
    let tool = env::var("WINDRES").unwrap_or_else(|_| "windres.exe".into());
    let target = out.join("shishi-icon.o");
    let status = Command::new(tool)
        .current_dir(workspace)
        .args(["-i", "assets/shishi.rc", "-O", "coff", "-o"])
        .arg(&target)
        .status();
    link(status, &target)
}

fn rc(workspace: &Path, out: &Path) -> bool {
    let Some(tool) = find_rc() else { return false };
    let target = out.join("shishi-icon.res");
    let status = Command::new(tool)
        .current_dir(workspace)
        .args(["/nologo", "/fo"])
        .arg(&target)
        .arg("assets/shishi.rc")
        .status();
    link(status, &target)
}

/// The Windows SDK is not always on PATH; look in the standard layout.
fn find_rc() -> Option<PathBuf> {
    if let Some(path) = env::var_os("RC").map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(path);
    }
    let roots = env::var_os("WindowsSdkDir")
        .map(|r| PathBuf::from(r).join("bin"))
        .into_iter()
        .chain([PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin")]);
    for root in roots {
        let Ok(dir) = fs::read_dir(root) else { continue };
        let mut versions: Vec<_> = dir.flatten().map(|e| e.path()).collect();
        versions.sort();
        if let Some(found) = versions.into_iter().rev().map(|v| v.join("x64/rc.exe")).find(|p| p.is_file()) {
            return Some(found);
        }
    }
    None
}
