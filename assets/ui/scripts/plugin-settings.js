let cancelEmbeddedPluginSettings = null;

// One source editor: host owns collection state; plugins own their fields.
// Completion resolves only after the host has durably saved the source.
function showPluginSettings(settings, editor) {
  cancelEmbeddedPluginSettings?.();
  const { host } = editor;
  const dialog = $('#dialog');
  dialog.classList.remove('plugin-settings-compact');
  dialog.style.removeProperty('--plugin-content-height');
  const frame = document.createElement('iframe');
  frame.title = '消息来源配置';
  frame.setAttribute('sandbox', 'allow-scripts');
  host.replaceChildren(frame);
  delete host.dataset.pluginDirty;
  let closed = false;
  let completed = null;
  let saving = false;
  const controls = [...$('#dialog').querySelectorAll('input,button')];
  const disabled = controls.map(control => control.disabled);
  const setSaving = value => {
    saving = value;
    if (value) host.dataset.pending = 'true';
    else delete host.dataset.pending;
    controls.forEach((control, i) => { control.disabled = value || disabled[i]; });
  };
  const dispose = () => {
    if (saving && !completed) return false;
    if (closed) return;
    closed = true;
    window.removeEventListener('message', receive);
    setSaving(false);
    cancelEmbeddedPluginSettings = null;
    void rpc('plugin_settings', { session: settings.session, action: 'cancel' }).catch(() => {});
  };
  cancelEmbeddedPluginSettings = dispose;
  const receive = async (event) => {
    const request = event.data;
    if (closed || event.source !== frame.contentWindow || request?.session !== settings.session) return;
    if (request.channel === 'shishi-plugin-completed' && completed?.id === request.id) {
      dispose();
      if (completed.action === 'cancel') editor.cancel();
      else void editor.complete().catch(error => toast(error.message));
      return;
    }
    if (request.channel !== 'shishi-plugin' || completed) return;
    const { id, action, method, params, config, name, dirty, options, height } = request;
    if (!Number.isSafeInteger(id) || !['call', 'complete', 'cancel', 'suggest_name', 'set_dirty', 'pick_path', 'set_content_height'].includes(action)) return;
    if (action === 'set_content_height') {
      const valid = height === null || (typeof height === 'number' && Number.isFinite(height) && height > 0 && height <= 100_000);
      if (valid) {
        dialog.classList.toggle('plugin-settings-compact', height !== null);
        if (height === null) dialog.style.removeProperty('--plugin-content-height');
        else dialog.style.setProperty('--plugin-content-height', Math.min(4000, Math.max(100, Math.ceil(height))) + 'px');
      }
      frame.contentWindow.postMessage({ channel: 'shishi-plugin-result', id, result: valid }, '*');
      return;
    }
    if (saving) {
      if (action === 'suggest_name' || action === 'set_dirty')
        frame.contentWindow.postMessage({ channel: 'shishi-plugin-result', id, result: false }, '*');
      else frame.contentWindow.postMessage({ channel: 'shishi-plugin-result', id, error: '请等待当前操作完成' }, '*');
      return;
    }
    if (action === 'cancel') {
      completed = { id, action };
      frame.contentWindow.postMessage({ channel: 'shishi-plugin-result', id, result: true, complete: true }, '*');
      return;
    }
    try {
      if (action === 'pick_path') {
        if (!options || !['file', 'directory'].includes(options.kind)) throw Error('请选择文件或目录');
        setSaving(true);
        try {
          const result = await rpc('pick', { type: options.kind, kind: options.title || '路径', default_path: options.defaultPath || '', filters: options.filters || [] });
          if (!closed) frame.contentWindow.postMessage({ channel: 'shishi-plugin-result', id, result: result || null }, '*');
        } finally { setSaving(false); }
        return;
      }
      if (action === 'set_dirty') {
        if (typeof dirty !== 'boolean') throw Error('配置修改状态无效');
        host.dataset.pluginDirty = String(dirty);
        frame.contentWindow.postMessage({ channel: 'shishi-plugin-result', id, result: true }, '*');
        return;
      }
      if (action === 'suggest_name') {
        const result = editor.suggestName(name);
        frame.contentWindow.postMessage({ channel: 'shishi-plugin-result', id, result }, '*');
        return;
      }
      const source = action === 'complete' ? editor.source() : undefined;
      if (action === 'complete') setSaving(true);
      const result = await rpc('plugin_settings', { session: settings.session, action, method, params, config, source });
      if (closed) return;
      if (action === 'complete') completed = { id, action };
      frame.contentWindow.postMessage({ channel: 'shishi-plugin-result', id, result, complete: action === 'complete' }, '*');
    } catch (error) {
      if (action === 'complete') setSaving(false);
      if (!closed) frame.contentWindow.postMessage({ channel: 'shishi-plugin-result', id, error: error.message }, '*');
    }
  };
  window.addEventListener('message', receive);
  const session = JSON.stringify(settings.session).replace(/</g, '\\u003c');
  const bridge = `<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; connect-src 'none'; frame-src 'none'; base-uri 'none'; form-action 'none'"><${'script'}>(()=>{
    let next=0,explicitDirty=false;const pending=new Map();
    function send(action,fields){return new Promise((resolve,reject)=>{const id=++next;pending.set(id,{resolve,reject});parent.postMessage({channel:'shishi-plugin',session:${session},id,action,...fields},'*');});}
    addEventListener('message',event=>{const v=event.data;if(event.source!==parent||v?.channel!=='shishi-plugin-result')return;const p=pending.get(v.id);if(!p)return;pending.delete(v.id);v.error?p.reject(Error(v.error)):p.resolve(v.result);if(v.complete)queueMicrotask(()=>parent.postMessage({channel:'shishi-plugin-completed',session:${session},id:v.id},'*'));});
    // Existing plugins conservatively keep edits open. New plugins report their
    // actual draft state so searching and virtualized controls do not mark edits.
    for(const event of ['input','change'])addEventListener(event,e=>{if(!explicitDirty&&e.isTrusted&&e.target.matches('input,select,textarea,[contenteditable]'))void send('set_dirty',{dirty:true});});
    window.shishiPlugin=Object.freeze({pickPath:options=>send('pick_path',{options}),setContentHeight:height=>send('set_content_height',{height}),call:(method,params={})=>send('call',{method,params}),suggestSourceName:name=>send('suggest_name',{name}),setDirty:dirty=>{explicitDirty=true;return send('set_dirty',{dirty});},complete:config=>send('complete',{config}),cancel:()=>send('cancel',{})});
  })();<\/script>`;
  frame.srcdoc = bridge + settings.html;
}
