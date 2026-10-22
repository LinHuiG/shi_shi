const taskChangeViews = new Map();
let taskChangeRequest = 0,
  taskChangeTimer = null;
function taskChangeView(id) {
  if (!taskChangeViews.has(id))
    taskChangeViews.set(id, { query: '', sort: 'desc', open: false, page: 1, all: null });
  return taskChangeViews.get(id);
}
function taskChangesSection(t) {
  const v = taskChangeView(t.id);
  return `<details class="task-changes-disclosure detail-panel" ${v.open ? 'open' : ''}><summary>事项变更记录 <span id="task-changes-count">${v.all == null ? '' : v.all + ' 条'}</span></summary><div id="task-changes" data-preserve-content></div>${detailCollapseFooter('事项变更记录')}</details>`;
}
async function renderTaskChanges(id) {
  const host = $('#task-changes');
  if (!host || selected !== id) return;
  const v = taskChangeView(id),
    request = ++taskChangeRequest;
  setHtml(
    host,
    `<div class="evidence-tools"><input id="task-changes-search" type="search" maxlength="200" aria-label="搜索事项变更记录" placeholder="搜索变更内容" value="${esc(v.query)}"><button data-task-changes-sort>${v.sort === 'asc' ? '时间 ↑ 正序' : '时间 ↓ 倒序'}</button><span class="tiny" id="task-changes-summary" role="status"></span></div><div id="task-changes-list" data-preserve-content></div><div id="task-changes-pages"></div>`,
  );
  try {
    const data = await rpc('task_changes', { id, query: v.query, sort: v.sort, page: v.page });
    if (request !== taskChangeRequest || selected !== id || $('#task-changes') !== host) return;
    const rows = Array.isArray(data?.rows) ? data.rows : [];
    v.all = data?.all || 0;
    v.page = data?.page || 1;
    $('#task-changes-count').textContent = v.all + ' 条';
    $('#task-changes-summary').textContent = v.query.trim()
      ? `匹配 ${data.total || 0} / 共 ${v.all} 条`
      : `共 ${v.all} 条`;
    setHtml(
      '#task-changes-list',
      rows.length
        ? rows
            .map(
              (a) =>
                `<article class="task-change-row" data-ui-key="task-change-${esc(a.index)}"><div class="task-change-heading"><span>${esc(actionNames[a.kind] || a.kind_label || '事项变更')}</span><time>${esc(date(a.time))}</time></div><div class="description">${esc(a.detail || '')}</div>${changeRows(a)}</article>`,
            )
            .join('')
        : `<p class="helper">${v.query.trim() ? '没有匹配的变更记录。' : '暂无保留的事项变更记录。'}</p>`,
    );
    setHtml(
      '#task-changes-pages',
      data?.pages > 1
        ? `<div class="task-change-pagination"><span class="tiny">第 ${v.page} / ${data.pages} 页</span><button data-task-changes-page="${v.page - 1}" ${v.page <= 1 ? 'disabled' : ''}>上一页</button><button data-task-changes-page="${v.page + 1}" ${v.page >= data.pages ? 'disabled' : ''}>下一页</button></div>`
        : '',
    );
  } catch (error) {
    if (request === taskChangeRequest && $('#task-changes') === host)
      setHtml(
        '#task-changes-list',
        `<p class="helper">${esc(error.message)}</p><button data-task-changes-retry>重试</button>`,
      );
  }
}
function queueTaskChangeSearch(el) {
  const v = taskChangeView(selected);
  v.query = el.value;
  v.page = 1;
  taskChangeRequest++;
  clearTimeout(taskChangeTimer);
  const id = selected;
  taskChangeTimer = setTimeout(() => void renderTaskChanges(id), 180);
}
document.addEventListener('input', (e) => {
  if (e.target.id === 'task-changes-search' && !e.isComposing) queueTaskChangeSearch(e.target);
});
document.addEventListener('compositionend', (e) => {
  if (e.target.id === 'task-changes-search') queueTaskChangeSearch(e.target);
});
document.addEventListener(
  'toggle',
  (e) => {
    if (e.target.matches('.task-changes-disclosure')) {
      taskChangeView(selected).open = e.target.open;
      if (e.target.open) void renderTaskChanges(selected);
    }
  },
  true,
);
document.addEventListener('click', (e) => {
  const button = e.target.closest('button');
  if (!button) return;
  const v = taskChangeView(selected);
  if (button.hasAttribute('data-task-changes-sort')) {
    v.sort = v.sort === 'asc' ? 'desc' : 'asc';
    v.page = 1;
    void renderTaskChanges(selected);
  }
  if (button.dataset.taskChangesPage) {
    v.page = Number(button.dataset.taskChangesPage);
    void renderTaskChanges(selected);
  }
  if (button.hasAttribute('data-task-changes-retry')) void renderTaskChanges(selected);
});
