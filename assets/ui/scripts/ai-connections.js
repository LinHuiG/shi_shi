// Explicit connection editing, discovery and save/cancel lifecycle.
function editConnection(c, isNew = false) {
  if (!leaveConnectionEditor()) return;
  closeConnectionEditor();
  connectionEditor = {
    connection: {
      ...{ executable: '', base_url: '', key_env: null, credential_id: null },
      ...structuredClone(c),
    },
    expected: isNew ? null : structuredClone(c),
    models: structuredClone(c.models || []),
    request: 0,
    isNew,
  };
  const host = isNew
    ? $('#new-connection-editor')
    : [...document.querySelectorAll('[data-connection-editor]')].find(
        (el) => el.dataset.connectionEditor === c.id,
      );
  host.innerHTML = `<form id="connection-form" class="connection-form" onsubmit="return false"><div class="connection-fields connection-basic"><label>名称<input data-ai-name maxlength="80" value="${esc(c.name || '')}"></label><label>类型<select data-ai-driver ${c.id === '' ? 'disabled' : ''}>${Object.entries(
    driverLabels,
  )
    .map(([k, v]) => `<option value="${k}" ${k === c.driver ? 'selected' : ''}>${v}</option>`)
    .join(
      '',
    )}</select></label></div><div data-ai-local><label>程序路径</label><div class="field-row"><input data-ai-path value="${esc(c.executable || '')}" spellcheck="false"><button type="button" data-ai-detect>自动检测</button><button type="button" data-ai-pick>选择文件</button></div><div class="connection-detection"><label>检测结果</label><select data-ai-candidates aria-label="检测结果" hidden></select></div></div><div data-ai-api class="connection-fields"><label>API 地址<input data-ai-base value="${esc(c.base_url || '')}" placeholder="https://api.example.com/v1" spellcheck="false"></label><label>API 密钥<input data-ai-key type="password" autocomplete="new-password" placeholder="${isNew ? '输入密钥' : '留空保持原密钥'}"></label><details class="connection-advanced"><summary>高级选项</summary><label class="check"><input type="checkbox" data-ai-stream ${c.stream !== false ? 'checked' : ''}>流式响应</label><p class="helper">持续显示供应商公开的响应和思考内容；接入不支持时可关闭。</p><label>密钥环境变量<input data-ai-key-env maxlength="64" value="${esc(c.key_env || '')}"></label><label class="check"><input type="checkbox" data-ai-key-clear>清除已存密钥</label></details></div><div class="catalog-heading"><b>模型</b><button type="button" data-ai-models>刷新模型</button></div><div class="connection-model-toolbar"><input type="search" data-ai-model-search aria-label="搜索模型" placeholder="搜索模型、编号或供应商"><div id="connection-model-controls"></div></div><div id="connection-models"></div><div class="field-row connection-model-add"><input data-ai-model-id maxlength="300" placeholder="模型编号" aria-label="模型编号"><button type="button" data-ai-model-add>添加模型</button></div><p id="connection-status" role="status"></p><div id="connection-workspace-plugin" hidden><p class="helper" data-workspace-plugin-status></p><button type="button" data-workspace-plugin-install>安装 SS 会话插件</button></div><div id="connection-setup" hidden><p class="helper">DSH 内置 ACP 环境尚未初始化，可在确认后自动配置，无需安装第三方插件。</p><button type="button" data-ai-initialize>初始化 ACP 环境</button></div><div class="editor-actions"><button type="button" data-ai-cancel>取消</button><button type="button" class="primary" data-ai-save>保存</button></div></form>`;
  connectionTypeChanged(false);
  renderConnectionModels();
  connectionEditor.baseline = connectionValue();
  $('[data-ai-name]').focus();
}
function closeConnectionEditor() {
  if (connectionEditor) connectionEditor.request++;
  $('#connection-form')?.remove();
  connectionEditor = null;
}
async function checkWorkspacePlugin(install = false) {
  const editor = connectionEditor,
    host = $('#connection-workspace-plugin');
  if (!editor || !host) return;
  const connection = connectionDraft(),
    key = JSON.stringify([connection.driver, connection.executable]);
  host.hidden = connection.driver !== 'dsh';
  if (host.hidden || !connection.executable) return;
  const button = host.querySelector('button'),
    label = host.querySelector('[data-workspace-plugin-status]');
  if (install) editor.busy = true;
  button.disabled = true;
  label.textContent = install ? '正在安装 SS 会话插件…' : '正在检查 SS 会话接入…';
  try {
    const result = await rpc('dsh_workspace_plugin', { connection, install, confirmed: install });
    if (
      connectionEditor !== editor ||
      JSON.stringify([connectionDraft().driver, connectionDraft().executable]) !== key
    )
      return;
    button.hidden = result.installed === true;
    label.textContent = result.installed
      ? 'SS 会话插件已安装，使用 DSH 原生工作区显示会话；安装后请重启 DSH。'
      : '原生工作区未自动关联时，可安装最小会话桥接插件。';
  } catch (error) {
    if (connectionEditor === editor) label.textContent = 'SS 会话插件：' + error.message;
  } finally {
    if (install) editor.busy = false;
    if (button.isConnected) {
      button.disabled = false;
      if (install) button.focus();
    }
  }
}
function connectionTypeChanged(clear = true) {
  const f = $('#connection-form');
  if (!f) return;
  $('#connection-setup').hidden = true;
  connectionEditor.setup = null;
  const api = f.querySelector('[data-ai-driver]').value === 'api';
  f.querySelector('[data-ai-local]').hidden = api;
  f.querySelector('[data-ai-api]').hidden = !api;
  if (clear) {
    connectionEditor.models = [];
    connectionEditor.catalog = [];
    connectionEditor.request++;
    renderConnectionModels();
  }
  void checkWorkspacePlugin();
}
function connectionDraft() {
  const f = $('#connection-form'),
    value = (k) => f.querySelector('[data-ai-' + k + ']').value.trim();
  return {
    ...connectionEditor.connection,
    name: value('name'),
    driver: value('driver'),
    executable: value('path'),
    base_url: value('base'),
    key_env: value('key-env') || null,
    stream: f.querySelector('[data-ai-stream]').checked,
    models: structuredClone(connectionEditor.models),
  };
}
function connectionValue() {
  return JSON.stringify([
    connectionDraft(),
    $('[data-ai-key]').value,
    $('[data-ai-key-clear]').checked,
    $('[data-ai-model-id]').value.trim(),
  ]);
}
function connectionFingerprint() {
  return connectionValue();
}
function connectionDirty() {
  return !!connectionEditor && connectionValue() !== connectionEditor.baseline;
}
function modelProvider(m) {
  let route;
  try {
    route = JSON.parse(m.model);
  } catch {}
  return (
    m.providerName ||
    m.provider ||
    (Array.isArray(route) && route.length === 2 ? route[0] : '') ||
    connectionEditor?.connection?.name ||
    '当前接入'
  );
}
function modelName(m) {
  let route;
  try {
    route = JSON.parse(m.model);
  } catch {}
  return m.displayName || (Array.isArray(route) && route.length === 2 ? route[1] : m.model);
}
function modelSourceLabel(m, catalog) {
  const source = modelProvider(m),
    same = catalog.some(
      (v) =>
        v.model !== m.model &&
        modelProvider(v) === source &&
        v.provider &&
        m.provider &&
        v.provider !== m.provider,
    );
  return `${source}${same ? ' (' + m.provider + ')' : ''}`;
}
function modelLabel(m, catalog) {
  return `${modelSourceLabel(m, catalog)} · ${modelName(m)}`;
}
function restoreConnectionFocus() {
  const form = $('#connection-form');
  const target = connectionEditor?.lastFocus;
  if (form)
    (target?.isConnected && form.contains(target)
      ? target
      : form.querySelector('input,select,button')
    )?.focus({ preventScroll: true });
}
function leaveConnectionEditor() {
  const editor = connectionEditor;
  if (!editor) return true;
  if (editor.saving || editor.busy) {
    toast(editor.saving ? '正在保存连接' : '正在读取连接，请稍后重试');
    restoreConnectionFocus();
    return false;
  }
  if (connectionDirty() && !confirm('连接修改尚未保存，是否放弃修改？\n选择“取消”继续编辑。')) {
    restoreConnectionFocus();
    return false;
  }
  closeConnectionEditor();
  return true;
}
document.addEventListener('focusin', (e) => {
  if (connectionEditor && e.target.closest('#connection-form'))
    connectionEditor.lastFocus = e.target;
});
document.addEventListener('focusout', (e) => {
  const editor = connectionEditor;
  if (
    editor &&
    e.target.closest('#connection-form') &&
    !$('#connection-form')?.contains(e.relatedTarget)
  )
    setTimeout(() => {
      if (
        connectionEditor === editor &&
        !editor.gesture &&
        !editor.internalAction &&
        document.hasFocus() &&
        !$('#connection-form')?.contains(document.activeElement)
      )
        dismissUnchangedConnectionEditor();
    }, 0);
});
document.addEventListener(
  'pointerdown',
  (e) => {
    if (connectionEditor && e.button === 0)
      connectionEditor.gesture = {
        id: e.pointerId,
        inside: !!e.target.closest('#connection-form'),
        ended: false,
      };
  },
  true,
);
document.addEventListener(
  'pointerup',
  (e) => {
    const editor = connectionEditor,
      gesture = editor?.gesture;
    if (gesture?.id !== e.pointerId) return;
    gesture.ended = true;
    setTimeout(() => {
      if (connectionEditor === editor && editor.gesture === gesture) editor.gesture = null;
    }, 0);
  },
  true,
);
document.addEventListener(
  'pointercancel',
  () => {
    if (connectionEditor) connectionEditor.gesture = null;
  },
  true,
);
// Decide before any delegated action can navigate, remove a connection or save settings.
document.addEventListener(
  'click',
  (e) => {
    const editor = connectionEditor;
    if (!editor) return;
    const inside = !!e.target.closest('#connection-form');
    if (!inside && editor.gesture?.inside && editor.gesture.ended && e.detail > 0) {
      e.preventDefault();
      e.stopImmediatePropagation();
      return;
    }
    if (inside) {
      editor.internalAction = true;
      setTimeout(() => {
        if (connectionEditor === editor) {
          if (document.hasFocus() && !$('#connection-form')?.contains(document.activeElement))
            restoreConnectionFocus();
          editor.internalAction = false;
        }
      }, 0);
      return;
    }
    if (e.target.closest('[data-page],[data-recent],[data-view],[data-prompt-section]')) return;
    if (!e.target.closest('[data-ai-add],[data-ai-edit],[data-ai-remove]')) {
      dismissUnchangedConnectionEditor();
      return;
    }
    if (!leaveConnectionEditor()) {
      e.preventDefault();
      e.stopImmediatePropagation();
    }
  },
  true,
);
function dismissUnchangedConnectionEditor() {
  if (connectionEditor && !connectionEditor.saving && !connectionEditor.busy && !connectionDirty())
    closeConnectionEditor();
}
async function saveConnectionEditor() {
  const editor = connectionEditor;
  if (!editor || editor.saving) return;
  const connection = connectionDraft();
  if (!connection.name) throw Error('请输入连接名称');
  if (!connection.models.length) throw Error('请至少勾选一个模型；可先刷新模型或手动添加');
  if (
    (connection.driver === 'api' && !connection.base_url) ||
    (connection.driver !== 'api' && !connection.executable)
  )
    throw Error('请填写连接地址或程序路径');
  editor.saving = true;
  editor.request++;
  const controls = [...$('#connection-form').querySelectorAll('input,select,button')];
  controls.forEach((el) => (el.disabled = true));
  try {
    const result = await rpc('connection_save', {
      connection,
      expected: editor.expected,
      key: connection.driver === 'api' ? $('[data-ai-key]').value : '',
      clear_key: connection.driver === 'api' && $('[data-ai-key-clear]').checked,
    });
    closeConnectionEditor();
    acceptConnectionConfig(result.config);
    toast('连接已保存');
  } finally {
    editor.saving = false;
    controls.forEach((el) => (el.disabled = false));
    if (connectionEditor === editor) restoreConnectionFocus();
  }
}
function launcherIdentity(path) {
  return (path || '').replaceAll('\\', '/').replace(/\/+$/, '').toLowerCase();
}
async function connectCandidate(path) {
  const editor = connectionEditor;
  if (!editor) return;
  const draft = connectionDraft();
  if (
    !editor.isNew &&
    editor.expected?.executable &&
    launcherIdentity(path) !== launcherIdentity(editor.expected.executable)
  ) {
    $('#connection-status').textContent = '已有配置已保留；修改路径后请刷新模型并保存。';
    return;
  }
  const existing = draftConnections().find(
    (c) => c.driver === draft.driver && launcherIdentity(c.executable) === launcherIdentity(path),
  );
  if (existing && existing.id !== draft.id) {
    closeConnectionEditor();
    toast('此程序已接入，已保留现有连接');
    return;
  }
  $('[data-ai-path]').value = path;
  if (!$('[data-ai-name]').value.trim()) $('[data-ai-name]').value = driverLabels[draft.driver];
  if (editor.isNew && draft.driver === 'codex' && !state.config.codex) {
    editor.connection.id = '';
    editor.expected = defaultConnection();
  }
  if ((await discoverConnectionModels()) && connectionEditor === editor && editor.models.length) {
    const after = connectionValue();
    if (after === editor.discoveredValue) {
      try {
        await saveConnectionEditor();
      } catch (error) {
        if (connectionEditor === editor)
          $('#connection-status').textContent = '保存接入失败：' + error.message;
      }
    }
  }
}

function renderConnectionModels() {
  const editor = connectionEditor,
    host = $('#connection-models');
  if (!host) return;
  const catalog = [
    ...new Map([...(editor.catalog || []), ...editor.models].map((m) => [m.model, m])).values(),
  ];
  editor.catalog = catalog;
  const selected = new Set(editor.models.map((m) => m.model)),
    query = (editor.modelQuery || '').toLocaleLowerCase(),
    visible = catalog
      .map((m, i) => ({ m, i }))
      .filter(({ m }) =>
        [m.model, m.displayName, m.provider, m.providerName, modelLabel(m, catalog)].some((v) =>
          String(v || '')
            .toLocaleLowerCase()
            .includes(query),
        ),
      );
  editor.visibleModels = visible.map(({ m }) => m.model);
  if (
    selected.size &&
    $('#connection-status').textContent === '请至少勾选一个模型；可先刷新模型或手动添加'
  )
    $('#connection-status').textContent = `已选择 ${selected.size} 个模型`;
  $('#connection-model-controls').innerHTML = catalog.length
    ? `<div class="connection-model-controls"><span>已选 ${selected.size} / ${catalog.length} · 显示 ${visible.length}</span><button type="button" data-ai-model-all>全选当前结果</button><button type="button" data-ai-model-invert>反选当前结果</button></div>`
    : '';
  host.innerHTML =
    visible
      .map(
        ({ m, i }) =>
          `<div class="model-chip model-catalog-row"><label class="check"><input type="checkbox" data-ai-model-toggle="${i}" ${selected.has(m.model) ? 'checked' : ''}><span class="model-catalog-text"><small>${esc(modelSourceLabel(m, catalog))}</small><strong>${esc(modelName(m))}</strong></span></label><button type="button" aria-label="删除模型 ${esc(modelLabel(m, catalog))}" data-ai-model-remove="${i}">×</button></div>`,
      )
      .join('') ||
    (catalog.length
      ? '<span class="muted">没有匹配的模型，清空关键词可查看全部。</span>'
      : '<span class="muted">暂无模型，请检测或手动添加。</span>');
}
async function discoverConnectionModels() {
  const editor = connectionEditor;
  if (!editor) return;
  const connection = connectionDraft();
  if (
    (connection.driver === 'api' && !connection.base_url) ||
    (connection.driver !== 'api' && !connection.executable)
  )
    return;
  const request = ++editor.request,
    fingerprint = connectionFingerprint();
  editor.busy = true;
  editor.setup = null;
  $('#connection-setup').hidden = true;
  $('#connection-status').textContent = '正在读取模型…';
  try {
    if (connection.driver === 'dsh') {
      const setup = await rpc('ai_connection_status', { connection });
      if (
        connectionEditor !== editor ||
        request !== editor.request ||
        fingerprint !== connectionFingerprint()
      )
        return;
      editor.setup = setup;
      void checkWorkspacePlugin();
      $('#connection-setup').hidden = !setup.can_initialize_acp;
      if (!setup.ready) {
        $('#connection-status').textContent = setup.can_initialize_acp
          ? 'ACP 环境尚未初始化。请点击下方“初始化 ACP 环境”，完成后自动刷新模型。'
          : setup.status || 'Harness 未就绪';
        return false;
      }
    }
    const data = await rpc('models', { connection, key: $('[data-ai-key]').value });
    if (
      connectionEditor !== editor ||
      request !== editor.request ||
      fingerprint !== connectionFingerprint()
    )
      return;
    const rows = Array.isArray(data?.models) ? data.models : [];
    editor.models = [
      ...new Map(
        rows
          .filter((v) => v && typeof v.model === 'string' && v.model.trim())
          .map((v) => [
            v.model,
            {
              model: v.model,
              provider: typeof v.provider === 'string' ? v.provider : undefined,
              reasoningEffortSupport: v.reasoningEffortSupport,
              providerName: typeof v.providerName === 'string' ? v.providerName : undefined,
              displayName: typeof v.displayName === 'string' ? v.displayName : v.model,
              supportedReasoningEfforts: Array.isArray(v.supportedReasoningEfforts)
                ? v.supportedReasoningEfforts
                    .filter((e) => typeof e?.reasoningEffort === 'string')
                    .map((e) => ({ reasoningEffort: e.reasoningEffort }))
                : [],
            },
          ]),
      ).values(),
    ];
    editor.catalog = structuredClone(editor.models);
    renderConnectionModels();
    $('#connection-status').textContent = editor.models.length
      ? `已读取 ${editor.models.length} 个模型`
      : '未检测到模型，请手动添加后保存。';
    editor.discoveredValue = connectionValue();
    return true;
  } catch (error) {
    if (
      connectionEditor === editor &&
      request === editor.request &&
      fingerprint === connectionFingerprint()
    )
      $('#connection-status').textContent =
        connection.driver === 'dsh'
          ? '模型检测失败：' +
            error.message +
            '\n处理后点击“刷新模型”重试；手动添加模型不能修复 ACP 启动环境。'
          : '模型检测失败，请手动添加或重试：' + error.message;
  } finally {
    editor.busy = false;
  }
}
async function initializeConnectionAcp() {
  const editor = connectionEditor;
  if (!editor || editor.busy || editor.saving || !editor.setup?.can_initialize_acp) return;
  const connection = connectionDraft(),
    fingerprint = connectionFingerprint();
  if (
    !confirm(
      `初始化 DSH 内置 ACP 环境？\n程序：${connection.executable}\n配置目录：${editor.setup.dsh_home}\n仅创建缺失的 ACP 配置，不重置已有配置或凭据，不发起 AI 推理。`,
    )
  )
    return;
  const request = ++editor.request;
  editor.busy = true;
  const button = $('[data-ai-initialize]');
  button.disabled = true;
  $('#connection-status').textContent = '正在初始化 ACP 环境…';
  try {
    await rpc('dsh_initialize_acp', { connection, confirmed: true });
    if (
      connectionEditor !== editor ||
      request !== editor.request ||
      fingerprint !== connectionFingerprint()
    )
      return;
    editor.busy = false;
    await discoverConnectionModels();
  } catch (error) {
    if (
      connectionEditor === editor &&
      request === editor.request &&
      fingerprint === connectionFingerprint()
    )
      $('#connection-status').textContent = 'ACP 初始化失败：' + error.message;
  } finally {
    editor.busy = false;
    if (button.isConnected) {
      button.disabled = false;
      if (connectionEditor === editor) button.focus();
    }
  }
}
async function detectConnection() {
  const editor = connectionEditor;
  if (!editor) return;
  const c = connectionDraft(),
    request = ++editor.request,
    original = c.executable,
    fingerprint = connectionFingerprint();
  editor.busy = true;
  $('#connection-status').textContent = '正在查找…';
  try {
    const data = await rpc('detect', { driver: c.driver, saved: original });
    if (
      connectionEditor !== editor ||
      request !== editor.request ||
      connectionDraft().driver !== c.driver ||
      fingerprint !== connectionFingerprint()
    )
      return;
    const rows =
      c.driver === 'dsh'
        ? data.candidates || []
        : data.codex
          ? [{ path: data.codex, label: 'Codex CLI' }]
          : [];
    const select = $('[data-ai-candidates]');
    select.hidden = rows.length < 2 && !original;
    select.innerHTML =
      '<option value="">选择检测结果</option>' +
      rows
        .map(
          (r) =>
            `<option value="${esc(r.path)}" title="${esc(r.path)}">${esc(r.label)}${r.version ? ' ' + esc(r.version) : ''} · ${esc(r.path.replaceAll('\\', '/').split('/').slice(-2).join('/'))}</option>`,
        )
        .join('');
    if (
      rows.length === 1 &&
      (!original || launcherIdentity(rows[0].path) === launcherIdentity(original))
    ) {
      await connectCandidate(rows[0].path);
    } else {
      $('#connection-status').textContent = rows.length
        ? original
          ? '已保留填写的路径；可选择检测结果。'
          : '找到多个安装，请选择。'
        : '未找到安装，请选择新版启动器。';
    }
  } catch (error) {
    if (connectionEditor === editor && request === editor.request)
      $('#connection-status').textContent = error.message;
  } finally {
    editor.busy = false;
  }
}
function acceptConnectionConfig(config, syncProfiles = false) {
  state.config.ai_connections = config.ai_connections;
  state.config.codex = config.codex;
  state.config.codex_models = config.codex_models || [];
  state.config.codex_name = config.codex_name || '默认 Codex';
  if (!$('#ai-connections')) return;
  $('#ai-connections').innerHTML = draftConnections().map(connectionCard).join('');
  if (syncProfiles) {
    state.config.model_profiles = config.model_profiles;
    for (const [kind, m, e] of modelFields) {
      const c = modelChoice(kind);
      $('#cfg-' + kind + '-connection').value = c.unconfigured
        ? '__unselected__'
        : c.connection_id || '';
      $('#' + m).innerHTML =
        `<option value="${esc(c.model || '')}">${esc(c.model || '未配置')}</option>`;
      $('#' + e).innerHTML = `<option value="${esc(c.reasoning_effort || '')}">默认</option>`;
    }
  }
  const routes = modelFields.map(([kind, m, e]) => [
    kind,
    m,
    e,
    $('#' + m)?.value,
    $('#' + e)?.value,
  ]);
  refreshConnectionOptions();
  loadModels();
  for (const [kind, m, e, model, effort] of routes) {
    if ($('#' + m) && model) {
      $('#' + m).value = model;
      updateEfforts(true, m);
      $('#' + e).value = effort;
    }
  }
  if (syncProfiles) {
    settingsBaseline = readSettingsValues();
    settingsDirty = false;
  }
}
document.addEventListener('input', (e) => {
  if (e.target.matches('[data-ai-model-search]') && connectionEditor) {
    connectionEditor.modelQuery = e.target.value;
    renderConnectionModels();
  }
});
document.addEventListener('change', (e) => {
  if (e.target.matches('[data-ai-model-toggle]')) {
    const editor = connectionEditor,
      index = Number(e.target.dataset.aiModelToggle),
      selected = new Set(editor.models.map((m) => m.model)),
      model = editor.catalog[index];
    if (e.target.checked) selected.add(model.model);
    else selected.delete(model.model);
    editor.models = editor.catalog.filter((m) => selected.has(m.model));
    editor.request++;
    renderConnectionModels();
    $('[data-ai-model-toggle="' + index + '"]')?.focus();
    return;
  }
  if (e.target.matches('[data-ai-driver]')) {
    connectionTypeChanged();
    return;
  }
  if (e.target.matches('[data-ai-candidates]') && e.target.value) {
    e.target.title = e.target.value;
    void connectCandidate(e.target.value);
    return;
  }
  if (e.target.matches('[data-ai-path],[data-ai-base],[data-ai-key],[data-ai-key-env]')) {
    if (e.target.matches('[data-ai-path],[data-ai-base]')) {
      $('#connection-setup').hidden = true;
      connectionEditor.setup = null;
      connectionEditor.models = [];
      connectionEditor.catalog = [];
      renderConnectionModels();
    }
    connectionEditor.request++;
    void checkWorkspacePlugin();
    return;
  }
  if (e.target.dataset.aiRoute) {
    const row = modelFields.find(([kind]) => kind === e.target.dataset.aiRoute);
    $('#' + row[1]).innerHTML = '';
    $('#' + row[2]).innerHTML = '';
    settingsDirty = true;
    loadModels();
  }
});
document.addEventListener('click', async (e) => {
  const b = e.target.closest('button');
  if (
    !b ||
    (!b.hasAttribute('data-workspace-plugin-install') &&
      !Object.keys(b.dataset).some((k) => k.startsWith('ai')))
  )
    return;
  try {
    if (b.hasAttribute('data-ai-add')) {
      if (readConnections().length >= 20) throw Error('最多配置 20 个附加连接');
      editConnection(
        {
          id: 'connection-' + Date.now().toString(36) + '-' + ++connectionSequence,
          name: '',
          driver: 'codex',
          models: [],
        },
        true,
      );
      return;
    }
    if (b.hasAttribute('data-ai-edit')) {
      editConnection(draftConnections().find((c) => c.id === b.dataset.aiEdit));
      return;
    }
    if (b.hasAttribute('data-ai-cancel')) {
      closeConnectionEditor();
      return;
    }
    if (b.hasAttribute('data-ai-remove')) {
      const id = b.dataset.aiRemove,
        connection = draftConnections().find((c) => c.id === id);
      if (!connection) throw Error('连接已变化，请刷新后重试');
      if (
        !confirm(
          `确定删除 AI 接入“${connection.name}”？\n删除后，使用此接入的用途会自动清空，已开始的处置与历史记录仍会保留。`,
        )
      )
        return;
      if (!(await flushSettings())) return;
      b.disabled = true;
      const result = await rpc('connection_delete', { id });
      closeConnectionEditor();
      acceptConnectionConfig(result.config, true);
      return;
    }
    if (b.hasAttribute('data-ai-detect')) {
      await detectConnection();
      return;
    }
    if (b.hasAttribute('data-ai-models')) {
      await discoverConnectionModels();
      return;
    }
    if (b.hasAttribute('data-workspace-plugin-install')) {
      await checkWorkspacePlugin(true);
      return;
    }
    if (b.hasAttribute('data-ai-initialize')) {
      await initializeConnectionAcp();
      return;
    }
    if (b.hasAttribute('data-ai-pick')) {
      const editor = connectionEditor;
      editor.busy = true;
      try {
        const value = await rpc('pick', {
          kind: driverLabels[connectionDraft().driver],
          type: 'file',
        });
        if (value && editor === connectionEditor) {
          $('[data-ai-path]').value = value;
          editor.request++;
        }
      } finally {
        editor.busy = false;
      }
      return;
    }
    if (b.hasAttribute('data-ai-model-add')) {
      const input = $('[data-ai-model-id]'),
        id = input.value.trim();
      if (!id) throw Error('请输入模型编号');
      if (!connectionEditor.models.some((v) => v.model === id))
        connectionEditor.models.push({ model: id, displayName: id, supportedReasoningEfforts: [] });
      connectionEditor.request++;
      input.value = '';
      renderConnectionModels();
      return;
    }
    if (b.hasAttribute('data-ai-model-remove')) {
      const editor = connectionEditor,
        model = editor.catalog[Number(b.dataset.aiModelRemove)];
      editor.request++;
      editor.models = editor.models.filter((m) => m.model !== model.model);
      editor.catalog = editor.catalog.filter((m) => m.model !== model.model);
      renderConnectionModels();
      return;
    }
    if (b.hasAttribute('data-ai-model-all') || b.hasAttribute('data-ai-model-invert')) {
      const editor = connectionEditor,
        selected = new Set(editor.models.map((m) => m.model));
      const visible = new Set(editor.visibleModels || []);
      editor.models = editor.catalog.filter((m) =>
        visible.has(m.model)
          ? b.hasAttribute('data-ai-model-all') || !selected.has(m.model)
          : selected.has(m.model),
      );
      editor.request++;
      renderConnectionModels();
      return;
    }
    if (b.hasAttribute('data-ai-save')) {
      await saveConnectionEditor();
      return;
    }
  } catch (error) {
    if ($('#connection-status')) $('#connection-status').textContent = error.message;
    else toast(error.message);
  } finally {
    b.disabled = false;
  }
});
