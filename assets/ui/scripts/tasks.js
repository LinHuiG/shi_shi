const reviewPages = ['reports', 'schedules', 'learning'];
let reviewSection = 'reports';
function reviewTabs() {
  const tabs = [
    ['reports', '报表与归档'],
    ['schedules', '定时事项'],
    ['learning', '迭代建议'],
  ];
  return `<div class="pills" role="tablist" aria-label="回顾内容" style="margin-bottom:16px">${tabs.map(([p, label]) => `<button role="tab" data-page="${p}" aria-selected="${page === p}" class="${page === p ? 'active' : ''}">${label}</button>`).join('')}</div>`;
}
function pendingMessageLabel() {
  return state.pending ? `${state.pending} 条等待分析` : '无等待消息';
}

// The bottom-bar phase. Its AI-allowed rule is the top bar's: work_state
// working or overtime (`Status::ai_allowed`).
function statusPhase() {
  const elapsed = state.ai_started ? Math.max(0, Math.floor((Date.now() - state.ai_started) / 1000)) : 0;
  return state.ai_cancelling
    ? '正在中断 AI…'
    : state.ai_busy
      ? `<span class="spin"></span>${state.analyzing ? '正在分析 ' + state.analyzing + ' 条消息' : 'AI 正在处理'}${elapsed ? ' · 已用 ' + elapsed + ' 秒' : ''}`
      : state.analyzing
        ? `<span class="spin"></span>正在准备 ${state.analyzing} 条消息`
        : state.busy
          ? '<span class="spin"></span>正在读取消息'
          : state.ai_interrupted_at
            ? '本次 AI 已中断'
            : !['working', 'overtime'].includes(state.work_state?.status)
              ? '非消息自动分析时段'
              : '就绪';
}

// The 1s tick while AI runs: elapsed seconds, speed and the live line only;
// lists, counts and the sidebar wait for a real state change.
function aiClock() {
  if (!state) return;
  updateAiLiveDetail();
  updateStatusPhase(statusPhase());
}

function status() {
  if (!state) return;
  syncLogUsage();
  updateAiLiveDetail();
  const lastAi = (state.activity || []).find((a) => ['ai_success', 'ai_failed', 'ai_cancelled'].includes(a.kind));
  const contextIncomplete = lastAi?.detail?.context_usage?.budget_exhausted === true;
  setHtml('#work-status', workStatusBar());
  const refresh = $('#top-actions [data-action="scan"], #top-actions [data-action="refresh-messages"]');
  if (refresh) {
    const busy = !!(scanPending || state.busy || (refresh.dataset.action === 'scan' && state.ai_busy));
    refresh.disabled = busy;
    refresh.setAttribute('aria-busy', String(busy));
    refresh.querySelector('.refresh-glyph')?.classList.toggle('rotating', busy);
  }
  if (!$('#scan-controls [data-action="toggle-scan"]'))
    setHtml(
      '#scan-controls',
      '<button class="mini-switch" role="switch" aria-label="增量消息扫描" data-action="toggle-scan" title="启停增量消息扫描"><i></i></button><button class="scan-label quiet" data-action="scan-options" title="打开扫描与 AI 分析设置"></button>',
    );
  const scanSwitch = $('#scan-controls [data-action="toggle-scan"]');
  scanSwitch.classList.toggle('on', !!state.config.enabled);
  scanSwitch.setAttribute('aria-checked', String(!!state.config.enabled));
  scanSwitch.disabled = scanToggleBusy;
  motionText(
    $('#scan-controls [data-action="scan-options"]'),
    `${state.config.enabled ? '自动' : '暂停'} · ${state.config.interval_minutes}分钟`,
  );
  const counts = state.task_focus_counts;
  // Keep an expired wakeup until the clock tick handles it, even if another
  // state update arrives first. A cancelled deferral costs at most one refresh.
  taskNextDeferralAt = Math.min(taskNextDeferralAt, state.next_deferral_at ?? Infinity);
  const n = state.open_task_count;
  setHtml(
    '#sidebar-context',
    `<div class="side-section-label">关注</div>${Object.entries(taskFocusViews)
      .map(
        ([view, label]) =>
          `<button class="side-view" data-view="${view}">${label}<span>${counts[label]}</span></button>`,
      )
      .join('')}`,
  );
  motionText($('#nav-count'), n || '');
  if (page === 'tasks') motionText($('#page-subtitle'), `${n} 项未完成`);
  const scanLabel = state.config.enabled ? '自动扫描' : '扫描暂停',
    pendingLabel = state.analyzing ? state.analyzing + ' 条分析中' : pendingMessageLabel();
  setHtml(
    '#side-status',
    `<span class="dot ${state.config.enabled ? '' : 'off'}"></span><span>${scanLabel}</span><span class="side-pending">${pendingLabel}</span>`,
  );
  $('#side-status').title = `${scanLabel} · ${pendingLabel}`;
  const phase = statusPhase();
  updateStatusBar(
    `<span class="phase-label" data-preserve-content></span>${contextIncomplete ? '<button class="quiet context-warning" data-page="logs" title="已采用分析结果，但补查覆盖不完整">补查覆盖不完整</button>' : ''}${state.ai_busy ? `<button class="quiet" data-action="ai-interrupt" data-request-id="${esc(state.ai_request_id || '')}" ${state.ai_cancelling ? 'disabled' : ''}>${state.ai_cancelling ? '正在中断…' : '中断本次 AI'}</button><button class="ai-live quiet" data-ai-live title="查看 AI 实时进度" aria-label="查看 AI 实时进度"><span class="live-dot"></span><span data-live-line data-preserve-content></span></button>` : `<span class="status-time">最近事项分析 ${esc(state.last_analysis_success_at ? date(state.last_analysis_success_at) : '尚未分析')}</span>`}${state.ai_busy ? '<span class="token-speed status-speed" data-status-speed data-preserve-content hidden></span>' : ''}${state.last_error ? `<button class="status-error quiet" data-page="logs" title="${esc(state.last_error)}">处理失败 · 查看原因</button>` : ''}${state.report_outcome?.status === 'failed' ? `<button class="status-error quiet" data-page="reports" title="${esc(state.report_outcome.error)}">${esc(state.report_outcome.label)}失败</button>` : ''}<button class="quiet backlog-trigger" data-action="backlog" aria-expanded="false" aria-controls="backlog-panel" title="管理待分析消息">${pendingMessageLabel()}</button>`,
    phase,
  );
}

function render(nextKey) {
  syncTaskRecordPanel();
  if (!state) return;
  if (page === 'review') page = reviewSection;
  if (reviewPages.includes(page)) reviewSection = page;
  if (page === 'sources') {
    page = 'settings';
    pendingSettingsSection = 'sources';
  }
  syncReaderPage();
  if (page !== 'tasks') resetTaskMulti();
  if (state._page && state._page !== page) {
    for (const el of document.querySelectorAll('.nav[data-page]'))
      el.classList.toggle('active', el.dataset.page === page);
    motionText($('#page-title'), names[page]);
    motionText($('#page-subtitle'), '');
    setHtml('#top-actions', '');
    if ($('#view').dataset.loadingPage !== page) {
      $('#view').dataset.loadingPage = page;
      setHtml('#view', '<div class="page"><p class="helper" role="status">正在读取…</p></div>', { animate: false });
    }
    return refreshState(true);
  }
  delete $('#view').dataset.loadingPage;
  $('#view').classList.remove('plugin-drag-over');
  renderEpoch++;
  renderedPage = page;
  renderedKey = nextKey ?? viewKey(state, page);
  if (page !== 'sources') $('#view').dataset.contentPage = page;
  for (const el of document.querySelectorAll('[data-page]'))
    el.classList.toggle(
      'active',
      el.dataset.page === page || (el.dataset.page === 'review' && reviewPages.includes(page)),
    );
  motionText($('#page-title'), names[page]);
  const subtitles = {
    tasks: `${state.open_task_count} 项未完成`,
    schedules: '每日或每周，到点自动生成待处理事项',
    inbox: '查询完整消息缓存 · 支持时间排序与搜索',
    tools: '管理AI可调用的本地工具',
    logs: '判断依据、人工修改、用量和错误记录',
    reports: `工作 ${state.config.work_start}–${state.config.work_end} · 下班后一次汇总与归档`,
    learning: '从你的修正和处置记录中生成，由你确认后采用',
  };
  motionText($('#page-subtitle'), subtitles[page] || '');
  const scanBusy = !!(scanPending || state.busy),
    scanAndAnalyzeBusy = scanBusy || !!state.ai_busy;
  const actions = {
    tasks: `<button class="icon-button" aria-label="刷新并分析" title="读取并分析消息；手动操作不受工作时段限制" data-action="scan" aria-busy="${scanAndAnalyzeBusy}" ${scanAndAnalyzeBusy ? 'disabled' : ''}><span class="refresh-glyph ${scanAndAnalyzeBusy ? 'rotating' : ''}">${icon('refresh')}</span></button><button class="primary" data-action="new">${icon('add')}新建事项</button>`,
    schedules: '<button class="primary" data-action="new-schedule">＋ 新建定时事项</button>',
    inbox: `<button data-action="refresh-messages" title="读取新增消息" aria-busy="${scanBusy}" ${scanBusy ? 'disabled' : ''}>刷新</button><button data-action="analyze">分析待处理消息</button>`,
    reports:
      '<button data-action="archive-history">归档版本</button><button class="primary" data-action="archive" title="归档已结束事项，并用一次 AI 调用完成汇总与归档记忆维护；手动操作不受工作时段限制">手动归档</button>',
    learning: '<button data-action="learn">生成判断规则建议</button>',
    sources: '',
    guide: '',
    tools: '<button data-tool-history>调用记录</button><button class="primary" data-tool-new>＋ 新增工具</button>',
    settings:
      '<span id="settings-save-status" role="status"><span data-settings-save-message>更改后自动保存</span><button type="button" data-settings-retry hidden>重试</button></span>',
  };
  setHtml('#top-actions', actions[page] ?? '<button data-action="open-data">打开数据目录</button>');
  const renderers = {
    tasks: renderTasks,
    schedules: renderSchedules,
    sources: renderSources,
    tools: showToolLibrary,
    guide: renderGuide,
    settings: async () => {
      await renderSettings();
      if (page === 'settings' && pendingSettingsSection) {
        document.getElementById('settings-' + pendingSettingsSection)?.scrollIntoView({ block: 'start' });
        pendingSettingsSection = '';
      }
    },
    inbox: renderInbox,
    logs: renderLogs,
    reports: renderReports,
  };
  return (renderers[page] || renderLearning)();
}
function taskState(t) {
  if (t.archived) return { key: 'archived', label: '已归档 · ' + t.status };
  if (t.status === '已完成') return { key: 'done', label: '已完成' };
  if (t.status === '已取消') return { key: 'cancelled', label: '已取消' };
  if (t.status === '无需处理') return { key: 'dismissed', label: '无需处理' };
  if (t.generation === 'interrupted') return { key: 'interrupted', label: '已中断' };
  if (t.generation === 'failed') return { key: 'failed', label: '生成失败' };
  if (t.generation === 'pending') return { key: 'generating', label: '正在生成' };
  if (taskFocus(t) === '延后处理') return { key: 'pending', label: '延后处理' };
  if (t.blocker?.trim() || t.waiting_for?.trim()) return { key: 'blocked', label: '被阻塞' };
  if (t.status === '处理中') return { key: 'processing', label: '处理中' };
  return { key: 'pending', label: '待处理' };
}
function statusBadge(t) {
  const s = taskState(t);
  return `<span class="task-status state-${s.key}"><i></i>${s.label}</span>`;
}
const taskFocusViews = { pending: '待处理', processing: '处理中', blocked: '被阻塞', deferred: '延后处理' };
const taskFocusLabels = Object.values(taskFocusViews);
let taskClockDay = shanghaiDay(Date.now()),
  taskNextDeferralAt = Infinity;
function taskFocus(t, at = Date.now()) {
  if (!deadlineActive(t)) return '';
  if (t.deferred_until > at) return '延后处理';
  if (t.blocker?.trim() || t.waiting_for?.trim()) return '被阻塞';
  return t.status;
}
function taskScopeMatches(t, scope = tab, at = Date.now()) {
  return scope === '归档'
    ? t.archived
    : !t.archived &&
        !t.merged_into &&
        (scope === '全部' || (taskFocusLabels.includes(scope) ? taskFocus(t, at) === scope : t.status === scope));
}
function taskQuickViewMatches(t, view = quickView, at = Date.now()) {
  return (
    !view ||
    (taskFocusViews[view]
      ? taskFocus(t, at) === taskFocusViews[view]
      : view === 'today' && deadlineActive(t) && t.due && shanghaiDay(new Date(t.due).getTime()) === shanghaiDay(at))
  );
}
function taskSearchMatches(t, text = query) {
  return (
    !text ||
    [t.title, t.description, t.blocker, t.number == null ? '' : `#${t.number}`]
      .join(' ')
      .toLowerCase()
      .includes(text.toLowerCase())
  );
}
function notificationCanLeaveDialog() {
  if ($('#overlay').hidden || modalClosing) return true;
  const dialog = $('#dialog');
  if (dialog.querySelector('[data-pending="true"],[data-plugin-dirty="true"]')) return false;
  if (modalOutsideDismiss) return modalOutsideDismiss();
  // No generic modal baseline exists. Only task forms with unchanged native
  // input defaults can be closed here; keep other dialogs and custom editors.
  if (!dialog.querySelector('#f-title,#natural-input')) return false;
  return [...dialog.querySelectorAll('input,textarea,select')].every((input) => {
    if (input.type === 'checkbox' || input.type === 'radio') return input.checked === input.defaultChecked;
    if (input.tagName === 'SELECT')
      return input.value === ([...input.options].find((option) => option.defaultSelected) || input.options[0])?.value;
    return input.value === input.defaultValue;
  });
}
let notifiedTaskRequest = 0;
window.openNotifiedTask = async function (id) {
  const request = ++notifiedTaskRequest;
  const unavailable = (message) => {
    toast(message);
    return false;
  };
  if (typeof id !== 'string' || !id) return unavailable('这条事项已不可用');
  try {
    if (!notificationCanLeaveDialog()) return unavailable('请先保存或关闭当前编辑，再查看事项');
    if (!(await leaveSettings())) return false;
    const fromPage = page,
      fromTask = selected,
      dialog = $('#dialog').firstElementChild;
    const next = await rpc('state', {
      page: 'tasks',
      selected_task: id,
    });
    if (request !== notifiedTaskRequest) return false;
    if (
      page !== fromPage ||
      selected !== fromTask ||
      $('#dialog').firstElementChild !== dialog ||
      !notificationCanLeaveDialog() ||
      settingsChanged() ||
      connectionEditor ||
      memoryEditor
    )
      return unavailable('当前页面正在编辑，请稍后再查看事项');
    const task = next.tasks?.find((task) => task.id === id);
    if (!task || task.merged_into) return unavailable('这条事项已删除或合并');
    // Keep the user's live filters, even when the new snapshot has older saved
    // filters. Relax each rejecting group independently against fresh task data.
    tagSelection();
    prioritySelection();
    state = next;
    const tagsChanged = revealTaskTags(task);
    if (!taskScopeMatches(task)) tab = task.archived ? '归档' : '全部';
    if (!taskQuickViewMatches(task)) quickView = '';
    if (!taskSearchMatches(task)) query = '';
    if (!$('#overlay').hidden) closeModal();
    page = 'tasks';
    selected = id;
    await render();
    if (tagsChanged) saveTagFilters();
    const row = [...document.querySelectorAll('.task-row')].find((row) => row.dataset.task === id);
    if (!row || selected !== id) return unavailable('暂时无法打开这条事项，请重试');
    taskFeed.reveal(row);
    row.focus({ preventScroll: true });
    return true;
  } catch {
    return unavailable('暂时无法读取这条事项，请重试');
  }
};
function renderTaskList() {
  syncTaskRecordPanel();
  const savedTagBar = rememberTagBar();
  const pane = $('.task-list'),
    detail = $('#task-detail');
  const key = [tab, quickView, query, JSON.stringify(tagSelection()), JSON.stringify(prioritySelection())].join('|');
  const same = pane?.dataset.viewKey === key;
  const top = same ? pane.scrollTop : 0;
  const anchor = same
    ? [...pane.querySelectorAll('[data-task]')].find(
        (el) => el.getBoundingClientRect().bottom > pane.getBoundingClientRect().top,
      )
    : null;
  const anchorId = anchor?.dataset.task,
    offset = anchor ? anchor.getBoundingClientRect().top - pane.getBoundingClientRect().top : 0;
  const list = taskFeedRows();
  syncTaskMulti(list);
  // Until the first page arrives the list cannot tell a selection is gone.
  if (taskFeed?.data && !list.some((t) => t.id === selected)) selected = list[0]?.id || '';
  const detailTop = detail?.dataset.taskId === selected ? detail.scrollTop : 0;
  syncTaskRecordPanel();
  setHtml(
    '#view',
    `${tagFilterBar()}${list.length || taskMulti.active || tab === '归档' || !taskFeed?.done || taskFeed.error ? `<div class="split ${taskMulti.active ? 'task-bulk-layout' : ''}" data-ui-key="task-split">${taskBulkBar(list)}<div class="task-list" data-ui-key="task-list">${list.map((t, index) => taskMultiRow(`<button class="task-row state-${taskState(t).key} ${selected === t.id ? 'selected' : ''}" data-task="${t.id}" data-feed-key="${esc(t.id)}"><div class="row">${t.generation === 'pending' ? '<span class="badge blue"><span class="spin"></span>正在生成</span>' : t.generation === 'interrupted' ? '<span class="badge">已中断</span>' : t.generation === 'failed' ? '<span class="badge urgent">生成失败</span>' : ''}${extraTagBadges(t)}${t.manual ? '<span class="tiny">手动修改过</span>' : ''}</div><div class="task-title"><span class="task-number" aria-label="事项编号 ${esc(t.number)}">#${esc(t.number)}</span>${esc(t.title)}</div>${taskFocus(t) === '延后处理' ? `<div class="tiny">${esc(taskDeferralTime(t.deferred_until))} 恢复</div>` : ''}<div class="tiny" style="color:${isOverdue(t) ? 'var(--red-fg)' : 'var(--text-muted)'}">${esc(dueLabel(t))}</div><div class="task-meta"><span>${statusBadge(t)} ${handlingBadge(t.id)}${t.status === '待处理' && t.blocker?.trim() ? ' ' + esc(t.blocker) : ''}${t.status === '已完成' && t.status_changed_at ? ' · ' + esc(new Date(t.status_changed_at).toLocaleTimeString('zh-CN', { timeZone: 'Asia/Shanghai', hour: '2-digit', minute: '2-digit' })) : ''}</span><span>${t.evidence_count} 条依据</span></div></button>`, t)).join('')}${taskFeedStatus()}</div><div class="detail" id="task-detail" data-ui-key="detail-${esc(selected)}" data-task-id="${esc(selected)}" data-preserve-content></div></div>` : empty(Object.keys(tagSelection()).length || prioritySelection().length ? '没有匹配的事项' : '暂无事项', Object.keys(tagSelection()).length || prioritySelection().length ? '可调整标签，或点击“全部”清空标签筛选。' : state.ai_busy ? '消息已读取，AI 正在整理。完成后事项会自动显示。' : state.pending ? `还有 ${state.pending} 条相关消息等待分析。可在消息页继续分析。` : state.config.enabled ? '暂时没有需要处理的新事项。' : '可以开启自动扫描，或手动新建事项。', state.pending ? '<button data-action="analyze">分析待处理消息</button>' : '<button data-page="settings">打开设置</button>')}`,
    { animate: same && !taskMulti.active },
  );
  restoreTagBar(savedTagBar);
  for (const checkbox of document.querySelectorAll('[data-task-check]'))
    checkbox.checked = taskMulti.ids.has(checkbox.dataset.taskCheck);
  if (selected) renderDetail({ animate: same && detail?.dataset.taskId === selected });
  const nextPane = $('.task-list');
  if (nextPane) {
    nextPane.dataset.viewKey = key;
    nextPane.scrollTop = top;
    const row = anchorId
      ? [...nextPane.querySelectorAll('[data-task]')].find((el) => el.dataset.task === anchorId)
      : null;
    if (row) nextPane.scrollTop += row.getBoundingClientRect().top - nextPane.getBoundingClientRect().top - offset;
  }
  const nextDetail = $('#task-detail');
  if (nextDetail) {
    nextDetail.dataset.taskId = selected;
    nextDetail.scrollTop = detailTop;
  }
  if (selected && !state.tasks.some((t) => t.id === selected)) void refreshState(true);
}
function messageImages(m) {
  return (m.images || []).length
    ? `<div class="message-images">${m.images.map((image, i) => `<div class="message-image"><button class="quiet" data-image-id="${esc(image.id)}">${icon('image')} 图片 ${i + 1} · 按需预览</button></div>`).join('')}</div>`
    : '';
}
function messageBody(raw) {
  const text = String(raw || '');
  return `<div class="message-body">${text.length > 900 ? `<div>${esc(text.slice(0, 420))}…</div><details><summary>展开完整正文</summary><div>${esc(text)}</div></details>` : `<div>${esc(text)}</div>`}</div>`;
}
// The snapshot carries the full selected task; list rows are slim.
function selectedTask() {
  return state?.tasks.find((t) => t.id === selected);
}
function taskById(id) {
  return state?.tasks.find((t) => t.id === id) || taskFeedRows().find((t) => t.id === id);
}
function shanghaiDay(at) {
  return Math.floor((at + 8 * 3600000) / 86400000);
}
function taskDeferralTime(until, at = Date.now()) {
  const day = shanghaiDay(until),
    current = shanghaiDay(at),
    days = day - current;
  const local = new Date(until + 8 * 3600000);
  const weekday = local.getUTCDay() || 7;
  const currentWeekday = new Date(current * 86400000).getUTCDay() || 7;
  const weeks = Math.floor((day - current + currentWeekday - 1) / 7);
  const weekLabel = `周${'一二三四五六日'[weekday - 1]}`;
  const label =
    days < 0
      ? `${-days}天前`
      : (['今天', '明天', '后天'][days] ??
        (weeks === 0 ? `本${weekLabel}` : weeks === 1 ? `下${weekLabel}` : `${weeks}周后${weekLabel}`));
  return `${label} ${String(local.getUTCHours()).padStart(2, '0')}:${String(local.getUTCMinutes()).padStart(2, '0')}`;
}
function refreshTaskClock(at = Date.now()) {
  if (!state) return;
  const day = shanghaiDay(at);
  if (day !== taskClockDay || at >= taskNextDeferralAt) {
    taskClockDay = day;
    taskNextDeferralAt = Infinity;
    void refreshState(true);
  }
  const menu = $('.task-defer.is-open');
  if (menu && Math.floor(at / 60000) !== taskDeferMenuMinute) setTaskDeferMenu(menu, true);
}
function taskDeferralChoices(at = Date.now()) {
  const local = new Date(at + 8 * 3600000),
    day = Date.UTC(local.getUTCFullYear(), local.getUTCMonth(), local.getUTCDate()) - 8 * 3600000;
  const [hour, minute] = state.config.work_start.split(':').map(Number),
    start = (hour * 60 + minute) * 60000;
  return [
    { choice: 'hour', label: '一小时后处理', until: at + 3600000 },
    { choice: 'afternoon', label: '下午处理', until: day + 13 * 3600000 },
    { choice: 'evening', label: '晚上处理', until: day + 17 * 3600000 },
    { choice: 'tomorrow', label: '明天处理', until: day + 86400000 + start },
    { choice: 'next_week', label: '下周一处理', until: day + (8 - (local.getUTCDay() || 7)) * 86400000 + start },
  ].filter((item) => item.until > at);
}
function taskStatusActions(t) {
  const open =
    $('[data-task-status]')?.dataset.taskStatus === t.id && $('[data-task-status]').classList.contains('is-open');
  const active = deadlineActive(t);
  const primary = active ? (t.status === '待处理' ? 'start' : 'done') : 'reopen';
  const labels = { start: '开始处理', done: '处理完成', reopen: '重新打开' };
  const delays = taskDeferralChoices();
  return `<div class="task-status-actions ${open ? 'is-open' : ''}" data-task-status="${esc(t.id)}"><button class="primary" data-task-status-trigger data-action="${primary}" aria-haspopup="menu" aria-expanded="${!!open}" aria-controls="task-status-options">${labels[primary]}<span class="task-status-chevron" aria-hidden="true"></span></button><div class="task-status-popover" id="task-status-options" role="menu" aria-label="事项状态操作" ${open ? '' : 'inert'}><div class="task-status-menu">${active ? `${primary !== 'start' ? '<button role="menuitem" data-action="start">开始处理</button>' : ''}${primary !== 'done' ? '<button role="menuitem" data-action="done">处理完成</button>' : ''}<div class="task-defer"><button role="menuitem" data-action="defer" data-defer-trigger aria-haspopup="menu" aria-expanded="false" aria-controls="task-defer-options">延后处理<span aria-hidden="true">›</span></button><div id="task-defer-options" class="task-defer-popover" role="menu" aria-label="延后时间" inert><div class="task-status-menu">${delays.map(({ choice, label, until }) => `<button role="menuitem" data-action="defer" data-defer-choice="${choice}" title="${esc(taskDeferralTime(until))} 恢复">${label}<span class="tiny">${esc(taskDeferralTime(until))}</span></button>`).join('')}</div></div></div>` : ''}<button role="menuitem" data-action="dismiss">无需处理</button><button role="menuitem" data-action="archive-note">${t.archived ? '归档备注' : '归档'}</button></div></div></div>`;
}
let taskStatusCloseTimer,
  taskDeferMenuMinute = -1;
function setTaskStatusMenu(host, open) {
  clearTimeout(taskStatusCloseTimer);
  if (!host?.isConnected) return;
  host.classList.toggle('is-open', open);
  host.querySelector('[data-task-status-trigger]').setAttribute('aria-expanded', String(open));
  if (!open) setTaskDeferMenu(host.querySelector('.task-defer'), false);
  const menu = host.querySelector('[role="menu"]');
  menu.inert = !open;
  if (open) {
    menu.style.left = '0';
    const bounds = menu.getBoundingClientRect(),
      pane = host.closest('.detail')?.getBoundingClientRect(),
      right = Math.min(innerWidth - 12, pane?.right ?? innerWidth - 12);
    menu.style.left = `${Math.max(12 - bounds.left, Math.min(0, right - bounds.right))}px`;
  }
}
function setTaskDeferMenu(host, open) {
  if (!host) return;
  host.classList.toggle('is-open', open);
  host.querySelector('[data-defer-trigger]').setAttribute('aria-expanded', String(open));
  const menu = host.querySelector('[role="menu"]');
  menu.inert = !open;
  if (open) {
    taskDeferMenuMinute = Math.floor(Date.now() / 60000);
    host.classList.remove('opens-left');
    menu.style.translate = '';
    const choices = taskDeferralChoices();
    for (const button of menu.querySelectorAll('[data-defer-choice]')) {
      const choice = choices.find((item) => item.choice === button.dataset.deferChoice);
      if (!choice && button === document.activeElement)
        host.querySelector('[data-defer-trigger]').focus({ preventScroll: true });
      button.hidden = !choice;
      if (choice) {
        button.title = `${taskDeferralTime(choice.until)} 恢复`;
        button.querySelector('.tiny').textContent = taskDeferralTime(choice.until);
      }
    }
    menu.style.top = '0px';
    const anchor = host.getBoundingClientRect(),
      width = menu.offsetWidth;
    const left = anchor.right + width <= innerWidth - 12 ? anchor.right : anchor.left - width;
    menu.style.left = `${Math.max(12, Math.min(left, innerWidth - width - 12))}px`;
    menu.style.top = `${Math.max(12, Math.min(anchor.top, innerHeight - menu.offsetHeight - 12))}px`;
  }
}
document.addEventListener('pointerover', (e) => {
  const defer = e.target.closest('.task-defer');
  if (defer) setTaskDeferMenu(defer, true);
  else {
    const open = document.querySelector('.task-defer.is-open');
    if (open && !open.contains(e.relatedTarget)) setTaskDeferMenu(open, false);
  }
  const host = e.target.closest('[data-task-status]');
  if (host && !host.contains(e.relatedTarget)) setTaskStatusMenu(host, true);
});
document.addEventListener('pointerout', (e) => {
  const host = e.target.closest('[data-task-status]');
  if (host && !host.contains(e.relatedTarget)) {
    clearTimeout(taskStatusCloseTimer);
    taskStatusCloseTimer = setTimeout(() => {
      if (!host.contains(document.activeElement)) setTaskStatusMenu(host, false);
    }, 120);
  }
});
document.addEventListener('focusin', (e) => {
  const host = e.target.closest('[data-task-status]');
  if (host) setTaskStatusMenu(host, true);
});
document.addEventListener('focusout', (e) => {
  const host = e.target.closest('[data-task-status]');
  if (host && !host.contains(e.relatedTarget) && !host.matches(':hover')) setTaskStatusMenu(host, false);
});
document.addEventListener('pointerdown', (e) => {
  const host = $('[data-task-status].is-open');
  if (host && !host.contains(e.target)) setTaskStatusMenu(host, false);
});
document.addEventListener('click', (e) => {
  const host = e.target.closest('[data-task-status]');
  if (host && e.target.closest('[data-action]')) setTaskStatusMenu(host, false);
});
document.addEventListener('keydown', (e) => {
  const host = e.target.closest('[data-task-status]');
  if (!host) return;
  const defer = e.target.closest('.task-defer');
  const submenu = defer?.querySelector('[role="menu"]');
  if (e.key === 'ArrowRight' && defer) {
    e.preventDefault();
    setTaskDeferMenu(defer, true);
    submenu.querySelector('button')?.focus();
    return;
  }
  if (['ArrowLeft', 'Escape'].includes(e.key) && defer?.classList.contains('is-open')) {
    e.preventDefault();
    e.stopImmediatePropagation();
    defer.querySelector('[data-defer-trigger]').focus();
    setTaskDeferMenu(defer, false);
    return;
  }
  const menu = submenu?.contains(e.target) ? submenu : host.querySelector('[role="menu"]');
  const items = [...menu.querySelectorAll('[role="menuitem"]')].filter(
    (item) => !item.hidden && item.closest('[role="menu"]') === menu,
  );
  if (['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(e.key)) {
    e.preventDefault();
    setTaskStatusMenu(host, true);
    const index = items.indexOf(document.activeElement),
      next =
        e.key === 'Home'
          ? 0
          : e.key === 'End' || (index < 0 && e.key === 'ArrowUp')
            ? items.length - 1
            : (index + (e.key === 'ArrowUp' ? -1 : 1) + items.length) % items.length;
    items[next]?.focus();
  } else if (e.key === 'Escape' && host.classList.contains('is-open')) {
    e.preventDefault();
    e.stopImmediatePropagation();
    host.querySelector('[data-task-status-trigger]').focus({ preventScroll: true });
    setTaskStatusMenu(host, false);
  }
});
const taskOrigins = {
  ai_create: '消息分析新建',
  manual_edit: '手动新建',
  generation_start: '按描述整理新建',
  schedule_generate: '定时生成',
};
async function renderDetail(options) {
  syncTaskRecordPanel();
  const t = selectedTask();
  if (!t || !$('#task-detail')) return;
  const previousDefer = $('[data-task-status]')?.dataset.taskStatus === t.id ? $('.task-defer.is-open') : null;
  const deferFocused = previousDefer?.contains(document.activeElement);
  const focusedChoice = deferFocused ? document.activeElement.dataset.deferChoice : '';
  if ($('#task-detail').dataset.taskId !== t.id) {
    $('#task-detail').replaceChildren();
    $('#task-detail').dataset.taskId = t.id;
    $('#task-detail').dataset.uiKey = 'detail-' + t.id;
  }
  if (t.generation) {
    setHtml(
      '#task-detail',
      `<div class="generation-detail"><div class="generation-symbol">${t.generation === 'pending' ? '<span class="spin"></span>' : '!'}</div><h2>${t.generation === 'pending' ? '正在整理事项' : t.generation === 'interrupted' ? '本次任务已中断' : '生成未完成'}</h2><p>${t.generation === 'pending' ? '完成后自动更新，可先处理其他事项。一段话包含多件事时会拆分保存。' : esc(t.generation_error)}</p><div class="generation-input">${esc(t.description)}</div><div class="row">${['failed', 'interrupted'].includes(t.generation) ? '<button data-action="retry-generation">重新执行</button>' : ''}<button data-action="edit">手动编辑</button><button class="quiet" data-action="dismiss">取消事项</button></div></div>`,
      options,
    );
    return;
  }
  setHtml(
    '#task-detail',
    `<div class="row">${extraTagBadges(t)}${statusBadge(t)}</div><h2><span class="detail-task-number">#${esc(t.number)}</span>${esc(t.title)}</h2><div class="task-times"><span>生成于 ${esc(t.created ? date(t.created) : '未记录')}${taskOrigins[t.origin] ? ` · ${taskOrigins[t.origin]}` : ''}</span><span>上次变更 ${esc(t.updated ? date(t.updated) : '未记录')}</span></div>${t.reason ? `<div class="reason">${esc(t.reason)}</div>` : ''}<div class="detail-actions"><button class="ai-dispose" data-handle="start" data-task-id="${esc(t.id)}"><span class="ai-mark">${icon('prompts')}</span>${handlingButton(t.id)}</button>${taskStatusActions(t)}<button data-action="edit">编辑</button><button data-action="merge">合并</button>${t.has_tool_calls ? `<button data-tool-run-history="${esc(t.id)}">工具调用记录</button>` : ''}</div>${taskFocus(t) === '延后处理' ? `<div class="deferred-note">${esc(taskDeferralTime(t.deferred_until))} 恢复；${t.resume_processing ? '到期恢复处理中，可提前开始。' : '可随时开始处理。'}</div>` : ''}${t.schedule_id ? `<div class="helper">定时事项 · ${esc(t.scheduled_for)} 生成</div>` : ''}${t.blocker?.trim() ? `<div class="block">有阻塞 · ${esc(t.blocker)}${t.waiting_for?.trim() ? `<div class="muted">等待 ${esc(t.waiting_for)}</div>` : ''}</div>` : ''}${t.waiting_for?.trim() && !t.blocker?.trim() ? `<div class="waiting-note">等待 ${esc(t.waiting_for)} 的回复或处理结果</div>` : ''}${t.archive_note ? `<section class="archive-note"><h3>归档备注</h3><div class="description">${esc(t.archive_note)}</div><div class="helper">${esc(date(t.archived_at))} · 会作为后续汇总与建议的参考</div></section>` : ''}${t.description ? `<section class="detail-panel task-description"><h3>事项描述</h3><div class="description">${esc(t.description)}</div></section>` : ''}${t.due ? `<section><h3>截止时间</h3>${esc(dueLabel(t))}<div class="helper">后续对话有明确新证据时可自动更新期限</div></section>` : ''}${handlingNote(t)}${taskRecordButtons(t)}`,
    options,
  );
  const statusMenu = $('[data-task-status].is-open');
  if (statusMenu) setTaskStatusMenu(statusMenu, true);
  if (previousDefer && statusMenu) {
    const submenu = statusMenu.querySelector('.task-defer');
    setTaskDeferMenu(submenu, true);
    if (deferFocused)
      (
        submenu?.querySelector(`[data-defer-choice="${focusedChoice}"]:not([hidden])`) ||
        submenu?.querySelector('[data-defer-trigger]') ||
        statusMenu.querySelector('[data-task-status-trigger]')
      ).focus({ preventScroll: true });
  }
  syncTaskRecordPanel();
}
let backlogPreview = null,
  backlogSequence = 0,
  backlogTimer = null;
function closeBacklog() {
  clearTimeout(backlogTimer);
  backlogSequence++;
  const panel = $('#backlog-panel');
  if (panel) {
    panel.removeAttribute('id');
    motionExit(panel);
  }
  $('[data-action="backlog"]')?.setAttribute('aria-expanded', 'false');
  backlogPreview = null;
}
async function openBacklog() {
  if ($('#backlog-panel')) {
    closeBacklog();
    return;
  }
  closeBacklog();
  const panel = document.createElement('section');
  panel.id = 'backlog-panel';
  panel.className = 'backlog-panel';
  panel.setAttribute('aria-label', '管理待分析消息');
  panel.innerHTML = `<div class="backlog-head"><h3>待分析消息</h3><span id="backlog-total">${state.pending} 条</span></div><p>仅分析最近 ${state.config.analysis_lookback_hours} 小时，更早消息自动排除。<span id="backlog-active">${state.analyzing ? `另有 ${state.analyzing} 条正在分析，不计入等待。` : ''}</span></p><p id="backlog-retries" class="error-box" hidden></p><div class="backlog-options"><label for="backlog-hours">本次跳过 <input id="backlog-hours" type="number" min="1" max="87600" value="${state.config.analysis_lookback_hours}"> 小时前的消息</label><div class="backlog-preview-row"><p id="backlog-count" role="status">正在计算…</p><button id="backlog-apply" data-action="backlog-apply" disabled>跳过</button></div></div><div class="backlog-foot"><span>保留原文 · 不改设置</span><button class="backlog-run" data-action="backlog-analyze" ${state.ai_busy || state.analyzing || !state.pending ? 'disabled' : ''}>立即分析<span aria-hidden="true">→</span></button></div>`;
  document.body.append(panel);
  const button = $('[data-action="backlog"]'),
    rect = button.getBoundingClientRect();
  panel.style.left = Math.max(16, Math.min(rect.left, innerWidth - panel.offsetWidth - 16)) + 'px';
  button.setAttribute('aria-expanded', 'true');
  $('#backlog-hours').focus({ preventScroll: true });
  await previewBacklog();
}
async function previewBacklog() {
  const input = $('#backlog-hours');
  if (!input) return;
  const hours = Number(input.value),
    sequence = ++backlogSequence;
  backlogPreview = null;
  $('#backlog-apply').disabled = true;
  if (!Number.isInteger(hours) || hours < 1 || hours > 87600) {
    $('#backlog-count').textContent = '请输入 1–87600 小时';
    return;
  }
  const result = await rpc('analysis_backlog', { hours });
  if (sequence !== backlogSequence || !$('#backlog-panel') || Number($('#backlog-hours').value) !== hours) return;
  backlogPreview = result;
  $('#backlog-total').textContent = `${result.eligible} 条`;
  const retries = $('#backlog-retries'),
    reasons = result.retry_reasons;
  retries.hidden = !reasons.length;
  retries.textContent =
    reasons
      .map((row) => `${row.count} 条等待重试：${row.reason}`)
      .join('；') + (result.retry_reasons_more ? `；另有 ${result.retry_reasons_more} 类原因，可在日志中核对。` : '');
  $('#backlog-count').textContent = result.count
    ? `共 ${result.eligible} 条，可跳过 ${result.count} 条`
    : '没有需要跳过的消息';
  $('#backlog-apply').disabled = !result.count;
  $('#backlog-active').textContent = result.analyzing ? `另有 ${result.analyzing} 条正在分析，不计入等待。` : '';
}
