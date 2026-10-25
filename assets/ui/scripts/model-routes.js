// Model routing and catalogs shared by the AI settings controls.
function modelChoice(kind) {
  const profiles = state.config.model_profiles;
  if (profiles && Object.hasOwn(profiles, kind) && profiles[kind] === null)
    return { unconfigured: true, model: '', reasoning_effort: '' };
  return (
    profiles?.[kind] || {
      model: state.config.model || '',
      reasoning_effort: state.config.reasoning_effort || '',
    }
  );
}
function retentionSummary() {
  const d = state?.retention_digest;
  if (!d || typeof d !== 'object') return '';
  const parts = [];
  if (d.messages?.count) parts.push(`${d.messages.count} 条过期消息`);
  if (d.records?.count) parts.push(`${d.records.count} 条操作记录`);
  if (d.tasks?.count) parts.push(`${d.tasks.count} 个已归档事项`);
  if (!parts.length) return '';
  return `<p class="helper">已汇总 ${parts.join('、')}${d.compressed ? '（已进一步压缩）' : ''}；只保存数量和类型等统计，不保留原文。</p>`;
}
const modelFields = [
  [
    'filter',
    'cfg-filter-model',
    'cfg-filter-effort',
    '消息过滤',
    '用独立模型判断新消息是否需要提前执行完整分析。过滤只决定是否提前分析，不推进分析游标、不删除消息、不把消息标为已分析；判断为等待时仍有定时完整分析兜底，失败或不确定会提前完整分析一次；同一决定不会因分析失败或中断而重复触发。过滤调用不占每日调用上限，用量单独显示。',
  ],
  [
    'analysis',
    'cfg-model',
    'cfg-effort',
    '事项更新',
    '根据消息及相关上下文生成新事项，或更新已有事项的进展、状态和下一步行动。',
  ],
  [
    'handling',
    'cfg-handling-model',
    'cfg-handling-effort',
    'AI一键处置',
    '自动拉起所选的 Codex 或 DeepSeek Harness 桌面端，结合事项信息和相关上下文开展处置；执行过程遵循已有授权与审批设置。此入口需要外部 agent。事项分析、生成、修改和归档的注册工具调用沿用各自用途的模型。',
  ],
  [
    'review',
    'cfg-review-model',
    'cfg-review-effort',
    '归档分析',
    '生成工作报表、归档事项并维护归档记忆；不修改系统提示词。',
  ],
];
let profileCatalogs = {},
  connectionSequence = 0,
  connectionEditor = null;
const apiEfforts = ['none', 'minimal', 'low', 'medium', 'high', 'xhigh'].map((reasoningEffort) => ({
  reasoningEffort,
}));
const driverLabels = {
  codex: 'Codex CLI',
  dsh: 'DeepSeek Harness',
  api: 'OpenAI Chat Completions 兼容 API',
};
function help(text) {
  return `<button type="button" class="help-dot" aria-label="说明" data-help="${esc(text)}">?</button>`;
}
function defaultConnection() {
  return {
    id: '',
    name: state.config.codex_name || '默认 Codex',
    driver: 'codex',
    executable: state.config.codex || '',
    base_url: '',
    key_env: null,
    credential_id: null,
    models: state.config.codex_models || [],
    stream: true,
  };
}
function draftConnections() {
  return [
    ...(state.config.codex ? [defaultConnection()] : []),
    ...(state.config.ai_connections || []),
  ];
}
function handlingConfigured() {
  return draftConnections().some(
    (c) => ['codex', 'dsh'].includes(c.driver) && c.executable?.trim(),
  );
}
function refreshHandlingAvailability() {
  const unavailable = !handlingConfigured(),
    row = document.querySelector('[data-model-profile="handling"]');
  if (row) {
    row.dataset.unavailable = String(unavailable);
    row.setAttribute('aria-disabled', 'false');
    row.querySelectorAll('select').forEach((el) => (el.disabled = false));
  }
  const note = $('#handling-availability-note');
  if (note) note.hidden = !unavailable;
}
function readConnections() {
  return structuredClone(state.config.ai_connections || []);
}
function connectionCard(c) {
  return `<article class="ai-connection" data-ai-connection="${esc(c.id)}"><div class="connection-summary"><b>${esc(c.name)}</b><span class="badge connection-driver" title="${esc(driverLabels[c.driver] || c.driver)}">${esc(driverLabels[c.driver] || c.driver)}</span><div class="connection-actions"><button data-ai-edit="${esc(c.id)}">编辑</button><button class="quiet" data-ai-remove="${esc(c.id)}">删除</button></div></div><div data-connection-editor="${esc(c.id)}"></div></article>`;
}
function extraConnectionFields() {
  return `<div id="ai-connections">${draftConnections().map(connectionCard).join('')}</div><button class="add-connection" data-ai-add>＋ 添加连接</button><div id="new-connection-editor"></div>`;
}
function connectionOptions(selected, connections = draftConnections(), kind = '') {
  return (
    `<option value="__unselected__" ${selected === '__unselected__' || !connections.some((c) => c.id === selected) ? 'selected' : ''}>未配置</option>` +
    connections
      .filter((c) => (kind !== 'handling' || c.driver !== 'api'))
      .map(
        (c) =>
          `<option value="${esc(c.id)}" ${c.id === selected ? 'selected' : ''}>${esc(c.name)}</option>`,
      )
      .join('') +
    (selected && selected !== '__unselected__' && !connections.some((c) => c.id === selected)
      ? `<option selected disabled value="${esc(selected)}">连接已失效</option>`
      : '')
  );
}
function refreshConnectionOptions() {
  for (const [kind] of modelFields) {
    const el = $('#cfg-' + kind + '-connection');
    if (el) {
      const selected = el.value;
      el.innerHTML = connectionOptions(selected, draftConnections(), kind);
      el.value = selected;
    }
  }
}
function selectedConnection(kind) {
  return draftConnections().find((c) => c.id === $('#cfg-' + kind + '-connection')?.value);
}
function modelProfileFields() {
  return `<div class="model-table-head"><span>用途</span><span>AI 接入</span><span>模型</span><span>强度</span></div>${modelFields
    .map(([kind, m, e, label, tip]) => {
      const c = modelChoice(kind),
        unavailable = kind === 'handling' && !handlingConfigured();
      return `<div class="model-profile" data-model-profile="${kind}" data-unavailable="${unavailable}" aria-disabled="false"><b>${label}${help(tip)}</b><select aria-label="${label}接入" id="cfg-${kind}-connection" data-ai-route="${kind}" >${connectionOptions(c.unconfigured ? '__unselected__' : c.connection_id || '', draftConnections(), kind)}</select><select aria-label="${label}模型" id="${m}" ><option value="${esc(c.model || '')}">${esc(c.model || '选择模型')}</option></select><select aria-label="${label}强度" id="${e}" ><option value="${esc(c.reasoning_effort || '')}">${esc(effortNames[c.reasoning_effort] || '默认')}</option></select><span class="route-error" id="model-status-${kind}"></span></div>${kind === 'handling' ? `<p class="helper handling-availability-note" id="handling-availability-note" ${unavailable ? '' : 'hidden'}>需要先配置 Codex 或 DSH 接入，才能使用 AI 一键处置。</p>` : ''}`;
    })
    .join('')}`;
}
function updateEfforts(preserve = false, only = '') {
  for (const [kind, m, e] of modelFields) {
    if (only && only !== m) continue;
    const select = $('#' + e);
    if (!select) continue;
    const previous = select.value,
      model = (profileCatalogs[kind] || []).find((v) => v.model === $('#' + m).value),
      efforts =
        selectedConnection(kind)?.driver === 'api' &&
        model?.reasoningEffortSupport !== 'declared' &&
        !model?.supportedReasoningEfforts?.length
          ? apiEfforts
          : model?.supportedReasoningEfforts || [];
    select.title =
      selectedConnection(kind)?.driver === 'api' && model?.reasoningEffortSupport !== 'declared'
        ? '供应商未声明强度能力；所选参数原样发送，不支持时会报错。'
        : '';
    select.innerHTML =
      '<option value="">默认</option>' +
      efforts
        .map(
          (v) =>
            `<option value="${esc(v.reasoningEffort)}">${esc(effortNames[v.reasoningEffort] || v.reasoningEffort)}</option>`,
        )
        .join('');
    if (preserve && previous && !model) {
      select.insertAdjacentHTML(
        'beforeend',
        `<option value="${esc(previous)}">${esc(effortNames[previous] || previous)}</option>`,
      );
      select.value = previous;
    } else
      select.value =
        preserve && efforts.some((v) => v.reasoningEffort === previous) ? previous : '';
  }
}
function loadModels() {
  profileCatalogs = {};
  for (const [kind, m] of modelFields) {
    const select = $('#' + m);
    if (!select) continue;
    const connection = selectedConnection(kind),
      catalog = connection?.models || [],
      previous = select.value;
    profileCatalogs[kind] = catalog;
    select.innerHTML =
      '<option value="">未配置</option>' +
      catalog
        .map(
          (v) =>
            `<option value="${esc(v.model)}">${esc(modelLabel({ ...v, providerName: v.providerName || v.provider || connection?.name }, catalog))}</option>`,
        )
        .join('');
    if (previous && !catalog.some((v) => v.model === previous))
      select.insertAdjacentHTML(
        'beforeend',
        `<option disabled value="${esc(previous)}">${esc(previous)}（需检测模型）</option>`,
      );
    select.value = previous || '';
    $('#model-status-' + kind).textContent =
      connection && !catalog.length ? '请在 AI 接入中检测或填写模型' : '';
    updateEfforts(true, m);
  }
  for (const el of document.querySelectorAll('.model-profile select:not([id$="effort"])'))
    el.title = el.selectedOptions[0]?.textContent || '';
  modelCatalog = profileCatalogs.analysis || [];
  refreshHandlingAvailability();
}
