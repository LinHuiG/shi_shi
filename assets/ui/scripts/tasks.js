function status() {
  if (!state) return;
  updateAiLiveDetail();
  const lastAi = (state.activity || []).find((a) =>
    ['ai_success', 'ai_failed', 'ai_cancelled'].includes(a.kind),
  );
  const contextIncomplete = lastAi?.detail?.context_usage?.budget_exhausted === true;
  setHtml('#work-status', workStatusBar());
  const refresh = $('#top-actions [data-action="scan"]');
  if (refresh?.classList.contains('icon-button')) {
    const busy = state.busy || state.ai_busy;
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
  $('#scan-controls [data-action="scan-options"]').textContent =
    `${state.config.enabled ? '自动' : '暂停'} · ${state.config.interval_minutes}分钟`;
  const current = state.tasks.filter(
    (t) => t.status === '待处理' && !t.archived && !t.merged_into && !t.deferred,
  );
  const n = current.length;
  const dueToday = (t) =>
    t.due &&
    new Date(t.due).toLocaleDateString('zh-CN', { timeZone: 'Asia/Shanghai' }) ===
      new Date().toLocaleDateString('zh-CN', { timeZone: 'Asia/Shanghai' });
  setHtml(
    '#sidebar-context',
    `<div class="side-section-label">关注</div><button class="side-view" data-view="daily">今日事项<span>${state.tasks.filter((t) => t.today).length}</span></button><button class="side-view" data-view="blocked">被阻塞<span>${current.filter((t) => t.blocker).length}</span></button><button class="side-view" data-view="today">今日到期<span>${current.filter(dueToday).length}</span></button><div class="side-section-label">最近事项</div>${
      current
        .slice()
        .sort((a, b) => b.updated - a.updated)
        .slice(0, 5)
        .map(
          (t) =>
            `<button class="recent-item" data-recent="${t.id}" title="${esc(t.title)}"><span class="recent-dot ${t.generation === 'pending' ? 'pulse' : ''}"></span>${esc(t.title)}</button>`,
        )
        .join('') || '<span class="side-empty">暂无事项</span>'
    }`,
  );
  motionText($('#nav-count'), n || '');
  setHtml(
    '#side-status',
    `<span class="dot ${state.config.enabled ? '' : 'off'}"></span>${state.config.enabled ? '自动扫描' : '扫描暂停'}<span class="side-pending">${state.analyzing ? state.analyzing + ' 条分析中' : (state.pending || 0) + ' 条待分析'}</span>`,
  );
  const elapsed = state.ai_started
    ? Math.max(0, Math.floor((Date.now() - state.ai_started) / 1000))
    : 0;
  const phase = state.ai_cancelling
    ? '正在中断 AI…'
    : state.ai_busy
      ? `<span class="spin"></span>${state.analyzing ? '正在分析 ' + state.analyzing + ' 条消息' : 'AI 正在处理'}${elapsed ? ' · 已用 ' + elapsed + ' 秒' : ''}`
      : state.analyzing
        ? `<span class="spin"></span>正在准备 ${state.analyzing} 条消息`
        : state.busy
          ? '<span class="spin"></span>正在读取消息'
          : state.ai_interrupted_at
            ? '本次 AI 已中断'
            : !state.in_work_hours
              ? '非消息自动分析时段'
              : '就绪';
  updateStatusBar(
    `<span class="phase-label" data-preserve-content></span>${contextIncomplete ? '<button class="quiet context-warning" data-page="logs" title="已采用分析结果，但补查覆盖不完整">补查覆盖不完整</button>' : ''}${state.ai_busy ? `<button class="quiet" data-action="ai-interrupt" data-request-id="${esc(state.ai_request_id || '')}" ${state.ai_cancelling ? 'disabled' : ''}>${state.ai_cancelling ? '正在中断…' : '中断本次 AI'}</button><button class="ai-live quiet" data-ai-live title="查看 AI 实时进度" aria-label="查看 AI 实时进度"><span class="live-dot"></span><span data-live-line data-preserve-content></span></button>` : `<span class="status-time">最近事项分析 ${esc(state.last_analysis_success_at ? date(state.last_analysis_success_at) : '尚未分析')}</span>`}${state.ai_busy ? '<span class="token-speed status-speed" data-status-speed data-preserve-content hidden></span>' : ''}${state.last_error ? `<button class="status-error quiet" data-page="logs" title="${esc(state.last_error)}">处理失败 · 查看原因</button>` : ''}${(state.morning_jobs || []).find((j) => j.status === 'failed') ? '<button class="status-error quiet" data-page="reports">早报失败</button>' : ''}<button class="quiet backlog-trigger" data-action="backlog" aria-expanded="false" aria-controls="backlog-panel" title="管理待分析消息">${state.pending ? state.pending + ' 条等待分析' : '无等待消息'}</button>`,
    phase,
  );
}

function render() {
  if (!state) return;
  if (state._page && state._page !== page) {
    void refreshState(true);
    return;
  }
  if (page === 'learning') {
    page = 'prompts';
    promptSection = 'advice';
  }
  $('#view').classList.remove('plugin-drag-over');
  renderEpoch++;
  renderedPage = page;
  renderedKey = viewKey(state, page);
  if (!['sources', 'prompts'].includes(page)) $('#view').dataset.contentPage = page;
  for (const el of document.querySelectorAll('[data-page]'))
    el.classList.toggle('active', el.dataset.page === page);
  $('#page-title').textContent = page === 'handling' ? '处置规则' : names[page];
  $('#page-subtitle').textContent =
    page === 'tasks'
      ? `${state.tasks.filter((t) => t.status === '待处理' && !t.archived && !t.merged_into && !t.deferred).length} 项待处理`
      : page === 'schedules'
        ? '每日或每周，到点自动生成待处理事项'
        : page === 'inbox'
          ? '查询完整消息缓存 · 支持时间排序与搜索'
          : page === 'prompts'
            ? '管理提示词、AI 记忆和迭代建议'
            : page === 'logs'
              ? '判断依据、人工修改、用量和错误记录'
              : page === 'reports'
                ? `工作 ${state.config.work_start}–${state.config.work_end} · 下班后一次汇总与归档`
                : '';
  setHtml(
    '#top-actions',
    page === 'tasks'
      ? `<button class="icon-button quiet" aria-label="刷新并分析" title="读取并分析消息；手动操作不受工作时段限制" data-action="scan" aria-busy="${state.busy || state.ai_busy}" ${state.busy || state.ai_busy ? 'disabled' : ''}><span class="refresh-glyph ${state.busy || state.ai_busy ? 'rotating' : ''}" aria-hidden="true">↻</span></button><button class="primary" data-action="new">＋ 新建事项</button>`
      : page === 'schedules'
        ? '<button class="primary" data-action="new-schedule">＋ 新建定时事项</button>'
        : page === 'inbox'
          ? '<button data-action="refresh-messages" title="读取新增消息">刷新</button><button data-action="analyze">分析待处理消息</button>'
          : page === 'reports'
            ? '<button data-action="archive-history">归档版本</button><button class="primary" data-action="archive" title="归档已结束事项，并用一次 AI 调用完成汇总与归档记忆维护；手动操作不受工作时段限制">手动归档</button>'
            : page === 'learning'
              ? '<button data-action="learn">生成判断规则建议</button>'
              : ['sources', 'guide'].includes(page)
                ? ''
                : page === 'prompts'
                  ? promptSection === 'advice'
                    ? '<button data-action="learn">生成判断规则建议</button>'
                    : ''
                  : page === 'settings'
                    ? '<span id="settings-save-status" role="status"><span data-settings-save-message>更改后自动保存</span><button type="button" data-settings-retry hidden>重试</button></span>'
                    : '<button data-action="open-data">打开数据目录</button>',
  );
  if (page === 'handling')
    return renderHandlingRules().catch((e) => {
      renderedKey = '';
      toast(e.message);
    });
  else if (page === 'tasks') renderTasks();
  else if (page === 'schedules') renderSchedules();
  else if (page === 'sources') return renderSources();
  else if (page === 'prompts')
    return renderSystemPrompts().catch((e) => {
      renderedKey = '';
      toast(e.message);
    });
  else if (page === 'guide') renderGuide();
  else if (page === 'settings') renderSettings();
  else if (page === 'inbox') return renderInbox();
  else if (page === 'logs') return renderLogs();
  else if (page === 'reports') renderReports();
  else renderLearning();
}
function taskState(t) {
  if (t.archived) return { key: 'archived', label: '已归档 · ' + t.status };
  if (t.status === '已完成') return { key: 'done', label: '已完成' };
  if (t.status === '已取消') return { key: 'cancelled', label: '已取消' };
  if (t.status === '无需处理') return { key: 'dismissed', label: '无需处理' };
  if (t.generation === 'interrupted') return { key: 'interrupted', label: '已中断' };
  if (t.generation === 'failed') return { key: 'failed', label: '生成失败' };
  if (t.generation === 'pending') return { key: 'generating', label: '正在生成' };
  if (t.blocker) return { key: 'blocked', label: '被阻塞' };
  if (t.waiting_for) return { key: 'waiting', label: '待回复' };
  return { key: 'pending', label: '待处理' };
}
function statusBadge(t) {
  const s = taskState(t);
  return `<span class="task-status state-${s.key}"><i></i>${s.label}</span>`;
}
function visibleTasks() {
  return state.tasks
    .filter(
      (t) =>
        tagMatches(t) &&
        (!quickView ||
          (quickView === 'blocked'
            ? !!t.blocker
            : t.due &&
              new Date(t.due).toLocaleDateString('zh-CN', { timeZone: 'Asia/Shanghai' }) ===
                new Date().toLocaleDateString('zh-CN', { timeZone: 'Asia/Shanghai' }))) &&
        !t.merged_into &&
        (tab === '今日事项'
          ? t.today
          : tab === '归档'
            ? t.archived
            : !t.archived && (tab === '全部' || (t.status === tab && !t.deferred))) &&
        (!query ||
          (t.title + ' ' + t.description + ' ' + t.blocker)
            .toLowerCase()
            .includes(query.toLowerCase())),
    )
    .sort(
      (a, b) =>
        (tab === '今日事项' ? Number(a.status !== '待处理') - Number(b.status !== '待处理') : 0) ||
        taskRank(a) - taskRank(b) ||
        b.updated - a.updated,
    );
}
function renderTasks() {
  const savedTagBar = rememberTagBar();
  const composingField =
      document.activeElement?.id === 'handling-question' ? document.activeElement : null,
    composerCaret = composingField
      ? [composingField.selectionStart, composingField.selectionEnd]
      : null;
  const pane = $('.task-list'),
    detail = $('#task-detail');
  const key = [
    tab,
    quickView,
    query,
    JSON.stringify(tagSelection()),
    JSON.stringify(prioritySelection()),
  ].join('|');
  const same = pane?.dataset.viewKey === key;
  const top = same ? pane.scrollTop : 0;
  const detailTop = detail?.dataset.taskId === selected ? detail.scrollTop : 0;
  const anchor = same
    ? [...pane.querySelectorAll('[data-task]')].find(
        (el) => el.getBoundingClientRect().bottom > pane.getBoundingClientRect().top,
      )
    : null;
  const anchorId = anchor?.dataset.task,
    offset = anchor ? anchor.getBoundingClientRect().top - pane.getBoundingClientRect().top : 0;
  const list = visibleTasks();
  if (!list.some((t) => t.id === selected)) selected = list[0]?.id || '';
  setHtml(
    '#view',
    `${tagFilterBar()}${tab === '今日事项' ? `<div class="daily-summary"><span>今日 · ${new Date().toLocaleDateString('zh-CN', { timeZone: 'Asia/Shanghai', month: 'long', day: 'numeric' })}</span><span class="daily-pending">${list.filter((t) => t.status === '待处理').length} 待处理</span><span class="daily-complete">${list.filter((t) => t.status === '已完成').length} 已完成</span><small>含之前未完成的事项</small></div>` : ''}${list.length ? `<div class="split"><div class="task-list">${list.map((t, index) => `<button class="task-row state-${taskState(t).key} ${selected === t.id ? 'selected' : ''}" data-task="${t.id}"><div class="row">${t.generation === 'pending' ? '<span class="badge blue"><span class="spin"></span>正在生成</span>' : t.generation === 'interrupted' ? '<span class="badge">已中断</span>' : t.generation === 'failed' ? '<span class="badge urgent">生成失败</span>' : ''}${extraTagBadges(t)}${t.manual ? '<span class="tiny">手动修改过</span>' : ''}</div><div class="task-title"><span class="task-number" aria-label="事项编号 ${taskNumber(t)}">#${taskNumber(t)}</span>${esc(t.title)}</div>${t.deferred ? `<div class="tiny">明日处理 · ${esc(date(t.deferred_until))} 恢复</div>` : ''}<div class="tiny" style="color:${t.due && new Date(t.due) < new Date() ? 'var(--red)' : 'var(--sub)'}">${esc(dueLabel(t))}</div><div class="task-meta"><span>${statusBadge(t)} ${handlingBadge(t.id)}${t.status === '待处理' && t.blocker ? ' ' + esc(t.blocker) : ''}${t.status === '已完成' && t.status_changed_at ? ' · ' + esc(new Date(t.status_changed_at).toLocaleTimeString('zh-CN', { timeZone: 'Asia/Shanghai', hour: '2-digit', minute: '2-digit' })) : ''}</span><span>${evidenceCount(t)} 条依据</span></div></button>`).join('')}</div><div class="detail" id="task-detail" data-ui-key="detail-${esc(selected)}" data-task-id="${esc(selected)}" data-preserve-content></div></div>` : empty(Object.keys(tagSelection()).length || prioritySelection().length ? '没有匹配的事项' : '暂无事项', Object.keys(tagSelection()).length || prioritySelection().length ? '可调整标签，或点击“全部”清空标签筛选。' : state.ai_busy ? '消息已读取，AI 正在整理。完成后事项会自动显示。' : state.pending ? `还有 ${state.pending} 条相关消息等待分析。可在消息收件箱继续分析。` : state.config.enabled ? '暂时没有需要处理的新事项。' : '可以开启自动扫描，或手动新建事项。', state.pending ? '<button data-action="analyze">分析待处理消息</button>' : '<button data-page="settings">打开设置</button>')}`,
  );
  restoreTagBar(savedTagBar);
  if (selected) renderDetail();
  const nextPane = $('.task-list');
  if (nextPane) {
    nextPane.dataset.viewKey = key;
    nextPane.scrollTop = top;
    const row = anchorId
      ? [...nextPane.querySelectorAll('[data-task]')].find((el) => el.dataset.task === anchorId)
      : null;
    if (row)
      nextPane.scrollTop +=
        row.getBoundingClientRect().top - nextPane.getBoundingClientRect().top - offset;
  }
  const nextDetail = $('#task-detail');
  if (nextDetail) {
    nextDetail.dataset.taskId = selected;
    nextDetail.scrollTop = detailTop;
  }
  measureHandlingReadings();
  updateHandlingLive();
  if (composerCaret && $('#handling-question')) {
    $('#handling-question').focus({ preventScroll: true });
    $('#handling-question').setSelectionRange(...composerCaret);
  }
}
function readableMessage(raw) {
  return { text: String(raw || ''), card: false };
}
function messageImages(m) {
  return (m.images || []).length
    ? `<div class="message-images">${m.images.map((image, i) => `<div class="message-image"><button class="quiet" data-image-id="${esc(image.id)}">▧ 图片 ${i + 1} · 按需预览</button></div>`).join('')}</div>`
    : '';
}
function messageBody(raw) {
  const m = readableMessage(raw);
  const text = esc(m.text);
  return `<div class="message-body ${m.card ? 'message-card' : ''}">${m.card ? '<div class="card-caption">消息卡片</div>' : ''}${m.text.length > 900 ? `<div>${esc(m.text.slice(0, 420))}…</div><details><summary>展开完整正文</summary><div>${text}</div></details>` : `<div>${text}</div>`}</div>`;
}
function taskNumber(t) {
  return esc(t.number ?? state.tasks.findIndex((v) => v.id === t.id) + 1);
}
function evidenceCount(t) {
  if (Number.isInteger(t?.evidence_count)) return t.evidence_count;
  const e = t?.evidence || {};
  return new Set([...(e.direct || []), ...(e.background || []), ...(e.completion || [])]).size;
}
function selectedTask() {
  return state.tasks.find((t) => t.id === selected);
}
async function renderDetail() {
  const t = selectedTask();
  if (!t || !$('#task-detail')) return;
  if ($('#task-detail').dataset.taskId !== t.id) {
    $('#task-detail').replaceChildren();
    $('#task-detail').dataset.taskId = t.id;
    $('#task-detail').dataset.uiKey = 'detail-' + t.id;
  }
  if (t.generation) {
    setHtml(
      '#task-detail',
      `<div class="generation-detail"><div class="generation-symbol">${t.generation === 'pending' ? '<span class="spin"></span>' : '!'}</div><h2>${t.generation === 'pending' ? '正在整理事项' : t.generation === 'interrupted' ? '本次任务已中断' : '生成未完成'}</h2><p>${t.generation === 'pending' ? '完成后自动更新，可先处理其他事项。一段话包含多件事时会拆分保存。' : esc(t.generation_error)}</p><div class="generation-input">${esc(t.description)}</div><div class="row">${['failed', 'interrupted'].includes(t.generation) ? '<button data-action="retry-generation">重新执行</button>' : ''}<button data-action="edit">手动编辑</button><button class="quiet" data-action="dismiss">取消事项</button></div></div>`,
    );
    return;
  }
  setHtml(
    '#task-detail',
    `<div class="row">${t.generation === 'pending' ? '<span class="badge blue"><span class="spin"></span>正在生成</span>' : t.generation === 'interrupted' ? '<span class="badge">已中断</span>' : t.generation === 'failed' ? '<span class="badge urgent">生成失败</span>' : ''}${extraTagBadges(t)}${statusBadge(t)}</div><h2><span class="detail-task-number">#${taskNumber(t)}</span>${esc(t.title)}</h2><div class="task-times"><span>生成于 ${esc(t.created ? date(t.created) : '未记录')}</span><span>上次变更 ${esc(t.updated ? date(t.updated) : '未记录')}</span></div><div class="reason">${esc(t.reason || '手动创建的事项')}</div><div class="detail-actions"><button class="ai-dispose" data-handle="start" data-task-id="${esc(t.id)}"><span class="ai-mark" aria-hidden="true">✦</span>${handlingButton(t.id)}</button><button class="primary" data-action="${t.status === '已完成' ? 'reopen' : 'done'}">${t.status === '已完成' ? '重新打开' : '标记完成'}</button><button data-action="edit">编辑</button>${t.status === '待处理' && !t.archived ? `<button data-action="${t.deferred ? 'resume-today' : 'tomorrow'}">${t.deferred ? '恢复今日' : '明日处理'}</button>` : ''}<button data-action="archive-note">${t.archived ? '归档备注' : '归档'}</button><button data-action="merge">合并</button><button data-action="dismiss">无需处理</button></div>${t.deferred ? `<div class="deferred-note">已安排明日处理，${esc(date(t.deferred_until))} 自动恢复到今日事项。</div>` : ''}${t.schedule_id ? `<div class="helper">定时事项 · ${esc(t.scheduled_for)} 生成</div>` : ''}${t.blocker ? `<div class="block">有阻塞 · ${esc(t.blocker)}${t.waiting_for ? `<div class="muted">等待 ${esc(t.waiting_for)}</div>` : ''}</div>` : ''}${t.waiting_for && !t.blocker ? `<div class="waiting-note">等待 ${esc(t.waiting_for)} 的回复或处理结果</div>` : ''}${t.archive_note ? `<section class="archive-note"><h3>归档备注</h3><div class="description">${esc(t.archive_note)}</div><div class="helper">${esc(date(t.archived_at))} · 会作为后续汇总与建议的参考</div></section>` : ''}${t.description ? `<section class="detail-panel task-description"><h3>事项描述</h3><div class="description">${esc(t.description)}</div></section>` : ''}${t.due ? `<section><h3>截止时间</h3>${esc(dueLabel(t))}<div class="helper">后续对话有明确新证据时可自动更新期限</div></section>` : ''}${handlingPanel(t)}<details class="evidence-disclosure detail-panel"><summary>消息依据 <span data-evidence-count>${evidenceCount(t)} 条</span></summary><div id="evidence" data-preserve-content><div class="muted">读取中…</div></div>${detailCollapseFooter('消息依据')}</details>${taskChangesSection(t)}<section><h3>下一步</h3><div class="advice"><p>${esc(t.next_step || '尚无建议，可以先补充问题现象和预期结果。')}</p><button data-action="prompt">复制排查提示词</button></div></section>`,
  );
  if (taskChangeView(t.id).open) void renderTaskChanges(t.id);
  const host = $('#evidence'),
    disclosure = host.closest('details');
  const view = evidenceView(t.id);
  setEvidenceAvailability(disclosure, evidenceCount(t));
  disclosure.open = view.open;
  try {
    const rows = await rpc('evidence', { id: t.id });
    if (selected !== t.id || $('#evidence') !== host) return;
    evidenceCache[t.id] = [
      ...new Map(rows.filter((m) => !m.withdrawn).map((m) => [m.id, m])).values(),
    ];
    t.evidence_count = evidenceCache[t.id].length;
    setHtml(
      host,
      `<div class="evidence-tools"><input id="evidence-search" aria-label="搜索消息依据" maxlength="200" placeholder="搜索消息、发送人或来源" value="${esc(view.query)}"><button data-evidence-sort></button><span class="tiny" id="evidence-summary" role="status"></span></div><div id="evidence-retries"></div><div id="evidence-list"></div>`,
    );
    renderEvidence(t.id);
    const retries = [
      ...new Set([
        ...(t.evidence?.direct || []),
        ...(t.evidence?.background || []),
        ...(t.evidence?.completion || []),
      ]),
    ].filter((id) => state.analysis_retries?.[id]);
    if (retries.length)
      $('#evidence-retries').innerHTML =
        `<div class="box error-box"><b>${retries.length} 条依据待重试</b><p>${esc(state.analysis_retries[retries[0]])}</p><p class="helper">冲突解决后会在下一次完整分析中处理；多次冲突会停止自动重试并保留消息，可人工处理。</p></div>`;
  } catch (e) {
    if ($('#evidence') === host) host.textContent = e.message;
  }
}
const evidenceViews = new Map();
function evidenceView(id) {
  if (!evidenceViews.has(id)) evidenceViews.set(id, { query: '', sort: 'desc', open: false });
  return evidenceViews.get(id);
}
function setEvidenceAvailability(disclosure, total) {
  const summary = disclosure.querySelector('summary'),
    empty = total === 0;
  summary.setAttribute('aria-disabled', String(empty));
  summary.tabIndex = empty ? -1 : 0;
  if (empty) {
    evidenceView(selected).open = false;
    disclosure.open = false;
  }
}
function renderEvidence(id) {
  if (selected !== id || !$('#evidence-list')) return;
  const view = evidenceView(id),
    all = evidenceCache[id] || [],
    terms = view.query.trim().toLocaleLowerCase().split(/\s+/).filter(Boolean);
  const rows = all
    .filter((m) =>
      terms.every((term) =>
        [m.text, m.sender, m.group, m.chat, m.source_name]
          .join(' ')
          .toLocaleLowerCase()
          .includes(term),
      ),
    )
    .slice()
    .sort((a, b) => {
      const cmp = Number(a.time) - Number(b.time) || String(a.id).localeCompare(String(b.id));
      return view.sort === 'asc' ? cmp : -cmp;
    });
  setEvidenceAvailability($('#evidence').closest('details'), all.length);
  $('[data-evidence-count]').textContent = all.length + ' 条';
  $('#evidence-summary').textContent = terms.length
    ? `匹配 ${rows.length} / 共 ${all.length} 条`
    : `共 ${all.length} 条`;
  $('[data-evidence-sort]').textContent = view.sort === 'asc' ? '时间 ↑ 正序' : '时间 ↓ 倒序';
  $('[data-evidence-sort]').setAttribute(
    'aria-label',
    '消息依据' + (view.sort === 'asc' ? '按时间正序，切换倒序' : '按时间倒序，切换正序'),
  );
  setHtml(
    '#evidence-list',
    rows.length
      ? rows
          .map(
            (m) =>
              `<div class="source" data-ui-key="evidence-${esc(m.id)}"><div class="tiny">${esc(m.source_name || '历史来源')} · ${esc(m.group)} · ${esc(m.sender)} · ${esc(date(m.time))}</div>${messageBody(m.text || '[' + m.kind + ']')}${messageImages(m)}</div>`,
          )
          .join('')
      : `<div class="evidence-empty muted">${all.length ? '没有匹配的消息，试试其他关键词。' : '无消息来源，或来源已撤回。'}</div>`,
  );
}
document.addEventListener('input', (e) => {
  if (e.target.id === 'evidence-search' && !e.isComposing) {
    evidenceView(selected).query = e.target.value;
    renderEvidence(selected);
  }
});
document.addEventListener('compositionend', (e) => {
  if (e.target.id === 'evidence-search') {
    evidenceView(selected).query = e.target.value;
    renderEvidence(selected);
  }
});
document.addEventListener('click', (e) => {
  if (e.target.closest('[data-evidence-sort]')) {
    const v = evidenceView(selected);
    v.sort = v.sort === 'asc' ? 'desc' : 'asc';
    renderEvidence(selected);
  }
});
document.addEventListener(
  'click',
  (e) => {
    if (e.target.closest('.evidence-disclosure>summary[aria-disabled="true"]')) e.preventDefault();
  },
  true,
);
document.addEventListener(
  'keydown',
  (e) => {
    if (
      ['Enter', ' '].includes(e.key) &&
      e.target.matches('.evidence-disclosure>summary[aria-disabled="true"]')
    )
      e.preventDefault();
  },
  true,
);
document.addEventListener(
  'toggle',
  (e) => {
    if (e.target.matches('.evidence-disclosure')) {
      if (e.target.querySelector('summary').getAttribute('aria-disabled') === 'true')
        e.target.open = false;
      evidenceView(selected).open = e.target.open;
    }
  },
  true,
);
let promptDefaults = null;
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
  panel.innerHTML = `<div class="backlog-head"><h3>待分析消息</h3><span id="backlog-total">${state.pending || 0} 条</span></div><p>仅分析最近 ${state.config.analysis_lookback_hours ?? 24} 小时，更早消息自动排除。<span id="backlog-active">${state.analyzing ? `另有 ${state.analyzing} 条正在分析，不计入等待。` : ''}</span></p><div class="backlog-options"><label for="backlog-hours">本次跳过 <input id="backlog-hours" type="number" min="1" max="87600" value="${state.config.analysis_lookback_hours ?? 24}"> 小时前的消息</label><div class="backlog-preview-row"><p id="backlog-count" role="status">正在计算…</p><button id="backlog-apply" data-action="backlog-apply" disabled>跳过</button></div></div><div class="backlog-foot"><span>保留原文 · 不改设置</span><button class="backlog-run" data-action="backlog-analyze" ${state.ai_busy || state.analyzing || !state.pending ? 'disabled' : ''}>立即分析<span aria-hidden="true">→</span></button></div>`;
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
  if (
    sequence !== backlogSequence ||
    !$('#backlog-panel') ||
    Number($('#backlog-hours').value) !== hours
  )
    return;
  backlogPreview = result;
  $('#backlog-total').textContent = `${result.eligible} 条`;
  $('#backlog-count').textContent = result.count
    ? `共 ${result.eligible} 条，可跳过 ${result.count} 条`
    : '没有需要跳过的消息';
  $('#backlog-apply').disabled = !result.count;
  $('#backlog-active').textContent = result.analyzing
    ? `另有 ${result.analyzing} 条正在分析，不计入等待。`
    : '';
}
