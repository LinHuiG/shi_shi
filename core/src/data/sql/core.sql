-- Small typed values that have no better home: last analysis trigger time,
-- daily call counters and similar scalars. Values are JSON text.
CREATE TABLE kv (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;

-- Runtime log shown on the logs page and in the status ring. `detail` is the
-- event's JSON object; `day` is the UTC+8 business day for daily paging.
CREATE TABLE activity (
    id INTEGER PRIMARY KEY,
    time INTEGER NOT NULL,
    day TEXT NOT NULL,
    kind TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX activity_day ON activity(day, id);
