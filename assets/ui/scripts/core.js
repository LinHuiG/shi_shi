function rpcError(message) {
  return String(message).startsWith('AI_UNCONFIGURED:')
    ? Object.assign(Error(''), { code: 'AI_UNCONFIGURED' })
    : Error(message);
}
let uiReady = false;
let seq = 0;
const pending = new Map();
function rpcRaw(method, params = {}) {
  return new Promise((resolve, reject) => {
    const id = ++seq;
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(Error('请求超时，请查看日志；不要重复提交同一操作。'));
    }, 300000);
    pending.set(id, { resolve, reject, timer });
    window.ipc.postMessage(JSON.stringify({ id, method, params }));
  });
}
window.receive = (v) => {
  const p = pending.get(v.id);
  if (!p) return;
  clearTimeout(p.timer);
  pending.delete(v.id);
  v.ok ? p.resolve(v.data) : p.reject(rpcError(v.error));
};
const longAiMethods = new Set([
  'tool_test',
  'scan',
  'analyze',
  'format',
  'schedule_format',
  'edit_ai',
  'report',
  'archive',
  'learn',
  'source_range',
  'prompt_retry',
  'handling_learn',
]);
async function rpc(method, params = {}) {
  if (!longAiMethods.has(method) || !state?.async_ai_requests) return rpcRaw(method, params);
  const accepted = await rpcRaw('ai_job_start', { method, params });
  if (!accepted?.job_id) throw Error('后台操作未返回编号，请查看日志；不要重复提交。');
  for (;;) {
    let result;
    try {
      result = await rpcRaw('ai_job_result', { job_id: accepted.job_id });
    } catch (error) {
      throw Error(
        `查询后台操作 ${accepted.job_id} 失败：${error.message}；后台可能仍在执行，请查看状态与日志。`,
      );
    }
    if (result.done) {
      if (!result.ok) throw rpcError(result.error);
      return result.data;
    }
    await new Promise((resolve) => setTimeout(resolve, 750));
  }
}
const $ = (s) => document.querySelector(s);
const esc = (s) =>
  String(s ?? '').replace(
    /[&<>"']/g,
    (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c],
  );
function recordedAiSource(detail) {
  if (detail?.source) return detail.source;
  let route;
  try {
    route = JSON.parse(detail?.model);
  } catch {}
  return Array.isArray(route) && route.length === 2 && route.every((v) => typeof v === 'string')
    ? {
        driver: 'dsh',
        connection_name: 'DSH（历史记录）',
        provider: route[0],
        model_name: route[1],
        label: 'DSH（历史记录） / ' + route[0],
      }
    : null;
}
function aiSourceLabel(source) {
  return source?.label || source?.connection_name || '来源未记录';
}
function aiModelLabel(detail) {
  const source = recordedAiSource(detail);
  let route;
  try {
    route = JSON.parse(detail.model);
  } catch {}
  const provider = source?.provider || (Array.isArray(route) ? route[0] : '');
  const model =
    source?.model_name || (Array.isArray(route) ? route[1] : detail.model) || '默认模型';
  return `${aiSourceLabel(source)}${!source?.provider && provider ? ' / ' + provider : ''} · ${model}`;
}
const aiLiveSpeedTip = '最近 1 秒 API 已上报输出 tokens 的增量；每秒计算一次。';
const aiSpeedTip =
  'DSH 均值：已完成且有用量与时间记录的步骤，其输出 tokens 总数 ÷ 首个输出到结束的耗时总和；排除首次输出前的等待、工具及本地校验时间。';
const liveTokenSample = {};
function tokenSpeed(output, elapsed) {
  const rate =
    Number.isFinite(output) && output >= 0 && Number.isFinite(elapsed) && elapsed > 0
      ? (output * 1000) / elapsed
      : NaN;
  return Number.isFinite(rate)
    ? rate.toLocaleString('en-US', { minimumFractionDigits: 1, maximumFractionDigits: 1 })
    : '—';
}
function generationSpeed(stats) {
  const value = stats?.steps > 0 ? tokenSpeed(stats.output_tokens, stats.elapsed_ms) : '—';
  return {
    known: value !== '—',
    average: true,
    text: value === '—' ? '' : value + ' tokens/s',
    tip: aiSpeedTip,
  };
}
function requestMetrics(requestId = '') {
  const live = state?.ai_live,
    liveMatches = live && (!requestId || live.request_id === requestId);
  if (state?.ai_busy && liveMatches)
    return { generation: live.generation, first: live.first_response_ms };
  const terminal = (state?.activity || []).find(
    (a) =>
      ['ai_success', 'ai_failed', 'ai_cancelled'].includes(a.kind) &&
      (!requestId || a.detail?.request_id === requestId),
  );
  if (terminal) {
    const d = terminal.detail;
    return { generation: d.generation, first: d.first_response_ms };
  }
  return liveMatches ? { generation: live.generation, first: live.first_response_ms } : {};
}
function sampleTokenRate(sample, key, count, at) {
  if (
    sample.key !== key ||
    !Number.isFinite(count) ||
    count < 0 ||
    !Number.isFinite(sample.count) ||
    count < sample.count ||
    at < sample.at
  ) {
    Object.assign(sample, {
      key,
      count: Number.isFinite(count) && count >= 0 ? count : null,
      at,
      rate: null,
    });
    return null;
  }
  const elapsed = at - sample.at;
  if (elapsed >= 1000) {
    sample.rate = elapsed <= 1500 ? ((count - sample.count) * 1000) / elapsed : null;
    sample.count = count;
    sample.at = at;
  }
  return sample.rate;
}
function requestSpeed(requestId = '') {
  const live = state?.ai_live;
  if (state?.ai_busy && live && (!requestId || live.request_id === requestId)) {
    const key = live.request_id + ':' + (state.ai_started || 0),
      at = Date.now();
    if (liveTokenSample.key !== key) Object.assign(liveTokenSample, { key, api: {}, shown: false });
    if (!Number.isFinite(live.first_response_ms))
      return { known: false, average: false, text: '', tip: aiLiveSpeedTip };
    const api = liveTokenSample.api,
      previousAt = api.at,
      apiRate = sampleTokenRate(api, key, live.usage_final ? null : live.output_tokens, at);
    if (api.at !== previousAt) {
      const reported = live.usage_elapsed_ms;
      api.fresh =
        Number.isFinite(api.reported) && reported > api.reported && reported - api.reported <= 1500;
      api.reported = Number.isFinite(live.output_tokens) && !live.usage_final ? reported : null;
    }
    if (Number.isFinite(apiRate) && api.fresh) {
      liveTokenSample.shown = true;
      return {
        known: true,
        average: false,
        text: tokenSpeed(apiRate, 1000) + ' tokens/s',
        tip: aiLiveSpeedTip,
      };
    }
    const mean = generationSpeed(live.generation);
    if (mean.known) {
      liveTokenSample.shown = true;
      return mean;
    }
    return {
      known: liveTokenSample.shown,
      average: false,
      text: liveTokenSample.shown ? '0.0 tokens/s' : '',
      tip: aiLiveSpeedTip,
    };
  }
  return generationSpeed(requestMetrics(requestId).generation);
}
function firstResponseLabel(value) {
  return Number.isFinite(value) && value >= 0
    ? value < 1000
      ? value.toLocaleString('en-US') + ' ms'
      : (value / 1000).toLocaleString('en-US', {
          minimumFractionDigits: 2,
          maximumFractionDigits: 2,
        }) + ' s'
    : '—';
}
function analysisProgressText() {
  const live = state?.ai_live,
    text =
      (live?.entries || []).at(-1)?.text ||
      live?.phase ||
      state?.ai_progress ||
      '正在连接，等待公开内容';
  return (
    Array.from(
      String(text)
        .split(/\r?\n/)
        .map((s) => s.trim())
        .filter(Boolean)
        .at(-1) || '',
    )
      .slice(-260)
      .join('') || '等待公开内容'
  );
}
const date = (t) =>
  t
    ? new Date(t).toLocaleString('zh-CN', { timeZone: 'Asia/Shanghai', hour12: false })
    : '尚未扫描';
let state = null,
  page = 'tasks',
  selected = '',
  tab = '待处理',
  query = '',
  inboxQuery = '',
  settingsDirty = false,
  modalTask = null,
  manualSources = [],
  toastTimer,
  evidenceCache = {},
  modalFromTrace = false;
const names = {
  tasks: '事项',
  schedules: '定时事项',
  inbox: '消息收件箱',
  reports: '报表与归档',
  learning: '迭代建议',
  logs: '系统日志',
  settings: '设置',
  sources: '消息来源',
  prompts: '系统提示词',
  guide: '使用说明',
};
function toast(s) {
  if (!s) return;
  $('#toast').textContent = s;
  $('#toast').hidden = false;
  clearTimeout(toastTimer);
  motionAnimate($('#toast'), [{ opacity: 0.3 }, { opacity: 1 }]);
  toastTimer = setTimeout(() => motionHide($('#toast')), 6000);
}
function dueLabel(t) {
  if (!t.due) return '';
  const d = new Date(t.due);
  if (!Number.isFinite(d.getTime())) return t.due;
  const hours = (d - Date.now()) / 3600000;
  return (hours < 0 ? '已逾期 · ' : hours < 24 ? '即将截止 · ' : '截止 ') + date(d.getTime());
}
function localDue(d) {
  return d ? new Date(new Date(d).getTime() + 8 * 3600000).toISOString().slice(0, 16) : '';
}
function badge(p) {
  return `<span class="badge ${p === '紧急' ? 'urgent' : p === '高' ? 'high' : p === '已完成' ? 'green' : p === '普通' ? 'normal' : p === '低' ? 'low' : ''}">${esc(p)}</span>`;
}
function empty(title, text, button = '') {
  return `<div class="empty"><div class="empty-icon">▤</div><div>${title}</div><p>${text}</p>${button}</div>`;
}
let modalClosing = null,
  modalOutsideDismiss = null,
  modalBack = null;
function setModal(html) {
  if (typeof cancelEmbeddedPluginSettings === 'function' && cancelEmbeddedPluginSettings() === false) return;
  const overlay = $('#overlay'),
    closing = modalClosing,
    opacity = getComputedStyle(overlay).opacity;
  modalClosing = null;
  closing?.animation.cancel();
  overlay.inert = false;
  delete overlay.dataset.closing;
  modalFromTrace = false;
  modalOutsideDismiss = null;
  modalBack = null;
  $('#dialog').classList.remove('wide-dialog', 'live-dialog', 'approval-shell', 'plugin-settings-dialog', 'plugin-settings-compact', 'tool-editor-dialog', 'tool-history-dialog', 'tool-detail-dialog');
  $('#dialog').innerHTML = html;
  overlay.hidden = false;
  if (closing) motionAnimate(overlay, [{ opacity }, { opacity: 1 }]);
  $('#dialog').querySelector('textarea,input')?.focus({ preventScroll: true });
  motionAnimate($('#dialog'), [
    { opacity: 0.4, transform: 'translateY(4px)' },
    { opacity: 1, transform: 'none' },
  ]);
}
function modalReturnPoint() {
  if ($('#overlay').hidden || modalClosing) return null;
  const dialog = $('#dialog'), nodes = [...dialog.childNodes], classes = dialog.className,
    scroll = dialog.scrollTop, focused = document.activeElement, task = modalTask,
    outside = modalOutsideDismiss, back = modalBack, fromTrace = modalFromTrace;
  return () => {
    setModal('');
    dialog.className = classes;
    dialog.replaceChildren(...nodes);
    modalTask = task;
    modalOutsideDismiss = outside;
    modalBack = back;
    modalFromTrace = fromTrace;
    dialog.scrollTop = scroll;
    if (focused?.isConnected) focused.focus({ preventScroll: true });
  };
}
async function copyText(text) {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {}
  const focused = document.activeElement, field = document.createElement('textarea');
  field.value = text;
  field.readOnly = true;
  field.style.cssText = 'position:fixed;left:-9999px;top:0';
  document.body.append(field);
  field.focus({ preventScroll: true });
  field.select();
  try { return !!document.execCommand('copy'); }
  catch { return false; }
  finally {
    field.remove();
    if (focused?.isConnected) focused.focus({ preventScroll: true });
  }
}
function closeModal() {
  const overlay = $('#overlay');
  if (overlay.hidden || modalClosing || $('#dialog').querySelector('[data-pending="true"]')) return;
  if (modalBack) {
    const back = modalBack;
    modalBack = null;
    back();
    return;
  }
  if (typeof cancelEmbeddedPluginSettings === 'function') cancelEmbeddedPluginSettings();
  modalTask = null;
  modalFromTrace = false;
  modalOutsideDismiss = null;
  const animation = motionAnimate(
    overlay,
    [{ opacity: getComputedStyle(overlay).opacity }, { opacity: 0 }],
    { fill: 'forwards' },
  );
  if (!animation) {
    overlay.hidden = true;
    return;
  }
  const closing = { animation };
  modalClosing = closing;
  overlay.inert = true;
  overlay.dataset.closing = 'true';
  animation.finished.then(
    () => {
      if (modalClosing !== closing) return;
      overlay.hidden = true;
      overlay.inert = false;
      delete overlay.dataset.closing;
      modalClosing = null;
      animation.cancel();
    },
    () => {},
  );
}
async function perform(method, params = {}, notice = '已完成') {
  try {
    const v = await rpc(method, params);
    await refreshState(true);
    if (notice) toast(notice);
    return v;
  } catch (e) {
    toast(e.message);
    throw e;
  }
}
const htmlMemo = new WeakMap();
function setHtml(selector, html) {
  const el = typeof selector === 'string' ? $(selector) : selector;
  if (!el) return;
  const previous = htmlMemo.get(el);
  if (previous?.input === html && previous.dom === el.innerHTML) return;
  reconcileHtml(el, html);
  htmlMemo.set(el, { input: html, dom: el.innerHTML });
}
let refreshInFlight = null,
  refreshForced = false,
  refreshQueued = false,
  renderedPage = '',
  renderedKey = '',
  renderEpoch = 0,
  pluginCatalogRevision = 0;
function viewKey(s, p) {
  switch (p) {
    case 'tasks':
      return JSON.stringify([
        s.tag_state,
        (s.handling || []).map(({ synced, updated, progress, ...r }) => ({
          ...r,
          rounds: r.rounds?.map(({ steps, ...round }) => round),
        })),
        s.tasks,
        s.messages,
        s.task_history_revision,
        s.last_analysis_success_at,
        s.usage.day,
        s.pending,
        s.config.enabled,
        s.config.tools,
        s.ai_busy,
        s.busy,
      ]);
    case 'inbox':
      return JSON.stringify([
        s.messages_revision ?? s.messages,
        s.pending,
        s.last_scan,
        s.config.enabled,
        s.config.interval_minutes,
        s.config.message_retention_days,
      ]);
    case 'schedules':
      return JSON.stringify(s.schedules);
    case 'reports':
      return JSON.stringify([s.reports, s.config.work_start, s.config.work_end, s.archive_attempt]);
    case 'learning':
      return JSON.stringify([s.suggestions, s.rule_version, s.tag_state, s.evolution]);
    case 'logs':
      return JSON.stringify([s.audit, s.activity, s.usage]);
    case 'sources':
      return JSON.stringify([s.chat_sources, pluginCatalogRevision]);
    case 'prompts':
      return JSON.stringify([
        promptSection,
        promptSection === 'advice'
          ? [s.suggestions, s.rule_version, s.tag_state, s.evolution]
          : promptSection === 'memory'
            ? s.memory_state
            : s.prompt_state,
      ]);
    default:
      return '';
  }
}
window.refreshState = function (force = false) {
  if (refreshInFlight) {
    if (force) {
      refreshForced = true;
      refreshQueued = true;
    }
    return refreshInFlight;
  }
  refreshForced = force;
  refreshInFlight = (async () => {
    do {
      refreshQueued = false;
      try {
        const forced = refreshForced;
        refreshForced = false;
        const requestPage = page;
        const next = await rpc('state', {
          page: requestPage,
          task: handlingWindow?.id || selected || undefined,
          core_revision: !forced && state?._page === requestPage ? state.core_revision : undefined,
        });
        if (page !== requestPage) {
          refreshQueued = true;
          refreshForced = true;
          continue;
        }
        if (
          next.tag_state?.unified &&
          JSON.stringify(next.tag_state.groups) !== JSON.stringify(state?.tag_state?.groups)
        ) {
          selectedTags = structuredClone(next.tag_state.filters || {});
          selectedPriorities = [];
        }
        state = { reports: [], audit: [], messages: [], ...state, ...next };
        if (next.handling_live?.length) {
          const previews = new Map(next.handling_live.map((run) => [run.id, run]));
          state.handling = (state.handling || []).map((run) => {
            const preview = previews.get(run.id);
            return preview ? { ...run, ...preview } : run;
          });
        }
        status();
        syncHandlingWindow();
        updateHandlingLive();
        syncHandlingApproval();
        syncTaskRecordPanel();
        syncUpdateNotice();
        const active = document.activeElement,
          editing =
            $('#view').contains(active) &&
            active?.matches('input,textarea,select') &&
            active.id !== 'inbox-search';
        const changed =
          renderedPage !== page || (!next._unchanged && renderedKey !== viewKey(state, page));
        if (
          (forced &&
            !(
              page === 'settings' &&
              renderedPage === 'settings' &&
              (settingsChanged() || settingsSavePromise || settingsQueue || connectionEditor)
            )) ||
          (changed &&
            page !== 'settings' &&
            (!editing || page === 'tasks') &&
            !settingsDirty &&
            $('#overlay').hidden)
        ) {
          const target = page,
            scroll = $('#view').scrollTop;
          await render();
          if (page === target) $('#view').scrollTop = scroll;
        }
        if (!uiReady) {
          uiReady = true;
          await rpc('ui_ready', {
            rendered: !!$('#page-title')?.textContent && !!$('#view')?.children.length,
            tag_catalog_ready: Array.isArray(state.tag_state?.groups),
            version: state.version,
            build_time: state.build_time,
            task_rows: document.querySelectorAll('.task-row').length,
            plugin_drop: typeof installPluginFile === 'function',
          });
        }
      } catch (e) {
        toast(e.message);
      }
    } while (refreshQueued);
  })().finally(() => {
    refreshInFlight = null;
  });
  return refreshInFlight;
};
let modelCatalog = [],
  modelRequest = 0,
  settingsBaseline = {},
  scanToggleBusy = false;
function readSettingsValues() {
  return Object.fromEntries(
    [...document.querySelectorAll('#view [id^="cfg-"]')]
      .filter((el) => el.matches('input,textarea,select'))
      .map((el) => [
        el.id,
        el.type === 'checkbox'
          ? el.disabled && el.dataset.savedChoice !== undefined
            ? el.dataset.savedChoice === 'true'
            : el.checked
          : el.type === 'number'
            ? Number(el.value)
            : el.id === 'cfg-leaders'
              ? el.value
                  .split(/[\n,，]/)
                  .map((s) => s.trim())
                  .filter(Boolean)
                  .join('\n')
              : el.value.trim(),
      ]),
  );
}
function settingsChanged() {
  if (page !== 'settings') return settingsDirty;
  const current = readSettingsValues();
  return Object.keys(settingsBaseline).some((id) => current[id] !== settingsBaseline[id]);
}
async function leaveSettings() {
  if (typeof leaveMemory === 'function' && memoryEditor && !leaveMemory()) return false;
  if (connectionEditor?.saving || connectionEditor?.busy) {
    toast(connectionEditor.saving ? '正在保存连接' : '正在读取连接，请稍后重试');
    restoreConnectionFocus();
    return false;
  }
  if (page === 'settings' && !(await flushSettings())) {
    toast('设置尚未保存，请修正输入或重试');
    return false;
  }
  if (connectionDirty() && !confirm('连接修改尚未保存，放弃修改？')) {
    restoreConnectionFocus();
    return false;
  }
  closeConnectionEditor();
  settingsDirty = false;
  return true;
}

const effortNames = {
  none: '无',
  minimal: '最少',
  low: '低',
  medium: '中',
  high: '高',
  xhigh: '很高',
  max: '最高',
  ultra: '超高',
};
