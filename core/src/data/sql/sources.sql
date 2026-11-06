-- sources
-- Message sources. `updated` is the optimistic-concurrency version, strictly
-- increasing as max(now, old+1); `cursor` is the plugin's private progress,
-- committed only together with a successful incremental scan.
CREATE TABLE sources (
    id TEXT PRIMARY KEY,
    plugin_id TEXT NOT NULL,
    account_key TEXT NOT NULL DEFAULT '',
    name TEXT NOT NULL,
    enabled INTEGER NOT NULL,
    config TEXT NOT NULL,
    cursor TEXT,
    last_scan INTEGER NOT NULL DEFAULT 0,
    last_error TEXT NOT NULL DEFAULT '',
    updated INTEGER NOT NULL,
    position INTEGER NOT NULL
) STRICT;
