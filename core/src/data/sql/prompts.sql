-- prompts
-- System prompt versions. The latest row (by rowid) of a target is the
-- policy in effect; `config/prompts.json` adds the built-in versions.
CREATE TABLE prompt_versions (
    id TEXT PRIMARY KEY,
    target TEXT NOT NULL,
    content TEXT NOT NULL,
    previous_id TEXT NOT NULL DEFAULT '',
    action TEXT NOT NULL,
    reason TEXT NOT NULL DEFAULT '',
    created INTEGER NOT NULL
) STRICT;
CREATE INDEX prompt_versions_target ON prompt_versions(target, created);

-- Prompt rewrite suggestions. Nothing creates new ones any more; retry and
-- adopt remain for existing rows. `evidence` and `evidence_ids` are JSON.
CREATE TABLE prompt_reviews (
    id TEXT PRIMARY KEY,
    day TEXT NOT NULL DEFAULT '',
    created INTEGER NOT NULL,
    status TEXT NOT NULL,
    base_version TEXT NOT NULL DEFAULT '',
    base_prompt TEXT NOT NULL DEFAULT '',
    evidence TEXT NOT NULL DEFAULT '{}',
    proposed_prompt TEXT NOT NULL DEFAULT '',
    reason TEXT NOT NULL DEFAULT '',
    evidence_ids TEXT NOT NULL DEFAULT '[]',
    error TEXT NOT NULL DEFAULT '',
    last_attempt INTEGER NOT NULL DEFAULT 0,
    archived_at INTEGER NOT NULL DEFAULT 0
) STRICT;

-- AI memory: current text plus a revision for optimistic saves. There is no
-- user-facing history.
CREATE TABLE memory (
    kind TEXT PRIMARY KEY CHECK (kind IN ('temporary', 'archived')),
    text TEXT NOT NULL DEFAULT '',
    revision INTEGER NOT NULL DEFAULT 0,
    updated INTEGER NOT NULL DEFAULT 0
) STRICT;
INSERT INTO memory(kind) VALUES ('temporary'), ('archived');
