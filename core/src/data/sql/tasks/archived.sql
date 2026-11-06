-- The archive page after listed.sql: archived, unmerged, generated tasks
-- as a pages::Page source keyed by level, recency and id. ?5/?6 the
-- `updated` range.
SELECT *, level_rank AS k0, -updated AS k1, id AS k2 FROM listed
WHERE archived = 1 AND merged_into = '' AND generation = '' AND updated >= ?5 AND updated <= ?6
