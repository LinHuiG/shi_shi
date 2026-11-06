-- Prefix of the task list queries: every task with `level_rank`, its
-- effective level (tags::effective_tags + tags::priority_rank). That is the
-- best catalogued level tag, raised to the second/third level when a
-- followed deadline of open work is within 2 hours/a day; 10000 without
-- one. ?1 now (ms), ?2 the level group name, ?3 its values (JSON array).
WITH ranked AS (
    SELECT t.*, coalesce(CASE WHEN base IS NULL THEN near WHEN near IS NULL THEN base
        ELSE min(base, near) END, 10000) AS level_rank
    FROM (
        SELECT t.*,
            (SELECT min(CAST(j.key AS INTEGER)) FROM task_tags g JOIN json_each(?3) j ON j.value = g.value
             WHERE g.task_id = t.id AND g.kind = ?2) AS base,
            CASE WHEN json_array_length(?3) > 0 AND t.follow_deadline = 1
                    AND t.status IN ('待处理', '处理中') AND t.due_ms IS NOT NULL THEN
                CASE WHEN t.due_ms - ?1 <= 7200000 THEN min(1, json_array_length(?3) - 1)
                     WHEN t.due_ms - ?1 <= 86400000 THEN min(2, json_array_length(?3) - 1) END
            END AS near
        FROM tasks t
    ) t
)
