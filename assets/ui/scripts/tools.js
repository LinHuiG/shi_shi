// Local tool library, draft testing and bounded execution history.
let toolLibrary = [], toolDraft = null, toolTestId = '', toolEditorSequence = 0;
const toolSamples = new Map();
const toolPurposes = {classify:'事项分析',format:'事项生成',edit:'一句话修改',archive:'归档',handling:'AI 一键处置',tool_call:'手动工具调用',tool_test:'测试运行'};
const toolStatus = { completed:'执行完成', nonzero_exit:'非零退出', start_failed:'启动失败', output_error:'输出解码/读取失败', timed_out:'超时', cancelled:'已取消', rejected:'执行前拒绝', running:'运行中', failed:'运行失败', interrupted:'已中断', unknown:'结果未确定' };
function toolSwitch(attribute, id, enabled, label, disabled = false) {
  return `<button type="button" class="mini-switch ${enabled ? 'on' : ''}" role="switch" aria-label="${esc(label)}" aria-checked="${!!enabled}" ${attribute}="${esc(id)}" ${disabled ? 'disabled' : ''}><i></i></button>`;
}
async function openToolLibrary() {
  toolLibrary = await rpc('tool_list');
  toolDraft = null;
  showToolLibrary();
}
function showToolLibrary(search = '') {
  toolDraft = null;
  $('#dialog').classList.remove('tool-editor-dialog');
  setModal(`<div class="tool-library"><div class="tool-heading"><h2>工具列表</h2><button data-tool-history>调用记录</button></div><div class="tool-toolbar"><button class="primary" data-tool-new>新增工具</button><input id="tool-search" placeholder="搜索工具名或说明……" value="${esc(search)}" aria-label="搜索工具"></div><div id="tool-list-rows"></div><div id="tool-error" class="error-box"></div><div class="dialog-foot"><button data-action="close">关闭</button></div></div>`);
  $('#dialog').classList.add('wide-dialog');
  modalOutsideDismiss = () => true;
  renderToolRows(search);
}
function renderToolRows(search = '') {
  const terms = search.trim().toLowerCase().split(/\s+/).filter(Boolean),
    rows = toolLibrary.filter(t => terms.every(q => `${t.name} ${t.display_name} ${t.description}`.toLowerCase().includes(q)));
  $('#tool-list-rows').innerHTML = !toolLibrary.length ? '<p class="helper">尚未注册工具。</p>' : !rows.length ? '<p class="helper">没有匹配的工具。</p>' : `<div class="tool-table">${rows.map(t => `<article class="tool-library-row"><div><b>${esc(t.display_name || t.name || '未命名草稿')}</b><div class="tiny">${esc(t.name)}</div></div><div class="tool-description">${esc(t.description || '尚未填写说明')}${t.compatibility_issue ? `<div class="helper">${esc(t.compatibility_issue)}</div>` : ''}<div class="tiny">${esc(t.parameters.map(p => p.name).join(' · ') || '无参数')}</div></div>${toolSwitch('data-tool-toggle',t.id,t.enabled,`启用 ${t.display_name || t.name}`)}<div class="tool-row-actions"><button data-tool-edit="${esc(t.id)}">编辑</button><button class="quiet" data-tool-delete="${esc(t.id)}">删除</button></div></article>`).join('')}</div>`;
}
function newTool() {
  return { id:'', definition_version:2, name:'', description:'', parameters:[], command_template:'', enabled:false, updated:0 };
}
function parameterFields(p = {}) {
  return `<div class="tool-parameter" ><input data-param="name" aria-label="参数名" placeholder="参数名" maxlength="64" value="${esc(p.name || '')}"><input data-param="description" aria-label="参数说明" placeholder="说明 AI 应传入什么" maxlength="1000" value="${esc(p.description || '')}"><button class="quiet" data-param-remove aria-label="移除参数">移除</button></div>`;
}
function showToolEditor(tool = newTool()) {
  toolDraft = structuredClone(tool);
  toolDraft.nameEdited = !!tool.name;
  toolDraft.sampleKey=tool.id || globalThis.crypto?.randomUUID?.() || String(Date.now());
  ++toolEditorSequence;
  setModal(`<div class="tool-editor"><div class="tool-heading"><h2>${tool.id ? '编辑工具' : '新增工具'}</h2><div class="tool-enabled"><span>启用</span>${toolSwitch('data-tool-draft-enabled','',tool.enabled,'启用此工具')}</div></div>${tool.compatibility_issue ? `<div class="error-box">${esc(tool.compatibility_issue)}。保存后采用简化配置，旧调用记录保留。${tool.working_directory ? `<br>原工作目录：${esc(tool.working_directory)}` : ''}</div>` : ''}<label>工具名<input id="tool-name" maxlength="64" value="${esc(tool.name)}" placeholder="如 query_records，可根据命令自动填写"></label><label>使用说明<textarea id="tool-description" rows="2" maxlength="1024" placeholder="告诉 AI 什么时候使用这个工具">${esc(tool.description)}</textarea></label><label>CMD 命令<textarea id="tool-command" rows="2" placeholder="如 python C:\\tools\\query.py {{keyword}}">${esc(tool.command_template)}</textarea></label><div class="tool-heading"><h3>参数</h3><button data-param-add>＋ 添加参数</button></div><div id="tool-parameters">${tool.parameters.map(p => parameterFields(p)).join('')}</div><details class="tool-test"><summary>测试运行</summary><div id="tool-samples"></div><div class="row"><button data-tool-command-preview>预览命令</button><button data-tool-test>测试运行</button><button data-tool-test-cancel hidden>取消测试</button></div><pre id="tool-command-output"></pre><div id="tool-test-result"></div></details><div id="tool-error" class="error-box"></div><div class="tool-editor-footer"><button data-tool-copy>复制工具制作提示词</button><div class="dialog-foot"><button data-tool-back>取消</button><button class="primary" data-tool-save>保存工具</button></div></div></div>`);
  $('#dialog').classList.add('wide-dialog');
  $('#dialog').classList.add('tool-editor-dialog');
  syncToolParameters();
}
function readToolForm() {
  return { id:toolDraft.id, updated:toolDraft.updated, definition_version:2,
    name:$('#tool-name').value.trim(), description:$('#tool-description').value,
    command_template:$('#tool-command').value,
    parameters:[...document.querySelectorAll('.tool-parameter')].map(row => ({name:row.querySelector('[data-param="name"]').value.trim(),description:row.querySelector('[data-param="description"]').value})),
    enabled:$('.tool-editor [data-tool-draft-enabled]').getAttribute('aria-checked') === 'true' };
}
function rememberToolSamples() {
  document.querySelectorAll('[data-tool-sample]').forEach(input => toolSamples.set(`${toolDraft.sampleKey}:${input.dataset.toolSample}`,input.value));
}
function renderToolSamples() {
  rememberToolSamples();
  const names=[...document.querySelectorAll('[data-param="name"]')].map(input => input.value.trim()).filter(Boolean);
  $('#tool-samples').innerHTML=names.map(name => `<label>${esc(name)}<input data-tool-sample="${esc(name)}" value="${esc(toolSamples.get(`${toolDraft.sampleKey}:${name}`) || '')}" placeholder="样例值"></label>`).join('') || '<p class="helper">此工具不需要参数。</p>';
}
function toolSampleArguments() {
  return Object.fromEntries([...document.querySelectorAll('[data-tool-sample]')].map(input => [input.dataset.toolSample,input.value]));
}
function commandToolName(command) {
  // Only infer a concrete executable/script name, never from arbitrary CMD syntax.
  const tokens=command.match(/"[^"\r\n]+"|[^\s]+/g) || [];
  let executable=(tokens[0] || '').replace(/^"|"$/g,'');
  if (/(?:^|[\\/])(?:python(?:3)?|pwsh|powershell|node)(?:\.exe)?$/i.test(executable)) {
    executable=(tokens.find((token,i) => i > 0 && /\.(?:py|ps1|js|cjs)"?$/i.test(token)) || '').replace(/^"|"$/g,'');
  }
  if (!/\.(?:exe|cmd|bat|py|ps1|js|cjs)$/i.test(executable)) return '';
  const name=executable.split(/[\\/]/).pop().replace(/\.[^.]+$/,'').replace(/[^a-z0-9_-]+/gi,'_').replace(/^_+|_+$/g,'');
  return (/^[0-9]/.test(name) ? 'tool_'+name : name).slice(0,64);
}
function syncToolParameters() {
  const command=$('#tool-command').value,
    names=[...new Set([...command.matchAll(/\{\{([a-z0-9_-]+)\}\}/gi)].map(match => match[1]))];
  // Preserve manual rows and every entered explanation, even after a command edit.
  for (const name of names) if (![...document.querySelectorAll('[data-param="name"]')].some(input => input.value.trim() === name)) $('#tool-parameters').insertAdjacentHTML('beforeend',parameterFields({name}));
  if (!toolDraft.nameEdited) $('#tool-name').value=commandToolName(command);
  renderToolSamples();
}
function commandResultHtml(r) {
  if (!r) return '<p class="helper">已记录调用意图，结果尚未确定。不会自动重放。</p>';
  return `<div class="helper">${esc(toolStatus[r.status] || r.status)} · 退出码 ${esc(r.exit_code ?? '无')} · ${r.duration_ms} ms${r.truncated ? ' · 输出已截断' : ''}${r.cancelled ? ' · 已取消' : ''}${r.timed_out ? ' · 已超时' : ''}</div>${r.error ? `<div class="error-box">${esc(r.error)}</div>` : ''}<label>stdout<pre>${esc(r.stdout || '')}</pre></label><label>stderr<pre>${esc(r.stderr || '')}</pre></label>`;
}
let toolHistorySequence=0, toolHistoryFilters={}, toolHistoryTimer;
async function openToolRunHistory(taskId) {
  clearTimeout(toolHistoryTimer);
  const back=modalReturnPoint();
  setModal(`<div class="tool-run-history" data-history-task="${esc(taskId || '')}"><h2>工具调用记录</h2><div class="tool-history-filters"><label>工具<select id="tool-history-filter"></select></label><label>用途<select id="tool-history-purpose"><option value="">全部用途</option>${Object.entries(toolPurposes).map(([id,name]) => `<option value="${id}" ${toolHistoryFilters.purpose === id ? 'selected' : ''}>${name}</option>`).join('')}</select></label><label>日期<input id="tool-history-day" type="date" value="${esc(toolHistoryFilters.day || '')}"></label><label class="tool-history-search">搜索<input id="tool-history-search" maxlength="200" placeholder="搜索工具、参数或执行结果…" value="${esc(toolHistoryFilters.query || '')}"></label></div><div id="tool-history-count" class="helper"></div><div id="tool-history-list" aria-live="polite"></div><div id="tool-error" class="error-box"></div><div class="tool-history-footer"><div id="tool-history-pagination" class="row"></div><button data-action="close">关闭</button></div></div>`);
  modalBack=back;
  $('#dialog').classList.add('tool-history-dialog');
  await refreshToolHistory();
}
async function refreshToolHistory(page = 1) {
  const sequence=++toolHistorySequence, list=$('#tool-history-list');
  if (!list) return;
  list.setAttribute('aria-busy','true');
  try {
    const result=await rpc('tool_run_history',{view:'calls',...toolHistoryFilters,page,task_id:list.closest('[data-history-task]').dataset.historyTask});
    if (sequence !== toolHistorySequence || !list.isConnected || $('#overlay').hidden) return;
    const historyTools=[...new Map([...(result.tools || []),...toolLibrary].map(tool => [tool.id,tool])).values()];
    $('#tool-history-filter').innerHTML=[['','全部工具'],...historyTools.map(t => [t.id,t.name])].map(([id,name]) => `<option value="${esc(id)}" ${toolHistoryFilters.tool_id === id ? 'selected' : ''}>${esc(name)}</option>`).join('');
    $('#tool-history-count').textContent=`共 ${result.total} 条`;
    list.innerHTML=result.rows.length ? `<table class="tool-history-table"><thead><tr><th>工具 name</th><th>时间</th><th>调用方式</th><th>执行结果</th><th>参数</th></tr></thead><tbody>${result.rows.map(r => {
      const args=JSON.stringify(r.arguments ?? {}), status=toolStatus[r.status] || r.status || '结果未确定';
      return `<tr data-tool-history-open="${esc(r.run_id)}" data-call-index="${r.call_index}" tabindex="0" role="button" aria-label="${esc(r.tool)} ${esc(date(r.started))}"><td data-label="工具 name" title="${esc(r.tool)}">${esc(r.tool || '未记录')}</td><td data-label="时间">${esc(date(r.started))}</td><td data-label="调用方式">${esc(toolPurposes[r.purpose] || '历史记录')}</td><td data-label="执行结果"><span class="tool-history-status ${['completed','running'].includes(r.status) ? '' : 'tool-history-warning'}">${esc(status)}</span>${r.exit_code == null ? '' : `<span class="tiny">退出码 ${esc(r.exit_code)}</span>`}</td><td data-label="参数" title="${esc(args)}"><span class="tool-history-arguments">${esc(args)}</span></td></tr>`;
    }).join('')}</tbody></table>` : '<p class="helper tool-history-empty">没有匹配的调用记录。</p>';
    $('#tool-history-pagination').innerHTML=result.pages > 1 ? `<button data-tool-history-page="${result.page-1}" ${result.page<=1 ? 'disabled' : ''}>上一页</button><span class="tiny">${result.page} / ${result.pages}</span><button data-tool-history-page="${result.page+1}" ${result.page>=result.pages ? 'disabled' : ''}>下一页</button>` : '';
    $('#tool-error').textContent='';
  } finally { if (sequence === toolHistorySequence) list.removeAttribute('aria-busy'); }
}
function toolHistoryDetailHtml(run) {
  return `${run.error ? `<div class="error-box">${esc(run.error)}</div>` : ''}${run.calls.map(c => {
    const r=c.result, status=r?.status || 'unknown', tone=status === 'completed' ? 'success' : ['running','unknown'].includes(status) ? 'neutral' : 'warning',
      entries=c.arguments && typeof c.arguments === 'object' && !Array.isArray(c.arguments) ? Object.entries(c.arguments) : [['参数',c.arguments]];
    return `<section class="tool-call-record tool-call-detail"><div class="tool-call-heading"><h3>${esc(c.tool || '未记录工具名')}</h3><span class="tool-result-badge ${tone}">${esc(toolStatus[status] || status)}</span></div><dl class="tool-call-facts"><div><dt>调用时间</dt><dd>${esc(c.started ? date(c.started) : '未记录')}</dd></div><div><dt>调用方式</dt><dd>${esc(toolPurposes[run.purpose] || '历史记录')}</dd></div><div><dt>耗时</dt><dd>${r && Number.isFinite(r.duration_ms) ? `${esc(r.duration_ms)} ms` : '未记录'}</dd></div><div><dt>退出码</dt><dd>${esc(r?.exit_code ?? '无')}</dd></div></dl>${r?.error ? `<div class="error-box">${esc(r.error)}</div>` : ''}${!r ? '<p class="helper">已记录调用意图，结果尚未确定。不会自动重放。</p>' : ''}${r?.truncated || r?.cancelled || r?.timed_out ? `<p class="tool-call-flags">${[r.truncated ? '输出已截断' : '',r.cancelled ? '已取消' : '',r.timed_out ? '已超时' : ''].filter(Boolean).map(esc).join(' · ')}</p>` : ''}<div class="tool-call-layout"><section class="tool-call-section"><h4>参数 <span>${entries.length}</span></h4>${entries.length ? `<dl class="tool-call-arguments">${entries.map(([name,value]) => `<div><dt>${esc(name)}</dt><dd>${esc(typeof value === 'string' ? value : JSON.stringify(value) ?? '未记录')}</dd></div>`).join('')}</dl>` : '<p class="tool-output-empty">无参数</p>'}</section><section class="tool-call-section tool-call-outputs"><h4>标准输出 <span>stdout</span></h4>${r?.stdout ? `<pre>${esc(r.stdout)}</pre>` : '<p class="tool-output-empty">无标准输出</p>'}<h4>错误输出 <span>stderr</span></h4>${r?.stderr ? `<pre class="tool-output-error">${esc(r.stderr)}</pre>` : '<p class="tool-output-empty">无错误输出</p>'}</section></div><details class="tool-call-identifiers"><summary>请求信息</summary><dl><div><dt>请求编号</dt><dd>${esc(run.request_id || '未记录')}</dd></div><div><dt>调用编号</dt><dd>${esc(c.call_id || '未记录')}</dd></div></dl></details></section>`;
  }).join('')}${run.pages > 1 ? `<div class="row"><button data-tool-history-call-page="${run.page-1}" ${run.page<=1 ? 'disabled' : ''}>上一页调用</button><span>${run.page} / ${run.pages}</span><button data-tool-history-call-page="${run.page+1}" ${run.page>=run.pages ? 'disabled' : ''}>下一页调用</button></div>` : ''}${run.answer ? `<div class="description">${esc(run.answer)}</div>` : ''}`;
}
async function openToolHistoryDetail(runId, callIndex) {
  if ($('#tool-history-list')?.getAttribute('aria-busy') === 'true') return;
  clearTimeout(toolHistoryTimer);
  const sequence=++toolHistorySequence, back=modalReturnPoint(), parent=$('#dialog').firstElementChild,
    taskId=!$('#overlay').hidden ? $('[data-history-task]')?.dataset.historyTask || '' : '',
    wasHidden=$('#overlay').hidden,
    run=await rpc('tool_run_get',{run_id:runId,task_id:taskId,...(callIndex === undefined ? {} : {call_index:callIndex})});
  if (sequence !== toolHistorySequence || (!wasHidden && ($('#overlay').hidden || parent !== $('#dialog').firstElementChild))) return;
  if (!run) throw Error('调用记录已清理');
  setModal(`<div class="tool-run-history tool-run-detail" data-history-task="${esc(taskId)}" data-run-id="${esc(runId)}" ${callIndex === undefined ? '' : `data-call-index="${callIndex}"`}><h2>工具调用详情</h2><div id="tool-history-result">${toolHistoryDetailHtml(run)}</div><div id="tool-error" class="error-box"></div><div class="dialog-foot"><button data-action="close">关闭</button></div></div>`);
  modalBack=back;
  $('#dialog').classList.add('tool-history-dialog','tool-detail-dialog');
}
async function readToolHistoryRun(page = 1) {
  const detail=$('.tool-run-detail');
  if (!detail) return;
  const sequence=++toolHistorySequence, run=await rpc('tool_run_get',{run_id:detail.dataset.runId,task_id:detail.dataset.historyTask,...(detail.dataset.callIndex === undefined ? {page} : {call_index:Number(detail.dataset.callIndex)})});
  if (sequence !== toolHistorySequence || !detail.isConnected || $('#overlay').hidden) return;
  if (!run) throw Error('调用记录已清理');
  $('#tool-history-result').innerHTML=toolHistoryDetailHtml(run);
}
document.addEventListener('input', e => {
  if (e.target.id === 'tool-history-search') {
    toolHistoryFilters.query=e.target.value;
    ++toolHistorySequence;
    clearTimeout(toolHistoryTimer);
    $('#tool-history-list').setAttribute('aria-busy','true');
    toolHistoryTimer=setTimeout(() => refreshToolHistory().catch(error => {
      if ($('#tool-error')) $('#tool-error').textContent=error.message;
    }),180);
  }
  if (e.target.closest('.tool-editor')) {
    ++toolEditorSequence;
    if (e.target.id === 'tool-name') toolDraft.nameEdited=true;
    if (e.target.id === 'tool-command') syncToolParameters();
    if (e.target.dataset.param === 'name') renderToolSamples();
    if (e.target.hasAttribute('data-tool-sample')) rememberToolSamples();
  }
  if (e.target.id === 'tool-search') renderToolRows(e.target.value);
});
document.addEventListener('change', async e => {
  if (['tool-history-filter','tool-history-purpose','tool-history-day'].includes(e.target.id)) {
    clearTimeout(toolHistoryTimer);
    toolHistoryFilters={tool_id:$('#tool-history-filter').value,purpose:$('#tool-history-purpose').value,day:$('#tool-history-day').value,query:$('#tool-history-search').value};
    try { await refreshToolHistory(); } catch (error) { if ($('#tool-error')) $('#tool-error').textContent=error.message; }
    return;
  }
});
document.addEventListener('click', async e => {
  const row=e.target.closest('[data-tool-history-open]');
  if (row) {
    try { await openToolHistoryDetail(row.dataset.toolHistoryOpen,Number(row.dataset.callIndex)); }
    catch (error) { if ($('#tool-error')) $('#tool-error').textContent=error.message; }
    return;
  }
  const b=e.target.closest('button');
  if (!b || !Object.keys(b.dataset).some(key => key.startsWith('tool') || key.startsWith('param'))) return;
  try {
    if (b.hasAttribute('data-tool-log-run')) {
      return await openToolHistoryDetail(b.dataset.toolLogRun);
    }
    if (b.hasAttribute('data-tool-history')) { toolHistoryFilters={tool_id:b.dataset.toolHistory}; return await openToolRunHistory(''); }
    if (b.hasAttribute('data-tool-library')) return await openToolLibrary();
    if (b.hasAttribute('data-tool-new')) return showToolEditor();
    if (b.hasAttribute('data-tool-edit')) return showToolEditor(toolLibrary.find(t => t.id === b.dataset.toolEdit));
    if (b.hasAttribute('data-tool-back')) return showToolLibrary();
    if (b.hasAttribute('data-param-add')) {++toolEditorSequence; return $('#tool-parameters').insertAdjacentHTML('beforeend',parameterFields());}
    if (b.hasAttribute('data-param-remove')) {++toolEditorSequence; rememberToolSamples(); b.closest('.tool-parameter').remove(); renderToolSamples(); return;}
    if (b.hasAttribute('data-tool-draft-enabled')) {
      const enabled=b.getAttribute('aria-checked') !== 'true';
      b.setAttribute('aria-checked',String(enabled)); b.classList.toggle('on',enabled);
      return;
    }
    if (b.hasAttribute('data-tool-toggle')) {
      const tool=toolLibrary.find(t => t.id === b.dataset.toolToggle); b.disabled=true;
      try { const saved=await rpc('tool_toggle',{id:tool.id,updated:tool.updated,enabled:!tool.enabled}); toolLibrary=toolLibrary.map(t => t.id === saved.id ? saved : t); await refreshState(false); renderToolRows($('#tool-search').value); }
      finally { b.disabled=false; }
      return;
    }
    if (b.hasAttribute('data-tool-delete')) {
      const tool=toolLibrary.find(t => t.id === b.dataset.toolDelete);
      if (!confirm('删除此工具？历史调用记录会保留。')) return;
      await rpc('tool_delete',{id:tool.id,updated:tool.updated}); toolLibrary=toolLibrary.filter(t => t.id !== tool.id); renderToolRows($('#tool-search').value); await refreshState(false); return;
    }
    if (b.hasAttribute('data-tool-save')) {
      const draft=readToolForm(), editor=$('.tool-editor'), controls=[...editor.querySelectorAll('button,input,select,textarea')];
      editor.dataset.pending='true'; controls.forEach(el => el.disabled=true);
      try { const saved=await rpc('tool_save',{tool:draft}); toolLibrary=toolLibrary.filter(t => t.id !== saved.id).concat(saved); await refreshState(false); showToolLibrary(); } finally { delete editor.dataset.pending; controls.forEach(el => el.disabled=false); }
      return;
    }
    if (b.hasAttribute('data-tool-copy')) {
      b.disabled=true;
      try {
        const result=await rpc('tool_making_prompt',{tool:readToolForm()});
        if (await copyText(result.text)) { toast('工具制作提示词已复制'); return; }
        const back=modalReturnPoint();
        setModal('<div class="tool-prompt-preview"><h2>工具制作提示词</h2><p class="helper">自动复制未成功，已选中完整提示词，可按 Ctrl+C 复制。</p><textarea id="tool-copy-text" readonly rows="16"></textarea><div class="dialog-foot"><button data-action="close">关闭</button></div></div>');
        modalBack=back;
        $('#dialog').classList.add('tool-editor-dialog');
        $('#tool-copy-text').value=result.text;
        $('#tool-copy-text').select();
      } finally { b.disabled=false; }
      return;
    }
    if (b.hasAttribute('data-tool-command-preview')) {
      const sequence=toolEditorSequence, result=await rpc('tool_command_preview',{tool:readToolForm(),arguments:toolSampleArguments()}); if (sequence === toolEditorSequence && $('#tool-command-output')) $('#tool-command-output').textContent=result.command; return;
    }
    if (b.hasAttribute('data-tool-test-cancel')) { await rpc('tool_test_cancel',{test_id:toolTestId}); b.disabled=true; return; }
    if (b.hasAttribute('data-tool-test')) {
      const tool=readToolForm(), args=toolSampleArguments(), sequence=toolEditorSequence;
      toolTestId=globalThis.crypto?.randomUUID?.() || `tool-test-${Date.now()}-${Math.random().toString(36).slice(2)}`;
      const editor=$('.tool-editor'), controls=[...editor.querySelectorAll('button,input,select,textarea')], cancel=$('[data-tool-test-cancel]');
      editor.dataset.pending='true'; controls.forEach(el => el.disabled=true); cancel.hidden=false; cancel.disabled=false;
      try { const result=await rpc('tool_test',{tool,arguments:args,test_id:toolTestId}); if (sequence === toolEditorSequence && $('#tool-test-result')) $('#tool-test-result').innerHTML=commandResultHtml(result); }
      finally { toolTestId=''; delete editor.dataset.pending; controls.forEach(el => el.disabled=false); cancel.hidden=true; }
      return;
    }
    if (b.hasAttribute('data-tool-run-history')) {toolHistoryFilters={}; return await openToolRunHistory(b.dataset.toolRunHistory);}
    if (b.hasAttribute('data-tool-history-page')) return await refreshToolHistory(Number(b.dataset.toolHistoryPage));
    if (b.hasAttribute('data-tool-history-call-page')) return await readToolHistoryRun(Number(b.dataset.toolHistoryCallPage));

  } catch (error) {
    const target=!$('#overlay').hidden ? $('#tool-error') : document.querySelector('[data-tool-run-error]');
    if (target) target.textContent=error.message;
    else toast(error.message);
  }
});
document.addEventListener('keydown', e => {
  if (e.target.closest('[data-tool-history-open]') && ['Enter',' '].includes(e.key)) {
    e.preventDefault();
    e.target.closest('[data-tool-history-open]').click();
  }
});
