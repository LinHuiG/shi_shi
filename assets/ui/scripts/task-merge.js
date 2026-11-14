let mergePrimary = '',
  mergeQuery = '',
  mergeBusy = false,
  mergeVersions = {},
  mergeRows = [];
const mergeSelected = new Set();
function mergeContent(t) {
  const status = t.status === '处理中' ? 'processing' : t.status === '已完成' ? 'done' : '',
    tags = taskTags(t);
  return `<span class="merge-content"><span class="merge-copy"><b title="${esc(t.title)}">${esc(t.title)}</b><span class="merge-description" title="${esc(t.description)}">${esc(t.description || '暂无描述')}</span></span><span class="merge-label-area"><span class="merge-meta" title="${esc([t.status, ...tags.map((tag) => tag.kind + '：' + tag.value)].join(' · '))}"><span class="merge-status ${status}">${esc(t.status)}</span> ${tags.map((tag) => `<span class="merge-tag" title="${esc(tag.kind + '：' + tag.value)}">${esc(tag.value)}</span>`).join(' ')}</span></span></span>`;
}
async function mergeForm(t) {
  mergeRows = (await rpc('merge_candidates', { id: t.id })).rows;
  mergePrimary = t.id;
  mergeQuery = '';
  mergeBusy = false;
  mergeSelected.clear();
  mergeVersions = Object.fromEntries([...mergeRows, t].map((task) => [task.id, task.updated]));
  setModal(
    `<header class="merge-dialog-header"><div><h2>合并事项</h2><p class="subtitle">将所选内容合入保留事项，可在操作记录中撤销。</p></div><button class="merge-close" data-action="close" aria-label="关闭合并事项"><svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2" aria-hidden="true"><path d="M3 3l10 10M13 3L3 13"/></svg></button></header><div class="merge-current"><span class="merge-current-label"><svg viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.2" aria-hidden="true"><path d="M2 2h8v8H2zM4 6l1.5 1.5L8.5 4"/></svg>保留的事项</span>${mergeContent(t)}</div><div class="merge-tools"><label class="merge-search"><svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2" aria-hidden="true"><circle cx="6.6" cy="6.6" r="4.3"/><path d="m10 10 3.5 3.5"/></svg><input id="merge-search" name="merge-search" type="search" autocomplete="off" aria-label="搜索合并事项" placeholder="搜索标题、描述或标签…"></label><span id="merge-count" aria-live="polite"></span><div class="merge-selection" role="group" aria-label="选择事项"><button data-merge-select="all" title="选中当前搜索结果">全选</button><button data-merge-select="invert" title="反选当前搜索结果">反选</button><span class="merge-tools-separator" aria-hidden="true"></span><button data-merge-select="clear" title="清空全部已选事项">清空</button></div></div><div id="merge-candidates" class="merge-candidates"></div><div id="modal-error" class="error-box" role="alert"></div><div class="dialog-foot"><button data-action="close">取消</button><button class="primary" data-action="apply-merge" disabled>确认合并</button></div>`,
  );
  $('#dialog').classList.add('merge-dialog');
  renderMergeCandidates();
}
function renderMergeCandidates() {
  const all = mergeRows,
    available = new Set(all.map((t) => t.id)),
    q = mergeQuery.trim().toLowerCase();
  for (const id of mergeSelected) if (!available.has(id)) mergeSelected.delete(id);
  const rows = all.filter(
    (t) =>
      !q ||
      [t.title, t.description, ...taskTags(t).map((tag) => tag.kind + ' ' + tag.value)]
        .join(' ')
        .toLowerCase()
        .includes(q),
  );
  setHtml(
    '#merge-count',
    `已选<strong>${mergeSelected.size}</strong><span>/${all.length}</span>${q ? `<span> · 结果${rows.length}</span>` : ''}`,
    { animate: false },
  );
  setHtml(
    '#merge-candidates',
    rows
      .map(
        (t) =>
          `<label class="merge-option ${mergeSelected.has(t.id) ? 'selected' : ''}" data-merge-id="${esc(t.id)}"><input type="checkbox" name="merge-choice" value="${esc(t.id)}" aria-label="选择${esc(t.title)}" ${mergeSelected.has(t.id) ? 'checked' : ''}>${mergeContent(t)}</label>`,
      )
      .join('') || '<p class="helper">没有符合条件的事项</p>',
    { animate: false },
  );
  document.querySelectorAll('[name="merge-choice"]').forEach((el) => {
    el.checked = mergeSelected.has(el.value);
    el.disabled = mergeBusy;
  });
  document.querySelectorAll('[data-merge-select]').forEach((el) => {
    el.disabled = mergeBusy || (el.dataset.mergeSelect === 'clear' ? !mergeSelected.size : !rows.length);
  });
  $('#merge-search').disabled = mergeBusy;
  document.querySelectorAll('.merge-dialog [data-action="close"]').forEach((el) => (el.disabled = mergeBusy));
  updateMergeButton();
}
function updateMergeButton() {
  const button = $('[data-action="apply-merge"]');
  button.disabled = mergeBusy || !mergeSelected.size;
  button.setAttribute('aria-busy', String(mergeBusy));
  button.textContent = mergeBusy
    ? '正在合并…'
    : mergeSelected.size
      ? `确认合并（${mergeSelected.size}项）`
      : '确认合并';
}
async function applyMerge() {
  const button = $('[data-action="apply-merge"]');
  if (button.disabled || mergeBusy) return;
  const ids = [mergePrimary, ...mergeSelected],
    versions = Object.fromEntries(ids.map((id) => [id, mergeVersions[id]]));
  mergeBusy = true;
  button.dataset.pending = 'true';
  renderMergeCandidates();
  $('#modal-error').textContent = '';
  try {
    await perform('merge', { ids, versions }, `已合并${ids.length - 1}项，可在操作记录中撤销`);
    delete button.dataset.pending;
    closeModal();
  } catch (error) {
    $('#modal-error').textContent = error.message;
  } finally {
    mergeBusy = false;
    delete button.dataset.pending;
    if (!$('#overlay').hidden && $('#merge-candidates')) renderMergeCandidates();
  }
}

document.addEventListener('input', (e) => {
  if (e.target.dataset.adviceText) adviceDrafts.set(e.target.dataset.adviceText, e.target.value);
  if (e.target.id === 'merge-search') {
    mergeQuery = e.target.value;
    renderMergeCandidates();
  }
});
document.addEventListener('change', (e) => {
  const el = e.target;
  if (modelFields.some(([, m]) => m === el.id)) {
    settingsDirty = true;
    updateEfforts(false, el.id);
  }
  if (el.id === 'inbox-from' || el.id === 'inbox-until') {
    inboxFrom = $('#inbox-from').value;
    inboxUntil = $('#inbox-until').value;
    inboxSelected.clear();
    renderInbox();
  }
  if (el.dataset.inboxSelect) {
    el.checked ? inboxSelected.add(el.dataset.inboxSelect) : inboxSelected.delete(el.dataset.inboxSelect);
    renderInbox();
  }
  if (el.id === 'inbox-select-all') {
    for (const m of inboxRows()) el.checked ? inboxSelected.add(m.id) : inboxSelected.delete(m.id);
    renderInbox();
  }
  if (el.name === 'merge-choice') {
    if (mergeBusy) return;
    el.checked ? mergeSelected.add(el.value) : mergeSelected.delete(el.value);
    renderMergeCandidates();
  }
});
document.addEventListener('click', async (e) => {
  const b = e.target.closest('button');
  if (
    !b ||
    !['settingsTarget', 'promptSection', 'adviceReply', 'adviceAction', 'inboxDelete', 'workflow', 'mergeSelect'].some(
      (k) => b.dataset[k],
    )
  )
    return;
  try {
    if (b.dataset.mergeSelect) {
      if (mergeBusy) return;
      if (b.dataset.mergeSelect === 'clear') mergeSelected.clear();
      else
        for (const el of document.querySelectorAll('[name="merge-choice"]')) {
          if (b.dataset.mergeSelect === 'all' || !mergeSelected.has(el.value)) mergeSelected.add(el.value);
          else mergeSelected.delete(el.value);
        }
      renderMergeCandidates();
      return;
    }
    if (b.dataset.settingsTarget) {
      if (page !== 'settings') {
        if (!(await leaveSettings())) return;
        pendingSettingsSection = b.dataset.settingsTarget;
        page = 'settings';
        await render();
        return;
      }
      $('#settings-' + b.dataset.settingsTarget)?.scrollIntoView({
        block: 'start',
        behavior: 'smooth',
      });
      return;
    }
    if (b.dataset.promptSection) {
      if (!(await leaveSettings())) return;
      promptSection = b.dataset.promptSection === 'memory' ? 'memory' : 'history';
      if (page === 'settings') await renderSystemPrompts();
      else {
        page = 'settings';
        pendingSettingsSection = 'system-prompts';
        await render();
      }
      return;
    }
    if (b.dataset.adviceReply) {
      const key = b.dataset.adviceReply;
      adviceOpen.has(key) ? adviceOpen.delete(key) : adviceOpen.add(key);
      const box = b.closest('.advice-card').querySelector('.advice-composer');
      box.hidden = !adviceOpen.has(key);
      b.setAttribute('aria-expanded', adviceOpen.has(key));
      b.textContent = '批复 ' + (adviceOpen.has(key) ? '▴' : '▾');
      if (!box.hidden) box.querySelector('textarea').focus();
      return;
    }
    if (b.dataset.adviceAction) {
      const { kind, id, adviceAction: action } = b.dataset,
        key = adviceKey(kind, id);
      if (adviceBusy.has(key)) return;
      adviceBusy.add(key);
      b.disabled = true;
      try {
        if (action === 'feedback') {
          const text = (adviceDrafts.get(key) || '').trim();
          if (!text) throw Error('请填写批复意见');
          renderLearning();
          await rpc('advice_feedback', { kind, id, text });
          adviceDrafts.delete(key);
          adviceOpen.delete(key);
          toast('建议已按批复修改，等待你启用');
        } else if (action === 'retry') await rpc('prompt_retry', { id });
        else if (kind === 'tag')
          await rpc('tag_decide', {
            action: 'reject',
            id,
            expected: state.tag_state.versions.at(-1).id,
          });
        else if (kind === 'prompt' && action === 'enable') await rpc('prompt_adopt', { id });
        else await rpc('advice_decide', { kind, id, action });
      } finally {
        adviceBusy.delete(key);
        await refreshState(true);
      }
      return;
    }
    if (b.dataset.inboxDelete) {
      confirmMessageDelete([b.dataset.inboxDelete]);
      return;
    }
    switch (b.dataset.workflow) {
      case 'inbox-sort':
        inboxSort = inboxSort === 'asc' ? 'desc' : 'asc';
        await renderInbox();
        break;
      case 'clear-time':
        inboxFrom = '';
        inboxUntil = '';
        $('#inbox-from').value = '';
        $('#inbox-until').value = '';
        renderInbox();
        break;
      case 'retention-settings':
        page = 'settings';
        await render();
        $('#settings-storage')?.scrollIntoView({ block: 'start' });
        $('#cfg-retention').focus();
        break;
      case 'delete-selected':
        confirmMessageDelete([...inboxSelected]);
        break;
      case 'delete-messages':
        b.disabled = true;
        await rpc('messages_delete', { ids: deletingMessages });
        for (const id of deletingMessages) inboxSelected.delete(id);
        closeModal();
        inboxLoadedKey = '';
        await refreshState(true);
        toast('消息已删除，关联事项保留');
        break;
    }
  } catch (err) {
    if ($('#modal-error') && !$('#overlay').hidden) $('#modal-error').textContent = err.message;
    toast(err.message);
  } finally {
    if (!b.dataset.mergeSelect && !b.dataset.workflow?.startsWith('inbox-')) b.disabled = false;
  }
});

// ---- Work state, calendar correction, steps and archive versions ----
