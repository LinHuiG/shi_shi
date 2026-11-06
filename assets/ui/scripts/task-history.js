const taskRecordViews = new Map();
let taskRecordPanel = null,
  taskRecordTimer = 0,
  taskRecordPointer = null;
function recordView(id, kind) {
  const key = id + ':' + kind,
    view = taskRecordViews.get(key) || {
      id,
      archived: !!taskById(id)?.archived,
      query: '',
      sort: 'desc',
      start: '',
      end: '',
      top: 0,
      opened: new Set(),
    };
  taskRecordViews.delete(key);
  taskRecordViews.set(key, view);
  while (taskRecordViews.size > 64) taskRecordViews.delete(taskRecordViews.keys().next().value);
  return view;
}
function taskRecordButtons(t) {
  return `<div class="task-record-buttons">${[
    ['evidence', '消息依据', t.evidence_count, 'evidence'],
    ['changes', '事项变更记录', t.change_count, 'logs'],
  ]
    .map(
      ([kind, label, count, glyph]) =>
        `<button data-task-record="${kind}" aria-haspopup="dialog" aria-expanded="${taskRecordPanel?.id === t.id && taskRecordPanel.kind === kind}">${icon(glyph)}${label}${Number.isFinite(count) ? `<small data-record-count="${kind}">${count}</small>` : ''}</button>`,
    )
    .join('')}</div>`;
}
function rememberTaskRecord() {
  const p = taskRecordPanel;
  if (!p?.loaded) return;
  const v = recordView(p.id, p.kind),
    list = p.el.querySelector('.task-record-content');
  v.top = list.scrollTop;
  v.opened = new Set(
    [...list.querySelectorAll('details[open]')].map((d) => d.closest('[data-ui-key]')?.dataset.uiKey).filter(Boolean),
  );
}
function closeTaskRecord(focus = false) {
  const p = taskRecordPanel;
  if (!p) return;
  rememberTaskRecord();
  clearTimeout(taskRecordTimer);
  p.feed?.dispose();
  p.feed = null;
  clearHtml(p.el.querySelector('.task-record-content'));
  clearHtml(p.el);
  p.el.remove();
  taskRecordPanel = null;
  document.querySelectorAll('[data-task-record]').forEach((b) => b.setAttribute('aria-expanded', 'false'));
  if (focus && p.trigger?.isConnected) p.trigger.focus({ preventScroll: true });
}
function positionTaskRecord() {
  const el = taskRecordPanel?.el;
  if (!el) return;
  const rect = $('#task-detail')?.getBoundingClientRect(),
    width = Math.min(800, innerWidth - 24),
    height = Math.min(650, innerHeight - 96);
  Object.assign(el.style, {
    width: width + 'px',
    height: Math.max(220, height) + 'px',
    left:
      Math.max(
        12,
        Math.min(innerWidth - width - 12, rect ? rect.left + (rect.width - width) / 2 : (innerWidth - width) / 2),
      ) + 'px',
    top: Math.max(12, (innerHeight - height) / 2) + 'px',
  });
}
function syncTaskRecordPanel() {
  if (taskRecordPanel && (page !== 'tasks' || selected !== taskRecordPanel.id || !taskById(taskRecordPanel.id)))
    closeTaskRecord();
  for (const [key, view] of taskRecordViews) {
    const task = taskById(view.id);
    if (task) view.archived = !!task.archived;
    else if (!view.archived) taskRecordViews.delete(key);
  }
  const p = taskRecordPanel;
  if (!p) return;
  positionTaskRecord();
  document
    .querySelectorAll('[data-task-record]')
    .forEach((b) => b.setAttribute('aria-expanded', String(b.dataset.taskRecord === p.kind)));
  const revision = JSON.stringify([state.task_history_revision, state.messages_revision]);
  if (p.revision !== revision) {
    p.revision = revision;
    void loadTaskRecord();
  }
}
function openTaskRecord(kind, trigger) {
  if (!['evidence', 'changes'].includes(kind)) return;
  if (taskRecordPanel?.id === selected && taskRecordPanel.kind === kind) {
    closeTaskRecord(true);
    return;
  }
  closeTaskRecord();
  const id = selected,
    v = recordView(id, kind),
    title = kind === 'evidence' ? '消息依据' : '事项变更记录',
    el = document.createElement('section');
  el.id = 'task-record-window';
  el.className = 'task-record-window';
  el.setAttribute('role', 'dialog');
  el.setAttribute('aria-modal', 'false');
  el.setAttribute('aria-labelledby', 'task-record-title');
  el.innerHTML = `<header class="task-record-header"><div><h3 id="task-record-title">${title}</h3><span class="tiny">${esc(taskById(id)?.title || '')}</span></div><button class="quiet" data-record-close aria-label="关闭${title}">${icon('close')}</button></header><div class="task-record-tools"><input id="task-record-search" type="search" maxlength="200" aria-label="搜索${title}" placeholder="${kind === 'evidence' ? '搜索消息、发送人或来源' : '搜索变更内容'}" value="${esc(v.query)}"><button data-record-sort aria-label="切换时间顺序">${v.sort === 'asc' ? '时间 ↑ 正序' : '时间 ↓ 倒序'}</button><details class="record-range"><summary>时间区间<span data-record-range-active>${v.start || v.end ? ' · 已选' : ''}</span></summary><div><label>开始（北京时间）<input id="record-start" type="datetime-local" value="${esc(v.start)}"></label><label>结束（含所选分钟）<input id="record-end" type="datetime-local" value="${esc(v.end)}"></label><p id="record-range-error" class="error-box" role="alert"></p><div class="row"><button data-record-range-clear>清除</button><button data-record-range-apply>应用</button></div></div></details></div><div class="task-record-content" tabindex="0" aria-label="${title}内容"><p class="helper">正在读取…</p></div><footer class="task-record-footer"><span role="status" data-record-summary></span></footer>`;
  document.body.append(el);
  taskRecordPanel = {
    id,
    kind,
    el,
    trigger,
    loaded: false,
    revision: JSON.stringify([state.task_history_revision, state.messages_revision]),
  };
  positionTaskRecord();
  motionAnimate(el, [
    { opacity: 0, transform: 'scale(.985)' },
    { opacity: 1, transform: 'scale(1)' },
  ]);
  syncTaskRecordPanel();
  void loadTaskRecord();
  el.querySelector('input').focus({ preventScroll: true });
}
function recordRange(v) {
  const read = (value, end) => {
    if (!value) return null;
    const time = Date.parse(value + ':00+08:00');
    if (!Number.isFinite(time) || time < 0) throw Error('请选择有效的时间区间');
    return time + (end ? 59999 : 0);
  };
  const since = read(v.start, false),
    until = read(v.end, true);
  if (since != null && until != null && since > until) throw Error('开始时间不能晚于结束时间');
  return { since, until };
}
function recordMessage(m) {
  return `<article class="record-message" data-ui-key="message-${esc(m.id)}" data-feed-key="${esc(m.id)}"><div class="record-avatar" aria-hidden="true">${esc([...(m.sender || '消')][0])}</div><div class="record-message-main"><div class="record-message-heading"><b>${esc(m.sender || '未知发送人')}</b><span>${esc(m.source_name || '历史来源')}${m.group ? ' · ' + esc(m.group) : ''}</span><time>${esc(date(m.time))}</time></div>${messageBody(m.text || '[' + m.kind + ']')}${messageImages(m)}</div></article>`;
}
function recordChange(a) {
  return `<article class="event" data-ui-key="change-${esc(a.id)}" data-feed-key="${esc(a.id)}"><div class="event-dot"></div><div class="event-content"><div class="event-heading"><b>${esc(actionNames[a.kind] || a.kind_label || '事项变更')}</b><time>${esc(date(a.time))}</time></div><div class="event-description">${esc(a.change_summary || a.detail || '')}</div>${changeRows(a)}</div></article>`;
}
async function loadTaskRecord(reset = false) {
  const p = taskRecordPanel;
  if (!p) return;
  clearTimeout(taskRecordTimer);
  const v = recordView(p.id, p.kind),
    list = p.el.querySelector('.task-record-content');
  if (reset) {
    p.feed?.dispose();
    p.feed = null;
    v.top = 0;
    v.opened.clear();
    p.loaded = false;
    setHtml(list, '');
    list.scrollTop = 0;
  } else rememberTaskRecord();
  if (!p.feed) {
    const params = {
      [p.kind === 'evidence' ? 'task_id' : 'id']: p.id,
      query: v.query,
      sort: v.sort,
      ...recordRange(v),
    };
    p.feed = pageFeed({
      root: list,
      key: (row) => row.id,
      read: (args) => rpc(p.kind === 'evidence' ? 'evidence_query' : 'task_changes', { ...params, ...args }),
      alive: () => taskRecordPanel === p && page === 'tasks' && selected === p.id,
      render: (feed) => {
        rememberTaskRecord();
        const rows = feed.rows,
          retries = p.kind === 'evidence' ? rows.filter((m) => state.analysis_retries?.[m.id]) : [];
        setHtml(
          list,
          `${retries.length ? `<p class="error-box">${retries.length} 条依据待重试 · ${esc(state.analysis_retries[retries[0].id])}</p>` : ''}${rows.length ? (p.kind === 'changes' ? `<div class="timeline">${rows.map(recordChange).join('')}</div>` : rows.map(recordMessage).join('')) : !feed.loading && !feed.error ? `<p class="record-empty helper">${feed.data?.all ? '没有符合筛选条件的记录。' : '暂无保留的记录。'}</p>` : ''}${pageFeedStatus(feed)}`,
        );
        if (!p.loaded && feed.data) {
          for (const d of list.querySelectorAll('details'))
            d.open = v.opened.has(d.closest('[data-ui-key]')?.dataset.uiKey);
          list.scrollTop = v.top;
          p.loaded = true;
        }
        list.setAttribute('aria-busy', String(feed.loading));
        p.el.querySelector('[data-record-summary]').textContent = feed.data
          ? `已加载 ${rows.length} / ${feed.data.total} 条${feed.data.total !== feed.data.all ? ' · 保留 ' + feed.data.all + ' 条' : ''}`
          : feed.error
            ? '读取失败'
            : '正在读取…';
        const count = document.querySelector(`[data-record-count="${p.kind}"]`);
        if (count && feed.data) count.textContent = feed.data.all;
      },
    });
  }
  await p.feed.load(p.feed.page > 0);
}

document.addEventListener('click', (e) => {
  const b = e.target.closest('button');
  if (!b) return;
  if (b.dataset.taskRecord) {
    openTaskRecord(b.dataset.taskRecord, b);
    return;
  }
  const p = taskRecordPanel;
  if (!p || !p.el.contains(b)) return;
  const v = recordView(p.id, p.kind);
  if (b.hasAttribute('data-record-close')) {
    closeTaskRecord(true);
    return;
  }
  if (b.hasAttribute('data-record-sort')) {
    v.sort = v.sort === 'asc' ? 'desc' : 'asc';
    b.textContent = v.sort === 'asc' ? '时间 ↑ 正序' : '时间 ↓ 倒序';
    void loadTaskRecord(true);
  }
  if (b.hasAttribute('data-record-range-apply') || b.hasAttribute('data-record-range-clear')) {
    const clear = b.hasAttribute('data-record-range-clear'),
      start = $('#record-start'),
      end = $('#record-end'),
      draft = { start: clear ? '' : start.value, end: clear ? '' : end.value };
    try {
      recordRange(draft);
    } catch (error) {
      $('#record-range-error').textContent = error.message;
      return;
    }
    Object.assign(v, draft);
    start.value = v.start;
    end.value = v.end;
    $('#record-range-error').textContent = '';
    p.el.querySelector('.record-range').open = false;
    p.el.querySelector('[data-record-range-active]').textContent = v.start || v.end ? ' · 已选' : '';
    void loadTaskRecord(true);
  }
});
function searchTaskRecord(e) {
  if (e.target.id !== 'task-record-search' || e.isComposing || !taskRecordPanel) return;
  const p = taskRecordPanel,
    v = recordView(p.id, p.kind);
  v.query = e.target.value;
  p.feed?.dispose();
  p.feed = null;
  v.top = 0;
  v.opened.clear();
  p.loaded = false;
  clearTimeout(taskRecordTimer);
  taskRecordTimer = setTimeout(() => {
    if (taskRecordPanel === p) void loadTaskRecord(true);
  }, 180);
}
document.addEventListener('input', searchTaskRecord);
document.addEventListener('compositionend', searchTaskRecord);
document.addEventListener(
  'pointerdown',
  (e) => {
    taskRecordPointer = {
      x: e.clientX,
      y: e.clientY,
      outside: !!taskRecordPanel && !taskRecordPanel.el.contains(e.target),
      id: e.pointerId,
    };
  },
  true,
);
document.addEventListener(
  'pointerup',
  (e) => {
    const down = taskRecordPointer,
      p = taskRecordPanel;
    taskRecordPointer = null;
    if (
      p &&
      down?.outside &&
      down.id === e.pointerId &&
      !p.el.contains(e.target) &&
      !e.target.closest('[data-task-record]') &&
      $('#overlay').hidden &&
      Math.hypot(down.x - e.clientX, down.y - e.clientY) < 5
    )
      closeTaskRecord();
  },
  true,
);
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && taskRecordPanel && $('#overlay').hidden) {
    e.preventDefault();
    closeTaskRecord(true);
  }
});
window.addEventListener('resize', positionTaskRecord);
