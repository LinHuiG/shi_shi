-- handling
-- The session ShiShi started in Codex or DSH for a task, as JSON
-- (`crate::handling::Run`). The conversation itself lives in that client.
CREATE TABLE handling_runs (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks(id),
    status TEXT NOT NULL,
    created INTEGER NOT NULL,
    data TEXT NOT NULL
) STRICT;
CREATE INDEX handling_runs_task ON handling_runs(task_id, created);
