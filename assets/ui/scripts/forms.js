let modalSchedule = null;
const weekdayNames = ['一', '二', '三', '四', '五', '六', '日'];
function scheduleLabel(s) {
  return (
    (s.frequency === 'daily'
      ? '每日'
      : '每周' + (s.weekdays || []).map((d) => weekdayNames[d - 1]).join('、')) +
    ' · ' +
    s.time
  );
}
function renderSchedules() {
  const schedules = state.schedules || [];
  setHtml(
    '#view',
    `<div class="page schedule-page"><div class="schedule-intro">定时生成事项${help('应用运行时生效；重新打开只补当天已到点的事项，不补往日。')}</div>${schedules.length ? schedules.map((s) => `<article class="schedule-card ${s.enabled ? '' : 'paused'}"><div class="schedule-heading"><h2>${esc(s.title)}</h2><span class="badge ${s.enabled ? 'normal' : ''}">${s.enabled ? '已启用' : '已暂停'}</span></div><div class="schedule-timing">${esc(scheduleLabel(s))} </div>${s.description ? `<div class="description">${esc(s.description)}</div>` : ''}<div class="row">${extraTagBadges(s)}</div><div class="helper" style="margin-top:10px">${s.enabled ? (s.next_at ? '下次生成 ' + esc(date(s.next_at)) : '等待下次生成时间') : '暂停期间不生成事项'}${s.last_generated_on ? ' · 上次生成 ' + esc(s.last_generated_on) : ''}</div><div class="schedule-actions"><button data-schedule-edit="${s.id}">编辑</button><button data-schedule-toggle="${s.id}">${s.enabled ? '暂停' : '启用'}</button><button class="quiet" data-schedule-remove="${s.id}">删除</button></div></article>`).join('') : empty('暂无定时事项', '自己填写规则，或输入“每个工作日 9:00 整理本周计划”。', '<button class="primary" data-action="new-schedule">新建定时事项</button>')}</div>`,
  );
}
function scheduleForm(
  s = {
    title: '',
    description: '',
    frequency: 'daily',
    weekdays: [1],
    time: '09:00',
    tags: [],
    priority: '',
    environment: '',
    category: '',
    enabled: true,
  },
  natural = '',
) {
  modalSchedule = structuredClone(s);
  setModal(
    `<div class="schedule-form"><h2>${s.id ? '编辑定时事项' : '新建定时事项'}</h2><div class="subtitle">到点生成一条待处理事项。</div><div class="schedule-natural"><label for="schedule-natural">一句话填写</label><textarea id="schedule-natural" maxlength="6000" placeholder="例如：每个工作日早上9点整理本周计划；或每周五下午3点准备发布">${esc(natural)}</textarea><button data-action="format-schedule">用一句话填写表单</button><div class="helper">由所选 AI 接入整理，填好后可修改并保存。</div></div><div class="form-group"><label for="s-title">事项标题</label><input id="s-title" maxlength="200" value="${esc(s.title)}" placeholder="例如：整理本周计划"></div><div class="cols"><div class="form-group"><label for="s-frequency">重复频率</label><select id="s-frequency"><option value="daily" ${s.frequency === 'daily' ? 'selected' : ''}>每日</option><option value="weekly" ${s.frequency === 'weekly' ? 'selected' : ''}>每周</option></select></div><div class="form-group"><label for="s-time">生成时间</label><input id="s-time" type="time" value="${esc(s.time)}"></div></div><div class="form-group" id="s-weekday-group" ${s.frequency === 'weekly' ? '' : 'hidden'}><label>重复星期（可多选）</label><div class="weekdays">${weekdayNames.map((d, i) => `<label><input type="checkbox" name="s-weekday" value="${i + 1}" ${(s.weekdays || []).includes(i + 1) ? 'checked' : ''}>周${d}</label>`).join('')}</div></div><div class="form-group"><label for="s-description">事项描述（可选）</label><textarea id="s-description" maxlength="6000" rows="3">${esc(s.description)}</textarea></div>${taskTagEditor(s)}<label class="check"><input id="s-enabled" type="checkbox" ${s.enabled ? 'checked' : ''}>启用此定时事项</label><div class="helper">新建、修改时间或重新启用后，从下一个到点时间开始；同一规则每天最多生成一次。编辑或删除规则不会改动已生成事项。</div><div id="modal-error" class="error-box"></div><div class="dialog-foot"><button data-action="close">取消</button><button class="primary" data-action="save-schedule">保存定时事项</button></div></div>`,
  );
  $('#s-title').focus();
}
function readScheduleForm() {
  return {
    ...modalSchedule,
    title: $('#s-title').value,
    description: $('#s-description').value,
    frequency: $('#s-frequency').value,
    weekdays:
      $('#s-frequency').value === 'weekly'
        ? [...document.querySelectorAll('[name="s-weekday"]:checked')].map((el) => Number(el.value))
        : [],
    time: $('#s-time').value,
    tags: readTaskTags(),
    priority: '',
    category: '',
    environment: '',
    enabled: $('#s-enabled').checked,
  };
}
document.addEventListener('change', (e) => {
  if (e.target.id === 's-frequency') $('#s-weekday-group').hidden = e.target.value !== 'weekly';
});
function newModal(text = '', sources = []) {
  manualSources = sources;
  modalTask = null;
  setModal(
    `<h2>新建事项</h2><div class="subtitle">描述需要做的事情，所选 AI 仅整理成事项字段。</div><div class="form-group"><textarea id="natural-input" placeholder="例如：明天下午前确认测试环境的接口格式，先等测试补充样例。">${esc(text)}</textarea></div><div class="helper">不会执行任务、排查问题或读取额外资料。会在后台生成并直接保存，你可以随时修改。一句话可建立多条事项。</div><div id="modal-error" class="error-box"></div><div class="dialog-foot"><button data-action="close">取消</button><button data-action="manual-form">手动填写</button><button class="primary" data-action="format-new">整理成事项</button></div>`,
  );
}
function formModal(t) {
  modalTask = structuredClone(t);
  setModal(
    `<h2>${t.id ? '编辑事项' : '新建事项'}</h2>${t.id ? `<div class="schedule-natural"><label for="edit-instruction">一句话修改</label><textarea id="edit-instruction" maxlength="6000" rows="3" placeholder="例如：问题已定位，现在等测试验证，优先级改为高，截止时间延到明天17点。"></textarea><button data-action="ai-edit-task">AI 分析并更新</button>${help('仅修改当前事项，未涉及的内容保留；下方仍可手动编辑。')}</div>` : ''}<div class="form-group"><label>标题</label><input id="f-title" value="${esc(t.title)}"></div><div class="form-group"><label>描述</label><textarea id="f-description">${esc(t.description)}</textarea></div>${taskTagEditor(t, true)}<div class="cols"><div class="form-group"><label>状态</label><select id="f-status">${['待处理', '已完成', '无需处理', '已取消'].map((p) => `<option ${p === t.status ? 'selected' : ''}>${p}</option>`).join('')}</select></div></div><details class="task-other-settings" ${t.id && (t.due || t.blocker?.trim() || t.waiting_for?.trim() || t.next_step?.trim()) ? 'open' : ''}><summary>其他设置</summary><div class="form-group"><label>截止时间</label><input id="f-due" type="datetime-local" value="${esc(localDue(t.due))}"></div><div class="form-group"><label>阻塞点</label><input id="f-blocker" value="${esc(t.blocker)}"></div><div class="form-group"><label>等待谁</label><input id="f-wait" value="${esc(t.waiting_for)}"></div><div class="form-group"><label>下一步</label><input id="f-next" value="${esc(t.next_step)}"></div></details><div id="modal-error" class="error-box"></div><div class="dialog-foot"><button data-action="close">取消</button><button class="primary" data-action="save-task">保存事项</button></div>`,
  );
}
function readTaskForm() {
  return {
    ...modalTask,
    tags: readTaskTags(),
    title: $('#f-title').value,
    description: $('#f-description').value,
    priority: '',
    category: '',
    environment: '',
    status: $('#f-status').value,
    due: $('#f-due').value ? $('#f-due').value + ':00+08:00' : '',
    follow_deadline: true,
    blocker: $('#f-blocker').value,
    waiting_for: $('#f-wait').value,
    next_step: $('#f-next').value,
  };
}
function blankTask() {
  return {
    title: '',
    description: '',
    tags: [],
    priority: '',
    category: '',
    environment: '',
    status: '待处理',
    blocker: '',
    waiting_for: '',
    next_step: '',
    due: '',
    evidence: { direct: manualSources, background: [], completion: [] },
    updated: 0,
    follow_deadline: true,
  };
}
document.addEventListener('input', (e) => {
  if (e.target.id === 'backlog-hours') {
    backlogPreview = null;
    $('#backlog-apply').disabled = true;
    $('#backlog-count').textContent = '正在计算…';
    clearTimeout(backlogTimer);
    backlogTimer = setTimeout(() => previewBacklog().catch((e) => toast(e.message)), 300);
  }
  if (page === 'settings' && e.target.id.startsWith('cfg-')) settingsDirty = true;
  if (e.target.id === 'task-search') {
    query = e.target.value;
    const pos = e.target.selectionStart;
    renderTasks();
    $('#task-search').focus();
    $('#task-search').setSelectionRange(pos, pos);
  }
  if (e.target.id === 'inbox-search' && !e.isComposing) {
    inboxQuery = e.target.value;
    renderInbox();
  }
});
