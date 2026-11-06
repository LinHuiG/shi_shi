-- The task page's rows after ranked.sql: the list columns with `focus`
-- (task_focus, as Task::focus), filtered as the page filters them. ?4 the
-- catalog ([{name, values}]), ?7 lowercase text, ?8 tag filters ({kind:
-- [values]}, NULL for none), ?9 level values (NULL for any). A tag is
-- effective (tags::effective_tags) when it is a catalogued non-level tag of
-- the task, or the level `level_rank` names.
, listed AS (
    SELECT id, number, title, status, generation, manual, blocker, waiting_for, due, due_ms, follow_deadline,
        deferred_until, archived, merged_into, status_changed_at, updated, level_rank,
        task_focus(archived, merged_into, status, deferred_until, blocker, waiting_for, ?1) AS focus
    FROM ranked r
    WHERE (?7 = '' OR has_text(title || ' ' || description || ' ' || blocker || ' #' || number, ?7))
        AND NOT EXISTS (
            SELECT 1 FROM json_each(?8) f
            WHERE f.type = 'array' AND json_array_length(f.value) > 0 AND NOT EXISTS (
                SELECT 1 FROM json_each(f.value) v
                WHERE f.key = ?2 AND r.level_rank < 10000
                        AND json_extract(?3, '$[' || r.level_rank || ']') = v.value
                    OR f.key <> ?2 AND EXISTS (
                        SELECT 1 FROM task_tags g, json_each(?4) c, json_each(c.value, '$.values') cv
                        WHERE g.task_id = r.id AND g.kind = f.key AND g.value = v.value
                            AND json_extract(c.value, '$.name') = g.kind AND cv.value = g.value)))
        AND (json_type(?9) IS NOT 'array' OR json_array_length(?9) = 0 OR r.level_rank < 10000 AND EXISTS (
            SELECT 1 FROM json_each(?9) v WHERE json_extract(?3, '$[' || r.level_rank || ']') = v.value))
)
