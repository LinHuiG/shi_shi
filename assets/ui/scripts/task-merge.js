let mergePrimary = null,
  mergeTarget = '',
  mergeQuery = '';
function mergeForm(t) {
  mergePrimary = structuredClone(t);
  mergeTarget = '';
  mergeQuery = '';
  setModal(
    `<h2>合并事项</h2><p class="subtitle">保留当前事项，将所选事项的描述和消息依据合并进来。可在操作记录中撤销。</p><div class="merge-current"><span class="tiny">保留的事项</span><b>${esc(t.title)}</b><p>${esc(t.description)}</p><span class="tiny">${esc(t.status)}</span></div><input id="merge-search" aria-label="搜索合并事项" placeholder="搜索标题、描述或标签"><div class="helper" id="merge-count"></div><div id="merge-candidates" class="merge-candidates"></div><div id="merge-environment"></div><div id="modal-error" class="error-box"></div><div class="dialog-foot"><button data-action="close">取消</button><button class="primary" data-action="apply-merge" disabled>确认合并</button></div>`,
  );
  $('#dialog').classList.add('wide-dialog');
  renderMergeCandidates();
}
function mergeCandidates() {
  return state.tasks
    .filter((t) => t.id !== mergePrimary.id && !t.archived && !t.merged_into)
    .sort(
      (a, b) =>
        Number(a.status !== '待处理') - Number(b.status !== '待处理') || b.updated - a.updated,
    );
}
function renderMergeCandidates() {
  const all = mergeCandidates(),
    q = mergeQuery.trim().toLowerCase(),
    rows = all.filter(
      (t) =>
        !q ||
        (
          t.title +
          ' ' +
          t.description +
          ' ' +
          taskTags(t)
            .map((v) => v.kind + ' ' + v.value)
            .join(' ')
        )
          .toLowerCase()
          .includes(q),
    );
  $('#merge-count').textContent =
    `${all.length} 条可合并事项（不含当前事项） · 搜索结果 ${rows.length} 条`;
  setHtml(
    '#merge-candidates',
    rows
      .map(
        (t) =>
          `<label class="merge-option ${mergeTarget === t.id ? 'selected' : ''}"><input type="radio" name="merge-choice" value="${esc(t.id)}" ${mergeTarget === t.id ? 'checked' : ''}><span><b>${esc(t.title)}</b><span class="tiny">${esc(t.status)} · ${esc(
            taskTags(t)
              .map((tag) => tag.kind + ' · ' + tag.value)
              .join('、'),
          )}</span><p>${esc(t.description || '暂无描述')}</p></span></label>`,
      )
      .join('') || '<p class="helper">没有符合条件的事项</p>',
  );
  const target = all.find((t) => t.id === mergeTarget);
  setHtml('#merge-environment', '');
  updateMergeButton();
}
function updateMergeButton() {
  const t = mergeCandidates().find((t) => t.id === mergeTarget);
  $('[data-action="apply-merge"]').disabled = !t;
}
async function applyMerge() {
  const b = $('[data-action="apply-merge"]');
  if (b.disabled) return;
  b.disabled = true;
  try {
    await perform('merge', { ids: [mergePrimary.id, mergeTarget] }, '已合并，可在操作记录中撤销');
    closeModal();
  } catch (e) {
    $('#modal-error').textContent = e.message;
    updateMergeButton();
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
    inboxPage = 1;
    inboxSelected.clear();
    renderInbox();
  }
  if (el.id === 'inbox-page-size') {
    inboxPageSize = Number(el.value);
    inboxPage = 1;
    inboxSelected.clear();
    renderInbox();
  }
  if (el.dataset.inboxSelect) {
    el.checked
      ? inboxSelected.add(el.dataset.inboxSelect)
      : inboxSelected.delete(el.dataset.inboxSelect);
    renderInbox();
  }
  if (el.id === 'inbox-select-all') {
    for (const m of inboxRows()) el.checked ? inboxSelected.add(m.id) : inboxSelected.delete(m.id);
    renderInbox();
  }
  if (el.name === 'merge-choice') {
    mergeTarget = el.value;
    renderMergeCandidates();
  }
  if (el.id === 'merge-confirm-environment') updateMergeButton();
});
document.addEventListener('click', async (e) => {
  const b = e.target.closest('button');
  if (
    !b ||
    ![
      'settingsTarget',
      'promptSection',
      'adviceReply',
      'adviceAction',
      'inboxDelete',
      'workflow',
    ].some((k) => b.dataset[k])
  )
    return;
  try {
    if (b.dataset.settingsTarget) {
      $('#settings-' + b.dataset.settingsTarget)?.scrollIntoView({
        block: 'start',
        behavior: 'smooth',
      });
      return;
    }
    if (b.dataset.promptSection) {
      if (!(await leaveSettings())) return;
      settingsDirty = false;
      promptEditing = false;
      promptSection = ['advice', 'memory'].includes(b.dataset.promptSection)
        ? b.dataset.promptSection
        : 'history';
      page = 'prompts';
      await render();
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
      case 'inbox-prev':
        inboxPage = Math.max(1, inboxPage - 1);
        inboxSelected.clear();
        await renderInbox();
        break;
      case 'inbox-next':
        inboxPage = Math.min(inboxData.pages, inboxPage + 1);
        inboxSelected.clear();
        await renderInbox();
        break;
      case 'inbox-jump':
        inboxPage = Math.max(
          1,
          Math.min(inboxData.pages, Number($('#inbox-page-jump').value) || 1),
        );
        inboxSelected.clear();
        await renderInbox();
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
      case 'handling-history':
        page = 'handling';
        await render();
        break;
    }
  } catch (err) {
    if ($('#modal-error') && !$('#overlay').hidden) $('#modal-error').textContent = err.message;
    toast(err.message);
  } finally {
    if (!b.dataset.workflow?.startsWith('inbox-')) b.disabled = false;
  }
});

// ---- Work state, calendar correction, steps and archive versions ----
