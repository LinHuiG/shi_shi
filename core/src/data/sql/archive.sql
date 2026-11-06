-- archive
-- Message ids cited by archived memory and archive rollback material. They
-- stay out of retention like the evidence of open tasks.
CREATE TABLE message_refs (
    owner TEXT NOT NULL,
    message_id TEXT NOT NULL,
    PRIMARY KEY (owner, message_id)
) STRICT;
CREATE INDEX message_refs_message ON message_refs(message_id);

-- One archive run. `work_end` attempts are queued by the work-boundary
-- transaction together with their claim; a manual run is created running.
-- Failures are final: nothing retries an attempt automatically.
CREATE TABLE archive_attempts (
    seq INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    trigger TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'succeeded', 'failed')),
    business_day TEXT NOT NULL DEFAULT '',
    created INTEGER NOT NULL,
    started INTEGER NOT NULL DEFAULT 0,
    finished INTEGER NOT NULL DEFAULT 0,
    error TEXT NOT NULL DEFAULT '',
    report_id TEXT NOT NULL DEFAULT '',
    reused INTEGER NOT NULL DEFAULT 0,
    batches INTEGER NOT NULL DEFAULT 0,
    processed INTEGER NOT NULL DEFAULT 0,
    remaining INTEGER NOT NULL DEFAULT 0,
    archived_tasks INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE INDEX archive_attempts_status ON archive_attempts(status);

-- Morning reports queued by the work boundary; run once, never retried.
CREATE TABLE morning_jobs (
    seq INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    business_day TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'succeeded', 'failed')),
    error TEXT NOT NULL DEFAULT '',
    created INTEGER NOT NULL,
    finished INTEGER NOT NULL DEFAULT 0
) STRICT;

-- Reports, newest by seq first, at most 60. `body` is the report object;
-- its sections hold `report_items` keys instead of rows.
CREATE TABLE reports (
    seq INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    time INTEGER NOT NULL,
    body TEXT NOT NULL
) STRICT;

-- Frozen display rows of report sections, keyed "{section}:{sha256}" so an
-- unchanged row is stored once. Rows no report cites are removed.
CREATE TABLE report_items (
    key TEXT PRIMARY KEY,
    row TEXT NOT NULL
) STRICT;

-- Archive versions: the classify prompt (by version id), the enabled rules
-- and the tag catalog after an archive commit or a rollback.
CREATE TABLE archive_groups (
    seq INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    created INTEGER NOT NULL,
    action TEXT NOT NULL CHECK (action IN ('archive', 'rollback')),
    restored_from TEXT NOT NULL DEFAULT '',
    attempt_id TEXT NOT NULL DEFAULT '',
    report_id TEXT NOT NULL DEFAULT '',
    reason TEXT NOT NULL DEFAULT '',
    prompt_version TEXT NOT NULL DEFAULT '',
    active_rules TEXT NOT NULL DEFAULT '[]',
    rule_version INTEGER NOT NULL DEFAULT 0,
    tag_groups TEXT NOT NULL DEFAULT '[]',
    tag_version TEXT NOT NULL DEFAULT '',
    archived_version INTEGER NOT NULL DEFAULT 0
) STRICT;
