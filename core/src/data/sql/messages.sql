-- messages
-- Normalised messages. `id` is the length-prefixed message key; `content_hash`
-- is the fingerprint of text and withdrawal, and `updated_at` only advances
-- when it changes. `error_hash/error_count` belong to analysis: a message
-- whose current content failed three times is skipped until it changes.
-- Columns that queries filter or search on are real columns; the rest of
-- the serialised message is `payload`.
CREATE TABLE messages (
    id TEXT PRIMARY KEY,
    source_id TEXT NOT NULL,
    external_id TEXT NOT NULL,
    chat TEXT NOT NULL,
    time INTEGER NOT NULL,
    inserted_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    withdrawn INTEGER NOT NULL,
    content_hash TEXT NOT NULL,
    error_hash TEXT,
    error_count INTEGER NOT NULL DEFAULT 0,
    source_name TEXT NOT NULL,
    sender TEXT NOT NULL,
    "group" TEXT NOT NULL,
    text TEXT NOT NULL,
    payload TEXT NOT NULL
) STRICT;
CREATE INDEX messages_time ON messages(time, id);
CREATE INDEX messages_updated ON messages(updated_at);
CREATE INDEX messages_source ON messages(source_id, time);

-- Image references; the files stay in the source client's cache.
CREATE TABLE message_images (
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    idx INTEGER NOT NULL,
    image_id TEXT NOT NULL UNIQUE,
    path TEXT NOT NULL,
    PRIMARY KEY (message_id, idx)
) STRICT;

-- Tombstones of manually deleted messages so scans never import them again.
-- A tombstone expires with retention: past it the age filter already blocks
-- re-import.
CREATE TABLE deleted_messages (
    id TEXT PRIMARY KEY,
    deleted_at INTEGER NOT NULL,
    message_time INTEGER NOT NULL
) STRICT;
