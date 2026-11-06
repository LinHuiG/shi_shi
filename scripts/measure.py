"""Static size report: product code, tests, dependencies, binaries, installer.

    python scripts/measure.py [source-dir]

Lines are non-blank. Test code: `tests/` dirs, `tests.rs`, `testing.rs`,
`test_support.rs`, `src/bin/mock_*`, files (with their submodule dirs) declared
by `#[cfg(test)] mod x;`, and inline `#[cfg(test)] mod x { ... }` blocks, which
end at the first `}` line at the `mod` line's indentation. The rest of a file
stays product. Runtime memory and CPU need a Windows host and are measured
separately.
"""
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(sys.argv[1] if len(sys.argv) > 1 else Path(__file__).resolve().parent.parent)
SKIP = {"vendor", "target", "build", ".git", "node_modules"}
TEST_FILES = {"tests.rs", "testing.rs", "test_support.rs"}
CFG_TEST = re.compile(r"^\s*#\[cfg\(test\)\]\s*(.*)$")
MOD = re.compile(r"^(\s*)(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*([;{])")


def files():
    for dirpath, dirs, names in os.walk(ROOT):
        dirs[:] = [d for d in dirs if d not in SKIP]
        for name in names:
            yield Path(dirpath) / name


def lines(path):
    return [l for l in path.read_text(encoding="utf-8", errors="replace").splitlines() if l.strip()]


def is_test(rel):
    parts = rel.parts
    return "tests" in parts or rel.name in TEST_FILES or (rel.name.startswith("mock_") and "bin" in parts)


def split_tests(path, body):
    """(product lines, test module lines, files of external test modules)."""
    # A module declared in lib.rs, main.rs or mod.rs sits beside it, otherwise
    # in a directory named after the file.
    home = path.parent if path.name in {"lib.rs", "main.rs", "mod.rs"} else path.with_suffix("")
    product, tests, external = [], 0, []
    i = 0
    while i < len(body):
        cfg = CFG_TEST.match(body[i])
        start = i if cfg and cfg.group(1) else i + 1
        line = start < len(body) and re.sub(r"#\[cfg\(test\)\]\s*", "", body[start], count=1)
        module = cfg and line and MOD.match(line)
        if not module:
            product.append(body[i])
            i += 1
            continue
        end = start
        if module.group(3) == ";":
            external.append(home / module.group(2))
        elif line.count("{") != line.count("}"):
            end = next(j for j in range(start + 1, len(body)) if body[j].rstrip() == module.group(1) + "}")
        tests += end + 1 - i
        i = end + 1
    return product, tests, external


def main():
    rust, external = {}, set()
    for path in files():
        if path.suffix == ".rs" and not is_test(path.relative_to(ROOT)):
            rust[path] = split_tests(path, lines(path))
            external.update(rust[path][2])

    def declared_test(path):
        return path.with_suffix("") in external or any(p in external for p in path.parents)

    product, tests = {}, {}
    for path in files():
        rel = path.relative_to(ROOT)
        ext = path.suffix
        if ext not in {".rs", ".js", ".css", ".html", ".sql", ".cjs", ".ps1"}:
            continue
        kind = "rust" if ext == ".rs" else "ui" if rel.parts[:2] == ("assets", "ui") else ext[1:]
        if is_test(rel) or declared_test(path):
            tests[kind] = tests.get(kind, 0) + len(lines(path))
            continue
        body = lines(path)
        if ext == ".rs":
            body, inline, _ = rust[path]
            tests["rust-inline"] = tests.get("rust-inline", 0) + inline
        product[kind] = product.get(kind, 0) + len(body)
    report = {"source": str(ROOT), "product": product, "product_total": sum(product.values()),
              "tests": tests, "tests_total": sum(tests.values())}
    tree = shutil.which("cargo") and subprocess.run(["cargo", "tree", "--offline", "-p", "shishi", "-e", "normal", "--prefix", "none",
                           "--target", "x86_64-pc-windows-gnu"], cwd=ROOT, capture_output=True, text=True)
    if tree and tree.returncode == 0:
        crates = {l.split(" (")[0].strip() for l in tree.stdout.splitlines() if l.strip()}
        report["dependencies_windows"] = len({c for c in crates if not c.startswith("shishi")})
    target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target")) / "x86_64-pc-windows-gnu/release"
    exe = target / "shishi.exe"
    if exe.is_file():
        report[exe.name] = exe.stat().st_size
    for setup in sorted((ROOT / "build").glob("Shishi-Setup-*.exe")):
        report[setup.name] = setup.stat().st_size
    print(json.dumps(report, ensure_ascii=False, indent=1))


if __name__ == "__main__":
    main()
