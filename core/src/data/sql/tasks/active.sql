-- The task page outside the archive after listed.sql, as a pages::Page
-- source keyed by list position: unarchived, unmerged tasks in ?5 scope
-- ('全部', '未完成' for every open task, a focus view or a status) and ?6 quick view ('', a focus view,
-- or 'today': open work due on the Shanghai day of ?1). Priority order is
-- level, then most recent, then number; deferred tasks keep the slots
-- that order gives them but fill them in return order.
, picked AS (
    SELECT * FROM listed
    WHERE archived = 0 AND merged_into = ''
        AND (?5 = '全部' OR CASE WHEN ?5 = '未完成' THEN status IN ('待处理', '处理中')
            WHEN ?5 IN ('待处理', '处理中', '被阻塞', '延后处理') THEN focus IS ?5 ELSE status = ?5 END)
        AND (?6 = '' OR CASE WHEN ?6 = 'today' THEN status IN ('待处理', '处理中') AND due_ms IS NOT NULL
                AND (due_ms + 28800000) / 86400000 = (?1 + 28800000) / 86400000
            ELSE focus IS ?6 END)
), ordered AS (
    SELECT *, row_number() OVER (ORDER BY level_rank, updated DESC, number) AS slot,
        row_number() OVER (PARTITION BY focus IS '延后处理' ORDER BY level_rank, updated DESC, number) AS nth_slot,
        row_number() OVER (PARTITION BY focus IS '延后处理'
            ORDER BY deferred_until, level_rank, updated DESC, number) AS nth_return
    FROM picked
)
SELECT o.*, coalesce(s.slot, o.slot) AS k0 FROM ordered o
LEFT JOIN ordered s ON o.focus IS '延后处理' AND s.focus IS '延后处理' AND s.nth_slot = o.nth_return
