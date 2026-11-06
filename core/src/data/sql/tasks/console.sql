-- `tasks_query` after ranked.sql: filter, order, count and page. ?4
-- archived (NULL = any), ?5 status, ?6 focus (Task::focus), ?7 lowercase
-- text, ?8 limit, ?9 offset. Deferred tasks keep the slots the priority
-- order gives them but fill them in return order. Always one row: `total`,
-- plus the page row's columns (NULL id on an empty page).
, picked AS (
    SELECT * FROM (
        SELECT *, task_focus(archived, merged_into, status, deferred_until, blocker, waiting_for, ?1) AS focus
        FROM ranked
    )
    WHERE (?4 IS NULL OR archived = ?4) AND (?5 IS NULL OR status = ?5) AND (?6 IS NULL OR focus = ?6)
        AND (?7 = '' OR has_text(title, ?7) OR has_text(description, ?7) OR instr('#' || number, ?7) > 0
            OR id = ?7)
), ordered AS (
    SELECT *, row_number() OVER (ORDER BY level_rank, updated DESC, id) AS slot,
        row_number() OVER (PARTITION BY focus IS '延后处理' ORDER BY level_rank, updated DESC, id) AS nth_slot,
        row_number() OVER (PARTITION BY focus IS '延后处理'
            ORDER BY deferred_until, level_rank, updated DESC, id) AS nth_return
    FROM picked
), placed AS (
    SELECT o.*, coalesce(s.slot, o.slot) AS position FROM ordered o
    LEFT JOIN ordered s ON o.focus IS '延后处理' AND s.focus IS '延后处理' AND s.nth_slot = o.nth_return
)
SELECT n.total, p.id, p.number, p.title, p.status, p.focus, p.due, p.deferred_until, p.updated, p.archived,
    p.merged_into, (SELECT json_group_array(json_object('kind', kind, 'value', value))
        FROM (SELECT kind, value FROM task_tags WHERE task_id = p.id ORDER BY kind, value)) AS tags
FROM (SELECT count(*) AS total FROM picked) n
LEFT JOIN (SELECT * FROM placed ORDER BY position LIMIT ?8 OFFSET ?9) p ON 1
ORDER BY p.position
