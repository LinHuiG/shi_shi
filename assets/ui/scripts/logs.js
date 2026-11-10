let logFilter = 'ai',
  logPurpose = 'all',
  logFeed = null,
  logFilterKey = '',
  logLoadedKey = '',
  logDay = '',
  logQuery = '',
  logSearchTimer = null;
let quickView = '';
const fieldNames = {
  ai_timeout_seconds: 'AI调用超时',
  deferred_until: '延后至',
  frequency: '重复频率',
  weekdays: '星期',
  time: '生成时间',
  analysis_related_only: '仅分析相关消息',
  message_filter_enabled: '消息过滤',
  model_profiles: '用途模型',
  report_prompt: '报表汇总提示词',
  classify_prompt: '消息分析提示词',
  format_prompt: '新建事项提示词',
  learning_prompt: '优化建议提示词',
  tags: '标签',
  archived_at: '归档时间',
  merged_into: '合并到',
  archive_note: '归档备注',
  archived: '已归档',
  model: '模型',
  reasoning_effort: '推理强度',
  category: '事项类型',
  title: '标题',
  description: '描述',
  priority: '优先级',
  environment: '环境',
  status: '状态',
  blocker: '阻塞点',
  waiting_for: '等待对象',
  due: '截止时间',
  enabled: '自动扫描',
  interval_minutes: '扫描间隔',
  analysis_interval_minutes: 'AI 分析间隔',
  leaders: '领导名单',
};
function changeRows(a) {
  if (!a.before || !a.after) return '';
  const rows = Object.entries(fieldNames).filter(
    ([key]) => JSON.stringify(a.before[key]) !== JSON.stringify(a.after[key]),
  );
  return rows.length
    ? `<details class="change-details"><summary>${rows.length} 个字段有变化</summary><div class="change-table">${rows
        .map(([k, label]) => {
          const value = (v) =>
            v == null || v === ''
              ? '未设置'
              : Array.isArray(v)
                ? v.map((item) => (typeof item === 'object' ? JSON.stringify(item) : item)).join('、')
                : typeof v === 'boolean'
                  ? v
                    ? '开启'
                    : '关闭'
                  : k === 'deferred_until'
                    ? v
                      ? date(v)
                      : '立即显示'
                    : typeof v === 'object'
                      ? JSON.stringify(v)
                      : String(v);
          return `<div><span>${label}</span><del>${esc(value(a.before[k]))}</del><span>→</span><strong>${esc(value(a.after[k]))}</strong></div>`;
        })
        .join('')}</div></details>`
    : '';
}
function usageCard(d) {
  const u = d.usage || {},
    known = Number.isFinite(u.input_tokens) && Number.isFinite(u.output_tokens),
    rate = generationSpeed(d.generation),
    speed = rate.known ? `<span class="usage-speed" title="${esc(rate.tip)}">平均速度 ${rate.text}</span>` : '';
  if (!known)
    return `<div class="usage-chips unknown">${esc(d.source.label)}：${d.usage_status === 'request_failed_without_complete_usage' ? '本次请求失败，未获得完整用量' : '未提供完整 token 用量'} · 不计入已确认总量${d.source?.driver === 'dsh' ? '（上下文容量不等于 token 用量）' : ''}${speed}</div>`;
  const cached = u.cached_input_tokens;
  return `<div class="usage-chips">${u.partial ? '<span>已返回部分用量，后续未确认部分仍记为未知</span>' : ''}<span>总计 <b>${(u.input_tokens + u.output_tokens).toLocaleString()}</b></span><span>输入 ${u.input_tokens.toLocaleString()}</span><span>输出 ${u.output_tokens.toLocaleString()}</span><span>缓存命中 ${cached == null ? '未提供' : cached.toLocaleString()}</span>${speed}</div>`;
}
function requestBatchSummary(detail) {
  const batch = detail.batch;
  if (!batch) return '';
  const count = (value) => Number.isSafeInteger(value) && value >= 0,
    format = (value) => value.toLocaleString('zh-CN'),
    total = batch.total_input_chars,
    limit = batch.max_input_chars,
    parts = [];
  if (count(total)) parts.push(`请求占用 ${format(total)}${count(limit) && limit > 0 ? '/' + format(limit) : ''} 字符`);
  if (count(batch.selected)) parts.push(`本批 ${batch.selected}${count(batch.pending) ? '/' + batch.pending : ''} 条`);
  if (count(batch.message_chars)) parts.push(`消息 ${format(batch.message_chars)} 字符`);
  const reduced = batch.reduced_by ?? [],
    reasons = [];
  if (reduced.includes('reply_context')) reasons.push('精简引用摘录');
  if (reduced.includes('task_summaries')) reasons.push('精简事项摘要');
  if (reduced.includes('input_budget')) reasons.push('按完整请求输入上限缩批');
  let adjustment = '';
  if (reasons.length) {
    const label = reduced.includes('input_budget')
      ? count(batch.initial_selected) && count(batch.selected) && batch.initial_selected > batch.selected
        ? `缩批 ${batch.initial_selected}→${batch.selected} 条（输入上限）`
        : '按输入上限缩批'
      : reasons.length === 1
        ? reasons[0]
        : '精简引用和事项摘要';
    adjustment = ` · <span title="${esc(reasons.join('；'))}">${esc(label)}</span>`;
  }
  return (parts.length ? ' · ' + esc(parts.join(' · ')) : '') + adjustment;
}
function shanghaiLogDay() {
  const parts = new Intl.DateTimeFormat('en-US', {
    timeZone: 'Asia/Shanghai',
    year: 'numeric',
    month: '2-digit',
    day: '2-digit',
  }).formatToParts(new Date());
  const get = (kind) => parts.find((p) => p.type === kind).value;
  return `${get('year')}-${get('month')}-${get('day')}`;
}
function releaseLogs() {
  clearTimeout(logSearchTimer);
  logFeed?.dispose();
  logFeed = null;
  logFilterKey = '';
  logLoadedKey = '';
  clearHtml($('#log-results'));
}
async function renderLogs() {
  if (page !== 'logs') return;
  const day = logDay || shanghaiLogDay(),
    params = { filter: logFilter, purpose: logFilter === 'ai' ? logPurpose : 'all', query: logQuery, day },
    filter = JSON.stringify(params);
  if (!logFeed || logFilterKey !== filter || !$('#log-results')) {
    releaseLogs();
    logFilterKey = filter;
    renderLogShell(day);
    const root = $('#log-results');
    setHtml(root, '');
    root.scrollTop = 0;
    logFeed = pageFeed({
      root,
      key: (a) => a.id,
      read: (p) => rpc('activity_query', { ...params, ...p }),
      alive: () => page === 'logs' && root.isConnected,
      render: (feed) => {
        if (feed.data) {
          state.activity_day_revision = feed.data.revision;
          renderedKey = viewKey(state, 'logs');
          logLoadedKey = JSON.stringify([filter, feed.data.revision]);
        }
        renderLogShell(day);
        const rows = feed.rows;
        setHtml(
          root,
          `${rows.length ? `<div class="timeline">${rows.map(logEntry).join('')}</div>` : !feed.loading && !feed.error ? empty('暂无记录', '所选日期和类型下没有记录。') : ''}${pageFeedStatus(feed)}`,
        );
        root.setAttribute('aria-busy', String(feed.loading));
        const summary = $('#log-count');
        if (summary)
          summary.textContent = feed.data
            ? `已加载 ${rows.length} / ${feed.data.total} 条`
            : feed.error
              ? '读取失败'
              : '正在查询…';
      },
    });
  }
  const key = JSON.stringify([filter, state.activity_day_revision]);
  if (key !== logLoadedKey) {
    logLoadedKey = key;
    await logFeed.load(logFeed.page > 0);
  } else logFeed.paint();
}
function logEntry(a) {
  const d = a.detail;
  let title = actionNames[a.kind] || '运行记录',
    body = '',
    bad = false;
  if (a.kind === 'resume_pending') {
    title = '继续上次未完成工作';
    body = `待生成 ${d.tasks} 个事项${d.scan ? ' · 重新扫描' : ''}${d.analysis ? ' · 重新分析' : ''}`;
  }
  if (a.kind === 'scan_clock') body = d.message;
  if (a.kind === 'scan') {
    const errors = d.errors;
    title = errors.length ? (d.sources > 0 ? '消息读取部分完成' : '消息读取失败') : '消息读取完成';
    body =
      (d.added || d.updated ? `新增 ${d.added} 条 · 更新 ${d.updated} 条` : '无新增或更新消息') +
      ` · ${(d.elapsed_ms / 1000).toFixed(1)} 秒` +
      (errors.length ? ' · ' + errors.join('；') : '');
    bad = errors.length > 0;
  }
  if (a.kind === 'ai_start') {
    title = state.ai_busy && Math.abs(a.time - state.ai_started) < 2000 ? '正在分析' : '分析开始 · 尚无结束记录';
    body =
      (d.batch ? '' : (d.messages ? `本批 ${d.messages} 条消息` : '文本整理请求') + ' · ') +
      `${aiModelLabel(d)} · ${d.reasoning_effort || '默认'} 推理`;
  }
  if (a.kind === 'ai_success') {
    title = d.analysis_degraded
      ? 'AI 返回降级结果 · 纠正后仍无结论'
      : d.kind === 'classify'
        ? 'AI 已返回分析结果'
        : 'AI 分析完成';
    body = `${aiModelLabel(d)} · ${d.reasoning_effort || '默认'} 推理 · ${Math.round(d.elapsed_ms / 1000)} 秒`;
  }
  if (a.kind === 'ai_cancelled') {
    title = 'AI 分析已中断';
    body = '本次任务已取消，自动执行设置保持不变；可手动重新执行。';
  }

  if (a.kind === 'analysis_skipped') {
    title = '跳过旧消息';
    body = `本次跳过 ${d.skipped} 条 · 保留原始消息`;
  }
  if (a.kind === 'archive_result') {
    title = d.remaining ? '归档批次已提交 · 继续处理' : '归档已提交';
    body = d.reused
      ? '无新增材料，已保存沿用的报表'
      : `处理 ${d.processed || 0} 条变更 · 剩余 ${d.remaining || 0} 条继续处理`;
  }
  if (a.kind === 'archive_failed') {
    title = '归档提交失败';
    body = d.error || '业务结果未提交，材料与游标已保留';
    bad = true;
  }
  if (a.kind === 'analysis_result') {
    const unchanged = d.applied === 0,
      reason = d.no_action_reason?.trim();
    title = d.analysis_degraded ? '分析降级接收 · 无事项变更' : unchanged ? '分析完成 · 无事项变更' : '事项已更新';
    body =
      `分析 ${d.processed} 条消息，新增或更新 ${d.applied} 个事项` +
      ` · 还有 ${d.remaining} 条等待分析` +
      (unchanged && reason ? ` · ${reason}` : '');
  }
  if (
    ['error', 'request_error', 'ai_failed', 'ai_rejected', 'generation_failed', 'analysis_rejected'].includes(a.kind)
  ) {
    title =
      a.kind === 'ai_rejected'
        ? 'AI 输入超限 · 未调用'
        : a.kind === 'ai_failed'
          ? d.response_received
            ? 'AI 已返回 · 结果处理失败'
            : 'AI 分析失败'
          : a.kind === 'analysis_rejected'
            ? '分析结果未采用 · 消息保留待处理'
            : '操作未完成';
    body = d.error || d.message || '请查看详细日志';
    bad = true;
  }
  if (['report', 'archive'].includes(d.kind)) {
    const name = d.kind === 'report' ? '报表生成' : '归档';
    const titles = {
      ai_start: name + '开始',
      ai_success: name + '完成',
      ai_failed: name + '失败',
      ai_cancelled: name + '已中断',
      ai_rejected: name + '输入超限 · 未调用',
    };
    title = titles[a.kind] || title;
    if (d.kind === 'archive' && a.kind === 'ai_success') title = '归档响应已返回 · 提交结果见归档记录';
  }
  if (d.kind === 'message_filter') {
    const titles = {
      ai_start: '消息过滤开始',
      ai_success: '消息过滤完成',
      ai_failed: '消息过滤失败',
      ai_cancelled: '消息过滤已中断',
      ai_rejected: '消息过滤输入超限 · 未调用',
    };
    title = titles[a.kind] || title;
  }
  if (a.kind.startsWith('tool_call_')) {
    bad =
      !!d.error ||
      ['failed', 'rejected', 'nonzero_exit', 'start_failed', 'output_error', 'timed_out'].includes(d.status);
    title = `工具调用 · ${d.tool || ''} · ${a.kind === 'tool_call_started' ? '开始' : toolStatus[d.status] || d.status || ''}`;
    body = `${toolPurposes[d.kind] || '历史用途'}${d.elapsed_ms == null ? '' : ` · ${d.elapsed_ms} ms`}${d.exit_code == null ? '' : ` · 退出码 ${d.exit_code}`}${d.error ? ` · ${d.error}` : ''}`;
  }
  if (d.kind === 'tool_call') title = title.replace('AI 分析', '工具调用');
  if (a.kind.startsWith('handling_')) {
    title = 'AI 处置 · ' + (handlingLabels[d.status] || d.status || '状态更新');
    body = d.error || d.conclusion || d.question || '';
    bad = d.status === 'failed';
  }
  if (a.kind === 'advice_auto_decision') {
    title = '次日建议自动评估';
    body = a.detail || '';
  }
  if (a.kind === 'codex_visibility') {
    title = 'Codex 前台提示';
    body = d.error || '';
  }
  if (d.context_usage?.image_calls) body += ` · 按需读图 ${d.context_usage.image_calls} 次`;
  if (d.trigger) {
    const triggers = {
      manual: '手动操作',
      message_volume: '消息体积触发',
      analysis_interval: '分析间隔触发',
      message_filter: '消息过滤提前触发',
      message_filter_failed: '过滤失败回退分析',
      scan_batch: '新增消息过滤',
      manual_archive: '手动归档',
      nightly_archive: '每日归档',
      manual_report: '手动报表',
      scheduled_report: '早晚定时报表',
      scheduled: '自动执行',
    };
    body += ' · 触发：' + (triggers[d.trigger] || d.trigger);
  }
  const batch = ['ai_start', 'ai_success', 'ai_failed', 'ai_cancelled', 'ai_rejected'].includes(a.kind)
    ? requestBatchSummary(d)
    : '';
  const tokenLine =
    ['ai_success', 'ai_failed', 'ai_cancelled'].includes(a.kind) || a.kind === 'handling_round'
      ? `<div class="event-description">${a.kind === 'ai_failed' ? esc(aiModelLabel(d) + ' · ' + (d.reasoning_effort || '默认') + ' 推理 · ' + Math.round(d.elapsed_ms / 1000) + ' 秒') : ''}</div>` +
        usageCard(d)
      : '';
  return `<article data-event-key="${esc(a.id)}" data-feed-key="${esc(a.id)}" class="event ${bad ? 'event-error' : ''}"><div class="event-dot"></div><div class="event-content"><div class="event-heading"><b>${esc(title)}</b><time>${esc(date(a.time))}</time>${a.audit && ['manual_edit', 'manual_status', 'manual_ai_edit', 'merge'].includes(a.kind) ? `<button class="quiet" data-undo="${esc(a.id)}">撤销</button>` : ''}</div>${a.task_number != null ? `<div class="event-task">#${esc(a.task_number)} ${esc(a.task_title)}</div>` : ''}<div class="event-description">${esc(body)}${batch}</div>${tokenLine}${d.thread_id && ['codex', 'dsh'].includes(d.source?.driver) ? `<button class="quiet" data-analysis-session="${esc(d.thread_id)}" data-session-driver="${esc(d.source.driver)}">${d.source.driver === 'dsh' ? '在 DSH 查看会话' : '在 Codex 查看'} ↗</button>` : ''}${d.run_id && a.kind.startsWith('tool_call_') ? `<button data-tool-log-run="${esc(d.run_id)}">查看调用详情</button>` : ''}${d.request_id && !a.kind.startsWith('tool_call_') && a.kind !== 'ai_rejected' ? `<button class="quiet" data-trace="${esc(d.request_id)}">查看提示词与输出 ↗</button>` : ''}${a.audit ? changeRows(a) : ''}</div></article>`;
}

function logUsageContent() {
  const u = state.usage;
  return `<div><span>今日尝试调用</span><b>${u.calls}<small> / ${state.config.daily_calls}</small></b></div><div><span>已确认 tokens</span><b>${(u.input_tokens + u.output_tokens).toLocaleString()}</b></div><div><span>输入 / 输出</span><b style="font-size:16px">${u.input_tokens.toLocaleString()} <small>/</small> ${u.output_tokens.toLocaleString()}</b></div><div><span>已返回缓存命中</span><b>${u.cached_reports ? u.cached_input_tokens.toLocaleString() : '—'}</b></div>`;
}
function syncLogUsage() {
  if (page === 'logs' && $('.usage-line')) setHtml('.usage-line', logUsageContent());
}
function renderLogShell(day) {
  setHtml(
    '#view',
    `<div class="activity-page"><div class="usage-line">${logUsageContent()}</div><div class="activity-toolbar"><div class="log-view-controls"><div class="pills">${[
      ['ai', 'AI 记录'],
      ['all', '全部记录'],
      ['changes', '事项变更'],
      ['errors', '异常'],
    ]
      .map(([k, v]) => `<button data-log-filter="${k}" class="${logFilter === k ? 'active' : ''}">${v}</button>`)
      .join('')}</div>${
      logFilter === 'ai'
        ? `<label class="log-purpose"><span>用途</span><select id="log-purpose" aria-label="AI 记录用途">${[
            ['all', '全部'],
            ['analysis', 'AI 分析'],
            ['message_filter', '消息过滤'],
            ['report', '报表'],
            ['archive', '归档'],
            ['format', '事项生成'],
            ['edit', '一句话修改'],
            ['tool_call', '工具调用'],
          ]
            .map(([key, label]) => `<option value="${key}" ${logPurpose === key ? 'selected' : ''}>${label}</option>`)
            .join('')}</select></label>`
        : '<span class="log-purpose-placeholder" aria-hidden="true"></span>'
    }</div><input class="log-search" id="log-search" type="search" maxlength="200" aria-label="搜索系统日志" placeholder="编号、事项或日志内容" value="${esc(logQuery)}"><div class="log-pagination"><label>日期 <input id="log-day" type="date" value="${esc(day)}"></label><span class="tiny" id="log-count"></span></div></div><div id="log-results" data-preserve-content tabindex="0" aria-label="日志记录"></div></div>`,
  );
  const input = $('#log-day');
  if (input.value !== day) input.value = day;
}

document.addEventListener('change', (e) => {
  if (e.target.id === 'log-day') {
    logDay = e.target.value;
    void renderLogs();
  }
  if (e.target.id === 'log-purpose') {
    logPurpose = e.target.value;
    void renderLogs();
  }
});

function queueLogSearch(el) {
  logQuery = el.value;
  releaseLogs();
  setHtml('#log-results', '<p class="helper" role="status">正在查询…</p>');
  clearTimeout(logSearchTimer);
  logSearchTimer = setTimeout(() => {
    if (page === 'logs') void renderLogs();
  }, 180);
}
document.addEventListener('input', (e) => {
  if (e.target.id === 'log-search' && !e.isComposing) queueLogSearch(e.target);
});
document.addEventListener('compositionend', (e) => {
  if (e.target.id === 'log-search') queueLogSearch(e.target);
});
