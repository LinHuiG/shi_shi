// The task list pages through active_tasks_query, or archived_tasks_query on
// the archive tab; the backend filters, sorts and counts. `selected` stays
// listed through the response's pinned row while it passes the filters.
let taskFeed = null,
  taskFeedFilter = '',
  taskFeedRevision = '';
function releaseTaskFeed() {
  taskFeed?.dispose();
  taskFeed = null;
  taskFeedFilter = '';
  taskFeedRevision = '';
  clearHtml($('.task-list'));
}
function taskFeedRequest() {
  const filters = { query, tag_filters: structuredClone(tagSelection()), priorities: [...prioritySelection()] };
  return tab === '归档'
    ? ['archived_tasks_query', filters]
    : ['active_tasks_query', { ...filters, scope: tab, view: taskFocusViews[quickView] || quickView }];
}
function taskFeedRows() {
  const rows = taskFeed?.rows || [],
    pinned = taskFeed?.data?.selected;
  return pinned && pinned.id === selected && !rows.some((t) => t.id === pinned.id) ? [...rows, pinned] : rows;
}
function taskFeedStatus() {
  if (!taskFeed) return '<p class="helper" role="status">正在读取事项…</p>';
  // A list that fits one page reads as before paging; longer ones show progress.
  if (tab !== '归档' && taskFeed.done && !taskFeed.loading && !taskFeed.error && taskFeed.page <= 1) return '';
  return `${!taskFeed.rows.length && taskFeed.done && tab === '归档' ? '<p class="helper">没有符合条件的归档事项。</p>' : ''}${pageFeedStatus(taskFeed)}`;
}
async function renderTasks() {
  const [method, params] = taskFeedRequest(),
    filter = JSON.stringify([method, params]);
  const revision = JSON.stringify([
    filter,
    state.tasks_revision,
    state.task_focus_counts,
    taskClockDay,
    state.tag_state.groups,
  ]);
  const root = $('.task-list');
  // An empty result shows the empty state instead of the list; reopen the
  // feed when the filters or the tasks change.
  if (!taskFeed || taskFeedFilter !== filter || (!root && revision !== taskFeedRevision)) {
    releaseTaskFeed();
    taskFeedFilter = filter;
    renderTaskList();
    const list = $('.task-list');
    taskFeed = pageFeed({
      root: list,
      key: (t) => t.id,
      read: (p) => rpc(method, { ...params, selected: selected || undefined, ...p }),
      alive: () => page === 'tasks' && list.isConnected,
      render: () => renderTaskList(),
    });
  }
  // A selection made elsewhere (a notification, a new task) loads its row.
  const missing = selected && taskFeed.data && !taskFeed.loading && !taskFeedRows().some((t) => t.id === selected);
  if (revision !== taskFeedRevision || missing) {
    taskFeedRevision = revision;
    await taskFeed.load(taskFeed.page > 0);
  } else if (root) taskFeed.paint();
  else renderTaskList();
}
