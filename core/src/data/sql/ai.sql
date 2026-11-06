-- ai
-- Daily AI call accounting. `main` counts every purpose except the message
-- filter, which has its own row and no daily limit. A row for an older day
-- reads as zero; reservation resets it when the day changes.
CREATE TABLE usage_daily (
    day TEXT NOT NULL,
    purpose TEXT NOT NULL CHECK (purpose IN ('main', 'message_filter')),
    calls INTEGER NOT NULL DEFAULT 0,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cached_input_tokens INTEGER NOT NULL DEFAULT 0,
    cached_reports INTEGER NOT NULL DEFAULT 0,
    unreported_calls INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (day, purpose)
) STRICT, WITHOUT ROWID;

-- Analysis progress and the last deduplicated error; one row.
CREATE TABLE analysis_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    pending INTEGER NOT NULL DEFAULT 0,
    pending_since INTEGER NOT NULL DEFAULT 0,
    last_analysis_at INTEGER NOT NULL DEFAULT 0,
    last_success_at INTEGER NOT NULL DEFAULT 0,
    ai_interrupted_at INTEGER NOT NULL DEFAULT 0,
    analysis_interrupted_at INTEGER NOT NULL DEFAULT 0,
    last_error TEXT NOT NULL DEFAULT ''
) STRICT;
INSERT INTO analysis_state(id) VALUES (1);

-- Work calendar: user corrections win over published holiday data, which
-- wins over the weekend rule. `kind` is workday or holiday.
CREATE TABLE calendar_overrides (
    date TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('workday', 'holiday')),
    updated INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

-- One published holiday-cn year: `days` is a JSON object date → kind.
-- The file for year Y may carry December Y-1 dates.
CREATE TABLE holiday_years (
    year INTEGER PRIMARY KEY,
    days TEXT NOT NULL,
    fetched_at INTEGER NOT NULL
) STRICT;

-- Per business day: first moment worked, first overtime start, the end
-- eligibility seen at the end boundary, and the boundary claims. A claim id
-- is the id of the morning job or archive attempt it authorized.
CREATE TABLE work_days (
    date TEXT PRIMARY KEY,
    first_worked_at INTEGER,
    first_overtime_at INTEGER,
    end_eligibility TEXT CHECK (end_eligibility IN ('not_eligible', 'worked_before_end', 'overtime_before_end')),
    morning_claim TEXT,
    morning_claimed_at INTEGER,
    end_claim TEXT UNIQUE,
    end_claimed_at INTEGER
) STRICT, WITHOUT ROWID;

-- Live work runtime: manual overtime, the schedule of the last saved tick
-- and the monotonic clock floor that keeps a passed boundary from firing
-- again after the clock goes back. One row.
CREATE TABLE work_runtime (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    overtime_started_at INTEGER,
    overtime_day TEXT,
    last_schedule TEXT,
    clock_floor INTEGER
) STRICT;
INSERT INTO work_runtime(id) VALUES (1);

-- API keys by opaque connection reference. `secret` is the entry's own
-- DPAPI (current user) ciphertext of the UTF-8 key; nothing else is kept in
-- clear. `updated` is the time (unix ms) of the last write.
CREATE TABLE credentials (
    id TEXT PRIMARY KEY,
    secret BLOB NOT NULL,
    updated INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

-- Retained AI traces (only with `retain_ai_traces`): one row per entry of
-- a request (`input.txt`, `output.txt`, `events.jsonl`, `round-0001/…`).
-- `at` is the request's start (unix ms), for retention and comparison.
CREATE TABLE ai_traces (
    request_id TEXT NOT NULL,
    name TEXT NOT NULL,
    body TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (request_id, name)
) STRICT, WITHOUT ROWID;
CREATE INDEX ai_traces_at ON ai_traces(at);
