let storageChoosing = false,
  storagePending = false;
async function chooseDataDirectory() {
  if (storageChoosing || storagePending) return;
  storageChoosing = true;
  try {
    if (!(await flushSettings())) throw Error('请先修正或保存当前设置');
    const directory = await rpc('pick', { kind: '选择新的数据目录', type: 'directory' });
    if (!directory) return;
    const current = state.data_directory || state.root;
    if (launcherIdentity(directory) === launcherIdentity(current)) {
      toast('数据目录未改变');
      return;
    }
    setModal(
      `<h2>更改数据目录</h2><p>是否将当前数据迁移到新目录？</p><dl class="storage-locations"><dt>当前目录</dt><dd>${esc(current)}</dd><dt>新目录</dt><dd>${esc(directory)}</dd></dl><p class="helper">选择“是”：复制当前配置、事项、消息、插件和日志，按数据大小显示进度，完成后重启。需要空文件夹；原目录的数据保留。临时文件和界面缓存会重新生成。</p><p class="helper">选择“否”：不复制旧数据，切换目录并重启。新目录已有拾事数据时使用已有数据，否则从空数据开始。</p><div id="storage-change-error" class="error-box" role="status"></div><div class="dialog-foot"><button data-action="close">取消</button><button data-storage-apply="switch">否，直接切换</button><button class="primary" data-storage-apply="copy">是，迁移数据</button></div>`,
    );
    $('#dialog').dataset.storageDirectory = directory;
  } catch (error) {
    toast(error.message);
  } finally {
    storageChoosing = false;
  }
}
async function applyDataDirectory(copy) {
  if (storagePending) return;
  const directory = $('#dialog').dataset.storageDirectory;
  if (!directory) return;
  storagePending = true;
  const buttons = [...$('#dialog').querySelectorAll('button')];
  buttons.forEach((b) => {
    b.disabled = true;
    b.dataset.pending = 'true';
  });
  $('#storage-change-error').textContent = '正在打开进度窗口，拾事将自动重启…';
  try {
    await rpc('storage_change', { directory, copy });
  } catch (error) {
    storagePending = false;
    buttons.forEach((b) => {
      b.disabled = false;
      delete b.dataset.pending;
    });
    $('#storage-change-error').textContent = error.message;
  }
}
document.addEventListener('click', (e) => {
  if (e.target.closest('[data-storage-change]')) void chooseDataDirectory();
  const apply = e.target.closest('[data-storage-apply]');
  if (apply) void applyDataDirectory(apply.dataset.storageApply === 'copy');
});
