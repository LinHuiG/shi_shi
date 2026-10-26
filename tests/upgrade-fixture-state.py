"""Inspect only the caller's isolated upgrade fixture, across both store layouts."""
import json
from pathlib import Path
import sqlite3
import sys

directory = Path(sys.argv[2]).resolve()
if (directory / "synthetic-preserved.txt").read_text(encoding="utf-8") != "Synthetic upgrade preservation":
    raise RuntimeError("Not an isolated upgrade fixture")
operation = sys.argv[1]
database = directory / "state.sqlite3"
legacy = directory / "state.json"
task_id = "synthetic-retired-task-field"


def connection(write=False):
    return sqlite3.connect(database.as_uri() + ("?mode=rw" if write else "?mode=ro"), uri=True)


if operation == "config":
    if database.exists():
        with connection() as db:
            name = db.execute("SELECT config FROM metadata WHERE id=1").fetchone()[0]
        if Path(name).name != name or not name.endswith(".json"):
            raise RuntimeError("Invalid fixture settings reference")
        config = json.loads((directory / "settings" / name).read_text(encoding="utf-8-sig"))
    else:
        config = json.loads(legacy.read_text(encoding="utf-8-sig"))["config"]
    # 1.1.30 predates the optional SS tool catalog. Only these known empty
    # defaults may be added; every existing value is still compared exactly.
    config.setdefault("tool_call_prompt", "")
    config.setdefault("tools", [])
    print(json.dumps(config, sort_keys=True, separators=(",", ":")))
elif operation == "seed":
    task = dict(id=task_id, title="Synthetic retained task", description="Keep this description",
                status="待处理", blocker="Keep blocker", waiting_for="Synthetic owner",
                next_step="Retired task text", steps=[dict(id="old-step", title="Keep step history", next_step="Retired step text")])
    if database.exists():
        with connection(True) as db:
            ordinal = db.execute("SELECT coalesce(max(ordinal),-1)+1 FROM tasks").fetchone()[0]
            db.execute("INSERT INTO tasks(id,ordinal,payload) VALUES(?,?,?)", (task_id, ordinal, json.dumps(task)))
    else:
        state = json.loads(legacy.read_text(encoding="utf-8-sig"))
        state.setdefault("tasks", []).append(task)
        legacy.write_text(json.dumps(state), encoding="utf-8")
elif operation == "verify":
    with connection() as db:
        row = db.execute("SELECT payload FROM tasks WHERE id=?", (task_id,)).fetchone()
    assert row, "Synthetic task disappeared"
    task = json.loads(row[0])
    assert task["title"] == "Synthetic retained task"
    assert task["description"] == "Keep this description"
    assert task["blocker"] == "Keep blocker"
    assert task["waiting_for"] == "Synthetic owner"
    assert "next_step" not in task
    assert task["steps"][0]["title"] == "Keep step history"
    assert "next_step" not in task["steps"][0]
    print("PASS retired task/step fields removed; other task fields and step history preserved")
else:
    raise RuntimeError("Unknown fixture operation")
