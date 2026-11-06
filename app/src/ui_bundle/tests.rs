#[test]
fn embedded_bundle_matches_disk_and_has_one_style_and_script() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../assets/ui");
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
