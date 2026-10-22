const adviceDrafts = new Map(),
  adviceOpen = new Set(),
  adviceBusy = new Set();
function adviceKey(kind, id) {
  return kind + ':' + id;
}
function ruleDecision(r) {
  return r.decision || (r.active ? 'enabled' : 'pending');
}
function adviceCard(kind, r) {
  const key = adviceKey(kind, r.id),
    status =
      kind === 'rule'
        ? ruleDecision(r)
        : kind === 'handling'
          ? r.adopted
            ? 'adopted'
            : r.rejected
              ? 'rejected'
              : 'ready'
          : r.status,
    ready = ['pending', 'ready'].includes(status) && !(kind === 'prompt' && status === 'pending'),
    title =
      kind === 'rule'
        ? r.title
        : kind === 'tag'
          ? r.tag.kind + ' · ' + r.tag.value
          : kind === 'handling'
            ? 'AI一键处置提示词与项目匹配'
            : '消息分析系统提示词',
    body = kind === 'rule' ? r.rule : r.reason || r.error || '',
    text = kind === 'prompt' ? r.proposed_prompt : kind === 'handling' ? r.policy.prompt : body;
  return `<article class="box advice-card" data-advice-card="${esc(key)}"><div class="row"><span class="badge">${{ rule: '判断规则', tag: '标签枚举', prompt: '系统提示词', handling: '处置提示词' }[kind]}</span><span class="tiny">${esc(date(r.created))}</span><span class="advice-status">${esc({ pending: kind === 'prompt' ? '等待生成' : '待确认', ready: '待确认', enabled: '已启用', adopted: '已启用', rejected: '已拒绝', unchanged: '无需修改', superseded: '已被替代', failed: '生成失败', interrupted: '已中断' }[status] || status)}</span></div><h3>${esc(title)}</h3><p class="advice-reason">${esc(body.length > 120 ? body.slice(0, 120) + '…' : body)}</p><details class="advice-details"><summary>内容与依据</summary><p>${esc(body)}</p><pre>${esc(text)}</pre>${kind === 'tag' ? `<pre>${esc((r.excerpts || []).join('\n'))}</pre>` : ''}${kind === 'prompt' ? `<details><summary>修改前的提示词</summary><pre>${esc(r.base_prompt)}</pre></details>` : ''}<div class="helper">依据：${esc((r.evidence_ids || r.evidence || []).join('、') || '见操作记录')}</div></details>${ready ? `<div class="row advice-actions">${kind === 'tag' ? `<button data-tag-preview="${esc(r.id)}">预览并启用</button>` : kind === 'handling' ? `<button data-handle="policy-proposal" data-id="${esc(r.id)}">预览并启用</button>` : `<button class="primary" data-advice-action="enable" data-kind="${kind}" data-id="${esc(r.id)}">启用</button>`}<button data-advice-action="reject" data-kind="${kind}" data-id="${esc(r.id)}">拒绝</button>${kind === 'prompt' ? `<button data-advice-action="retry" data-kind="prompt" data-id="${esc(r.id)}">按当前版本重新生成</button>` : ''}<button class="quiet" data-advice-reply="${esc(key)}" aria-expanded="${adviceOpen.has(key)}">批复 ${adviceOpen.has(key) ? '▴' : '▾'}</button></div><div class="advice-composer" ${adviceOpen.has(key) ? '' : 'hidden'}><label>批复意见</label><textarea data-advice-text="${esc(key)}" rows="3" maxlength="4000" placeholder="说明你希望怎样调整这条建议。" ${adviceBusy.has(key) ? 'disabled' : ''}>${esc(adviceDrafts.get(key) || '')}</textarea><div class="row"><span class="helper">AI 建议可先预览，只有你手动启用后生效。</span><button data-advice-action="feedback" data-kind="${kind}" data-id="${esc(r.id)}" ${adviceBusy.has(key) ? 'disabled' : ''}>${adviceBusy.has(key) ? '正在修改…' : '提交批复'}</button></div></div>` : kind === 'prompt' && ['failed', 'interrupted'].includes(status) ? `<button data-advice-action="retry" data-kind="prompt" data-id="${esc(r.id)}">重新生成</button>` : ''}</article>`;
}
function tagHistory() {
  const s = state.tag_state;
  if (!s) return '';
  return `<details class="box"><summary>标签目录与版本历史（${s.versions?.length || 0}）</summary>${[
    ...(s.versions || []),
  ]
    .reverse()
    .map(
      (v, i) =>
        `<details class="tag-version"><summary>${esc(date(v.created))}${i ? '' : ' · 当前生效'}</summary><p>${esc(v.reason)}</p>${v.groups.map((g) => `<div class="helper"><b>${esc(g.name)}</b>：${g.values.map(esc).join('、')}</div>`).join('')}${i ? `<button data-tag-rollback="${esc(v.id)}">回滚此标签版本</button>` : ''}</details>`,
    )
    .join('')}</details>`;
}
let promptSection = 'history';
function systemPromptTabs() {
  return `<div class="pills" role="tablist" aria-label="系统提示词内容" style="margin-bottom:16px"><button role="tab" data-prompt-section="history" aria-selected="${promptSection === 'history'}" class="${promptSection === 'history' ? 'active' : ''}">提示词与版本</button><button role="tab" data-prompt-section="advice" aria-selected="${promptSection === 'advice'}" class="${promptSection === 'advice' ? 'active' : ''}">迭代建议</button><button role="tab" data-prompt-section="memory" aria-selected="${promptSection === 'memory'}" class="${promptSection === 'memory' ? 'active' : ''}">AI 记忆</button></div>`;
}
async function renderSystemPrompts() {
  if (promptSection === 'memory') {
    ++promptRequest;
    await renderMemory();
  } else if (promptSection === 'advice') {
    ++promptRequest;
    renderLearning();
  } else await renderPromptHistory();
}
function renderLearning() {
  $('#view').dataset.contentPage = 'prompts';
  $('#view').dataset.renderedPromptSection = 'advice';
  const active = document.activeElement,
    key = active?.dataset.adviceText,
    caret = key ? [active.selectionStart, active.selectionEnd] : null;
  const rows = [
      ...(state.suggestions || []).filter((r) => !r.archived_at).map((r) => ['rule', r]),
      ...(state.tag_state?.proposals || []).filter((r) => !r.archived_at).map((r) => ['tag', r]),
      ...(state.evolution?.prompts || [])
        .filter(
          (r) =>
            !r.archived_at &&
            ['ready', 'adopted', 'rejected', 'unchanged', 'superseded'].includes(r.status) &&
            r.reason?.trim() &&
            (r.status === 'unchanged' || r.proposed_prompt?.trim()),
        )
        .map((r) => ['prompt', r]),
      ...(state.evolution?.handling || [])
        .filter((r) => !r.archived_at)
        .map((r) => ['handling', r]),
    ].sort((a, b) => b[1].created - a[1].created),
    settled = ([k, r]) =>
      k === 'rule'
        ? ['enabled', 'rejected', 'integrated'].includes(ruleDecision(r))
        : k === 'handling'
          ? r.adopted || r.rejected
          : ['adopted', 'rejected', 'unchanged', 'superseded'].includes(r.status),
    pending = rows.filter(([k, r]) => (k === 'prompt' ? r.status === 'ready' : !settled([k, r]))),
    done = rows.filter(settled);
  setHtml(
    '#view',
    `<div class="page advice-page">${systemPromptTabs()}<div class="box"><h2>待确认 ${pending.length} 条</h2>${help('保留历史及手动生成的建议；可预览、启用、拒绝或批复。归档不再修改或自动采用提示词。')}<div class="row"><button data-handle="learn">从处置记录生成建议</button><button data-prompt-section="history">提示词与版本</button></div></div>${pending.map(([k, r]) => adviceCard(k, r)).join('') || empty('没有待确认的建议', '继续使用和修正事项，有充分依据时会产生新建议。')}${done.length ? `<details class="box"><summary>已处理 ${done.length} 条 · 下次归档后收起</summary>${done.map(([k, r]) => adviceCard(k, r)).join('')}</details>` : ''}${tagHistory()}</div>`,
  );
  if (key) {
    const el = [...document.querySelectorAll('[data-advice-text]')].find(
      (el) => el.dataset.adviceText === key,
    );
    if (el) {
      el.focus({ preventScroll: true });
      el.setSelectionRange(...caret);
    }
  }
}
let promptTarget = 'classify',
  promptEditing = false;
async function renderPromptHistory() {
  const request = ++promptRequest,
    epoch = renderEpoch;
  if (
    $('#view').dataset.contentPage !== 'prompts' ||
    $('#view').dataset.renderedPromptSection !== 'history'
  ) {
    setHtml(
      '#view',
      `<div class="page prompt-workspace">${systemPromptTabs()}<div class="box">读取系统提示词…</div></div>`,
    );
    $('#view').dataset.contentPage = 'prompts';
    $('#view').dataset.renderedPromptSection = 'history';
  }
  const result = await rpc('prompt_history');
  if (
    page !== 'prompts' ||
    promptSection !== 'history' ||
    epoch !== renderEpoch ||
    request !== promptRequest
  )
    return;
  promptHistory = result;
  promptEditing = false;
  renderPromptWorkspace();
}
function renderPromptWorkspace() {
  const result = promptHistory,
    versions = (result.versions || [])
      .filter((v) => v.target === promptTarget)
      .slice()
      .reverse(),
    current = versions.find((v) => v.id === result.current[promptTarget]);
  setHtml(
    '#view',
    `<div class="page prompt-workspace">${systemPromptTabs()}<div class="prompt-toolbar"><label>用途 <select id="prompt-target">${Object.entries(
      promptLabels,
    )
      .map(
        ([k, label]) =>
          `<option value="${k}" ${k === promptTarget ? 'selected' : ''}>${label}</option>`,
      )
      .join(
        '',
      )}</select></label>${help('系统提示词仅由你配置；编辑、手动采用和回滚保留版本。固定协议由程序追加，AI 不能修改。')}<span class="tiny">${current ? '更新于 ' + esc(date(current.created)) : ''}</span><button data-prompt-edit>编辑</button>${promptTarget === 'handling' ? '<button data-handle="policy-edit">项目匹配</button><button data-workflow="handling-history">匹配版本</button>' : ''}</div><section class="prompt-current"><textarea id="prompt-content" readonly spellcheck="false" aria-label="当前提示词" maxlength="20000">${esc(current?.content || '')}</textarea><div class="editor-actions" id="prompt-editor-actions" hidden><button data-prompt-reset>恢复内置</button><button data-prompt-cancel>取消</button><button class="primary" data-prompt-save>保存</button></div><div class="error-box" id="prompt-error"></div></section><details class="prompt-history"><summary>历史版本 · ${versions.length}</summary>${versions.map((v) => `<details class="prompt-version"><summary><span>${esc(date(v.created))}</span><span>${esc({ initial: '初始', manual: '编辑', adopt: '采用', auto_adopt: '自动采用', consolidate: '归档', rollback: '回滚' }[v.action] || v.action)}</span>${current?.id === v.id ? '<span class="badge">当前</span>' : ''}</summary><p class="helper">${esc(v.reason || '')}</p><div class="version-compare">${current?.id !== v.id ? `<label>当前<textarea readonly rows="8">${esc(current?.content || '')}</textarea></label>` : ''}<label>此版本<textarea readonly rows="8">${esc(v.content)}</textarea></label></div>${current?.id !== v.id ? `<div class="editor-actions"><button data-prompt-rollback="${esc(v.id)}">回滚到此版本</button></div>` : ''}</details>`).join('')}</details></div>`,
  );
}
document.addEventListener('change', (e) => {
  if (e.target.id === 'prompt-target') {
    if (promptEditing && !confirm('放弃当前提示词编辑？')) {
      e.target.value = promptTarget;
      return;
    }
    promptTarget = e.target.value;
    promptEditing = false;
    settingsDirty = false;
    renderPromptWorkspace();
  }
});
document.addEventListener('click', async (e) => {
  const b = e.target.closest('button');
  if (!b) return;
  try {
    if (b.hasAttribute('data-prompt-edit')) {
      promptEditing = true;
      settingsDirty = true;
      $('#prompt-content').readOnly = false;
      $('#prompt-editor-actions').hidden = false;
      b.hidden = true;
      $('#prompt-content').focus();
    }
    if (b.hasAttribute('data-prompt-cancel')) {
      promptEditing = false;
      settingsDirty = false;
      renderPromptWorkspace();
    }
    if (b.hasAttribute('data-prompt-reset')) {
      promptDefaults = promptDefaults || (await rpc('prompts'));
      if (promptEditing && $('#prompt-content'))
        $('#prompt-content').value = promptDefaults[promptTarget] || '';
    }
    if (b.hasAttribute('data-prompt-save')) {
      const controls = [...$('.prompt-workspace').querySelectorAll('button,select,textarea')];
      controls.forEach((el) => (el.disabled = true));
      try {
        await rpc('prompt_save', {
          target: promptTarget,
          text: $('#prompt-content').value,
          expected: promptHistory.current[promptTarget],
        });
        promptEditing = false;
        settingsDirty = false;
        await refreshState(true);
        toast('提示词已保存');
      } finally {
        controls.forEach((el) => (el.disabled = false));
      }
    }
  } catch (error) {
    if ($('#prompt-error')) $('#prompt-error').textContent = error.message;
    else toast(error.message);
  }
});
