-- tasks
-- Tasks. `updated` is the version: every write is
-- `UPDATE … WHERE id=? AND updated=?` and checks one changed row.
CREATE TABLE tasks (
    id TEXT PRIMARY KEY,
    number INTEGER NOT NULL UNIQUE,
    title TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    summary TEXT NOT NULL DEFAULT '',
    summary_fallback INTEGER NOT NULL DEFAULT 1,
    content_updated_at INTEGER NOT NULL DEFAULT 0,
    summary_updated_at INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL CHECK (status IN ('待处理', '处理中', '已完成', '无需处理', '已取消')),
    blocker TEXT NOT NULL DEFAULT '',
    waiting_for TEXT NOT NULL DEFAULT '',
    due TEXT NOT NULL DEFAULT '',
    due_ms INTEGER,
    follow_deadline INTEGER NOT NULL DEFAULT 1,
    deadline_changed INTEGER NOT NULL DEFAULT 0,
    deferred_until INTEGER NOT NULL DEFAULT 0,
    resume_processing INTEGER NOT NULL DEFAULT 0,
    generation TEXT NOT NULL DEFAULT '',
    generation_error TEXT NOT NULL DEFAULT '',
    manual INTEGER NOT NULL DEFAULT 0,
    archived INTEGER NOT NULL DEFAULT 0,
    archived_at INTEGER NOT NULL DEFAULT 0,
    archive_note TEXT NOT NULL DEFAULT '',
    merged_into TEXT NOT NULL DEFAULT '',
    reason TEXT NOT NULL DEFAULT '',
    schedule_id TEXT NOT NULL DEFAULT '',
    scheduled_for TEXT NOT NULL DEFAULT '',
    suggested_project_id TEXT NOT NULL DEFAULT '',
    suggested_project_reason TEXT NOT NULL DEFAULT '',
    decision_context TEXT,
    status_changed_at INTEGER NOT NULL,
    created INTEGER NOT NULL,
    updated INTEGER NOT NULL
) STRICT;
CREATE INDEX tasks_live ON tasks(archived, merged_into, status);
CREATE INDEX tasks_resume ON tasks(deferred_until) WHERE resume_processing = 1;
CREATE UNIQUE INDEX tasks_schedule_day ON tasks(schedule_id, scheduled_for) WHERE schedule_id <> '';

CREATE TABLE task_tags (
    task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (task_id, kind, value)
) STRICT;

-- Evidence keeps its message id after the message is pruned or deleted;
-- the task only stops showing that row.
CREATE TABLE task_evidence (
    task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    message_id TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('direct', 'background', 'completion')),
    position INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (task_id, message_id, role)
) STRICT;
CREATE INDEX task_evidence_message ON task_evidence(message_id);

-- The change log of every domain, ordered by `seq`; the page addresses rows
-- by `token`. before/after are full JSON snapshots.
CREATE TABLE task_audit (
    seq INTEGER PRIMARY KEY,
    token TEXT NOT NULL UNIQUE,
    time INTEGER NOT NULL,
    kind TEXT NOT NULL,
    task_id TEXT NOT NULL DEFAULT '',
    detail TEXT NOT NULL DEFAULT '',
    change_summary TEXT,
    before TEXT,
    after TEXT
) STRICT;
CREATE INDEX task_audit_task ON task_audit(task_id, time, seq);
CREATE INDEX task_audit_time ON task_audit(time);

-- Recurring task templates. `last_generated_on` is the UTC+8 day of the
-- last generation; generation and this marker commit together.
CREATE TABLE schedules (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    tags TEXT NOT NULL DEFAULT '[]',
    frequency TEXT NOT NULL CHECK (frequency IN ('daily', 'weekly')),
    weekdays TEXT NOT NULL DEFAULT '[]',
    time TEXT NOT NULL,
    enabled INTEGER NOT NULL,
    active_from INTEGER NOT NULL,
    last_generated_on TEXT NOT NULL DEFAULT '',
    created INTEGER NOT NULL,
    updated INTEGER NOT NULL
) STRICT;

-- Tag catalog history. The last row is the current catalog; `id` is the
-- optimistic lock the page sends back as `expected`.
CREATE TABLE tag_versions (
    seq INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    created INTEGER NOT NULL,
    action TEXT NOT NULL,
    reason TEXT NOT NULL,
    priority_kind TEXT NOT NULL,
    groups TEXT NOT NULL,
    assignments TEXT NOT NULL
) STRICT;
INSERT INTO tag_versions(id, created, action, reason, priority_kind, groups, assignments)
VALUES (lower(hex(randomblob(16))), CAST(strftime('%s', 'now') AS INTEGER) * 1000, 'initial',
    '统一标签目录；预设可维护，兼容旧分类数据', '事项等级',
    '[{"name":"事项类型","values":["问题排查","紧急事项","日常事务"]},{"name":"事项等级","values":["紧急","高","普通","低"]}]',
    '[]');

CREATE TABLE tag_proposals (
    seq INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL,
    value TEXT NOT NULL,
    reason TEXT NOT NULL,
    evidence TEXT NOT NULL DEFAULT '[]',
    excerpts TEXT NOT NULL DEFAULT '[]',
    status TEXT NOT NULL DEFAULT 'pending',
    created INTEGER NOT NULL,
    archived_at INTEGER NOT NULL DEFAULT 0
) STRICT;

-- Learned judgement rules. `evidence` is a JSON list of task_audit seq.
CREATE TABLE suggestions (
    seq INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL,
    rule TEXT NOT NULL,
    evidence TEXT NOT NULL DEFAULT '[]',
    active INTEGER NOT NULL DEFAULT 0,
    decision TEXT NOT NULL DEFAULT 'pending',
    created INTEGER NOT NULL,
    updated INTEGER NOT NULL,
    archived_at INTEGER NOT NULL DEFAULT 0
) STRICT;

-- One row per accepted feedback round on a suggestion of any kind.
CREATE TABLE advice_revisions (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    suggestion_id TEXT NOT NULL,
    time INTEGER NOT NULL,
    feedback TEXT NOT NULL,
    reason TEXT NOT NULL,
    before TEXT NOT NULL,
    after TEXT NOT NULL
) STRICT;
