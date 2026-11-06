const taskMulti = { active: false, expanded: false, closing: false, busy: false, ids: new Set(), timer: null };
let taskContextMenu = null;
function closeTaskContext() {
  taskContextMenu?.remove();
  taskContextMenu = null;
}
function resetTaskMulti() {
  clearTimeout(taskMulti.timer);
  taskMulti.active = taskMulti.expanded = taskMulti.closing = false;
  taskMulti.ids.clear();
  closeTaskContext();
}
function syncTaskMulti(list) {
  const visible = new Set(list.map((t) => t.id));
  for (const id of taskMulti.ids) if (!visible.has(id)) taskMulti.ids.delete(id);
}
function setTaskMulti(active, id) {
  if (taskMulti.busy) return;
  closeTaskContext();
  clearTimeout(taskMulti.timer);
  if (active) {
    const opening = !taskMulti.active || taskMulti.closing;
    taskMulti.active = true;
    taskMulti.closing = false;
    if (id) taskMulti.ids.add(id);
    if (opening) taskMulti.expanded = false;
    renderTasks();
    const space = $('.task-bulk-space');
    if (space) {
      space.getBoundingClientRect();
      taskMulti.expanded = true;
      space.classList.add('is-open');
    }
  } else {
    taskMulti.expanded = false;
    taskMulti.closing = true;
    $('.task-bulk-space')?.classList.remove('is-open');
    const finish = () => {
      if (!taskMulti.closing) return;
      resetTaskMulti();
      if (page === 'tasks') renderTasks();
    };
    if (motionReduced()) finish();
    else taskMulti.timer = setTimeout(finish, 170);
  }
}
function taskBatchItems() {
  return taskFeedRows()
    .filter((t) => taskMulti.ids.has(t.id))
    .map((t) => ({ id: t.id, updated: t.updated }));
}
function taskBatchReason(action, tasks) {
  if (!tasks.length) return '请先选择事项';
  if (tasks.some((t) => t.merged_into || t.generation)) return '含未生成完成或已合并事项';
  const active = (t) => !t.archived && ['待处理', '处理中'].includes(t.status);
  if (action === 'reopen') return tasks.some(active) ? '含尚未结束事项' : '';
  if (action === 'archive') return tasks.some((t) => t.archived) ? '含已归档事项' : '';
  return tasks.every(active) ? '' : '含已结束或已归档事项';
}
function taskBulkBar(list) {
  if (!taskMulti.active) return '';
  const tasks = list.filter((t) => taskMulti.ids.has(t.id));
  const disabled = taskMulti.busy ? 'disabled' : '';
  const action = (key, label, extra = '') => {
    const reason = taskBatchReason(key, tasks);
    return `<button type="button" role="menuitem" data-task-batch="${key}" ${extra} ${reason || taskMulti.busy ? 'disabled' : ''} title="${esc(reason)}">${label}${reason && tasks.length ? `<span class="tiny">${esc(reason)}</span>` : ''}</button>`;
  };
  return `<div class="task-bulk-space ${taskMulti.expanded ? 'is-open' : ''}" data-ui-key="task-bulk-space"><div class="task-bulk-clip"><div class="task-bulk-bar" role="toolbar" aria-label="批量管理事项"><span class="task-bulk-count" role="status">已选中<b>${tasks.length}/${list.length}</b></span><button type="button" class="quiet" data-task-multi-all ${disabled} ${list.length ? '' : 'disabled'}>全选</button><button type="button" class="quiet" data-task-multi-invert ${disabled} ${list.length ? '' : 'disabled'}>反选</button><div class="task-bulk-actions"><button type="button" data-task-batch-menu aria-haspopup="menu" aria-expanded="false" ${disabled} ${tasks.length ? '' : 'disabled'}>${taskMulti.busy ? '正在保存…' : '变更状态'}<span class="task-status-chevron" aria-hidden="true"></span></button><div class="task-bulk-menu task-status-menu" role="menu" hidden>${action('start', '开始处理')}${action('done', '处理完成')}<details class="task-bulk-defer"><summary>延后处理</summary>${taskDeferralChoices()
    .map((choice) => action('defer', choice.label, `data-batch-choice="${choice.choice}"`))
    .join(
      '',
    )}</details>${action('dismiss', '无需处理')}${action('reopen', '重新打开')}${action('archive', '归档')}</div></div><button type="button" class="quiet task-multi-exit" data-task-multi-exit ${disabled}>退出多选</button></div></div></div>`;
}
function taskMultiRow(html, task) {
  if (!taskMulti.active) return html;
  return `<div class="task-choice ${taskMulti.ids.has(task.id) ? 'is-checked' : ''}" data-ui-key="choice-${esc(task.id)}">${html}<input type="checkbox" class="task-check" data-task-check="${esc(task.id)}" aria-label="选择事项：${esc(task.title)}" ${taskMulti.ids.has(task.id) ? 'checked' : ''} ${taskMulti.busy ? 'disabled' : ''}></div>`;
}
function updateTaskBulkBar() {
  if (page !== 'tasks' || !taskMulti.active) return;
  renderTasks();
}
async function runTaskBatch(action, options = {}, items = taskBatchItems()) {
  if (taskMulti.busy || !items.length) return;
  taskMulti.busy = true;
  updateTaskBulkBar();
  const archiveDialog = $('[data-task-batch-archive]');
  if (archiveDialog) archiveDialog.querySelectorAll('button,select').forEach((el) => (el.disabled = true));
  try {
    const result = await rpc('task_batch_action', { items, action, ...options });
    for (const item of items) taskMulti.ids.delete(item.id);
    if (archiveDialog?.isConnected) closeModal();
    await refreshState(true);
    toast(`已更新${result.updated}项事项`);
  } catch (error) {
    if (archiveDialog?.isConnected) $('#modal-error').textContent = error.message;
    else toast(error.message);
  } finally {
    taskMulti.busy = false;
    if (archiveDialog?.isConnected)
      archiveDialog.querySelectorAll('button,select').forEach((el) => (el.disabled = false));
    updateTaskBulkBar();
  }
}
document.addEventListener('contextmenu', (event) => {
  const row = event.target.closest('.task-row[data-task]');
  if (page !== 'tasks' || !row || !$('#overlay').hidden) return;
  event.preventDefault();
  closeTaskContext();
  const menu = document.createElement('div');
  menu.className = 'task-context-menu task-status-menu';
  menu.setAttribute('role', 'menu');
  const leave = taskMulti.active && !taskMulti.closing;
  menu.innerHTML = `<button type="button" role="menuitem" ${taskMulti.busy ? 'disabled' : ''}>${leave ? '退出多选' : '多选'}</button>`;
  document.body.append(menu);
  menu.style.left = `${Math.max(8, Math.min(event.clientX, innerWidth - menu.offsetWidth - 8))}px`;
  menu.style.top = `${Math.max(8, Math.min(event.clientY, innerHeight - menu.offsetHeight - 8))}px`;
  menu.querySelector('button').onclick = () => setTaskMulti(!leave, row.dataset.task);
  taskContextMenu = menu;
  menu.querySelector('button').focus({ preventScroll: true });
});
document.addEventListener('pointerdown', (event) => {
  if (!event.target.closest('.task-context-menu')) closeTaskContext();
  if (!event.target.closest('.task-bulk-actions')) {
    const menu = $('.task-bulk-menu');
    if (menu) menu.hidden = true;
    $('[data-task-batch-menu]')?.setAttribute('aria-expanded', 'false');
  }
});
document.addEventListener('change', (event) => {
  const id = event.target.dataset.taskCheck;
  if (!id || taskMulti.busy) return;
  if (event.target.checked) taskMulti.ids.add(id);
  else taskMulti.ids.delete(id);
  updateTaskBulkBar();
});
document.addEventListener('click', (event) => {
  const button = event.target.closest('button');
  if (!button || button.disabled) return;
  if (button.hasAttribute('data-task-multi-exit')) setTaskMulti(false);
  else if (button.hasAttribute('data-task-multi-all') || button.hasAttribute('data-task-multi-invert')) {
    for (const task of taskFeedRows()) {
      if (button.hasAttribute('data-task-multi-invert') && taskMulti.ids.has(task.id)) taskMulti.ids.delete(task.id);
      else taskMulti.ids.add(task.id);
    }
    updateTaskBulkBar();
  } else if (button.hasAttribute('data-task-batch-menu')) {
    const menu = $('.task-bulk-menu');
    menu.hidden = !menu.hidden;
    button.setAttribute('aria-expanded', String(!menu.hidden));
    if (!menu.hidden) {
      const rect = button.getBoundingClientRect();
      menu.style.left = `${Math.max(8, Math.min(rect.left, innerWidth - menu.offsetWidth - 8))}px`;
      menu.style.top = `${Math.max(8, Math.min(rect.bottom + 4, innerHeight - menu.offsetHeight - 8))}px`;
    }
  } else if (button.dataset.taskBatch) {
    const action = button.dataset.taskBatch;
    if (action === 'archive') {
      const items = taskBatchItems();
      setModal(
        `<div data-task-batch-archive><h2>归档${items.length}项事项</h2><div class="form-group"><label for="batch-archive-status">本批归档结果</label><select id="batch-archive-status"><option value="">请选择归档结果</option>${['已完成', '无需处理', '已取消'].map((s) => `<option>${s}</option>`).join('')}</select></div><p class="helper">保留每条事项原有的归档备注。</p><div id="modal-error" class="error-box"></div><div class="dialog-foot"><button data-action="close">取消</button><button class="primary" data-batch-archive-save>保存并归档</button></div></div>`,
      );
      $('[data-batch-archive-save]').onclick = () => {
        const status = $('#batch-archive-status').value;
        if (!status) {
          $('#modal-error').textContent = '请选择本批归档结果';
          return;
        }
        void runTaskBatch('archive', { status }, items);
      };
    } else void runTaskBatch(action, button.dataset.batchChoice ? { choice: button.dataset.batchChoice } : {});
  }
});
document.addEventListener('keydown', (event) => {
  if (event.key !== 'Escape' || !$('#overlay').hidden) return;
  if (taskContextMenu) {
    closeTaskContext();
    event.preventDefault();
    return;
  }
  if (page === 'tasks' && taskMulti.active && !taskMulti.busy) {
    setTaskMulti(false);
    event.preventDefault();
  }
});
window.addEventListener('blur', closeTaskContext);
