let inboxSort = 'desc',
  inboxFrom = '',
  inboxUntil = '',
  inboxFilter = '',
  inboxLoadedKey = '',
  inboxFeed = null;
const inboxSelected = new Set();
function inboxRows() {
  return inboxFeed?.rows || [];
}
function releaseInbox() {
  inboxFeed?.dispose();
  inboxFeed = null;
  inboxFilter = '';
  inboxLoadedKey = '';
  inboxSelected.clear();
  clearHtml($('#inbox-content'));
}
function inboxParameters() {
  const p = { query: inboxQuery, sort: inboxSort };
  if (inboxFrom) p.since = new Date(inboxFrom + ':00+08:00').getTime();
  if (inboxUntil) p.until = new Date(inboxUntil + ':00+08:00').getTime() + 59999;
  return p;
}
async function renderInbox() {
  if (page !== 'inbox') return;
  const params = inboxParameters(),
    filter = JSON.stringify(params);
  if (!$('#inbox-search'))
    setHtml(
      '#view',
      `<div class="inbox-tools"><div class="inbox-filters"><label class="check"><input type="checkbox" id="inbox-select-all">选中已加载</label><button data-workflow="delete-selected" id="inbox-delete-selected">删除选中</button><input id="inbox-search" aria-label="搜索消息" maxlength="200" placeholder="搜索消息" value="${esc(inboxQuery)}"><label>开始 <input type="datetime-local" id="inbox-from" value="${esc(inboxFrom)}"></label><label>结束 <input type="datetime-local" id="inbox-until" value="${esc(inboxUntil)}"></label><button data-workflow="clear-time">全部时间</button>${help('可搜索消息、发送人、会话和来源；过期清理设置位于设置页。')}</div><div class="row inbox-query-status"><button data-workflow="inbox-sort" id="inbox-sort"></button><span id="inbox-summary" class="tiny"></span><button class="quiet inbox-retention" data-workflow="retention-settings">保留设置</button></div></div><div class="page" id="inbox-content"></div>`,
    );
  if (!inboxFeed || inboxFilter !== filter) {
    releaseInbox();
    inboxFilter = filter;
    setHtml('#inbox-content', '');
    $('#view').scrollTop = 0;
    inboxFeed = pageFeed({
      root: $('#view'),
      read: (p) => rpc('messages_query', { ...params, ...p }),
      key: (m) => m.id,
      render: paintInbox,
      alive: () => page === 'inbox' && !!$('#inbox-search'),
    });
  }
  $('#inbox-sort').textContent = inboxSort === 'asc' ? '时间 ↑ 正序' : '时间 ↓ 倒序';
  $('#inbox-sort').setAttribute(
    'aria-label',
    '收件箱' + (inboxSort === 'asc' ? '按时间正序，切换倒序' : '按时间倒序，切换正序'),
  );
  const key = JSON.stringify([filter, state.messages_revision, state.config.message_retention_days]);
  if (key !== inboxLoadedKey) {
    inboxLoadedKey = key;
    await inboxFeed.load(inboxFeed.page > 0);
  } else inboxFeed.paint();
}
function paintInbox(feed) {
  const rows = feed.rows;
  for (const id of inboxSelected) if (!rows.some((m) => m.id === id)) inboxSelected.delete(id);
  motionText(
    $('#inbox-summary'),
    `已加载 ${rows.length}${feed.data ? ' / ' + feed.data.total : ''} 条 · 待分析 ${state.pending} 条 · 自动保留 ${state.config.message_retention_days} 天 · 最近读取 ${date(state.last_scan)}`,
  );
  $('#inbox-delete-selected').textContent = `删除选中${inboxSelected.size ? '（' + inboxSelected.size + '）' : ''}`;
  $('#inbox-delete-selected').disabled = !inboxSelected.size;
  const checked = rows.filter((m) => inboxSelected.has(m.id)).length,
    selectAll = $('#inbox-select-all');
  selectAll.checked = rows.length > 0 && checked === rows.length;
  selectAll.indeterminate = checked > 0 && checked < rows.length;
  setHtml(
    '#inbox-content',
    (rows.length
      ? `<div class="box">${rows.map((m) => `<div class="inbox-row" data-message-key="${esc(m.id)}" data-feed-key="${esc(m.id)}"><input type="checkbox" aria-label="选择消息" data-inbox-select="${esc(m.id)}" ${inboxSelected.has(m.id) ? 'checked' : ''}>${senderAvatar(m.sender)}<div class="inbox-main"><div class="inbox-head"><b class="inbox-sender">${esc(m.sender)}</b>${badge(m.mine ? '我发出的' : m.private ? '私聊' : m.mention_me ? '@我' : m.mention_all ? '@全体' : '群消息')}<span class="inbox-where">${esc(m.source_name || '历史来源')} · ${esc(m.group)}</span><time class="inbox-time">${esc(date(m.time))}</time></div>${messageBody(m.text || '[' + m.kind + ']')}${messageImages(m)}<div class="inbox-actions"><button class="quiet small" data-build="${esc(m.id)}">补建事项</button><button class="quiet small" data-inbox-delete="${esc(m.id)}">删除消息</button></div></div></div>`).join('')}</div>`
      : !feed.loading && !feed.error
        ? empty('没有符合条件的消息', '可调整时间或搜索条件。已过期清理的消息不参与查询。')
        : '') + pageFeedStatus(feed),
  );
}
let deletingMessages = [];
function confirmMessageDelete(ids) {
  deletingMessages = ids;
  setModal(
    `<h2>删除 ${ids.length} 条消息</h2><p>从消息收件箱和上下文查询中移除这些消息，后续扫描不会再次导入。关联事项及处理记录会保留。</p><div id="modal-error" class="error-box"></div><div class="dialog-foot"><button data-action="close">取消</button><button data-workflow="delete-messages">确认删除</button></div>`,
  );
}
