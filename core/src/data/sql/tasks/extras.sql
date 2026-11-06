-- What a task list row adds for each task id in ?1 (JSON array): evidence
-- still shown, visible history rows, whether a tool call names it, and its
-- stored tags.
SELECT j.value AS id,
    (SELECT count(DISTINCT e.message_id) FROM task_evidence e
        JOIN messages m ON m.id = e.message_id WHERE e.task_id = j.value AND m.withdrawn = 0) AS evidence_count,
    (SELECT count(*) FROM task_audit a
        WHERE a.task_id = j.value AND substr(a.kind, 1, 14) <> 'archive_memory') AS change_count,
    EXISTS (SELECT 1 FROM tool_call_tasks c WHERE c.task_id = j.value) AS has_tool_calls,
    (SELECT json_group_array(json_object('kind', kind, 'value', value))
        FROM (SELECT kind, value FROM task_tags WHERE task_id = j.value ORDER BY kind, value)) AS tags
FROM json_each(?1) j
