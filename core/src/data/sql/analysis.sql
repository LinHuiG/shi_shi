-- analysis
-- Analysis progress per message version. A message is analysed when
-- `analyzed_hash = content_hash`; the filter has looked at it when
-- `filter_hash = content_hash`. `retry_reason` belongs to the version in
-- `error_hash` and reads as absent once the content changed.
ALTER TABLE messages ADD COLUMN analyzed_hash TEXT;
ALTER TABLE messages ADD COLUMN filter_hash TEXT;
ALTER TABLE messages ADD COLUMN retry_reason TEXT;
CREATE INDEX messages_pending ON messages(updated_at, time, id)
    WHERE withdrawn = 0 AND analyzed_hash IS NOT content_hash;

-- The early filter's last decision; one row. `early` hands pending work to
-- a full analysis before its interval; `last_run` starts the 60 s cooldown
-- and identifies the decision a reservation may consume.
CREATE TABLE filter_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    last_run INTEGER NOT NULL DEFAULT 0,
    early INTEGER NOT NULL DEFAULT 0,
    trigger TEXT NOT NULL DEFAULT '',
    reason TEXT NOT NULL DEFAULT ''
) STRICT;
INSERT INTO filter_state(id) VALUES (1);
