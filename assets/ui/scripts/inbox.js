let inboxSort = 'desc',
  inboxFrom = '',
  inboxUntil = '',
  inboxPage = 1,
  inboxPageSize = 50,
  inboxRequest = 0,
  inboxFilter = '',
  inboxData = { rows: [], page: 1, pages: 1, total: 0 },
  inboxLoadedKey = '';
const inboxSelected = new Set();
function inboxRows() {
  return inboxData.rows;
}
function inboxParameters() {
  const p = { query: inboxQuery, page: inboxPage, page_size: inboxPageSize, sort: inboxSort };
  if (inboxFrom) p.since = new Date(inboxFrom + ':00+08:00').getTime();
  if (inboxUntil) p.until = new Date(inboxUntil + ':00+08:00').getTime() + 59999;
  return p;
}
async function renderInbox() {
  const filter = JSON.stringify([inboxQuery, inboxFrom, inboxUntil, inboxPageSize, inboxSort]);
  if (filter !== inboxFilter) {
    inboxFilter = filter;
    inboxPage = 1;
    inboxSelected.clear();
  }
  if (!$('#inbox-search'))
    setHtml(
      '#view',
      `<div class="inbox-tools"><div class="inbox-filters"><label class="check"><input type="checkbox" id="inbox-select-all">选中本页</label><button data-workflow="delete-selected" id="inbox-delete-selected">删除选中</button><input id="inbox-search" aria-label="搜索消息" maxlength="200" placeholder="搜索消息" value="${esc(inboxQuery)}"><label>开始 <input type="datetime-local" id="inbox-from" value="${esc(inboxFrom)}"></label><label>结束 <input type="datetime-local" id="inbox-until" value="${esc(inboxUntil)}"></label><button data-workflow="clear-time">全部时间</button>${help('可搜索消息、发送人、会话和来源；过期清理设置位于设置页。')}</div><div class="row inbox-query-status"><button data-workflow="inbox-sort" id="inbox-sort"></button><span id="inbox-summary" class="tiny"></span><button class="quiet inbox-retention" data-workflow="retention-settings">保留设置</button></div></div><div class="page" id="inbox-content"></div><div class="row inbox-pager"><button data-workflow="inbox-prev">上一页</button><span id="inbox-page-label"></span><button data-workflow="inbox-next">下一页</button><label>每页 <select id="inbox-page-size">${[20, 50, 100].map((n) => `<option value="${n}" ${n === inboxPageSize ? 'selected' : ''}>${n} 条</option>`).join('')}</select></label><label>跳到 <input id="inbox-page-jump" type="number" min="1" value="${inboxPage}" aria-label="跳转页码"></label><button data-workflow="inbox-jump">前往</button></div>`,
    );
  $('#inbox-sort').textContent = inboxSort === 'asc' ? '时间 ↑ 正序' : '时间 ↓ 倒序';
  $('#inbox-sort').setAttribute(
    'aria-label',
    '收件箱' + (inboxSort === 'asc' ? '按时间正序，切换倒序' : '按时间倒序，切换正序'),
  );
  const params = inboxParameters(),
    key = JSON.stringify([
      params,
      state.messages_revision ?? state.messages,
      state.config.message_retention_days,
    ]);
  const request = ++inboxRequest;
  try {
    if (key !== inboxLoadedKey) {
      $('#inbox-summary').textContent = '正在查询…';
      const data = await rpc('messages_query', params);
      if (request !== inboxRequest || page !== 'inbox' || !$('#inbox-search')) return;
      inboxData = data;
      inboxPage = data.page;
      inboxLoadedKey = key;
    }
    const rows = inboxData.rows;
    for (const id of inboxSelected) if (!rows.some((m) => m.id === id)) inboxSelected.delete(id);
    $('#inbox-summary').textContent =
      `共 ${inboxData.total} 条 · 待分析 ${state.pending || 0} 条 · 自动保留 ${state.config.message_retention_days ?? 90} 天 · 最近读取 ${date(state.last_scan)}`;
    $('#inbox-page-label').textContent = `第 ${inboxPage} / ${inboxData.pages} 页`;
    $('[data-workflow="inbox-prev"]').disabled = inboxPage <= 1;
    $('[data-workflow="inbox-next"]').disabled = inboxPage >= inboxData.pages;
    $('#inbox-page-jump').max = inboxData.pages;
    if (document.activeElement !== $('#inbox-page-jump')) $('#inbox-page-jump').value = inboxPage;
    $('#inbox-delete-selected').textContent =
      `删除选中${inboxSelected.size ? '（' + inboxSelected.size + '）' : ''}`;
    $('#inbox-delete-selected').disabled = !inboxSelected.size;
    const checked = rows.filter((m) => inboxSelected.has(m.id)).length,
      selectAll = $('#inbox-select-all');
    selectAll.checked = rows.length > 0 && checked === rows.length;
    selectAll.indeterminate = checked > 0 && checked < rows.length;
    setHtml(
      '#inbox-content',
      rows.length
        ? `<div class="box">${rows.map((m) => `<div class="inbox-row" data-message-key="${esc(m.id)}"><input type="checkbox" aria-label="选择消息" data-inbox-select="${esc(m.id)}" ${inboxSelected.has(m.id) ? 'checked' : ''}>${senderAvatar(m.sender)}<div class="inbox-main"><div class="inbox-head"><b class="inbox-sender">${esc(m.sender)}</b>${badge(m.mine ? '我发出的' : m.private ? '私聊' : m.mention_me ? '@我' : m.mention_all ? '@全体' : '群消息')}<span class="inbox-where">${esc(m.source_name || '历史来源')} · ${esc(m.group)}</span><time class="inbox-time">${esc(date(m.time))}</time></div>${messageBody(m.text || '[' + m.kind + ']')}${messageImages(m)}<div class="inbox-actions"><button data-build="${esc(m.id)}">补建事项</button><button class="quiet" data-inbox-delete="${esc(m.id)}">删除消息</button></div></div></div>`).join('')}</div>`
        : empty('没有符合条件的消息', '可调整时间或搜索条件。已过期清理的消息不参与查询。'),
    );
  } catch (e) {
    if (request === inboxRequest && page === 'inbox') {
      $('#inbox-summary').textContent = '查询失败：' + e.message;
      inboxLoadedKey = '';
    }
  }
}
let deletingMessages = [];
function confirmMessageDelete(ids) {
  deletingMessages = ids;
  setModal(
    `<h2>删除 ${ids.length} 条消息</h2><p>从消息收件箱和上下文查询中移除这些消息，后续扫描不会再次导入。关联事项及处理记录会保留。</p><div id="modal-error" class="error-box"></div><div class="dialog-foot"><button data-action="close">取消</button><button data-workflow="delete-messages">确认删除</button></div>`,
  );
}
