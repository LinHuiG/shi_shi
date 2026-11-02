// Shared by the main interface and the native desktop approval window.
const approvalText = (v) =>
  typeof v === 'string'
    ? v
    : Array.isArray(v)
      ? v.map((x) => (typeof x === 'string' ? x : JSON.stringify(x))).join('\n')
      : v == null
        ? ''
        : JSON.stringify(v, null, 2);
function approvalField(label, value, code = false) {
  const text = approvalText(value);
  return text
    ? `<div class="approval-field"><span>${esc(label)}</span>${code ? `<pre>${esc(text)}</pre>` : `<div>${esc(text)}</div>`}</div>`
    : '';
}
function approvalDialog(run, task, request) {
  const id = run.task_id;
  const method = request.method,
    p = request.params || {},
    key = JSON.stringify(request.id);
  let title = '查看请求',
    intro = '当前 AI 接入正在等待你的决定。',
    kind = '请求',
    content = '',
    buttons = '';
  if (method === 'dsh/tool/requestApproval') {
    kind = 'Harness 工具权限';
    title = '允许这次工具请求吗？';
    intro = '核对工具参数和操作范围后，选择 Harness 提供的授权选项。';
    content =
      approvalField('工具', p.tool_call?.title) +
      approvalField('工具参数与操作范围', p.tool_call, true);
    const options = Array.isArray(p.dsh_options) ? p.dsh_options : [];
    buttons = options
      .filter((o) =>
        ['allow_once', 'allow_always', 'reject_once', 'reject_always'].includes(o.kind),
      )
      .map(
        (o) =>
          `<button data-handle="answer" data-decision="dshOption" data-option-id="${esc(o.id)}" class="${o.kind.startsWith('reject') ? 'approval-deny' : ''}">${esc(o.name)} · ${esc({ allow_once: '仅本次', allow_always: '持续授权', reject_once: '拒绝本次', reject_always: '持续拒绝' }[o.kind])}</button>`,
      )
      .join('');
    if (options.some((o) => o.kind === 'allow_always'))
      content +=
        '<p class="approval-notice">持续授权由 Harness 管理，可能影响后续请求；仅本次不会扩大到以后。</p>';
  } else {
    intro = '此请求暂不支持在拾事中处理，请在相应客户端中继续。';
  }
  return `<div class="approval-dialog"><header><div class="approval-kicker"><span class="approval-symbol">◇</span> ${esc(kind)}<span class="approval-count">${run.requests.length > 1 ? `1 / ${run.requests.length} 个待处理` : '等待确认'}</span></div><h2>${esc(title)}</h2><p class="subtitle">${esc(intro)}</p></header><div class="approval-context"><span>${esc(run.project_name || '当前项目')}</span><span>${esc(task?.title || '当前事项')}</span></div><div class="approval-scroll"><div class="approval-body">${content}</div><details class="approval-technical"><summary>技术详情</summary><div class="helper">${esc(method)}</div><pre>${esc(JSON.stringify(p, null, 2))}</pre></details><div id="modal-error" class="error-box"></div></div><div class="dialog-foot approval-footer" id="handling-request" data-task-id="${esc(id)}" data-run-id="${esc(run.id)}" data-thread-id="${esc(run.thread_id)}" data-request-id="${esc(key)}"><button class="quiet" data-action="close">稍后处理</button><div>${buttons}</div></div></div>`;
}
