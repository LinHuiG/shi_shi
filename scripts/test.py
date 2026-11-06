"""Offline regression entry point.

    python scripts/test.py

Runs every crate's tests (synthetic data, mock providers, no real model),
checks the UI scripts parse, and off Windows type-checks the Windows build.
"""
import os
import platform
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CRATES = ["shishi-core", "shishi", "shishi-api", "shishi-acp", "shishi-plugin-sdk", "shishi-json-source", "shishi-migrate"]


def run(cmd, env=None):
    print("+", " ".join(cmd), flush=True)
    if subprocess.run(cmd, cwd=ROOT, env=env).returncode:
        sys.exit(1)


def main():
    tests = ["cargo", "test", "--offline", "--locked"]
    for crate in CRATES:
        tests += ["-p", crate]
    run(tests)
    run(["cargo", "fmt", "--all", "--check"])
    if shutil.which("node"):
        for script in sorted((ROOT / "assets/ui").rglob("*.js")):
            run(["node", "--check", str(script.relative_to(ROOT))])
        run(["node", "--test", "examples/dsh-workspace-bridge/index.test.js"])
    else:
        print("node not found: UI scripts and the DSH bridge not checked")
    if platform.system() != "Windows":
        env = dict(os.environ, WINDRES=os.environ.get("WINDRES", "x86_64-w64-mingw32-windres"))
        triple = ["--target", "x86_64-pc-windows-gnu"]
        run(["cargo", "check", "--offline", "--locked", "-p", "shishi", *triple], env)
        run(["cargo", "check", "--offline", "--locked", "-p", "shishi-core", "--tests", *triple], env)
        run(["cargo", "check", "--offline", "--locked", "-p", "shishi-migrate", *triple], env)
    print("all checks passed")


if __name__ == "__main__":
    main()
