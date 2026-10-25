// A deliberately small Markdown renderer: no HTML, images, or active URL schemes.
function handlingMd(value) {
  const inline = (text) => {
    const re = /`([^`\n]+)`|\[([^\]\n]+)\]\(([^\s)]+)\)|\*\*([^*\n]+)\*\*|\*([^*\n]+)\*/g;
    let out = '',
      last = 0,
      m;
    while ((m = re.exec(text))) {
      out += esc(text.slice(last, m.index));
      if (m[1] != null) out += `<code>${esc(m[1])}</code>`;
      else if (m[2] != null) {
        let url;
        try {
          url = new URL(m[3]);
        } catch {}
        out +=
          url && ['https:', 'http:'].includes(url.protocol)
            ? `<a href="${esc(url.href)}" target="_blank" rel="noopener noreferrer">${esc(m[2])}</a>`
            : esc(m[0]);
      } else out += m[4] != null ? `<strong>${esc(m[4])}</strong>` : `<em>${esc(m[5])}</em>`;
      last = re.lastIndex;
    }
    return out + esc(text.slice(last));
  };
  const lines = String(value || '').split(/\r?\n/);
  let html = '',
    code = null,
    list = '',
    paragraph = [];
  const flush = () => {
    if (paragraph.length) {
      html += `<p>${paragraph.map(inline).join('<br>')}</p>`;
      paragraph = [];
    }
    if (list) {
      html += `</${list}>`;
      list = '';
    }
  };
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (/^\s*```/.test(line)) {
      if (code !== null) {
        html += `<pre><code>${esc(code.join('\n'))}</code></pre>`;
        code = null;
      } else {
        flush();
        code = [];
      }
      continue;
    }
    if (code !== null) {
      code.push(line);
      continue;
    }
    if (line.includes('|') && i + 1 < lines.length && /^\s*\|?\s*:?-{3,}/.test(lines[i + 1])) {
      flush();
      const cells = (x) =>
        x
          .trim()
          .replace(/^\||\|$/g, '')
          .split('|')
          .map((x) => inline(x.trim()));
      html +=
        '<div class="md-table"><table><thead><tr>' +
        cells(line)
          .map((x) => `<th>${x}</th>`)
          .join('') +
        '</tr></thead><tbody>';
      i++;
      while (i + 1 < lines.length && lines[i + 1].includes('|'))
        html +=
          '<tr>' +
          cells(lines[++i])
            .map((x) => `<td>${x}</td>`)
            .join('') +
          '</tr>';
      html += '</tbody></table></div>';
      continue;
    }
    const heading = line.match(/^(#{1,6})\s+(.*)$/),
      bullet = line.match(/^\s*(?:([-+*])|\d+\.)\s+(.*)$/),
      quote = line.match(/^>\s?(.*)$/);
    if (heading) {
      flush();
      const n = Math.min(heading[1].length + 1, 6);
      html += `<h${n}>${inline(heading[2])}</h${n}>`;
    } else if (bullet) {
      if (paragraph.length) flush();
      const type = bullet[1] ? 'ul' : 'ol';
      if (list !== type) {
        flush();
        html += `<${type}>`;
        list = type;
      }
      html += `<li>${inline(bullet[2])}</li>`;
    } else if (quote) {
      flush();
      html += `<blockquote>${inline(quote[1])}</blockquote>`;
    } else if (!line.trim()) flush();
    else {
      if (list) flush();
      paragraph.push(line);
    }
  }
  flush();
  if (code !== null) html += `<pre><code>${esc(code.join('\n'))}</code></pre>`;
  return `<div class="handling-md">${html}</div>`;
}
function handlingDuration(ms) {
  if (!Number.isFinite(ms) || ms < 0) return '未知';
  const s = Math.floor(ms / 1000);
  return s >= 3600
    ? `${Math.floor(s / 3600)} 小时 ${Math.floor((s % 3600) / 60)} 分`
    : s >= 60
      ? `${Math.floor(s / 60)} 分 ${s % 60} 秒`
      : `${s} 秒`;
}
function handlingProcess(round) {
  return (
    (round.steps || [])
      .map(
        (step) =>
          `<details class="handling-step"><summary>${esc({ agentMessage: '进展说明', commandExecution: '执行命令', fileChange: '修改文件', mcpToolCall: '工具与上下文查询', webSearch: '网页检索', userMessage: '输入消息', functionCall: '执行工具', functionCallOutput: '工具输出' }[step.kind] || step.kind)} · ${esc(step.status || '已记录')}${step.duration_ms != null ? ' · ' + handlingDuration(step.duration_ms) : ''}</summary>${step.kind === 'agentMessage' ? handlingMd(step.text) : `<pre><code>${esc(step.text)}</code></pre>`}</details>`,
      )
      .join('') || '<p class="helper">暂未收到执行过程；只展示该接入返回的可见记录。</p>'
  );
}
function handlingBatch(id) {
  const run = taskRun(id),
    requests = (run?.requests || []).filter(approvalBatchEligible);
  if (!requests.length) {
    toast('没有可批量批准的完整请求');
    return;
  }
  setModal(
    `<div class="approval-dialog"><header><h2>批准当前 ${requests.length} 项请求</h2></header><div class="approval-scroll"><p class="subtitle">仅批准下面列出的当前请求，不批准以后新增的请求。权限申请仅在本轮有效；问题、外部服务授权及信息不完整的请求仍需单独处理。</p>${requests
      .map((q) => {
        const p = q.params || {};
        return `<section class="approval-context">${approvalField('类型', q.method === 'item/permissions/requestApproval' ? '本轮权限' : q.method === 'item/fileChange/requestApproval' ? '本次文件修改' : '本次命令')}${approvalField('原因', p.reason)}${approvalField('命令', p.command, true)}${approvalField('目录', p.cwd || run.cwd)}${approvalField('文件变更', p.changes || p.fileChanges, true)}${
          q.method === 'item/permissions/requestApproval'
            ? approvalPermissions(p.permissions)
                .rows.map(([k, v]) => approvalField(k, v, true))
                .join('')
            : ''
        }</section>`;
      })
      .join(
        '',
      )}<div id="modal-error" class="error-box"></div></div><div class="dialog-foot approval-footer"><button data-action="close">取消</button><button class="primary" id="handling-batch-confirm" data-handle="batch-confirm" data-task-id="${esc(id)}" data-run-id="${esc(run.id)}" data-thread-id="${esc(run.thread_id)}">全部批准这 ${requests.length} 项</button></div></div>`,
  );
  $('#dialog').classList.add('approval-shell');
  $('#handling-batch-confirm')._requestIds = requests.map((q) => JSON.stringify(q.id));
}

// Durable Codex investigations. User text is always escaped before rendering.
const handlingDesktop = (r) =>
  r?.connection?.driver === 'codex' || r?.transport === 'desktop' || r?.desktop_control;
const handlingState = (r) =>
  r?.stop_requested
    ? r.status
    : r?.desktop_control && r?.transport !== 'desktop'
      ? r.desktop_status || 'syncPending'
      : r?.status;
const handlingLabels = {
  ended: '本次处置已结束',
  detached: '本次处置已结束',
  stopping: '正在终止处置',
  terminated: '处置已终止',
  syncPending: '等待同步',
  starting: '正在创建会话',
  submitting: '正在提交',
  inProgress: '调查中',
  waiting: '等待你处理',
  completed: '已有结论 · 待你确认',
  failed: '执行失败',
  interrupted: '已停止',
  uncertain: '提交结果待核实',
  disconnected: '连接中断 · 请同步',
  idle: '尚未执行',
};
const handlingActive = (r) =>
  [
    'starting',
    'submitting',
    'inProgress',
    'waiting',
    'uncertain',
    'disconnected',
    'stopping',
  ].includes(r?.status);
const taskRuns = (id) =>
  (state?.handling || []).filter((r) => r.task_id === id && r.status !== 'setupCancelled');
const taskRun = (id) =>
  taskRuns(id)
    .filter((r) => r.current_source !== false)
    .at(-1);
const handlingRestart = (r) =>
  ['terminated', 'detached', 'ended'].includes(r?.status) ||
  (r?.restartable ?? (r?.status === 'failed' && (r.rounds || []).every((x) => x.not_submitted)));
const handlingButton = (id) => {
  const r = taskRun(id);
  return r && !handlingRestart(r)
    ? 'AI 处置 / 追问'
    : taskRuns(id).length
      ? '重新处置'
      : 'AI一键处置';
};
let handlingProposal = null, handlingProjects = [];
const handlingDrafts = new Map(),
  handlingErrors = new Map(),
  handlingComposers = new Set(),
  handlingSending = new Set(),
  handlingStarting = new Set();
function handlingPanel(t) {
  const r = taskRun(t.id);
  if (!r) return '';
  if (['terminated', 'detached', 'ended'].includes(r.status))
    return `<section class="handling-terminated"><span class="helper">本次处置已结束，可重新发起。</span><button class="quiet" data-handle="history" data-task-id="${esc(t.id)}">查看上次过程</button></section>`;
  const desktop = handlingDesktop(r),
    last = r.rounds?.at(-1),
    waiting =
      r.waiting_on === 'approval'
        ? '等待审批，请在 Codex 处理'
        : r.waiting_on === 'input'
          ? '等待你在 Codex 回答'
          : null;
  return `<section class="handling-panel">
 <div class="handling-heading"><h3>AI一键处置</h3><span class="helper handling-source">${esc(r.project_name)} · ${esc(aiModelLabel({ model: r.model_reported === false ? '模型未知' : r.model, source: r.connection ? { driver: r.connection.driver, connection_name: r.connection.name } : null }))}</span><span class="badge ${handlingState(r) === 'completed' ? 'green' : 'blue'}">${esc(waiting || handlingLabels[handlingState(r)] || handlingState(r))}</span></div>
 ${r.error ? `<div class="error-box">${esc(r.error)}</div>` : ''}${handlingLivePanel(r)}${handlingConclusion(t.id, r) || (handlingState(r) === 'completed' ? '<p class="helper">本轮结束但没有最终文字结论，可在相应客户端查看或继续追问。</p>' : '')}
 <div class="handling-actions"><div class="handling-actions-left">
 ${r.thread_id && desktop ? `<button data-handle="open" data-task-id="${esc(t.id)}">在 Codex 客户端打开</button>` : ''}
 <button class="quiet" data-handle="history" data-task-id="${esc(t.id)}">过程与上下文查询</button><button data-handle="sync" data-task-id="${esc(t.id)}">同步状态</button>
 ${(!desktop || !r.thread_id) && ['uncertain', 'disconnected', 'starting', 'submitting'].includes(r.status) ? `<button data-handle="reconnect" data-task-id="${esc(t.id)}">恢复同步</button>` : ''}
 ${!desktop && r.requests?.length ? `<button class="primary" data-handle="requests" data-task-id="${esc(t.id)}">处理请求 (${r.requests.length})</button>` : ''}
 </div><div class="handling-actions-right"><button data-handle="terminate" data-task-id="${esc(t.id)}" data-run-id="${esc(r.id)}" ${desktop ? 'title="结束后拾事不再同步，Codex 继续运行"' : ''}>${desktop ? '结束本次处置' : r.status === 'stopping' ? '重试终止' : handlingActive(r) ? '终止 AI 处置' : '结束本次处置'}</button>
 <button data-handle="followup" data-task-id="${esc(t.id)}" aria-expanded="${handlingComposers.has(t.id)}">${handlingComposers.has(t.id) ? '收起追问 ▴' : '↳ 继续追问 ▾'}</button></div></div>
 ${handlingComposer(t.id)}</section>`;
}
async function beginHandling(id, choose = false) {
  if (handlingStarting.has(id)) {
    toast('正在创建处置，请稍候');
    return;
  }
  const existing = taskRun(id);
  if (existing && !handlingRestart(existing)) {
    handlingFollowup(id);
    return;
  }
  const p = await rpc('handling_prepare', { task_id: id });
  handlingProjects = p.projects;
  const recommended = p.suggested?.project_id || p.default_project_id;
  const choices = p.projects.filter((x) => x.id === recommended);

  setModal(
    `<h2>AI一键处置</h2><p class="subtitle">选择项目或本地目录，会话按所选执行端保存，可继续追问。首轮调查定位并给出建议，沿用所选项目说明、技能、权限和审批。</p><p class="helper">可切换项目，也可选择本地目录。</p><div class="form-group"><label>项目</label><select id="handling-project"><option value="">请选择项目</option>${p.projects.map((x, i) => `<option value="${i}" ${choices.length === 1 && x === choices[0] ? 'selected' : ''}>${esc(x.name)} · ${esc(x.cwd)}</option>`).join('')}</select><button style="margin-top:8px" data-handle="add-project">＋ 选择本地项目目录</button></div><div class="form-group"><label>本次补充说明（可选）</label><textarea id="handling-instructions" maxlength="2000" placeholder="例如：先核对复现步骤和相关日志。"></textarea></div><details style="margin-top:12px"><summary>查看当前通用调查提示词</summary><pre style="white-space:pre-wrap">${esc(p.boundary || '')}\n\n${esc(p.prompt)}</pre></details><p class="helper">${esc(modelChoice('handling').model || '默认模型')} · ${esc(modelChoice('handling').reasoning_effort || '默认推理强度')}。上下文接口只提供此事项相关会话和时间范围内的消息。</p>${taskRuns(id).some((r) => handlingActive(r) && r.thread_id) ? '<label class="check"><input id="handling-stop-previous" type="checkbox">确认停止原处置后开始</label>' : ''}<div id="modal-error" class="error-box"></div><div class="dialog-foot"><button data-action="close">取消</button><button class="primary" data-handle="submit" data-task-id="${esc(id)}">开始调查</button></div>`,
  );
}
function handlingBadge(id) {
  const r = taskRun(id);
  if (!r) return '';
  const status = handlingState(r),
    busy = ['starting', 'submitting', 'inProgress'].includes(status),
    text = busy
      ? r.desktop_control
        ? 'Codex 正在处理'
        : 'AI 处理中'
      : {
          waiting: 'AI 等待你处理',
          completed: 'AI 已有结论',
          interrupted: 'AI 已中断',
          failed: 'AI 失败',
          uncertain: 'AI 待核实',
          disconnected: 'AI 连接中断',
        }[status] || (status === 'syncPending' ? '等待同步' : '');
  return text
    ? `<span class="badge ${busy ? 'blue' : status === 'completed' ? 'green' : status === 'waiting' ? 'high' : ''}" data-handling-status="${esc(status)}">${busy ? '<span class="spin"></span>' : ''}${text}</span>`
    : '';
}
function handlingComposerHost(id) {
  return Array.from(document.querySelectorAll('[data-composer-task]')).find(
    (el) => el.dataset.composerTask === id,
  );
}
function updateHandlingSending(id) {
  const host = handlingComposerHost(id);
  if (!host) return;
  const sending = handlingSending.has(id);
  host.querySelector('textarea').disabled = sending;
  host.setAttribute('aria-busy', String(sending));
}
function handlingComposer(id) {
  if (!handlingComposers.has(id)) return '';
  const r = taskRun(id),
    sending = handlingSending.has(id),
    savedError = handlingErrors.get(id),
    error =
      savedError?.run_id === r?.id && savedError?.thread_id === r?.thread_id
        ? savedError.message
        : '';
  return `<div class="handling-composer" data-composer-task="${esc(id)}" data-run-id="${esc(r?.id)}" data-thread-id="${esc(r?.thread_id)}" aria-busy="${sending}"><textarea rows="1" id="handling-question" aria-label="继续追问当前 AI" maxlength="6000" ${sending ? 'disabled' : ''} placeholder="补充线索、纠正判断，或明确要求修复…">${esc(handlingDrafts.get(id) || '')}</textarea><div class="error-box" id="handling-inline-error" ${error ? '' : 'hidden'}>${esc(error)}</div></div>`;
}
async function sendHandlingQuestion(id) {
  if (handlingSending.has(id)) return;
  const host = handlingComposerHost(id);
  if (!host) return;
  const identity = { run_id: host.dataset.runId, thread_id: host.dataset.threadId };
  try {
    const question = host.querySelector('textarea').value.trim();
    if (!question) throw Error('请填写追问内容');
    const target = {
      task_id: id,
      run_id: host.dataset.runId,
      thread_id: host.dataset.threadId,
      text: question,
    };
    handlingSending.add(id);
    try {
      handlingErrors.delete(id);
      host.querySelector('.error-box').hidden = true;
      updateHandlingSending(id);
      const reply = await rpc('handling_followup', target);
      handlingDrafts.delete(id);
      handlingComposers.delete(id);
      handlingComposerHost(id)?.remove();
      await refreshState(true);
      toast(reply?.steered ? '已插话到当前任务' : '已发送到原会话');
    } finally {
      handlingSending.delete(id);
      updateHandlingSending(id);
    }
  } catch (error) {
    if (error.message === '本次处置已终止') {
      handlingErrors.delete(id);
      await refreshState(true);
      return;
    }
    handlingErrors.set(id, { ...identity, message: error.message });
    const current = handlingComposerHost(id),
      inline =
        current?.dataset.runId === identity.run_id &&
        current?.dataset.threadId === identity.thread_id
          ? current.querySelector('.error-box')
          : null;
    if (inline) {
      inline.hidden = false;
      inline.textContent = error.message;
    } else toast(error.message);
    await refreshState(false);
  }
}
function handlingFollowup(id) {
  const r = taskRun(id);
  if (!r) return;
  if (handlingComposers.has(id)) handlingComposers.delete(id);
  else handlingComposers.add(id);
  renderDetail();
  if (handlingComposers.has(id)) {
    const field = $('#handling-question');
    field?.focus();
    field?.scrollIntoView({ block: 'nearest' });
  }
}
function resizeHandlingComposer(field) {
  if (!field) return;
  const previous = field.scrollTop;
  field.style.height = '0px';
  field.style.height = Math.max(40, Math.min(field.scrollHeight, 160)) + 'px';
  field.style.overflowY = field.scrollHeight > 160 ? 'auto' : 'hidden';
  field.scrollTop = previous;
}
function resizeHandlingComposers() {
  document.querySelectorAll('#handling-question').forEach(resizeHandlingComposer);
}
document.addEventListener('input', (e) => {
  if (e.target.id === 'handling-question') {
    const id = e.target.closest('[data-composer-task]').dataset.composerTask;
    handlingDrafts.set(id, e.target.value);
    resizeHandlingComposer(e.target);
  }
});
document.addEventListener('keydown', (e) => {
  if (
    e.target.id !== 'handling-question' ||
    e.key !== 'Enter' ||
    e.isComposing ||
    e.keyCode === 229
  )
    return;
  e.preventDefault();
  if (e.repeat || e.target.disabled) return;
  if (e.ctrlKey) {
    const field = e.target;
    field.setRangeText('\n', field.selectionStart, field.selectionEnd, 'end');
    field.dispatchEvent(new Event('input', { bubbles: true }));
    return;
  }
  void sendHandlingQuestion(e.target.closest('[data-composer-task]').dataset.composerTask);
});
// Present the requested scope without interpreting command text as safe.
const approvalBusy = new Set();
function handlingRequests(id) {
  const run = taskRun(id),
    request = run?.requests?.[0];
  if (!request) {
    toast('当前没有待处理请求，请同步状态');
    return;
  }
  setModal(
    approvalDialog(
      run,
      state.tasks?.find((t) => t.id === id),
      request,
    ),
  );
  $('#dialog').classList.add('approval-shell');
  $('#handling-request [data-decision="decline"]')?.focus({ preventScroll: true });
}

function syncHandlingApproval() {
  if ($('#overlay').hidden) return;
  const host = $('#handling-request'),
    batch = $('#handling-batch-confirm');
  if (!host && !batch) return;
  const run = (state?.handling || []).find((r) => r.id === (host || batch).dataset.runId);
  const ids = host ? [host.dataset.requestId] : batch._requestIds || [];
  if (
    !run ||
    run.stop_requested ||
    run.desktop_control ||
    ids.some((id) => !run.requests?.some((q) => JSON.stringify(q.id) === id))
  )
    closeModal();
}
document.addEventListener('change', (event) => {
  const input = event.target;
  if (input.matches('[data-approval-option]')) {
    const answer = $(`[data-handling-answer="${input.dataset.approvalOption}"]`);
    if (answer) answer.value = input.value;
  }
});
document.addEventListener('input', (event) => {
  const input = event.target;
  if (input.matches('[data-handling-answer]')) {
    document
      .querySelectorAll(`[name="approval-choice-${input.dataset.handlingAnswer}"]`)
      .forEach((option) => (option.checked = option.value === input.value));
  }
});

async function previewHandlingProposal(id) {
  const result = await rpc('prompt_history');
  const proposal = state.evolution?.handling?.find(p => p.id === id),
    current = result.versions.find(v => v.id === result.current.handling);
  if (!proposal) throw Error('建议不存在，请刷新');
  handlingProposal = {id, expected:current.id};
  const stale = proposal.base !== current.id || proposal.adopted || proposal.rejected || !proposal.prompt;
  setModal(`<h2>预览处置提示词建议</h2><p>${esc(proposal.reason)}</p><details><summary>当前提示词</summary><pre>${esc(current.content)}</pre></details><label>建议提示词<textarea id="handling-proposal-prompt" rows="10" maxlength="20000">${esc(proposal.prompt || proposal.policy?.prompt || '')}</textarea></label>${stale ? '<p class="helper">此建议需要按当前版本重新生成，原历史保留。</p>' : ''}<div id="modal-error" class="error-box"></div><div class="dialog-foot"><button data-action="close">关闭</button><button class="primary" data-handle="proposal-adopt" ${stale ? 'disabled' : ''}>采用</button></div>`);
}
document.addEventListener('click', async (e) => {
  const b = e.target.closest('[data-handle]');
  if (!b) return;
  const action = b.dataset.handle,
    id = b.dataset.taskId || selected;
  const html = b.innerHTML;
  b.disabled = true;
  try {
    switch (action) {
      case 'start':
        await beginHandling(id);
        break;
      case 'add-project': {
        const path = await rpc('pick', { kind: '本地项目目录', type: 'directory' });
        if (!path) break;
        const added = await rpc('handling_add_project', { cwd: path });
        handlingProjects = await rpc('handling_projects');
        const select = $('#handling-project');
        select.innerHTML =
          '<option value="">请选择项目</option>' +
          handlingProjects
            .map((p, i) => `<option value="${i}">${esc(p.name)} · ${esc(p.cwd)}</option>`)
            .join('');
        select.value = String(handlingProjects.findIndex((p) => p.id === added.id));
        break;
      }
      case 'submit': {
        const i = $('#handling-project').value;
        if (i === '') throw Error('请选择项目');
        const p = handlingProjects[Number(i)];
        const params = {
          task_id: id,
          project_id: p.id,
          ...(p.connection_id ? {connection_id:p.connection_id} : {}),
          cwd: p.cwd,
          instructions: $('#handling-instructions').value,
          stop_previous: !!$('#handling-stop-previous')?.checked,
        };
        handlingStarting.add(id);
        closeModal();
        try {
          await rpc('handling_start', params);
          await refreshState(true);
          toast('已开始调查，持续同步进展');
        } finally {
          handlingStarting.delete(id);
        }
        break;
      }
      case 'followup':
        handlingFollowup(id);
        break;
      case 'reconnect': {
        const r = taskRun(id);
        if (!r.thread_id) {
          setModal(
            `<h2>关联已创建的正式会话</h2><p class="helper">上次创建结果不确定。先在对应接入客户端检查是否已有同标题会话，复制会话编号后关联；确实没有创建时才点下方重试。</p><input id="handling-thread-id" placeholder="会话编号"><div id="modal-error" class="error-box"></div><div class="dialog-foot"><button data-action="close">关闭</button><button data-handle="reset-setup" data-task-id="${esc(id)}">已核实未创建，重新开始</button><button data-handle="attach" data-task-id="${esc(id)}">关联并核实</button></div>`,
          );
        } else {
          await rpc('handling_reconnect', { task_id: id });
          await refreshState(true);
        }
        break;
      }
      case 'reset-setup':
        await rpc('handling_reset_setup', { task_id: id, confirmed_no_thread: true });
        closeModal();
        await refreshState(true);
        await beginHandling(id, true);
        break;
      case 'attach':
        await rpc('handling_reconnect', { task_id: id, thread_id: $('#handling-thread-id').value });
        closeModal();
        await refreshState(true);
        break;
      case 'sync':
        await rpc('handling_sync', { task_id: id });
        await refreshState(true);
        break;
      case 'open':
        try {
          await rpc('handling_open', { thread_id: b.dataset.threadId || taskRun(id).thread_id });
        } catch (error) {
          toast(error.message);
        }
        break;
      case 'terminate':
        try {
          await rpc('handling_terminate', { task_id: id, run_id: b.dataset.runId });
          handlingComposers.delete(id);
          toast(
            handlingDesktop(taskRun(id))
              ? '本次处置已结束，Codex 继续运行'
              : '处置已终止，可以重新发起',
          );
        } finally {
          await refreshState(true);
        }
        break;
      case 'requests':
        handlingRequests(id);
        break;
      case 'answer': {
        const host = $('#handling-request'),
          run = (state.handling || []).find((r) => r.id === host.dataset.runId),
          q = run?.requests?.find((q) => JSON.stringify(q.id) === host.dataset.requestId);
        if (!q) throw new Error('请求已变化，请关闭并同步状态后重试');
        const requestKey = host.dataset.runId + ':' + host.dataset.requestId;
        if (approvalBusy.has(requestKey)) break;
        approvalBusy.add(requestKey);
        host.querySelectorAll('button').forEach((button) => (button.disabled = true));
        try {
          const answers = {};
          (q.params.questions || []).forEach(
            (q, i) => (answers[q.id] = $(`[data-handling-answer="${i}"]`).value),
          );
          await rpc('handling_request', {
            task_id: host.dataset.taskId,
            run_id: host.dataset.runId,
            thread_id: host.dataset.threadId,
            request_id: host.dataset.requestId,
            action: b.dataset.decision,
            option_id: b.dataset.optionId,
            answers,
          });
          if ($('#handling-request') === host) closeModal();
          await refreshState(true);
        } finally {
          approvalBusy.delete(requestKey);
          host.querySelectorAll('button').forEach((button) => (button.disabled = false));
        }
        break;
      }
      case 'batch':
        handlingBatch(id);
        break;
      case 'batch-confirm':
        await rpc('handling_request', {
          task_id: id,
          run_id: b.dataset.runId,
          thread_id: b.dataset.threadId,
          action: 'acceptAll',
          request_ids: b._requestIds,
        });
        if ($('#handling-batch-confirm') === b) closeModal();
        await refreshState(true);
        toast('已批准所列请求，新增请求仍需确认');
        break;
      case 'history': {
        const runs = taskRuns(id),
          current = taskRun(id),
          logs = await rpc('handling_audit', { task_id: id });
        setModal(
          `<h2>处置过程</h2>${runs
            .slice()
            .reverse()
            .map(
              (r) =>
                `<details data-handling-history="${esc(r.id)}" ${r.id === current?.id ? 'open' : ''}><summary>${esc(r.project_name || '处置会话')} · ${esc(handlingLabels[handlingState(r)] || handlingState(r))}</summary><div class="helper">${esc(r.thread_id)}</div>${(r.rounds || []).map((x, i) => `<details ${i === r.rounds.length - 1 ? 'open' : ''}><summary>第 ${i + 1} 轮 · ${esc(handlingLabels[x.status] || x.status)}</summary><p class="helper">本轮耗时：${handlingDuration(x.duration_ms)} · 开始：${x.started_at ? esc(date(x.started_at)) : '未知'} · 结束：${x.completed_at ? esc(date(x.completed_at)) : '未知'}</p><h3>你的输入</h3>${handlingMd(x.question)}<h3>执行过程</h3>${handlingProcess(x)}${(x.supplements || []).map((v) => `<h3>补充指令${v.accepted ? '' : ' · 待核实'}</h3><pre style="white-space:pre-wrap">${esc(v.text)}</pre>`).join('')}<h3>最终结论</h3>${handlingMd(x.conclusion || '本轮尚无最终结论')}</details>`).join('')}</details>`,
            )
            .join(
              '',
            )}<h3 style="margin-top:20px">上下文查询记录（最近 100 条）</h3>${logs.map((l) => `<div class="source"><b>${esc(date(l.time))} · ${esc(l.tool)} · ${l.ok ? '成功' : '拒绝'}</b><pre style="white-space:pre-wrap">${esc(JSON.stringify(l.arguments))}</pre><div class="helper">返回消息：${esc((l.message_ids || []).join('、') || '无')}</div></div>`).join('') || '<p class="helper">尚无上下文接口查询。</p>'}<div class="dialog-foot"><button data-action="close">关闭</button></div>`,
        );
        break;
      }
      case 'learn': {
        b.textContent = '正在生成建议…';
        const r = await rpc('handling_learn');
        await refreshState(true);
        toast(r.unchanged ? r.reason : '建议已生成，请预览后采用');
        break;
      }
      case 'policy-proposal':
        await previewHandlingProposal(b.dataset.id);
        break;
      case 'proposal-adopt':
        await rpc('handling_adopt',{...handlingProposal,prompt:$('#handling-proposal-prompt').value});
        closeModal();
        await refreshState(true);
        toast('已采用，历史版本保留');
        break;
    }
  } catch (error) {
    if (error.message === '本次处置已终止') {
      await refreshState(true);
      return;
    }
    if ($('#modal-error') && !$('#overlay').hidden) $('#modal-error').textContent = error.message;
    else toast(error.message);
    if (['start', 'submit'].includes(action)) await refreshState(false);
  } finally {
    b.disabled = false;
    if (action !== 'followup') b.innerHTML = html;
  }
});
