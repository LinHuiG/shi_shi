const workLabels = {
  working: '工作中',
  overtime: '加班中',
  off_duty: '已下班',
  holiday: '假期',
  calendar_unknown: '日历未知',
};
function workStatusBar() {
  const ws = state?.work_state;
  if (!ws) return '<span class="helper">日历未就绪</span>';
  const cal = ws.calendar_day || {},
    status = ws.status || 'calendar_unknown';
  const source =
    { manual: '手动校正', cache: 'holiday-cn 缓存', weekend: '周末规则' }[cal.source] || '无来源';
  const stale = cal.stale ? ' · 缓存已过期' : '';
  const hint = `业务日 ${ws.business_date || '—'} · ${source}${stale}`;
  return `<button class="work-chip state-${esc(status)}" data-action="work-state" aria-haspopup="dialog" aria-controls="work-state-panel" title="${esc(hint)}">${esc(workLabels[status] || status)} · ${esc(ws.business_date || '—')}${status === 'calendar_unknown' ? ' · 请确认' : ''}</button>${status === 'working' ? '' : `<button class="quiet" data-action="overtime-toggle" title="加班只对当前业务日有效，下一业务日开始自动过期">${status === 'overtime' ? '结束加班' : '开启加班'}</button>`}`;
}
function closeWorkState(restore = false) {
  const panel = $('#work-state-panel');
  if (panel) motionExit(panel);
  const button = $('[data-action="work-state"]');
  button?.setAttribute('aria-expanded', 'false');
  if (restore) button?.focus({ preventScroll: true });
}
function positionWorkState() {
  const panel = $('#work-state-panel'),
    button = $('[data-action="work-state"]');
  if (!panel || !button) return;
  const rect = button.getBoundingClientRect();
  panel.style.left =
    Math.max(12, Math.min(rect.right - panel.offsetWidth, innerWidth - panel.offsetWidth - 12)) +
    'px';
  panel.style.top =
    Math.max(12, Math.min(rect.bottom + 8, innerHeight - panel.offsetHeight - 12)) + 'px';
}
function openWorkState() {
  if ($('#work-state-panel')) {
    closeWorkState(true);
    return;
  }
  closeBacklog();
  const ws = state?.work_state || {},
    cal = ws.calendar_day || {};
  const today = new Date().toLocaleDateString('en-CA', { timeZone: 'Asia/Shanghai' });
  const diagnostics = (cal.diagnostics || [])
    .map((d) =>
      d.kind === 'no_coverage'
        ? '无可用日历缓存'
        : d.kind === 'stale_cache'
          ? `缓存获取于 ${date(d.fetched_at_ms)}，已过期但仍可用`
          : d.kind,
    )
    .join('；');
  const panel = document.createElement('section');
  panel.id = 'work-state-panel';
  panel.className = 'backlog-panel work-state-panel';
  panel.setAttribute('role', 'dialog');
  panel.setAttribute('aria-label', '工作状态');
  panel.innerHTML = `<div class="backlog-head"><h3>工作状态</h3><button class="quiet" data-work-close aria-label="关闭工作状态">×</button></div><div class="work-state-summary"><b>${esc(workLabels[ws.status] || '未知')}</b><span class="helper">业务日 ${esc(ws.business_date || '—')}</span></div><p>自动 AI 在工作或加班时执行。手动操作不受时段限制。</p>${ws.status === 'working' ? '<p>当前在工作时段内。</p>' : `<button class="work-overtime" data-work-overtime="${ws.status === 'overtime' ? '0' : '1'}">${ws.status === 'overtime' ? '结束加班' : '开启加班'}</button><p>加班对当前业务日生效，下一业务日自动结束。</p>`}<details class="work-correction"><summary>校正日期</summary><div class="form-group"><label for="work-date">日期</label><input id="work-date" type="date" value="${esc(cal.date || today)}"></div><div class="form-group"><label for="work-kind">类型</label><select id="work-kind"><option value="workday">工作日 / 调休上班</option><option value="holiday" ${['holiday', 'weekend'].includes(cal.kind) ? 'selected' : ''}>假期 / 休息日</option></select></div><p>只影响所选日期，优先于日历缓存。</p><button class="primary" data-work-correct>保存校正</button></details><details><summary>日历详情</summary><p>日历日期 ${esc(cal.date || '—')} · ${esc({ workday: '工作日', weekend: '周末', holiday: '假期', unknown: '未知' }[cal.kind] || '未知')}</p><p>来源 ${esc({ manual: '手动校正', cache: 'holiday-cn 缓存', weekend: '周末规则' }[cal.source] || '无')}${cal.publication_year ? ` · 发布年份 ${esc(cal.publication_year)}` : ''}</p><p>${esc(diagnostics || (cal.kind === 'unknown' ? '没有可用日历缓存，请校正日期或开启加班。' : cal.stale ? '缓存已过期，将继续刷新。' : '日历缓存正常。'))}</p></details><div id="work-state-error" class="error-box" role="status"></div>`;
  document.body.append(panel);
  $('[data-action="work-state"]')?.setAttribute('aria-expanded', 'true');
  positionWorkState();
  panel.querySelector('[data-work-close]').focus({ preventScroll: true });
  panel.addEventListener('toggle', positionWorkState, true);
}
async function applyWorkState(params, message) {
  await rpc('work_state', params);
  closeWorkState(true);
  await refreshState(true);
  toast(message);
}
document.addEventListener('pointerdown', (e) => {
  if (!e.target.closest('#work-state-panel,[data-action="work-state"]')) closeWorkState();
});
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && $('#work-state-panel')) {
    e.preventDefault();
    closeWorkState(true);
  }
});
window.addEventListener('resize', positionWorkState);
window.addEventListener('blur', () => closeWorkState());
function archiveAttemptHistory(attempts) {
  return attempts.length
    ? `<details class="report-disclosure"><summary>归档执行记录 <span>${attempts.length} 次</span></summary>${attempts
        .slice()
        .reverse()
        .map(
          (a) =>
            `<div class="source"><b>${esc({ succeeded: '归档完成', failed: '归档未完成', running: '归档处理中', queued: '等待归档' }[a.status] || a.status)}</b><div class="tiny">${esc(date(a.finished || a.started || a.created))}</div><p>已处理 ${a.processed || 0} 条变更 · 已归档 ${a.archived_tasks || 0} 个事项 · 剩余 ${a.remaining || 0} 条</p>${a.error ? `<p>${esc(a.error)}</p>` : ''}</div>`,
        )
        .join('')}</details>`
    : '';
}
async function openArchiveHistory() {
  const history = await rpc('archive_history');
  const versions = history.versions || [],
    attempts = archiveAttemptHistory(history.attempts || []);
  if (!versions.length) {
    setModal(
      `<h2>归档版本历史</h2>${attempts}<p class="helper">尚无归档版本。</p><div class="dialog-foot"><button data-action="close">关闭</button></div>`,
    );
    return;
  }
  const rows = versions
    .slice()
    .reverse()
    .map(
      (v) =>
        `<div class="source"><div class="row"><b>${esc(v.action === 'rollback' ? '回滚版本' : '归档版本')}</b><span class="tiny">${esc(date(v.created))}</span>${v.id === history.current ? '<span class="badge green">当前</span>' : ''}</div><p class="helper">${esc(v.reason || '')}</p><button data-archive-view="${esc(v.id)}" ${v.id === history.current ? 'disabled' : ''}>比较并回滚到此版本</button></div>`,
    )
    .join('');
  setModal(
    `<h2>归档版本历史</h2>${attempts}<p class="helper">回滚只恢复提示词、规则和标签，保留当前 AI 记忆，不倒退消息进度，也不覆盖后续人工修改。</p>${rows}<div class="dialog-foot"><button data-action="close">关闭</button></div>`,
  );
}
async function compareArchiveVersion(id) {
  const [history, target] = await Promise.all([
    rpc('archive_history'),
    rpc('archive_version', { id }),
  ]);
  const currentId = history.current;
  if (!currentId || currentId === id) throw Error('当前版本无需回滚');
  const current = await rpc('archive_version', { id: currentId });
  const block = (title, a, b) =>
    `<details><summary>${esc(title)}</summary><div class="cols"><pre style="white-space:pre-wrap">${esc(a || '（空）')}</pre><pre style="white-space:pre-wrap">${esc(b || '（空）')}</pre></div></details>`;
  const rules = (x) => (x.rules || []).map((r) => `${r.title}：${r.rule}`).join('\n');
  const tags = (x) =>
    (x.tags || []).map((g) => `${g.name}：${(g.values || []).join('、')}`).join('\n');
  const memory = (x) => (x.memory || []).map((m) => `${m.key}：${m.text}`).join('\n');
  setModal(
    `<h2>比较归档版本</h2><p class="helper">左侧当前版本，右侧目标版本。确认后追加一个新的回滚版本。</p>${block('系统提示词', current.prompt, target.prompt)}${block('启用规则', rules(current), rules(target))}${block('标签目录', tags(current), tags(target))}<p class="helper">AI 记忆保持当前内容，不参与版本比较或回滚。</p><div id="modal-error" class="error-box"></div><div class="dialog-foot"><button data-action="close">取消</button><button class="primary" data-archive-rollback="${esc(id)}" data-expected="${esc(currentId)}">确认回滚</button></div>`,
  );
}
document.addEventListener('click', async (e) => {
  const b = e.target.closest('button');
  if (!b) return;
  const mine =
    b.dataset.action === 'work-state' ||
    b.dataset.action === 'overtime-toggle' ||
    b.dataset.action === 'archive-history' ||
    b.hasAttribute('data-work-close') ||
    b.hasAttribute('data-work-overtime') ||
    b.hasAttribute('data-work-correct') ||
    b.dataset.archiveView ||
    b.dataset.archiveRollback;
  if (!mine) return;
  try {
    if (b.hasAttribute('data-work-close')) {
      closeWorkState(true);
      return;
    }
    if (b.dataset.action === 'work-state') {
      openWorkState();
      return;
    }
    if (b.dataset.action === 'overtime-toggle') {
      const on = state?.work_state?.status !== 'overtime';
      b.disabled = true;
      await applyWorkState({ overtime: on }, on ? '已开启加班' : '已结束加班');
      return;
    }
    if (b.hasAttribute('data-work-overtime')) {
      b.disabled = true;
      await applyWorkState(
        { overtime: b.dataset.workOvertime === '1' },
        b.dataset.workOvertime === '1' ? '已开启加班' : '已结束加班',
      );
      return;
    }
    if (b.hasAttribute('data-work-correct')) {
      b.disabled = true;
      await applyWorkState(
        { date: $('#work-date').value, kind: $('#work-kind').value },
        '日期校正已保存',
      );
      return;
    }
    if (b.dataset.action === 'archive-history') {
      b.disabled = true;
      await openArchiveHistory();
      return;
    }
    if (b.dataset.archiveView) {
      b.disabled = true;
      await compareArchiveVersion(b.dataset.archiveView);
      return;
    }
    if (b.dataset.archiveRollback) {
      if (!confirm('确认回滚到此归档版本？只恢复提示词、规则和标签，保留当前 AI 记忆。')) return;
      b.disabled = true;
      await rpc('archive_rollback', {
        id: b.dataset.archiveRollback,
        expected: b.dataset.expected,
      });
      closeModal();
      await refreshState(true);
      toast('已追加回滚版本');
      return;
    }
  } catch (error) {
    if ($('#work-state-error')) $('#work-state-error').textContent = error.message;
    else if ($('#modal-error')) $('#modal-error').textContent = error.message;
    toast(error.message);
  } finally {
    b.disabled = false;
  }
});
