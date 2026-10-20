// Assembles the single-page UI from `assets/ui/manifest.txt`: styles into one
// `<style>` and scripts, in order, into one `<script>` (shared global scope).
// Included by build.rs for the embedded release bundle and compiled into the
// app for `--ui-dir`, so both paths produce byte-identical output. Plain `//`
// comments only: this file is also pulled in with `include!`.

/// `read` resolves a manifest-relative path to its UTF-8 text.
pub fn assemble(read: &dyn Fn(&str) -> std::io::Result<String>) -> std::io::Result<String> {
    let invalid = |message: String| std::io::Error::new(std::io::ErrorKind::InvalidData, message);
    let mut styles = String::new();
    let mut scripts = String::new();
    for line in read("manifest.txt")?.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (kind, path) = line
            .split_once(' ')
            .ok_or_else(|| invalid(format!("manifest line without a kind: {line}")))?;
        let text = read(path.trim())?.replace("\r\n", "\n");
        let target = match kind {
            "style" => &mut styles,
            "script" => &mut scripts,
            _ => return Err(invalid(format!("unknown manifest kind: {kind}"))),
        };
        target.push_str(&text);
        if !text.ends_with('\n') {
            target.push('\n');
        }
    }
    let shell = read("index.html")?.replace("\r\n", "\n");
    for marker in ["<!--STYLES-->", "/*SCRIPTS*/"] {
        if shell.matches(marker).count() != 1 {
            return Err(invalid(format!(
                "index.html must contain {marker} exactly once"
            )));
        }
    }
    Ok(shell
        .replacen("<!--STYLES-->", &format!("<style>\n{styles}</style>"), 1)
        .replacen("/*SCRIPTS*/", scripts.trim_end(), 1))
}

#[cfg(test)]
mod tests {
    #[test]
    fn embedded_bundle_matches_disk_and_has_one_style_and_script() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/ui");
        let disk = super::assemble(&|path| std::fs::read_to_string(dir.join(path))).unwrap();
        let embedded = include_str!(concat!(env!("OUT_DIR"), "/ui.html"));
        assert_eq!(disk, embedded, "build.rs and --ui-dir must agree");
        for marker in ["<!--STYLES-->", "/*SCRIPTS*/", "/*EXTENSIONS*/"] {
            assert!(!embedded.contains(marker), "{marker} left in bundle");
        }
        assert_eq!(embedded.matches("<style>").count(), 1);
        assert_eq!(embedded.matches("<script>").count(), 1);
        let bad = super::assemble(&|path| match path {
            "manifest.txt" => Ok("module x.js".into()),
            _ => Ok(String::new()),
        });
        assert!(bad.is_err());
    }
}
