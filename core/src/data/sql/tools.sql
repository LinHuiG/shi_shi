-- tools
-- Local command tools. `parameters` is a JSON array of {name, description};
-- `updated` is the version every save, toggle and delete checks.
CREATE TABLE tools (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    command_template TEXT NOT NULL DEFAULT '',
    parameters TEXT NOT NULL DEFAULT '[]',
    enabled INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
    updated INTEGER NOT NULL
) STRICT;
CREATE UNIQUE INDEX tools_name ON tools(name) WHERE name <> '';

-- One AI request (or local test) that may call tools; `tools` freezes the
-- definitions the request was offered, as JSON.
CREATE TABLE tool_runs (
    id TEXT PRIMARY KEY,
    request_id TEXT NOT NULL,
    purpose TEXT NOT NULL,
    source TEXT,
    task_id TEXT,
    tools TEXT NOT NULL,
    started INTEGER NOT NULL,
    finished INTEGER,
    status TEXT NOT NULL CHECK (status IN ('running', 'completed', 'failed', 'cancelled', 'interrupted',
        'rejected', 'start_failed', 'timed_out', 'nonzero_exit', 'output_error')),
    answer TEXT,
    error TEXT,
    usage TEXT
) STRICT;
CREATE INDEX tool_runs_started ON tool_runs(started);
CREATE INDEX tool_runs_task ON tool_runs(task_id);

-- One command call. The row is written with a NULL status before the
-- process starts, so an unfinished call is never replayed after a crash.
CREATE TABLE tool_calls (
    run_id TEXT NOT NULL REFERENCES tool_runs(id),
    seq INTEGER NOT NULL,
    call_id TEXT NOT NULL,
    tool_id TEXT NOT NULL,
    tool TEXT NOT NULL,
    arguments TEXT NOT NULL,
    started INTEGER NOT NULL,
    finished INTEGER,
    status TEXT,
    exit_code INTEGER,
    stdout TEXT,
    stderr TEXT,
    duration_ms INTEGER,
    truncated INTEGER,
    timed_out INTEGER,
    cancelled INTEGER,
    error TEXT,
    PRIMARY KEY (run_id, call_id),
    UNIQUE (run_id, seq)
) STRICT;
CREATE INDEX tool_calls_started ON tool_calls(started);
CREATE INDEX tool_calls_tool ON tool_calls(tool_id, started);

-- Tasks a call's result was used for, when the run itself has no task.
CREATE TABLE tool_call_tasks (
    run_id TEXT NOT NULL,
    call_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    PRIMARY KEY (run_id, call_id, task_id),
    FOREIGN KEY (run_id, call_id) REFERENCES tool_calls(run_id, call_id)
) STRICT;
CREATE INDEX tool_call_tasks_task ON tool_call_tasks(task_id);
