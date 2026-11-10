let archiveBannerTimer = null,
  reportViewRevision = null;
const reportViews = new Map(),
  reportOpened = new Set();
function releaseReports() {
  clearTimeout(archiveBannerTimer);
  archiveBannerTimer = null;
  reportViews.clear();
  reportViewRevision = null;
}
function reportView(r) {
  if (!reportViews.has(r.id))
    reportViews.set(r.id, { parts: new Map(), summary: null, stale: false, loading: false, error: '' });
  return reportViews.get(r.id);
}
function reportCount(r, section) {
  const count = {
    tasks: 'task_count',
    delta: 'delta_count',
    suggestions: 'suggestion_count',
    batch_summaries: 'batch_count',
  }[section];
  return r[count] ?? 0;
}
function reportRows(section, rows, offset = 0) {
  return rows
    .map((row, i) => {
      if (row.unavailable) return '<div class="source helper">该记录已过期或已清理</div>';
      if (section === 'tasks')
        return `<div class="source"><div class="row">${historicalTagBadges(row)}<span>${esc(row.title)}</span>${statusBadge(row)}</div>${row.waiting_for ? `<p>等待${esc(row.waiting_for)}</p>` : ''}${row.archive_note ? `<div class="archive-note"><span class="helper">归档备注</span><p>${esc(row.archive_note)}</p></div>` : ''}</div>`;
      if (section === 'delta')
        return `<div class="source"><span class="tiny">${esc(date(row.time))}</span><p>${esc(row.detail)}</p></div>`;
      if (section === 'suggestions')
        return `<div class="source"><div class="row">${esc(row.title)}<span class="badge ${row.active ? 'green' : ''}">${row.active ? '已启用' : '待处理 · 持续保留'}</span></div><p>${esc(row.rule)}</p></div>`;
      return `<div class="source"><b>第${offset + i + 1}批</b><p>${esc(row.summary)}</p>${['completed', 'attention', 'next_steps'].map((k) => (row[k] || []).map((v) => `<p>${esc(v)}</p>`).join('')).join('')}</div>`;
    })
    .join('');
}
function reportPart(r, section, title, unit) {
  const id = r.id,
    key = id + ':' + section,
    view = reportView(r),
    part = view.parts.get(section);
  const total = reportCount(r, section) - (section === 'batch_summaries' ? 1 : 0);
  const loaded = part?.rows.length || 0;
  return `<details class="report-disclosure" data-report-section="${section}" data-report-id="${esc(id)}" ${reportOpened.has(key) ? 'open' : ''}><summary>${title}<span>${Math.max(0, total)}${unit}</span></summary><div data-report-part-content>${section === 'suggestions' ? `<div class="row" style="margin:12px 0"><button data-page="learning">查看与处理迭代建议</button></div><p class="helper">${esc(r.learning?.message || r.learning?.skipped || '既有建议继续保留')}</p>` : ''}${part ? reportRows(section, part.rows) : ''}${part?.error ? `<p class="error-box">${esc(part.error)}</p>` : ''}${part?.loading ? '<p class="helper report-loading" role="status">正在读取…</p>' : !part && total > 0 ? '<p class="helper report-loading">展开后读取明细</p>' : part?.error || loaded < total ? `<button data-report-more="${section}" data-report-id="${esc(id)}">${part?.error ? '重试' : '加载更多'} · ${loaded}/${total}</button>` : total === 0 ? '<p class="helper">暂无记录</p>' : ''}</div></details>`;
}
function reportCard(r) {
  const ai = r.ai_summary,
    count = (status) => r.status_counts?.[status] ?? 0;
  return `<div class="report-body">${r.remaining_events ? `<p class="helper archive-coverage">本批已提交，仍有${r.remaining_events}条变更待处理；请以最近一次归档状态为准。</p>` : ''}${r.delta_omitted ? `<p class="helper">变更超出输入上限，最早的${r.delta_omitted}条未交给AI汇总。</p>` : ''}<p class="helper">本批归档${r.archived_tasks || 0}个事项${r.skipped_tasks ? ` · ${r.skipped_tasks}个事项处理期间发生变化，保留到下次归档` : ''}</p><div class="report-counts"><span><b>${count('待处理')}</b> 待处理</span><span><b>${count('处理中')}</b> 处理中</span><span><b>${count('已完成')}</b> 已完成</span><span>${reportCount(r, 'delta')}条新增变更</span></div><section class="report-summary"><div class="row"><h3>AI汇总</h3><span class="tiny">${r.summary_status === 'reused' ? '无新增变化 · 沿用上次汇总' : ''}</span></div>${
    ai
      ? `<p class="summary-lead">${esc(ai.summary)}</p><div class="report-columns">${[
          ['completed', '已完成', 'green'],
          ['attention', '需要关注', 'high'],
          ['next_steps', '下一步', 'blue'],
        ]
          .filter(([k]) => ai[k]?.length)
          .map(
            ([k, label, color]) =>
              `<div><h3><span class="badge ${color}">${label}</span></h3><ul>${ai[k].map((x) => `<li>${esc(x)}</li>`).join('')}</ul></div>`,
          )
          .join('')}</div>`
      : `<p class="muted">${r.summary_status === 'interrupted' ? '本次归档分析已中断，可点击“手动归档”重新执行。' : r.summary_status === 'pending' ? '尚未完成，可手动归档重新生成。' : r.summary_status === 'failed' ? `汇总失败：${esc(r.summary_error)}` : esc(r.summary_error || '该历史报表没有AI汇总，可生成新报表。')}</p>`
  }</section>${reportCount(r, 'batch_summaries') > 1 ? reportPart(r, 'batch_summaries', '本次前面批次的汇总', '批') : ''}${reportPart(r, 'tasks', '事项明细', '项')}${reportPart(r, 'suggestions', '持续跟踪的优化建议', '条')}${reportPart(r, 'delta', '本次增量记录', '条')}<div class="helper">消息扫描至${r.data_until ? esc(date(r.data_until)) : '尚未扫描'} · 待分析${r.pending_analysis || 0}条${r.previous_report?.time ? ` · 接续${esc(date(r.previous_report.time))}的汇总` : ''}</div></div>`;
}
function reportBodyContent(r, view) {
  if (view.summary)
    return `${view.error ? `<p class="error-box">${esc(view.error)}</p><button data-report-retry="${esc(r.id)}">重试</button>` : view.stale ? '<p class="helper" role="status">正在更新汇总…</p>' : ''}${reportCard(view.summary)}`;
  return view.error
    ? `<p class="error-box">${esc(view.error)}</p><button data-report-retry="${esc(r.id)}">重试</button>`
    : '<p class="helper report-loading" role="status">正在读取汇总…</p>';
}
function updateReportBody(id) {
  if (page !== 'reports') return;
  const r = state.reports.find((r) => r.id === id),
    host = document.querySelector(`[data-report-body="${CSS.escape(id)}"]`);
  if (r && host) setHtml(host, reportBodyContent(r, reportView(r)), { animate: false });
}
async function readReport(r, section, offset = 0) {
  return rpc('report_detail', { id: r.id, revision: state.reports_revision, section, offset, limit: 50 });
}
async function loadReport(r, section = 'summary') {
  const id = r.id,
    view = reportView(r),
    revision = reportViewRevision;
  const current = () => reportViews.get(id) === view && reportViewRevision === revision;
  let part;
  if (section === 'summary') {
    if ((view.summary && !view.stale) || view.loading) return;
    view.loading = true;
    view.error = '';
  } else {
    part = view.parts.get(section);
    if (!part) {
      part = { rows: [], next: 0, total: Infinity, loading: false, error: '' };
      view.parts.set(section, part);
    }
    if (part.loading || (!part.stale && part.next >= part.total)) return;
    part.loading = true;
    part.error = '';
  }
  updateReportBody(id);
  try {
    if (section === 'summary') {
      const result = await readReport(r, section);
      if (!current()) return;
      view.summary = result.report;
      view.stale = false;
    } else {
      const replacing = !!part.stale,
        rows = [];
      let offset = replacing ? 0 : part.next,
        result;
      const target = replacing ? Math.max(50, part.rows.length) : offset + 50;
      do {
        result = await readReport(r, section, offset);
        if (!current()) return;
        rows.push(...result.rows);
        if (result.next_offset <= offset) break;
        offset = result.next_offset;
      } while (offset < target && offset < result.total);
      if (replacing) part.rows = rows;
      else part.rows.push(...rows);
      part.next = result.next_offset;
      part.total = result.total;
      part.stale = false;
    }
  } catch (error) {
    if (!current()) return;
    if (part) part.error = error.message;
    else view.error = error.message;
  } finally {
    if (current()) {
      if (part) part.loading = false;
      else view.loading = false;
      updateReportBody(id);
      if (section === 'summary' && view.summary) ensureReportParts(r);
    }
  }
}
function ensureReportParts(r) {
  const id = r.id,
    view = reportView(r);
  for (const section of ['tasks', 'delta', 'suggestions', 'batch_summaries']) {
    if (reportOpened.has(id + ':' + section) && (!view.parts.has(section) || view.parts.get(section).stale))
      void loadReport(r, section);
  }
}
function visibleArchiveAttempt(attempt, at = Date.now()) {
  if (!attempt) return false;
  const outcome = state.report_outcome;
  if (
    ['failed', 'succeeded'].includes(attempt.status) &&
    outcome &&
    (outcome.kind !== 'archive' || outcome.id !== attempt.id)
  )
    return false;
  if (attempt.status !== 'succeeded') return true;
  const finished = Number(attempt.finished),
    day = (t) => new Date(t).toLocaleDateString('en-CA', { timeZone: 'Asia/Shanghai' });
  return finished > 0 && at - finished < 300000 && day(finished) === day(at);
}
function renderReports() {
  clearTimeout(archiveBannerTimer);
  const revision = state.reports_revision;
  if (reportViewRevision !== revision) {
    reportViewRevision = revision;
    for (const view of reportViews.values()) {
      view.stale = true;
      view.loading = false;
      view.error = '';
      for (const part of view.parts.values()) {
        part.stale = true;
        part.loading = false;
        part.error = '';
      }
    }
  }
  const reports = state.reports || [],
    ids = new Set(reports.map((r) => r.id));
  for (const id of reportViews.keys()) if (!ids.has(id)) reportViews.delete(id);
  if (reports.length) {
    const view = reportView(reports[0]);
    view.summary = reports[0];
    view.stale = false;
  }
  for (const key of reportOpened)
    if (!reports.some((r) => key === r.id || key.startsWith(r.id + ':'))) reportOpened.delete(key);
  const attempt = state.archive_attempt,
    visible = visibleArchiveAttempt(attempt);
  if (visible && attempt.status === 'succeeded') {
    const midnight = (Math.floor((Date.now() + 28800000) / 86400000) + 1) * 86400000 - 28800000;
    archiveBannerTimer = setTimeout(
      () => {
        if (page === 'reports') renderReports();
      },
      Math.max(1, Math.min(attempt.finished + 300000, midnight) - Date.now()),
    );
  }
  const banner = visible
    ? `<div class="box ${attempt.status === 'failed' ? 'error-box' : ''}"><b>${attempt.status === 'failed' ? (attempt.batches ? '归档部分已提交，后续未完成' : '最近一次归档失败，本次未提交') : attempt.status === 'succeeded' ? '归档完成' : '归档处理中'}</b>${attempt.error ? `<p>${esc(attempt.error)}</p>` : ''}<p class="tiny">${esc(date(attempt.finished || attempt.started || attempt.created))}</p><p class="helper">已处理 ${attempt.processed || 0} 条变更 · 已归档 ${attempt.archived_tasks || 0} 个事项 · 剩余 ${attempt.remaining || 0} 条${attempt.status === 'failed' ? '；已提交批次保留，未处理材料和游标保留；不会自动重试，请手动继续归档。' : ''}</p></div>`
    : '';
  const outcome = state.report_outcome,
    // A failed summary already shows in the latest report's card.
    shown =
      outcome?.kind !== 'archive' &&
      reports[0]?.summary_status === 'failed' &&
      reports[0].summary_error === outcome?.error,
    failure =
      outcome?.status === 'failed' && !shown && !(visible && outcome.kind === 'archive' && outcome.id === attempt.id)
        ? `<div class="box error-box report-failure"><b>${esc(outcome.label)}失败</b>${outcome.error ? `<p>${esc(outcome.error)}</p>` : ''}<p class="tiny">${esc(date(outcome.finished))}</p></div>`
        : '';
  setHtml(
    '#view',
    `<div class="page report-page">${reviewTabs()}${failure}${banner}${
      reports.length
        ? reports
            .map((r, i) => {
              const id = r.id,
                view = reportView(r),
                body = `<div data-report-body="${esc(id)}">${i === 0 || reportOpened.has(id) ? reportBodyContent(r, view) : ''}</div>`;
              return i === 0
                ? `<article class="report-card" data-report-key="${esc(id)}"><header><div class="tiny">最新汇总</div><h2>${esc(r.label)} <span>${esc(r.date)}</span></h2><time>${esc(date(r.time))}</time></header>${body}</article>`
                : `<details class="report-card report-history" data-report-key="${esc(id)}" ${reportOpened.has(id) ? 'open' : ''}><summary><span>${esc(r.label)}</span><time>${esc(date(r.time))}</time></summary>${body}</details>`;
            })
            .join('')
        : empty('暂无归档汇总', '点击“手动归档”保存已结束事项并调用AI分析；自动执行时间可在设置中调整。')
    }</div>`,
    { animate: false },
  );
  for (const [index, r] of reports.entries())
    if (index === 0 || reportOpened.has(r.id)) {
      const view = reportView(r);
      if (!view.summary || view.stale) void loadReport(r);
      else ensureReportParts(r);
    }
}
document.addEventListener(
  'toggle',
  (event) => {
    const el = event.target;
    if (page !== 'reports' || !el.isConnected || !el.matches('.report-history,.report-disclosure[data-report-section]'))
      return;
    const id = el.dataset.reportId || el.dataset.reportKey,
      r = state.reports.find((r) => r.id === id);
    if (!r) return;
    const section = el.dataset.reportSection,
      key = section ? id + ':' + section : id;
    if (el.open) reportOpened.add(key);
    else reportOpened.delete(key);
    if (el.open) {
      if (section) {
        const part = reportView(r).parts.get(section);
        if (!part || part.stale) void loadReport(r, section);
      } else {
        updateReportBody(id);
        void loadReport(r);
        ensureReportParts(r);
      }
    }
  },
  true,
);
document.addEventListener('click', (event) => {
  const button = event.target.closest('[data-report-more],[data-report-retry]');
  if (!button) return;
  const id = button.dataset.reportId || button.dataset.reportRetry,
    r = state.reports.find((r) => r.id === id);
  if (r) void loadReport(r, button.dataset.reportMore || 'summary');
});

const actionNames = {
  scan_clock: '读取断点已校正',
  manual_ai_edit: '一句话修改事项',
  schedule_edit: '保存定时事项',
  schedule_delete: '删除定时事项',
  schedule_generate: '定时生成事项',
  generation_start: '开始创建事项',
  generation_done: '事项已生成',
  generation_failed: '事项生成失败',
  scan_control: '调整自动扫描',
  settings: '修改设置',
  manual_edit: '人工编辑 / 补建',
  manual_status: '人工修改状态',
  defer_resumed: '延后到期恢复处理',
  ai_create: 'AI 新建事项',
  ai_update: 'AI 更新事项',
  merge: '合并事项',
  undo: '撤销修改',
  deadline_update: '对话更新截止时间',
  rule_change: '规则变更',
  archive: '归档',
  scan_gap: '扫描缺口',
};
