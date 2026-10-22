// Shared by the main interface and the native desktop approval window.
function approvalAllowed(p, decision) {
  return (
    !Array.isArray(p.availableDecisions) ||
    p.availableDecisions.some((x) => JSON.stringify(x) === JSON.stringify(decision))
  );
}
function approvalBatchEligible(q) {
  const p = q.params || {};
  if (q.method === 'item/commandExecution/requestApproval')
    return !!p.command && !p.additionalPermissions && approvalAllowed(p, 'accept');
  if (q.method === 'item/fileChange/requestApproval')
    return !!(p.changes || p.fileChanges)?.length && approvalAllowed(p, 'accept');
  if (q.method === 'item/permissions/requestApproval') {
    const s = approvalPermissions(p.permissions);
    return !s.unknown && s.rows.length > 0;
  }
  return false;
}
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
function approvalPermissions(value) {
  const rows = [];
  if (!value || typeof value !== 'object' || Array.isArray(value)) return { rows, unknown: true };
  let unknown = false;
  for (const [key, scope] of Object.entries(value)) {
    if (scope == null) continue;
    if (['network', 'networkAccess'].includes(key)) {
      if (typeof scope === 'boolean') rows.push(['网络访问', scope ? '允许联网' : '不申请联网']);
      else if (scope && typeof scope.enabled === 'boolean') {
        rows.push(['网络访问', scope.enabled ? '允许联网' : '不申请联网']);
        for (const [k, v] of Object.entries(scope)) {
          if (k === 'enabled') continue;
          if (['allowedDomains', 'allowed_domains'].includes(k))
            rows.push(['目标域名', approvalText(v)]);
          else unknown = true;
        }
      } else unknown = true;
    } else if (
      ['fileSystem', 'file_system', 'filesystem'].includes(key) &&
      scope &&
      typeof scope === 'object'
    ) {
      for (const [mode, paths] of Object.entries(scope)) {
        if (paths == null) continue;
        const label = {
          read: '读取文件',
          write: '写入文件',
          readPaths: '读取文件',
          writePaths: '写入文件',
        }[mode];
        if (label && (typeof paths === 'string' || Array.isArray(paths)))
          rows.push([label, approvalText(paths)]);
        else unknown = true;
      }
    } else unknown = true;
  }
  return { rows, unknown };
}
function approvalDialog(run, task, request, showBatch = true) {
  const id = run.task_id;
  const method = request.method,
    p = request.params || {},
    key = JSON.stringify(request.id);
  let title = '查看请求',
    intro = '当前 AI 接入正在等待你的决定。',
    kind = '请求',
    content = '',
    buttons = '',
    unsupported = false;
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
  } else if (method === 'item/commandExecution/requestApproval') {
    kind = '命令执行';
    title = '允许执行这条命令吗？';
    intro = '选择本次允许，或为当前命令申请更长的授权范围。';
    content =
      approvalField('执行原因', p.reason || p.justification || '请求未提供原因') +
      approvalField('工作目录', p.cwd || run.cwd || '请求未提供工作目录') +
      approvalField('命令', p.command, true);
    if (p.networkApprovalContext)
      content += approvalField(
        '网络访问',
        p.networkApprovalContext.host ||
          p.networkApprovalContext.hostname ||
          p.networkApprovalContext,
        true,
      );
    if (p.additionalPermissions) {
      const scope = approvalPermissions(p.additionalPermissions);
      content += scope.rows.map(([k, v]) => approvalField(k, v, true)).join('');
      if (scope.unknown) {
        unsupported = true;
        content += '<p class="approval-notice">附加权限无法完整展示，此处不授权。</p>';
      }
    }
    if (!p.command) {
      content +=
        '<p class="approval-notice">命令内容未提供，请先在 Codex 中核对；此处不直接授权。</p>';
      unsupported = true;
    }
  } else if (method === 'item/fileChange/requestApproval') {
    kind = '文件修改';
    title = '允许修改这些文件吗？';
    intro = '本次允许仅批准当前变更；本会话允许会扩大到会话内的文件修改授权。';
    content = approvalField('修改原因', p.reason || '请求未提供原因');
    const changes = p.changes || p.fileChanges || [];
    const items = Array.isArray(changes)
      ? changes
      : Object.entries(changes).map(([path, change]) => ({
          path,
          ...(typeof change === 'object' ? change : { diff: change }),
        }));
    content += items
      .map(
        (change) =>
          `<article class="approval-file"><div class="approval-file-title"><span>${esc({ add: '新增', delete: '删除', update: '修改' }[change.kind?.type || change.kind] || '变更')}</span><b>${esc(change.path || change.file || '未提供文件路径')}</b></div>${change.diff ? `<pre>${esc(change.diff)}</pre>` : ''}</article>`,
      )
      .join('');
    if (p.grantRoot) content += approvalField('申请写入范围', p.grantRoot, true);
    if (!items.length) {
      content +=
        '<p class="approval-notice">本次请求没有附带文件清单或差异。请在相应客户端中核对具体修改后处理。</p>';
      unsupported = true;
    }
  } else if (method === 'item/permissions/requestApproval') {
    kind = '权限申请';
    title = '允许本轮使用这些权限吗？';
    intro = '可批准本轮权限，或在本会话内保留下面列出的全部权限。不会关闭其他审批。';
    content = approvalField('申请原因', p.reason || '请求未提供原因');
    const summary = approvalPermissions(p.permissions);
    content += summary.rows.map(([label, value]) => approvalField(label, value, true)).join('');
    if (summary.unknown || !summary.rows.length) {
      content +=
        '<p class="approval-notice">包含尚未支持展示的权限字段。为避免遗漏授权范围，请在相应客户端中处理。</p>';
      unsupported = true;
    }
  } else if (method === 'item/tool/requestUserInput') {
    kind = '补充信息';
    title = '当前 AI 接入需要你补充信息';
    intro = '选择建议答案，或填写自己的回答。';
    content = (p.questions || [])
      .map(
        (question, i) =>
          `<fieldset class="approval-question"><legend>${esc(question.question || question.header || '请补充说明')}</legend>${(question.options || []).map((option, n) => `<label class="approval-choice"><input type="radio" name="approval-choice-${i}" data-approval-option="${i}" value="${esc(option.label)}"><span><b>${esc(option.label)}</b>${option.description ? `<small>${esc(option.description)}</small>` : ''}</span></label>`).join('')}<label class="approval-answer-label" for="approval-answer-${i}">你的回答</label>${question.isSecret ? `<input type="password" autocomplete="off" id="approval-answer-${i}" data-handling-answer="${i}" placeholder="输入回答">` : `<textarea id="approval-answer-${i}" data-handling-answer="${i}" placeholder="选择上方选项，或输入自己的回答"></textarea>`}</fieldset>`,
      )
      .join('');
    buttons =
      '<button class="primary" data-handle="answer" data-decision="answer">提交回答</button>';
  } else if (method === 'mcpServer/elicitation/request') {
    kind = '外部服务';
    title = '外部服务需要确认';
    intro = '此类授权需要在相应客户端中继续处理。';
    content =
      approvalField('服务', p.serverName || p.server_name) +
      approvalField('请求内容', p.message || p.reason || '外部服务请求授权');
    buttons =
      '<button class="approval-deny" data-handle="answer" data-decision="decline">拒绝请求</button>';
  } else {
    intro = '此请求暂不支持在拾事中处理，请在 Codex 中继续。';
    unsupported = true;
  }
  if (
    [
      'item/commandExecution/requestApproval',
      'item/fileChange/requestApproval',
      'item/permissions/requestApproval',
    ].includes(method)
  ) {
    const permission = method === 'item/permissions/requestApproval';
    buttons = approvalAllowed(p, 'decline')
      ? '<button class="approval-deny" data-handle="answer" data-decision="decline">拒绝</button>'
      : '';
    if (!unsupported) {
      if (approvalAllowed(p, 'accept'))
        buttons += `<button class="primary" data-handle="answer" data-decision="accept">${permission ? '允许本轮权限' : '仅允许本次'}</button>`;
      if (approvalAllowed(p, 'acceptForSession'))
        buttons += `<button data-handle="answer" data-decision="acceptForSession">${permission ? '本会话批准以上全部权限' : '本会话允许'}</button>`;
      const rule = p.proposedExecpolicyAmendment;
      if (
        method === 'item/commandExecution/requestApproval' &&
        Array.isArray(rule) &&
        rule.length &&
        rule.every((x) => typeof x === 'string' && x.trim()) &&
        approvalAllowed(p, { acceptWithExecpolicyAmendment: { execpolicy_amendment: rule } })
      ) {
        content +=
          approvalField('类似命令匹配规则（参数前缀）', JSON.stringify(rule), true) +
          '<p class="approval-notice">允许类似命令会保存此规则，可能对以后会话生效；不是仅批准当前这条命令。</p>';
        buttons +=
          '<button data-handle="answer" data-decision="acceptSimilar">允许类似命令</button>';
      }
    }
  }
  const batch = (run.requests || []).filter(approvalBatchEligible);
  if (showBatch && batch.length > 1)
    buttons += `<button data-handle="batch" data-task-id="${esc(id)}">全部批准…（当前 ${batch.length} 项）</button>`;

  return `<div class="approval-dialog"><header><div class="approval-kicker"><span class="approval-symbol">◇</span> ${esc(kind)}<span class="approval-count">${run.requests.length > 1 ? `1 / ${run.requests.length} 个待处理` : '等待确认'}</span></div><h2>${esc(title)}</h2><p class="subtitle">${esc(intro)}</p></header><div class="approval-context"><span>${esc(run.project_name || '当前项目')}</span><span>${esc(task?.title || '当前事项')}</span></div><div class="approval-scroll"><div class="approval-body">${content}</div><details class="approval-technical"><summary>技术详情</summary><div class="helper">${esc(method)}</div><pre>${esc(JSON.stringify(p, null, 2))}</pre></details><div id="modal-error" class="error-box"></div></div><div class="dialog-foot approval-footer" id="handling-request" data-task-id="${esc(id)}" data-run-id="${esc(run.id)}" data-thread-id="${esc(run.thread_id)}" data-request-id="${esc(key)}"><button class="quiet" data-action="close">稍后处理</button><div>${buttons}</div></div></div>`;
}
