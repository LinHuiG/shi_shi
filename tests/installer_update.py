"""Windows installer regression, using isolated data and no real AI/network.

python tests/installer_update.py <new-setup.exe> [<previous-setup.exe>]
Requires makensis on PATH. The optional previous package exercises the full
handoff/install/restart; its test wrapper adds /TEST so HKCU is untouched.
"""
import ctypes
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parent.parent
HIDDEN = subprocess.CREATE_NO_WINDOW
PROCESSES = []
KERNEL = ctypes.WinDLL("kernel32", use_last_error=True)
KERNEL.OpenProcess.argtypes = [ctypes.c_uint, ctypes.c_int, ctypes.c_uint]
KERNEL.OpenProcess.restype = ctypes.c_void_p
KERNEL.WaitForSingleObject.argtypes = [ctypes.c_void_p, ctypes.c_uint]
KERNEL.CloseHandle.argtypes = [ctypes.c_void_p]


def wait_for(condition, seconds=60):
    until = time.monotonic() + seconds
    while time.monotonic() < until:
        if condition():
            return
        time.sleep(0.1)
    raise AssertionError("timed out waiting for installer/backend")


def cli(root, *args, data=None):
    command = [str(root / "shishi.exe"), "--root", str(root), "--no-background-jobs", *args]
    if data is not None:
        command += ["--input", "-"]
    p = subprocess.run(command, input=json.dumps(data) if data is not None else None,
                       capture_output=True, encoding="utf-8", timeout=45, creationflags=HIDDEN)
    assert p.returncode == 0, p.stderr
    value = json.loads(p.stdout)
    assert value["ok"], value
    return value["data"]


def install(setup, root, update=False):
    command = subprocess.list2cmdline([str(setup), "/S", "/TEST"] + (["/UPDATE"] if update else []))
    # NSIS /D consumes the unquoted remainder, including spaces.
    return subprocess.run(command + " /D=" + str(root), timeout=120, creationflags=HIDDEN).returncode


def start(root, worker=False):
    args = ["--plugin-worker"] if worker else ["--root", str(root), "--background", "--no-background-jobs", "--no-auto-update"]
    p = subprocess.Popen([str(root / "shishi.exe"), *args], stdin=subprocess.PIPE,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, creationflags=HIDDEN)
    PROCESSES.append(p)
    if not worker:
        wait_for(lambda: route_pid(root) == p.pid)
        cli(root, "backend", "status")
    return p


def route_pid(root):
    path = root / "data/local-service.json"
    return json.loads(path.read_text())["pid"] if path.exists() else None


def stop(root):
    # A route file can survive process exit; wait for the process itself.
    process = KERNEL.OpenProcess(0x100000, False, cli(root, "backend", "status")["pid"])
    assert process, ctypes.WinError(ctypes.get_last_error())
    try:
        cli(root, "backend", "stop", "--force")
        assert KERNEL.WaitForSingleObject(process, 45_000) == 0, "backend did not exit"
    finally:
        KERNEL.CloseHandle(process)


def tasks(root):
    with sqlite3.connect(root / "data/shishi.db") as db:
        assert db.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
        return db.execute("SELECT id,title FROM tasks ORDER BY id").fetchall()


def seed(root):
    cli(root, "call", "edit", data={"task": {"title": "installer fixture", "description": "synthetic data", "status": "待处理"}})
    stop(root)
    return tasks(root)


def shutdown_error(work, released):
    root = work / f"shutdown error {released}"
    root.mkdir()
    (root / "shishi.exe").write_bytes(b"synthetic locked executable")
    script = (ROOT / "installer/setup.nsi").read_text(encoding="utf-8")
    body = script[script.index("!macro Free "):script.index("Function .onInit")]
    begin = body.index(" System::Call 'rstrtmgr::RmStartSession")
    end = body.index(" ${EndIf}", body.index(" System::Call 'rstrtmgr::RmEndSession")) + len(" ${EndIf}\n")
    # Keep WaitFree's real file checks; only the shutdown API result is synthetic.
    shutdown = " System::Call 'kernel32::CloseHandle(p r9)'\n" if released else ""
    body = body[:begin] + shutdown + " StrCpy $0 351\n" + body[end:]
    source, setup = root / "check.nsi", root / "check.exe"
    source.write_text(f'''Unicode true
!include "LogicLib.nsh"
Name "Synthetic shutdown check"
RequestExecutionLevel user
SilentInstall silent
OutFile "{setup}"
InstallDir "{root}"
Var UpdateMode
{body}
Section
 StrCpy $UpdateMode 1
 System::Call 'kernel32::CreateFileW(w "$INSTDIR\\shishi.exe", i 0xC0000000, i 0, p 0, i 3, i 0, p 0) p.r9'
 ${{If}} $9 == -1
  SetErrorLevel 9
  Quit
 ${{EndIf}}
 Call WaitFree
 SetErrorLevel 0
SectionEnd
''', encoding="utf-8")
    subprocess.run(["makensis", "-V2", "-INPUTCHARSET", "UTF8", str(source)], check=True)
    result = subprocess.run([str(setup)], creationflags=HIDDEN).returncode
    assert result == (0 if released else 2), f"shutdown error: released={released}, installer exit={result}"
    print(f"PASS: shutdown error with released={released} uses the actual file lock", flush=True)


def force_exit(setup, work):
    root, other = work / "active install", work / "other install"
    assert install(setup, root) == install(setup, other) == 0
    saved = seed(root)
    owner = start(root)
    worker = start(root, worker=True)
    unrelated = start(other)
    assert install(setup, root) == 2, "ordinary silent install must not force exit"
    assert all(p.poll() is None for p in [owner, worker, unrelated])
    assert install(setup, root, update=True) == 0
    for p in [owner, worker]:
        p.wait(10)
    assert unrelated.poll() is None, "another installation must keep running"
    assert tasks(root) == saved
    assert not list(root.rglob("*json_source*.dll"))
    print("PASS: force exit closes the owner and worker; other installation and data survive", flush=True)


def handoff(setup, previous, work):
    root = work / "automatic update"
    assert install(previous, root) == 0
    saved = seed(root)
    expected = (work / "active install/install-info.txt").read_text().splitlines()[0].removeprefix("Shishi ")
    wrapper = work / "test-update.exe"
    source = work / "test-update.nsi"
    # The real helper launches silently without /TEST. This test-only wrapper
    # forwards to the unchanged shipping installer with registration disabled.
    source.write_text(f'''Unicode true
RequestExecutionLevel user
SilentInstall silent
OutFile "{wrapper}"
Section
 ExecWait '"{setup}" /S /UPDATE /TEST /D={root}' $0
 SetErrorLevel $0
SectionEnd
''', encoding="utf-8")
    subprocess.run(["makensis", "-V2", "-INPUTCHARSET", "UTF8", str(source)], check=True)
    raw = wrapper.read_bytes()
    digest = hashlib.sha256(raw).hexdigest()
    cache = root / "data/updates"
    cache.mkdir(exist_ok=True)
    shutil.copyfile(wrapper, cache / f"Shishi-Setup-{expected}-{digest[:12]}.exe")
    package = {"version": expected, "tag": f"v{expected}", "size": len(raw), "digest": digest,
               "published": int(time.time() * 1000) + 60_000,
               "url": f"https://github.com/LinHuiG/shi_shi/releases/download/v{expected}/Shishi-Setup-{expected}.exe"}
    (cache / "ready.json").write_text(json.dumps(package), encoding="utf-8")
    parent = start(root)
    worker = start(root, worker=True)
    assert cli(root, "call", "update_install", data={}) is True
    parent.wait(45)
    wait_for(lambda: (root / "install-info.txt").read_text().splitlines()[0] == "Shishi " + expected
             and route_pid(root) not in [None, parent.pid], 120)
    worker.wait(10)
    wait_for(lambda: route_pid(root) not in [None, parent.pid])
    route = json.loads((root / "data/local-service.json").read_text())
    assert route["pid"] != parent.pid
    assert not (cache / "ready.json").exists(), "successful update must clear its ready marker"
    assert cli(root, "backend", "status")["gui"] is False, "background update must restart in the background"
    assert cli(root, "call", "state", data={})["version"] == expected
    assert tasks(root) == saved
    assert not (cache / "last-error.json").exists()
    stop(root)
    print("PASS: update handoff exits the old backend, installs and restarts the new version with preserved data", flush=True)


def main():
    assert os.name == "nt", "Windows only"
    setup = Path(sys.argv[1]).resolve(strict=True)
    work = Path(tempfile.mkdtemp(prefix="installer update ", dir=ROOT / "build"))
    print(f"Isolated fixtures: {work}", flush=True)
    try:
        shutdown_error(work, False)
        shutdown_error(work, True)
        force_exit(setup, work)
        if len(sys.argv) > 2:
            handoff(setup, Path(sys.argv[2]).resolve(strict=True), work)
    finally:
        for p in PROCESSES:
            if p.poll() is None:
                p.kill()
            p.wait(10)
            p.stdin.close()


if __name__ == "__main__":
    main()
