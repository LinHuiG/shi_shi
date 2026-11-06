use super::*;

fn run(entries: &[(&str, &[u8])]) -> (PathBuf, Result<PathBuf>) {
    let dir = crate::testing::temp_dir("zip");
    let package = dir.join("p.zip");
    std::fs::write(&package, build(entries)).unwrap();
    let dest = dir.join("out");
    std::fs::create_dir(&dest).unwrap();
    let result = extract(&package, &dest);
    (dest, result)
}

#[test]
fn extracts_root_and_nested_manifests() {
    let (dest, r) = run(&[("plugin.json", b"{}"), ("bin/a.dll", b"MZ")]);
    assert_eq!(r.unwrap(), dest.join("plugin.json"));
    assert_eq!(std::fs::read(dest.join("bin/a.dll")).unwrap(), b"MZ");
    let (dest, r) = run(&[("p/plugin.json", b"{}"), ("p/a.dll", b"MZ")]);
    assert_eq!(r.unwrap(), dest.join("p").join("plugin.json"));
}

#[test]
fn rejects_unsafe_shapes() {
    assert_eq!(run(&[("a.dll", b"")]).1.unwrap_err().0, ONE_MANIFEST);
    assert_eq!(run(&[("plugin.json", b""), ("x/PLUGIN.json", b"")]).1.unwrap_err().0, ONE_MANIFEST);
    assert_eq!(run(&[("a/b/plugin.json", b"")]).1.unwrap_err().0, MANIFEST_DEPTH);
    for bad in ["../plugin.json", "a/../plugin.json", "CON.txt", "com1", "a /x", "a.", "x:y", "a\\b"] {
        let err = run(&[("plugin.json", b""), (bad, b"")]).1.unwrap_err().0;
        assert!(err.starts_with("插件 ZIP 解压或校验失败："), "{bad}: {err}");
    }
    let err = run(&[("plugin.json", b""), ("A.dll", b""), ("a.DLL", b"")]).1.unwrap_err().0;
    assert!(err.contains("重复"));
    let dir = crate::testing::temp_dir("zip-bad");
    std::fs::write(dir.join("x.zip"), b"PK\x03\x04garbage").unwrap();
    assert!(extract(&dir.join("x.zip"), &dir).unwrap_err().0.starts_with("插件 ZIP 解压或校验失败"));
}

#[test]
fn corrupted_entry_fails_crc() {
    let mut bytes = build(&[("plugin.json", b"{\"id\":\"x\"}")]);
    let dir = crate::testing::temp_dir("zip-crc");
    // Flip a byte of the deflated data right after the local header.
    bytes[30 + "plugin.json".len()] ^= 0xFF;
    std::fs::write(dir.join("x.zip"), &bytes).unwrap();
    std::fs::create_dir(dir.join("o")).unwrap();
    assert!(extract(&dir.join("x.zip"), &dir.join("o")).is_err());
}
