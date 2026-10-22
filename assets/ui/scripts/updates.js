let updateCheckBusy = false,
  updateInstallBusy = false,
  requestedUpdateNotice = false;
const dismissedUpdates = new Set(),
  handledUpdateInstalls = new Set();
function aboutSection() {
  return `<section class="settings-card" id="settings-about"><header class="settings-section-head about-head"><h2>关于我们</h2><span class="tiny about-version">版本 ${esc(state.version || '—')}</span><span class="tiny about-build" id="version-build-time" title="发布时间">${state.build_time ? '发布于 ' + esc(date(state.build_time)) : ''}</span><div class="about-update-controls"><label class="check"><input id="cfg-auto-update" class="setting-switch" type="checkbox" role="switch" ${state.config.auto_update !== false ? 'checked' : ''}>自动更新</label><button data-update-check></button></div></header><p class="about-description">拾事是一款 AI 事项管理工具，帮助你从消息中整理事项、跟进进展。<br>项目在 GitHub 开源维护，欢迎交流反馈。</p><div class="about-links"><a data-about-link="github" href="https://github.com/LinHuiG/shi_shi" target="_blank" rel="noopener noreferrer"><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 .8a11.3 11.3 0 0 0-3.6 22c.6.1.8-.2.8-.5v-2c-3.2.7-3.9-1.4-3.9-1.4-.5-1.3-1.3-1.6-1.3-1.6-1.1-.8.1-.8.1-.8 1.2.1 1.8 1.2 1.8 1.2 1 1.8 2.7 1.3 3.4 1 .1-.8.4-1.3.7-1.6-2.6-.3-5.3-1.3-5.3-5.6 0-1.2.4-2.2 1.2-3-.1-.3-.5-1.5.1-3 0 0 1-.3 3.1 1.2a10.8 10.8 0 0 1 5.7 0C17 5.2 18 5.5 18 5.5c.6 1.5.2 2.7.1 3 .8.8 1.2 1.8 1.2 3 0 4.3-2.7 5.3-5.3 5.6.4.3.8 1 .8 2.1v3.1c0 .3.2.6.8.5A11.3 11.3 0 0 0 12 .8Z"/></svg>GitHub</a><span aria-hidden="true">·</span><a data-about-link="blog" href="https://blog.linhui.fun/" target="_blank" rel="noopener noreferrer">博客首页</a><span aria-hidden="true">·</span><a data-about-link="intro" href="https://blog.linhui.fun/posts/shishi-ai-context-engineering/" target="_blank" rel="noopener noreferrer">拾事介绍</a></div><p class="helper update-status" id="update-status" role="status"></p></section>`;
}
function updateCheckCooling(u = state?.updates) {
  return (
    u?.status === 'current' && Number.isFinite(u.current_until) && Date.now() < u.current_until
  );
}
function updateRetryWaiting(u = state?.updates) {
  return u?.status === 'failed' && Number.isFinite(u.retry_at) && Date.now() < u.retry_at;
}
function syncUpdateControls() {
  const u = state?.updates || {},
    busy =
      updateCheckBusy ||
      updateInstallBusy ||
      ['checking', 'downloading', 'installing'].includes(u.status),
    cooling = updateCheckCooling(u),
    retryWaiting = updateRetryWaiting(u),
    button = $('[data-update-check]');
  if (button) {
    button.disabled = busy || cooling || retryWaiting;
    button.setAttribute('aria-busy', String(busy));
    button.innerHTML = busy
      ? `<span class="spin" aria-hidden="true"></span>${u.status === 'downloading' ? `下载中 ${u.progress || 0}%` : u.status === 'installing' || updateInstallBusy ? '正在更新…' : '正在检查…'}`
      : cooling
        ? '暂无更新'
        : retryWaiting
          ? '稍后重试'
          : u.status === 'ready'
            ? '立即更新'
            : '检查更新';
  }
  const label = $('#update-status');
  if (label)
    label.textContent =
      u.error ||
      {
        idle: '',
        checking: '',
        current: '',
        downloading: `正在下载 ${u.version} · ${u.progress || 0}%`,
        ready: `${u.version} 已下载并校验，可以更新重启`,
        installing: '正在退出应用并安装更新…',
      }[u.status] ||
      '';
}
function updateConfirmation() {
  const u = state?.updates;
  if (u?.status !== 'ready' || !$('#overlay').hidden || connectionEditor || updateInstallBusy)
    return false;
  setModal(
    `<h2>新版本已准备好</h2><p>版本 ${esc(u.version)} 已下载并通过校验，是否更新并重启拾事？</p><p class="helper">更新会保留事项、消息和配置。正在进行的本地 AI 工作需先结束。${state.config.auto_update !== false ? '每天凌晨 04:00 会检查本地新包并自动安装重启，错过不补执行。' : ''}</p><div class="dialog-foot"><button data-update-later>稍后更新</button><button class="primary" data-update-confirm>立即更新</button></div>`,
  );
  return true;
}
function syncUpdateNotice() {
  syncUpdateControls();
  const u = state?.updates,
    enabled = state?.config.auto_update !== false,
    active = (request) =>
      request?.id && Date.now() >= request.expires_at - 60000 && Date.now() < request.expires_at;
  const install = u?.automatic_install;
  if (
    enabled &&
    u?.status === 'ready' &&
    active(install) &&
    !handledUpdateInstalls.has(install.id) &&
    !updateInstallBusy
  ) {
    handledUpdateInstalls.add(install.id);
    void installUpdate(install.id);
    return;
  }
  if (['failed', 'current', 'idle'].includes(u?.status)) requestedUpdateNotice = false;
  const reminder =
    enabled && active(u?.reminder) && !dismissedUpdates.has(u.reminder.id) ? u.reminder : null;
  if (
    u?.status === 'ready' &&
    (requestedUpdateNotice || reminder) &&
    !document.hidden &&
    !settingsChanged() &&
    updateConfirmation()
  ) {
    requestedUpdateNotice = false;
    if (reminder) dismissedUpdates.add(reminder.id);
  }
}
async function checkUpdate() {
  if (state?.updates?.status === 'ready') {
    updateConfirmation();
    return;
  }
  if (
    updateCheckBusy ||
    updateCheckCooling() ||
    updateRetryWaiting() ||
    ['checking', 'downloading', 'installing'].includes(state?.updates?.status)
  )
    return;
  updateCheckBusy = true;
  syncUpdateControls();
  try {
    if (!(await flushSettings())) return;
    state.updates = await rpc('update_check');
    requestedUpdateNotice = true;
    syncUpdateNotice();
  } catch (error) {
    requestedUpdateNotice = false;
    toast(error.message);
  } finally {
    updateCheckBusy = false;
    syncUpdateControls();
  }
}
async function installUpdate(scheduled = null) {
  if (updateInstallBusy) return;
  updateInstallBusy = true;
  const confirm = $('[data-update-confirm]');
  if (confirm) {
    confirm.disabled = true;
    confirm.dataset.pending = 'true';
    confirm.textContent = '正在准备…';
  }
  syncUpdateControls();
  try {
    if (!(await flushSettings())) return;
    await rpc('update_install', scheduled ? { scheduled } : {});
    if (state.updates) state.updates.status = 'installing';
    if (confirm) delete confirm.dataset.pending;
    closeModal();
  } catch (error) {
    toast(error.message);
  } finally {
    if (confirm?.isConnected) {
      confirm.disabled = false;
      delete confirm.dataset.pending;
      confirm.textContent = '立即更新';
    }
    updateInstallBusy = false;
    syncUpdateControls();
  }
}
document.addEventListener('click', (e) => {
  if (e.target.closest('[data-update-check]')) void checkUpdate();
  if (e.target.closest('[data-update-later]')) closeModal();
  if (e.target.closest('[data-update-confirm]')) void installUpdate();
});
document.addEventListener('click', (e) => {
  const link = e.target.closest('[data-about-link]');
  if (link) {
    e.preventDefault();
    void rpc('about_open', { target: link.dataset.aboutLink }).catch((error) =>
      toast(error.message),
    );
  }
});
