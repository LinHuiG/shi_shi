-- Connection-local views, created on every open.

-- A task with its tags and evidence as JSON, as `Task` reads it.
CREATE TEMP VIEW IF NOT EXISTS task_rows AS SELECT *,
    (SELECT json_group_array(json_object('kind', kind, 'value', value))
        FROM (SELECT kind, value FROM task_tags g WHERE g.task_id = t.id ORDER BY kind, value)) AS tags,
    json_object(
        'direct', json((SELECT json_group_array(message_id) FROM (SELECT message_id FROM task_evidence e
            WHERE e.task_id = t.id AND e.role = 'direct' ORDER BY position, message_id))),
        'background', json((SELECT json_group_array(message_id) FROM (SELECT message_id FROM task_evidence e
            WHERE e.task_id = t.id AND e.role = 'background' ORDER BY position, message_id))),
        'completion', json((SELECT json_group_array(message_id) FROM (SELECT message_id FROM task_evidence e
            WHERE e.task_id = t.id AND e.role = 'completion' ORDER BY position, message_id)))) AS evidence
FROM tasks t;
