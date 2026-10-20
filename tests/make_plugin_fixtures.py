"""Rebuild small ZIP fixtures; payloads are inert, never loadable DLLs."""
import json
import stat
import zipfile
from pathlib import Path

root = Path(__file__).parent / "fixtures" / "plugins"
root.mkdir(parents=True, exist_ok=True)
manifest = {
    "id": "zip-fixture", "name": "ZIP fixture", "version": "1.0.0",
    "api_version": 1, "entrypoint": "plugin.dll",
    "files": ["plugin.dll", "assets/说明.txt"],
    "capabilities": ["discover", "validate", "fetch_incremental", "fetch_range", "lookup_messages"],
    "config_fields": [],
}
files = {"plugin.json": json.dumps(manifest), "plugin.dll": "inert DLL fixture",
         "assets/说明.txt": "resource", "not-declared.txt": "must not be installed"}

def write(name, entries):
    with zipfile.ZipFile(root / (name + ".zip"), "w", zipfile.ZIP_DEFLATED) as archive:
        for path, data in entries:
            info = path if isinstance(path, zipfile.ZipInfo) else zipfile.ZipInfo(path, (2026, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            archive.writestr(info, data)

write("root", files.items())
write("nested", [("示例 插件/" + key, value) for key, value in files.items()])
write("source", [("sample/Cargo.toml", '[package]\nname="sample"'),
                 ("sample/examples/provider/plugin.json", json.dumps(manifest)),
                 ("sample/src/main.rs", "fn main() {}")])
write("traversal", list(files.items()) + [("../escape.txt", "must not escape")])
write("multiple", list(files.items()) + [("another/plugin.json", files["plugin.json"])])
write("duplicate", list(files.items()) + [("PLUGIN.DLL", "duplicate")])
write("missing", [(key, value) for key, value in files.items() if key != "plugin.dll"])
write("reserved", list(files.items()) + [("aux.txt", "device")])
link = zipfile.ZipInfo("link", (2026, 1, 1, 0, 0, 0))
link.create_system = 3
link.external_attr = (stat.S_IFLNK | 0o777) << 16
write("symlink", list(files.items()) + [(link, "../outside")])
(root / "corrupt.zip").write_bytes(b"This is not a ZIP archive")
