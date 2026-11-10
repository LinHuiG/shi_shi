// Codex starts in the desktop composer; DSH hands over after its first turn.

const handlingLabels = {
  waiting: '等你在 Codex 按回车',
  running: '首轮进行中',
  completed: '已交给客户端',
  failed: '启动失败',
  interrupted: '首轮已中断',
};
const handlingClient = (r) => ((r?.driver || r?.connection?.driver) === 'dsh' ? 'DSH' : 'Codex');
const taskRun = (id) => (state?.handling || []).find((r) => r.task_id === id);
let handlingChoices = null;
const handlingStarting = new Set();

// The detail action: start a session, or go back to the one handed over.
function handlingButton(id) {
  const r = taskRun(id);
  if (handlingStarting.has(id) || r?.status === 'running') return 'AI 处置中';
  return r && (r.thread_id || r.status === 'waiting') ? `在 ${handlingClient(r)} 中继续` : 'AI一键处置';
}
function handlingBadge(id) {
  const r = taskRun(id);
  if (r?.status === 'running')
    return `<span class="badge blue" data-handling-status="running"><span class="spin"></span>${handlingClient(r)} 处置中</span>`;
  return r?.status === 'failed' ? '<span class="badge" data-handling-status="failed">AI 处置失败</span>' : '';
}
// Under the actions: where the session lives, a failure, and a fresh start.
function handlingNote(t) {
  const r = taskRun(t.id);
  if (!r || r.status === 'running') return '';
  const where = r.thread_id
    ? `已交给 ${esc(handlingClient(r))}${r.project_name ? ` · ${esc(r.project_name)}` : ''}`
    : esc(handlingLabels[r.status] || r.status);
  return `<section class="handling-note"><span class="helper">${where}</span>${r.error ? `<span class="helper error-text">${esc(r.error)}</span>` : ''}${r.thread_id ? `<button class="quiet small" data-handle="restart" data-task-id="${esc(t.id)}">重新处置</button>` : ''}</section>`;
}

// ---------------------------------------------------------------- pickers

// The start dialog picks connection, model and effort in one menu; the
// settings' handling model is the default and a choice lasts only for this
// session.
const handlingConnection = (id) => handlingChoices?.connections.find((c) => c.id === id);
const handlingModelName = (m) => [m.providerName, m.displayName || m.model].filter(Boolean).join(' / ');
function handlingEfforts(pick) {
  const model = handlingConnection(pick.connection)?.models.find((m) => m.model === pick.model);
  const efforts = (model?.supportedReasoningEfforts || []).map((e) => e.reasoningEffort);
  if (pick.effort && !efforts.includes(pick.effort)) efforts.push(pick.effort);
  return efforts;
}
function handlingChip(pick) {
  const c = handlingConnection(pick.connection);
  if (c.driver === 'codex') return `<button type="button" class="handling-chip" data-handling-menu="model" aria-haspopup="listbox" aria-expanded="false"><span>${esc(c.name)}</span>${icon('chevron')}</button>`;
  const m = c?.models.find((m) => m.model === pick.model);
  const model = m ? m.displayName || m.model : pick.model || '默认模型';
  const effort = pick.effort ? effortNames[pick.effort] || pick.effort : '默认强度';
  const title = [c?.name || handlingClient(c), m ? handlingModelName(m) : model, effort].join(' · ');
  return `<button type="button" class="handling-chip" data-handling-menu="model" aria-haspopup="listbox" aria-expanded="false" title="${esc(title)}，仅本次会话生效"><span>${esc(model)}</span><small>${esc(effort)}</small>${icon('chevron')}</button>`;
}

let handlingMenu = null;
function closeHandlingMenu(focus = false) {
  const menu = handlingMenu;
  if (!menu) return;
  handlingMenu = null;
  menu.el.remove();
  menu.anchor.setAttribute('aria-expanded', 'false');
  if (focus && menu.anchor.isConnected) menu.anchor.focus({ preventScroll: true });
}
function handlingMenuItems(kind, host, query) {
  const q = query.trim().toLowerCase(),
    hit = (...texts) =>
      !q ||
      texts.some((t) =>
        String(t || '')
          .toLowerCase()
          .includes(q),
      );
  const option = (attrs, selected, body, title = '') =>
    `<button type="button" role="option" ${attrs} aria-selected="${selected}" ${title ? `title="${esc(title)}"` : ''}>${selected ? icon('check') : '<span class="icon"></span>'}<span class="handling-option-text">${body}</span></button>`;
  if (kind === 'project') {
    const current = handlingChoices.project;
    return (handlingChoices.list || [])
      .filter((p) => hit(p.name, p.cwd))
      .map((p) =>
        option(
          `data-project="${esc(p.id)}"`,
          p.id === current,
          `<b>${esc(p.name)}</b><small>${esc(p.cwd)}</small>`,
          p.cwd,
        ),
      )
      .join('');
  }
  const pick = handlingChoices.pick,
    connections = handlingChoices.connections;
  const groups = connections
    .map((c) => {
      const items = (c.driver === 'codex' ? [{ model: '', label: c.name }] : [
        { model: '', label: '接入默认模型' },
        ...c.models.map((m) => ({ model: m.model, label: handlingModelName(m) })),
      ])
        .filter((m) => hit(m.label, m.model, c.name))
        .map((m) =>
          option(
            `data-connection="${esc(c.id)}" data-model="${esc(m.model)}"`,
            c.id === pick.connection && (c.driver === 'codex' || m.model === (pick.model || '')),
            `<span>${esc(m.label)}</span>`,
            m.label,
          ),
        )
        .join('');
      return (
        items &&
        `<div role="group" aria-label="${esc(c.name || handlingClient(c))}"><div class="handling-menu-group">${esc(c.name || handlingClient(c))}</div>${items}</div>`
      );
    })
    .join('');
  if (handlingConnection(pick.connection).driver === 'codex') return groups;
  const efforts = ['', ...handlingEfforts(pick)]
    .map(
      (e) =>
        `<button type="button" data-effort="${esc(e)}" aria-pressed="${e === (pick.effort || '')}">${esc(e ? effortNames[e] || e : '默认')}</button>`,
    )
    .join('');
  return `${groups}<div class="handling-menu-efforts" role="group" aria-label="推理强度"><span>推理强度</span>${efforts}</div>`;
}
function renderHandlingMenu() {
  const { el, kind, host, input } = handlingMenu;
  const list = el.querySelector('.handling-menu-list');
  list.innerHTML = handlingMenuItems(kind, host, input.value) || '<p class="helper">没有匹配项</p>';
}
// Opens next to its anchor inside the viewport and the dialog: below for
// the project field, above for the bottom-left model chip when it fits.
function openHandlingMenu(anchor) {
  closeHandlingMenu();
  const kind = anchor.dataset.handlingMenu,
    host = anchor.closest('[data-handling-picker]'),
    bounds = (anchor.closest('.dialog, .handling-window') || document.body).getBoundingClientRect();
  const el = document.createElement('div');
  el.className = 'handling-menu';
  el.innerHTML = `<input type="search" aria-label="${kind === 'project' ? '搜索项目' : '搜索模型'}" placeholder="${kind === 'project' ? '搜索项目或路径' : '搜索模型或接入'}" autocomplete="off"><div class="handling-menu-list" role="listbox"></div>`;
  document.body.append(el);
  handlingMenu = { el, kind, host, anchor, input: el.querySelector('input') };
  renderHandlingMenu();
  const a = anchor.getBoundingClientRect(),
    left = Math.max(bounds.left + 8, 8),
    right = Math.min(bounds.right - 8, innerWidth - 8);
  el.style.width = `${Math.min(kind === 'project' ? Math.max(a.width, 320) : 340, right - left)}px`;
  const below = innerHeight - a.bottom - 12,
    above = a.top - 12,
    up = kind === 'model' ? above >= Math.min(el.offsetHeight, 360) || above > below : below < 200 && above > below;
  el.style.maxHeight = `${Math.min(420, up ? above : below)}px`;
  el.style.left = `${Math.max(left, Math.min(a.left, right - el.offsetWidth))}px`;
  el.style.top = `${up ? a.top - 4 - el.offsetHeight : a.bottom + 4}px`;
  anchor.setAttribute('aria-expanded', 'true');
  handlingMenu.input.focus({ preventScroll: true });
}
async function chooseHandlingConnection(id) {
  const c = handlingChoices;
  if (c.loading === id) return;
  const kept = c.project;
  c.loading = id;
  c.list = null;
  c.project = '';
  syncHandlingStart();
  let list = [],
    error = '';
  try {
    list = await rpc('handling_projects', { connection_id: id });
  } catch (e) {
    error = e.message;
  }
  if (handlingChoices !== c || c.loading !== id) return;
  c.loading = '';
  c.list = list;
  c.error = error;
  // A project chosen before the model stays chosen while the new list has it.
  c.project = ([kept, c.suggested].find((id) => id && list.some((p) => p.id === id)) ?? list[0]?.id) || '';
  syncHandlingStart();
}
document.addEventListener('click', (e) => {
  const anchor = e.target.closest('[data-handling-menu]');
  if (anchor) return handlingMenu?.anchor === anchor ? closeHandlingMenu() : openHandlingMenu(anchor);
  const b = handlingMenu && e.target.closest('.handling-menu button');
  if (!b) return;
  const { kind, host } = handlingMenu;
  if (kind === 'project') {
    handlingChoices.project = b.dataset.project;
    closeHandlingMenu(true);
    return syncHandlingStart();
  }
  const pick = { ...handlingChoices.pick };
  if (b.dataset.effort !== undefined) pick.effort = b.dataset.effort;
  else {
    if (pick.connection !== b.dataset.connection) pick.effort = '';
    Object.assign(pick, { connection: b.dataset.connection, model: b.dataset.model });
    if (!handlingEfforts(pick).includes(pick.effort)) pick.effort = '';
  }
  const switched = pick.connection !== handlingChoices.pick.connection;
  handlingChoices.pick = pick;
  host.innerHTML = handlingChip(pick);
  handlingMenu.anchor = host.firstElementChild;
  if (b.dataset.effort !== undefined) closeHandlingMenu(true);
  else renderHandlingMenu();
  if (switched) void chooseHandlingConnection(pick.connection);
});
document.addEventListener('input', (e) => {
  if (handlingMenu && e.target === handlingMenu.input) renderHandlingMenu();
});
document.addEventListener('pointerdown', (e) => {
  if (handlingMenu && !e.target.closest('.handling-menu, [data-handling-menu]')) closeHandlingMenu();
});
addEventListener('resize', () => closeHandlingMenu());
// Captured so Esc closes the menu before it closes the dialog.
document.addEventListener(
  'keydown',
  (e) => {
    if (!handlingMenu || e.isComposing) return;
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopImmediatePropagation();
      return closeHandlingMenu(true);
    }
    if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
    const items = [...handlingMenu.el.querySelectorAll('[role="option"]')];
    if (!items.length) return;
    e.preventDefault();
    const at = items.indexOf(document.activeElement),
      next = e.key === 'ArrowDown' ? (at + 1) % items.length : at <= 0 ? items.length - 1 : at - 1;
    items[next].focus();
  },
  true,
);

// ---------------------------------------------------------------- start

// Projects come only from the chosen connection's client; a suggestion
// counts only when that client offers it.
function syncHandlingStart() {
  const field = $('#handling-project');
  if (!field || !handlingChoices) return;
  const c = handlingChoices,
    project = c.list?.find((p) => p.id === c.project),
    client = handlingClient(handlingConnection(c.pick.connection));
  field.disabled = !c.list?.length;
  field.innerHTML = `<span class="handling-option-text">${project ? `<b>${esc(project.name)}</b><small>${esc(project.cwd)}</small>` : `<b>${c.list ? '没有可选项目' : '正在读取项目…'}</b>`}</span>${icon('chevron')}`;
  field.title = project?.cwd || '';
  const note = $('#handling-project-note');
  note.textContent = c.error
    ? `读取 ${client} 项目失败：${c.error}`
    : c.list && !c.list.length
      ? `请先在 ${client} 中添加项目。`
      : '';
  note.hidden = !note.textContent;
  note.classList.toggle('error-text', !!c.error);
  $('#handling-start-note').textContent = client === 'Codex'
    ? '拾事会在 Codex 桌面端新建对话并填好第一句，按回车即开始，过程中可随时中断、追加。'
    : '拾事用所选模型在 Codex 或 DSH 中跑完首轮调查，再把会话交给客户端继续；进展和结论由客户端通过拾事 CLI 写回事项。';
  $('#dialog [data-handle="submit"]').disabled = !project;
}
async function beginHandling(id) {
  const p = await rpc('handling_prepare', { task_id: id });
  if (!p.default) throw Error('请先在设置中添加 Codex 或 DeepSeek Harness 接入');
  const d = p.default;
  handlingChoices = { ...p, task: id, pick: { connection: d.connection_id, model: d.model, effort: d.effort } };
  setModal(
    `<h2>AI一键处置</h2><p class="subtitle" id="handling-start-note"></p><div class="form-group"><label id="handling-project-label">项目</label><button type="button" class="handling-select" id="handling-project" data-handling-menu="project" aria-haspopup="listbox" aria-expanded="false" aria-labelledby="handling-project-label"></button><p class="helper" id="handling-project-note" hidden></p></div><div class="form-group"><label for="handling-instructions">本次补充说明（可选）</label><textarea id="handling-instructions" maxlength="2000" placeholder="例如：先核对复现步骤和相关日志。"></textarea></div><details class="handling-prompt"><summary>查看当前通用调查提示词</summary><pre>${esc(p.prompt)}</pre></details><div id="modal-error" class="error-box"></div><div class="dialog-foot handling-start-foot"><span class="handling-chip-host" data-handling-picker>${handlingChip(handlingChoices.pick)}</span><button data-action="close">取消</button><button class="primary" data-handle="submit" data-task-id="${esc(id)}" disabled>开始调查</button></div>`,
  );
  syncHandlingStart();
  await chooseHandlingConnection(d.connection_id);
}

// ---------------------------------------------------------------- actions

async function handlingKickoff(kickoff) {
  await copyText(kickoff);
  toast('已在 Codex 新建对话并填好第一句，按回车开始（第一句也已复制）');
}

const handlingActions = {
  async start(id) {
    const r = taskRun(id);
    if (handlingStarting.has(id) || r?.status === 'running')
      return toast(r?.thread_id ? `${handlingClient(r)} 正在执行首轮，完成后即可继续` : '正在启动处置，请稍候');
    if (r && (r.thread_id || r.status === 'waiting')) {
      const v = await rpc('handling_open', { run_id: r.id });
      if (v && v.kickoff) await handlingKickoff(v.kickoff);
      else if (v && v.hint) toast(v.hint);
      return;
    }
    return beginHandling(id);
  },
  restart: (id) => beginHandling(id),
  async submit(id) {
    const c = handlingChoices,
      project = c.list?.find((p) => p.id === c.project);
    if (!project) throw Error('请选择项目');
    const params = {
      task_id: id,
      connection_id: c.pick.connection,
      cwd: project.cwd,
      project_name: project.name,
      instructions: $('#handling-instructions').value,
      model: c.pick.model,
      effort: c.pick.effort,
    };
    handlingStarting.add(id);
    closeModal();
    try {
      const v = await rpc('handling_start', params);
      await refreshState(true);
      if (v.kickoff) await handlingKickoff(v.kickoff);
      else toast(`已开始，首轮结束后会提醒，再到 ${handlingClient(handlingConnection(c.pick.connection))} 中继续`);
    } finally {
      handlingStarting.delete(id);
    }
  },
};
document.addEventListener('click', async (e) => {
  const b = e.target.closest('[data-handle]');
  if (!b) return;
  const html = b.innerHTML;
  b.disabled = true;
  try {
    await handlingActions[b.dataset.handle](b.dataset.taskId || selected, b);
  } catch (error) {
    if ($('#modal-error') && !$('#overlay').hidden) $('#modal-error').textContent = error.message;
    else toast(error.message);
  } finally {
    if (b.isConnected) {
      b.disabled = false;
      b.innerHTML = html;
    }
  }
});
